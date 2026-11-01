//! ksm_mark — root 下跨进程标记匿名内存为 MADV_MERGEABLE
//!
//! 设计目标：
//!   * 单线程，无锁、无原子、无共享状态 —— 天然无竞争
//!   * unsafe 只出现在 3 个极小点上（下文标 UNSAFE），其余全部 safe Rust
//!
//! 用法:
//!   ./ksm_mark              # 标记所有进程
//!   ./ksm_mark 1234 5678    # 只标记指定 PID

use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

const MADV_MERGEABLE: libc::c_int = 12;
const IOV_MAX: usize = 1024;

// aarch64 / x86_64 / armv7 上这两个 syscall 号相同
const NR_PIDFD_OPEN: libc::c_long = 434;
const NR_PROCESS_MADVISE: libc::c_long = 440;

// ============================================================
// ▍UNSAFE 区：只包两个 syscall，各 5 行，只做参数透传和 errno 检查。
// ============================================================

fn pidfd_open(pid: i32) -> io::Result<OwnedFd> {
    // UNSAFE #1：原始 syscall
    let ret = unsafe { libc::syscall(NR_PIDFD_OPEN, pid as libc::c_long, 0 as libc::c_uint) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    // UNSAFE #2：内核刚返回一个合法的新 fd，语义上归我们所有
    Ok(unsafe { OwnedFd::from_raw_fd(ret as RawFd) })
}

fn process_madvise(pidfd: &OwnedFd, iov: &[libc::iovec], advice: libc::c_int) -> io::Result<usize> {
    // UNSAFE #3：原始 syscall，iov 是连续内存、life 覆盖整个调用
    let ret = unsafe {
        libc::syscall(
            NR_PROCESS_MADVISE,
            pidfd.as_raw_fd(),
            iov.as_ptr() as *const libc::c_void,
            iov.len() as libc::c_long,
            advice,
            0 as libc::c_uint,
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret as usize)
    }
}

// ============================================================
// ▍以下全部 safe 代码
// ============================================================

/// /proc/<pid>/maps 中一行 rw-p 的匿名映射
#[derive(Debug, Clone, Copy)]
struct AnonymousRange {
    start: u64,
    end: u64,
}

fn is_private_rw(perms: &str) -> bool {
    let b = perms.as_bytes();
    b.len() >= 4 && b[0] == b'r' && b[1] == b'w' && b[3] == b'p'
}

fn is_anon(path: &str) -> bool {
    path.is_empty() || path == "[heap]" || path == "[stack]" || path.starts_with("[anon")
}

fn parse_maps_line(line: &str) -> Option<AnonymousRange> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }
    if !is_private_rw(parts[1]) {
        return None;
    }
    let path = parts.get(5).copied().unwrap_or("");
    if !is_anon(path) {
        return None;
    }
    let (s, e) = parts[0].split_once('-')?;
    let start = u64::from_str_radix(s, 16).ok()?;
    let end = u64::from_str_radix(e, 16).ok()?;
    if end <= start {
        return None;
    }
    Some(AnonymousRange { start, end })
}

fn collect_ranges(pid: i32) -> io::Result<Vec<AnonymousRange>> {
    let f = File::open(format!("/proc/{}/maps", pid))?;
    let mut ranges = Vec::with_capacity(64);
    for line in BufReader::new(f).lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if let Some(r) = parse_maps_line(&line) {
            ranges.push(r);
        }
    }
    Ok(ranges)
}

fn to_iovec(ranges: &[AnonymousRange]) -> Vec<libc::iovec> {
    ranges
        .iter()
        .map(|r| libc::iovec {
            iov_base: r.start as usize as *mut libc::c_void,
            iov_len: (r.end - r.start) as libc::size_t,
        })
        .collect()
}

/// 标记单个进程；返回成功标记的字节数。无锁、无共享状态。
fn mark_pid(pid: i32) -> io::Result<u64> {
    let pidfd = pidfd_open(pid)?;
    let ranges = collect_ranges(pid)?;
    if ranges.is_empty() {
        return Ok(0);
    }
    let iovs = to_iovec(&ranges);

    let mut total: u64 = 0;
    // IOV_MAX 上限分批，避免单次 syscall 过大
    for chunk in iovs.chunks(IOV_MAX) {
        total += process_madvise(&pidfd, chunk, MADV_MERGEABLE)? as u64;
    }
    Ok(total)
}

fn list_all_pids() -> Vec<i32> {
    std::fs::read_dir("/proc")
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let pids: Vec<i32> = if args.is_empty() || (args.len() == 1 && args[0] == "--all") {
        list_all_pids()
    } else {
        args.iter().filter_map(|s| s.parse::<i32>().ok()).collect()
    };

    let self_pid = std::process::id() as i32;
    let mut ok = 0u32;
    let mut fail = 0u32;
    let mut total_bytes = 0u64;

    // 单线程顺序处理，无任何共享可变状态
    for pid in pids {
        if pid == self_pid {
            continue;
        }
        match mark_pid(pid) {
            Ok(bytes) => {
                println!("pid {:<7} marked {} bytes", pid, bytes);
                ok += 1;
                total_bytes += bytes;
            }
            Err(e) => {
                eprintln!("pid {:<7} skipped: {}", pid, e);
                fail += 1;
            }
        }
    }

    println!("done: ok={} fail={} total_marked={} bytes", ok, fail, total_bytes);
}
