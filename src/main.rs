use libc::{c_int, c_void};
use std::arch::asm;

// 原始syscall号 aarch64
const SYS_setsid: usize = 115;
const SYS_setpriority: usize = 140;
const SYS_openat: usize = 56;
const SYS_dup2: usize = 24;
const SYS_close: usize = 57;

#[inline(always)]
unsafe fn syscall0(nr: usize) -> usize {
    let ret: usize;
    asm!("svc #0", out("x0") ret, in("x0") nr, options(nostack));
    ret
}
#[inline(always)]
unsafe fn syscall3(nr: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    asm!("svc #0", out("x0") ret, in("x0") nr, in("x1") a1, in("x2") a2, in("x3") a3, options(nostack));
    ret
}

fn daemonize() {
    unsafe {
        syscall0(SYS_setsid);
        // open /dev/null
        let devnull = syscall3(SYS_openat, libc::AT_FDCWD as usize, b"/dev/null\0".as_ptr() as usize, (libc::O_RDWR | libc::O_CLOEXEC) as usize, 0o666);
        if devnull as c_int >= 0 {
            syscall3(SYS_dup2, devnull, libc::STDIN_FILENO as usize, 0);
            syscall3(SYS_dup2, devnull, libc::STDOUT_FILENO as usize, 0);
            syscall3(SYS_dup2, devnull, libc::STDERR_FILENO as usize, 0);
            syscall0(SYS_close);
        }
    }
}

/// setpriority(PRIO_PROCESS, tid, nice)
fn set_tid_nice(tid: libc::pid_t, nice: i32) {
    unsafe {
        // PRIO_PROCESS = 0
        syscall3(SYS_setpriority, 0, tid as usize, nice as usize);
    }
}
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
    // 第14项是utime，索引13
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

fn daemonize() {
    unsafe {
        // 脱离终端会话
        setsid();
        // 重定向 stdin/stdout/stderr 到 /dev/null
        let devnull = open(c"/dev/null".as_ptr(), O_RDWR | O_CLOEXEC, 0o666);
        if devnull >= 0 {
            libc::dup2(devnull, libc::STDIN_FILENO);
            libc::dup2(devnull, libc::STDOUT_FILENO);
            libc::dup2(devnull, libc::STDERR_FILENO);
            libc::close(devnull);
        }
    }
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

        // 游戏运行循环
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
                    unsafe {
                        setpriority(PRIO_PROCESS, tid as u32, NICE_TARGET);
                    }
                }
            }
            thread::sleep(SCAN_SLEEP);
        }
        thread::sleep(POST_EXIT_SLEEP);
    }
}
