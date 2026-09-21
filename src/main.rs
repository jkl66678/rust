use libc::{setpriority, setsid, PRIO_PROCESS};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

const GAME_PACKAGE: &str = "com.tencent.tmgp.dfm";
const WAIT_SLEEP: Duration = Duration::from_millis(500);
const SCAN_SLEEP: Duration = Duration::from_millis(250);
const POST_EXIT_SLEEP: Duration = Duration::from_millis(800);
const NICE_TARGET: i32 = 19;
type Pid = i64;

fn is_thread_name(s: &str) -> bool {
    let Some(num_part) = s.strip_prefix("Thread-") else {
        return false;
    };
    num_part.chars().all(|c| c.is_ascii_digit())
}

fn daemonize() {
    unsafe {
        setsid();
        // 标准守护进程：把stdin/stdout/stderr重定向/dev/null
        let fd = libc::open(b"/dev/null\0".as_ptr(), libc::O_RDWR);
        if fd >= 0 {
            libc::dup2(fd, 0);
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
            libc::close(fd);
        }
    }
}

fn set_tid_nice(tid: Pid, nice: i32) {
    unsafe {
        setpriority(PRIO_PROCESS, tid as libc::id_t, nice);
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
    let task_path_buf = PathBuf::from(format!("/proc/{}/task", pid));
    let dir = match fs::read_dir(&task_path_buf) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut tids = Vec::new();
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

    loop {
        let game_pid = match find_game_pid() {
            Some(p) => p,
            None => {
                thread::sleep(WAIT_SLEEP);
                continue;
            }
        };

        loop {
            let proc_buf = PathBuf::from(format!("/proc/{}", game_pid));
            if !proc_buf.exists() {
                break;
            }

            let tids = list_tids(game_pid);
            for tid in tids {
                let comm = match read_comm(game_pid, tid) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if !is_thread_name(&comm) {
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
