use std::collections::{HashMap};
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use rustix::process::{self, ForkResult, PriorityKind};
use rustix::io::dup2;
use signal_hook::{consts::{SIGINT, SIGTERM}, iterator::Signals};

// ========== 配置区 ==========
const SAMPLE_INTERVAL_MS: u64 = 500;
const WINDOW_SEC: u64 = 3;
const AVG_CPU_THRESHOLD: f32 = 3.5;
const BURST_INSTANT_THRESHOLD: f32 = 25.0;
const TARGET_NICE: i32 = 19;
const GAME_PKG: &str = "com.tencent.tmgp.dfm";
const BIN_NAME: &[u8] = b"dfm-daemon";

#[derive(Debug, Default)]
struct ThreadCpuSample {
    prev_utime: u64,
    prev_stime: u64,
    total_delta: u64,
    sample_cnt: u64,
}

#[derive(Debug, Copy, Clone)]
struct ThreadState {
    orig_nice: i32,
    adjusted: bool,
}

fn set_thread_nice(tid: u32, nice: i32) -> bool {
    process::setpriority(PriorityKind::Process, tid, nice).is_ok()
}

fn get_thread_nice(tid: u32) -> Option<i32> {
    process::getpriority(PriorityKind::Process, tid).ok()
}

fn read_thread_comm(tid: u32) -> Option<String> {
    let path = format!("/proc/{tid}/comm");
    let mut buf = String::new();
    let mut f = fs::File::open(path).ok()?;
    f.read_to_string(&mut buf).ok()?;
    Some(buf.trim_end().to_string())
}

fn daemonize_self() {
    match process::fork().unwrap() {
        ForkResult::Child => {}
        ForkResult::Parent { .. } => std::process::exit(0),
    }
    let _ = process::setsid();
    match process::fork().unwrap() {
        ForkResult::Child => {}
        ForkResult::Parent { .. } => std::process::exit(0),
    }
    if let Ok(devnull) = OpenOptions::new().read(true).write(true).open("/dev/null") {
        let fd = devnull;
        let _ = dup2(&fd, rustix::io::STDIN_FD);
        let _ = dup2(&fd, rustix::io::STDOUT_FD);
        let _ = dup2(&fd, rustix::io::STDERR_FD);
    }
}

fn get_pid_via_pidof(pkg: &str) -> Option<u32> {
    let out = Command::new("pidof").arg("-s").arg(pkg).output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.trim().parse::<u32>().ok()
        }
        _ => None,
    }
}

fn get_pid_by_proc_scan(pkg: &str) -> Option<u32> {
    let dir_iter = match fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return None,
    };
    let mut buf = [0u8; 512];
    for entry in dir_iter.flatten() {
        let name = entry.file_name();
        let pid_str = match name.to_str() {
            Some(s) if s.chars().all(|c| c.is_ascii_digit()) => s,
            _ => continue,
        };
        let pid = pid_str.parse::<u32>().ok()?;
        let proc_path = format!("/proc/{pid}/cmdline");
        let mut f = match fs::File::open(proc_path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let n = f.read(&mut buf).unwrap_or(0);
        let cmdline = &buf[0..n];
        if cmdline.windows(pkg.len()).any(|w| w == pkg.as_bytes()) {
            return Some(pid);
        }
    }
    None
}

fn find_game_pid() -> Option<u32> {
    get_pid_via_pidof(GAME_PKG).or_else(|| get_pid_by_proc_scan(GAME_PKG))
}

fn read_tid_stat(tid: u32) -> Option<(u64, u64)> {
    let stat_path = format!("/proc/{tid}/stat");
    let mut stack_buf = [0u8; 1024];
    let mut fd = fs::File::open(stat_path).ok()?;
    let read_len = fd.read(&mut stack_buf).ok()?;
    if read_len == 0 {
        return None;
    }
    let content = std::str::from_utf8(&stack_buf[0..read_len]).ok()?;
    let tokens = content.split_whitespace();
    let mut idx = 0usize;
    let mut utime: Option<u64> = None;
    let mut stime: Option<u64> = None;
    for tok in tokens {
        idx += 1;
        match idx {
            14 => utime = tok.parse().ok(),
            15 => stime = tok.parse().ok(),
            _ => {}
        }
        if utime.is_some() && stime.is_some() {
            break;
        }
    }
    Some((utime?, stime?))
}

