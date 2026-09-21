use libc::{open, setsid, PRIO_PROCESS, setpriority, O_RDWR, O_CLOEXEC, c_int};
use regex::Regex;
use std::fs;
use std::io;
use std::os::unix::io::FromRawFd;
use std::path::Path;
use std::thread;
use std::time::Duration;

const GAME_PACKAGE: &str = "com.tencent.tmgp.dfm";
const WAIT_SLEEP: Duration = Duration::from_millis(500);
const SCAN_SLEEP: Duration = Duration::from_millis(250);
const POST_EXIT_SLEEP: Duration = Duration::from_millis(800);
const NICE_TARGET: i32 = 19;

type Pid = libc::pid_t;

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
