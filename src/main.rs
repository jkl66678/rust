use regex::Regex;
use std::fs;
use std::io;
use std::path::Path;
use std::thread;
use std::time::Duration;

const GAME_PACKAGE: &str = "com.tencent.tmgp.dfm";
const WAIT_SLEEP: Duration = Duration::from_millis(500);
const SCAN_SLEEP: Duration = Duration::from_millis(250);
const POST_EXIT_SLEEP: Duration = Duration::from_millis(800);
const NICE_TARGET: i32 = 19;
type Pid = i64;

// AArch64 Linux syscall numbers
const SYS_setsid: usize = 115;
const SYS_setpriority: usize = 140;
const SYS_openat: usize = 56;
const SYS_dup2: usize = 24;
const SYS_close: usize = 57;
const AT_FDCWD: usize = -100isize as usize;
const O_RDWR: usize = 0o2;
const O_CLOEXEC: usize = 0o200000;
const STDIN_FILENO: usize = 0;
const STDOUT_FILENO: usize = 1;
const STDERR_FILENO: usize = 2;

#[inline(always)]
unsafe fn syscall0(nr: usize) -> usize {
    let ret: usize;
    std::arch::asm!("svc #0", out("x0") ret, in("x0") nr, options(nostack));
    ret
}

#[inline(always)]
unsafe fn syscall3(nr: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    std::arch::asm!("svc #0", out("x0") ret, in("x0") nr, in("x1") a1, in("x2") a2, in("x3") a3, options(nostack));
    ret
}

fn daemonize() {
    unsafe {
        syscall0(SYS_setsid);
        let devnull = syscall3(SYS_openat, AT_FDCWD, b"/dev/null\0".as_ptr() as usize, O_RDWR | O_CLOEXEC, 0o666);
        if devnull as i64 >= 0 {
            syscall3(SYS_dup2, devnull, STDIN_FILENO, 0);
            syscall3(SYS_dup2, devnull, STDOUT_FILENO, 0);
            syscall3(SYS_dup2, devnull, STDERR_FILENO, 0);
            syscall0(SYS_close);
        }
    }
}

fn set_tid_nice(tid: Pid, nice: i32) {
    unsafe {
        // PRIO_PROCESS = 0
        syscall3(SYS_setpriority, 0, tid as usize, nice as usize);
    }
}

fn find_game_pid() -> Option<Pid> {
    let dir = fs::read_dir("/proc").ok()?;
    for entry in dir {
        let entry = entry.ok()?;
        let fname = entry.file_name();
        let fname_str = fname.to_str()?;
        let pid: Pid = fname_str.parse().ok()?;
        let cmdline_path = entry.path().join("cmdline");
        let mut buf = fs::read(cmdline_path).ok()?;
        if let Some(nul_pos) = buf.iter().position(|&b| b == b'\0') {
            buf.truncate(nul_pos);
        }
        let cmd = String::from_utf8_lossy(&buf);
        if cmd.contains(GAME_PACKAGE) {
            return Some(pid);
        }
    }
    None
}

fn read_comm(pid: Pid, tid: Pid) -> io::Result<String> {
    let p = format!("/proc/{}/task/{}/comm", pid, tid);
    let mut s = fs::read_to_string(p)?;
    s.truncate(s.trim_end_matches(&['\n','\0'][..]).len());
    Ok(s)
}

fn read_utime(pid: Pid, tid: Pid) -> io::Result<u64> {
    let p = format!("/proc/{}/task/{}/stat", pid, tid);
    let content = fs::read_to_string(p)?;
    let mut iter = content.split_whitespace();
    iter.nth(13)
        .and_then(|v| v.parse().ok())
        .ok_or(io::Error::new(io::ErrorKind::InvalidData, "utime parse fail"))
}

fn list_tids(pid: Pid) -> Vec<Pid> {
    let mut tids = Vec::new();
    let task_path = Path::new(&format!("/proc/{}/task", pid));
    let dir = match fs::read_dir(task_path) {
        Ok(d) => d,
        Err(_) => return tids,
    };
    for entry in dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name();
        let name_str = name.to_str().unwrap_or("");
        if let Ok(tid) = name_str.parse::<Pid>() {
            tids.push(tid);
        }
    }
    tids
}

fn main() {
    daemonize();
    let re = Regex::new(r"^Thread-[0-9]+$").unwrap();

    loop {
        let game_pid = match find_game_pid() {
            Some(p) => p,
            None => {
                thread::sleep(WAIT_SLEEP);
                continue;
            }
        };

        loop {
            let proc_path = format!("/proc/{}", game_pid);
            if !Path::new(&proc_path).exists() {
                break;
            }

            let tids = list_tids(game_pid);
            for tid in tids {
                let comm = match read_comm(game_pid, tid) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if !re.is_match(&comm) {
                    continue;
                }
                let utime = match read_utime(game_pid, tid) {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                if utime > 0 {
                    set_tid_nice(tid, NICE_TARGET);
                }
            }
            thread::sleep(SCAN_SLEEP);
        }
        thread::sleep(POST_EXIT_SLEEP);
    }
}