fn single_instance_check() -> bool {
    let dir_iter = match fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return true,
    };
    let mut read_buf = [0u8; 1024];
    for entry in dir_iter.flatten() {
        let fname = entry.file_name();
        let pid_str = match fname.to_str() {
            Some(s) if s.chars().all(|c| c.is_ascii_digit()) => s,
            _ => continue,
        };
        let pid = match pid_str.parse::<u32>() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let cmd_path = format!("/proc/{pid}/cmdline");
        let mut f = match fs::File::open(cmd_path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let n = f.read(&mut read_buf).unwrap_or(0);
        if !read_buf[0..n].windows(BIN_NAME.len()).any(|w| w == BIN_NAME) {
            continue;
        }
        let stat_path = format!("/proc/{pid}/stat");
        let mut sf = match fs::File::open(stat_path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let rn = sf.read(&mut read_buf).unwrap_or(0);
        let s = match std::str::from_utf8(&read_buf[0..rn]) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let t = s.split_whitespace();
        let mut idx = 0;
        let mut ppid_opt: Option<u32> = None;
        for token in t {
            idx += 1;
            if idx == 4 {
                ppid_opt = token.parse().ok();
                break;
            }
        }
        if ppid_opt == Some(1) {
            return false;
        }
    }
    true
}

fn restore_all(restore_list: &mut Vec<(u32, i32)>) {
    for (tid, old) in restore_list.iter() {
        let _ = set_thread_nice(*tid, *old);
    }
    restore_list.clear();
}

fn get_all_tid(pid: u32) -> Vec<u32> {
    let mut tids = Vec::new();
    let dir = match fs::read_dir(format!("/proc/{pid}/task")) {
        Ok(d) => d,
        Err(_) => return tids,
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let tid_str = match name.to_str() {
            Some(s) if s.chars().all(|c| c.is_ascii_digit()) => s,
            _ => continue,
        };
        if let Ok(tid) = tid_str.parse::<u32>() {
            tids.push(tid);
        }
    }
    tids
}

fn main() -> io::Result<()> {
    if !single_instance_check() {
        eprintln!("daemon already running");
        std::process::exit(1);
    }
    daemonize_self();

    let restore_list: Arc<Mutex<Vec<(u32, i32)>>> = Arc::new(Mutex::new(Vec::new()));
    let rl_clone = Arc::clone(&restore_list);

    thread::spawn(move || {
        let mut signals = Signals::new(&[SIGINT, SIGTERM]).unwrap();
        for _ in signals.forever() {
            let mut guard = rl_clone.lock().unwrap();
            restore_all(&mut guard);
            std::process::exit(0);
        }
    });

    let mut thread_state_map: HashMap<u32, ThreadState> = HashMap::new();
    let mut cpu_history: HashMap<u32, ThreadCpuSample> = HashMap::new();
    const JIFF_PER_SEC: u64 = 100;

    loop {
        let Some(game_pid) = find_game_pid() else {
            let mut g = restore_list.lock().unwrap();
            restore_all(&mut g);
            thread_state_map.clear();
            cpu_history.clear();
            thread::sleep(Duration::from_millis(SAMPLE_INTERVAL_MS));
            continue;
        };

        let tids = get_all_tid(game_pid);
        for tid in tids {
            // ========= 核心过滤：只处理这两类线程，其余全部跳过 =========
            let comm = match read_thread_comm(tid) {
                Some(c) => c,
                None => continue,
            };
            let is_target = comm == "TaskGraphNP 0" || comm.starts_with("Thread-");
            if !is_target {
                continue;
            }
            // ======================================================

            let (utime, stime) = match read_tid_stat(tid) {
                Some(v) => v,
                None => continue,
            };
            let entry = cpu_history.entry(tid).or_default();
            let delta = (utime + stime).saturating_sub(entry.prev_utime + entry.prev_stime);
            entry.prev_utime = utime;
            entry.prev_stime = stime;
            entry.total_delta += delta;
            entry.sample_cnt += 1;

            let instant_pct = delta as f32 * 100.0 / (JIFF_PER_SEC * SAMPLE_INTERVAL_MS / 1000) as f32;
            let avg_pct = entry.total_delta as f32 * 100.0 / (JIFF_PER_SEC * WINDOW_SEC) as f32;

            let state = thread_state_map.entry(tid).or_insert(ThreadState {
                orig_nice: 0,
                adjusted: false,
            });

            if state.adjusted {
                continue;
            }

            let need_throttle = if comm == "TaskGraphNP 0" {
                true
            } else {
                avg_pct > AVG_CPU_THRESHOLD || instant_pct > BURST_INSTANT_THRESHOLD
            };

            if need_throttle {
                if let Some(nice_val) = get_thread_nice(tid) {
                    state.orig_nice = nice_val;
                    state.adjusted = true;
                    let mut rl = restore_list.lock().unwrap();
                    rl.push((tid, nice_val));
                    let _ = set_thread_nice(tid, TARGET_NICE);
                }
            }
        }

        // 清理已经消失的TID
        cpu_history.retain(|tid, _| {
            if !thread_state_map.contains_key(tid) {
                false
            } else {
                true
            }
        });
        thread_state_map.retain(|tid, _| {
            let path = format!("/proc/{tid}");
            fs::exists(path).unwrap_or(false)
        });

        thread::sleep(Duration::from_millis(SAMPLE_INTERVAL_MS));
    }
}
