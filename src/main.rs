use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use libc;
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

/// 设置tid的nice，unsafe完全封装在函数内部
fn set_tid_nice(tid: u32, nice: i32) -> bool {
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, nice) == 0
    }
}

/// 获取tid的nice，unsafe完全封装在函数内部
fn get_tid_nice(tid: u32) -> Option<i32> {
    unsafe {
        let r = libc::getpriority(libc::PRIO_PROCESS, tid as libc::id_t);
        if r == -1 {
            let err = *libc::__errno_location();
            if err != 0 {
                return None;
            }
        }
        Some(r)
    }
}

/// 双fork + setsid 守护进程，unsafe收拢在此函数
fn daemonize() -> io::Result<()> {
    unsafe {
        let pid1 = libc::fork();
        if pid1 < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid1 > 0 {
            std::process::exit(0);
        }

        libc::setsid();

        let pid2 = libc::fork();
        if pid2 < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid2 > 0 {
            std::process::exit(0);
        }

        let devnull = libc::open(b"/dev/null\0".as_ptr(), libc::O_RDWR, 0);
        libc::dup2(devnull, 0);
        libc::dup2(devnull, 1);
        libc::dup2(devnull, 2);
        libc::close(devnull);
    }
    Ok(())
}

fn read_thread_comm(tid: u32) -> Option<String> {
    let path = format!("/proc/{tid}/comm");
    let mut f = fs::File::open(path).ok()?;
    let mut buf = String::new();
    f.read_to_string(&mut buf).ok()?;
    Some(buf.trim_end().to_string())
}

fn get_pid_via_pidof(pkg: &str) -> Option<u32> {
    let out = Command::new("pidof").arg(pkg).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.split_whitespace().next()?.parse().ok()
}

fn get_pid_by_proc_scan(pkg: &str) -> Option<u32> {
    let dir = fs::read_dir("/proc").ok()?;
    for entry in dir.flatten() {
        let name = entry.file_name();
        let pid_str = name.to_str()?;
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let pid: u32 = pid_str.parse().ok()?;
        let cmdline = fs::read_to_string(format!("/proc/{pid}/cmdline")).ok()?;
        if cmdline.contains(pkg) {
            return Some(pid);
        }
    }
    None
}

fn find_game_pid() -> Option<u32> {
    get_pid_via_pidof(GAME_PKG).or_else(|| get_pid_by_proc_scan(GAME_PKG))
}

fn read_tid_stat(tid: u32) -> Option<(u64, u64)> {
    let s = fs::read_to_string(format!("/proc/{tid}/stat")).ok()?;
    let sp = s.split_whitespace();
    let mut idx = 0;
    let mut utime: Option<u64> = None;
    let mut stime: Option<u64> = None;
    for val in sp {
        idx += 1;
        match idx {
            14 => utime = val.parse().ok(),
            15 => stime = val.parse().ok(),
            _ => {}
        }
    }
    Some((utime?, stime?))
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
            Some(s) => s,
            None => continue,
        };
        if let Ok(tid) = tid_str.parse::<u32>() {
            tids.push(tid);
        }
    }
    tids
}

fn single_instance_check() -> bool {
    let dir = match fs::read_dir("/proc").ok() {
        Some(d) => d,
        None => return true,
    };
    for entry in dir.flatten() {
        let fname = entry.file_name();
        let pid_str = fname.to_str();
        let Some(pid_str) = pid_str else { continue };
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let cmd_path = format!("/proc/{pid_str}/cmdline");
        let Ok(cmd) = fs::read(cmd_path) else { continue };
        if cmd.windows(BIN_NAME.len()).any(|w| w == BIN_NAME) {
            return false;
        }
    }
    true
}

fn restore_all(restore_list: &mut Vec<(u32, i32)>) {
    for (tid, old_nice) in &mut *restore_list {
        let _ = set_tid_nice(*tid, *old_nice);
    }
    restore_list.clear();
}

fn main() -> io::Result<()> {
    if !single_instance_check() {
        eprintln!("daemon already running");
        std::process::exit(1);
    }
    daemonize()?;

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
            let comm = match read_thread_comm(tid) {
                Some(c) => c,
                None => continue,
            };
            // 只处理两类目标线程，其余全部直接跳过，不读stat、不改优先级
            let is_target = comm == "TaskGraphNP 0" || comm.starts_with("Thread-");
            if !is_target {
                continue;
            }

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

            let state = thread_state_map.entry(tid).or_insert(ThreadState { orig_nice: 0, adjusted: false });
            if state.adjusted {
                continue;
            }

            let need_throttle = if comm == "TaskGraphNP 0" {
                true
            } else {
                avg_pct > AVG_CPU_THRESHOLD || instant_pct > BURST_INSTANT_THRESHOLD
            };

            if need_throttle {
                if let Some(nice_val) = get_tid_nice(tid) {
                    state.orig_nice = nice_val;
                    state.adjusted = true;
                    let mut rl = restore_list.lock().unwrap();
                    rl.push((tid, nice_val));
                    let _ = set_tid_nice(tid, TARGET_NICE);
                }
            }
        }

        // 清理已经消亡的tid
        thread_state_map.retain(|tid, _| fs::exists(format!("/proc/{tid}")).unwrap_or(false));
        cpu_history.retain(|tid, _| thread_state_map.contains_key(tid));

        thread::sleep(Duration::from_millis(SAMPLE_INTERVAL_MS));
    }
}
