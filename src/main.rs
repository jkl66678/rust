use std::collections::HashMap;
use std::fs;
use std::os::unix::io::RawFd;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::time::{interval, sleep};

const PID_FILE: &str = "/data/adb/dfm-daemon.pid";
const BLACK_LIST: &[&str] = &["ace", "Thread-"];
const STEP: i32 = 1;
const MAX_NICE: i32 = 19;
const GAME_PACKAGE: &str = "com.tencent.tmgp.dfm";

#[derive(Debug, Default)]
struct ThreadState {
    origin_nice: i32,
    current_nice: i32,
    target_nice: i32,

    high_sample_cnt: u32,
    low_sample_cnt: u32,
    pulse_cnt: u32,

    is_limited: bool,
}

#[derive(Debug, Clone)]
struct ThreadCpuSample {
    utime: u64,
    stime: u64,
    sample_time: Instant,
}

// 单实例校验
fn check_single_instance() -> bool {
    let p = Path::new(PID_FILE);
    if !p.exists() {
        return true;
    }
    let content = match fs::read_to_string(p) {
        Ok(s) => s,
        Err(_) => {
            let _ = fs::remove_file(p);
            return true;
        }
    };
    let old_pid: u32 = match content.trim().parse() {
        Ok(num) => num,
        Err(_) => {
            let _ = fs::remove_file(p);
            return true;
        }
    };
    if fs::metadata(format!("/proc/{old_pid}")).is_ok() {
        eprintln!("already running, pid={old_pid}, refuse new instance");
        return false;
    }
    let _ = fs::remove_file(p);
    true
}

fn write_pid_file() -> std::io::Result<()> {
    let pid = unsafe { libc::getpid() };
    fs::write(PID_FILE, pid.to_string())
}

fn clean_pid_file() {
    let _ = fs::remove_file(PID_FILE);
}

fn daemonize() -> std::io::Result<()> {
    unsafe {
        match libc::fork() {
            -1 => return Err(std::io::Error::last_os_error()),
            0 => {}
            parent => {
                std::process::exit(0);
            }
        }
        let sid = libc::setsid();
        if sid == -1 {
            return Err(std::io::Error::last_os_error());
        }
        match libc::fork() {
            -1 => return Err(std::io::Error::last_os_error()),
            0 => {}
            _ => {
                std::process::exit(0);
            }
        }
        let devnull = libc::open(b"/dev/null\0".as_ptr(), libc::O_RDWR, 0o644);
        libc::dup2(devnull, libc::STDIN_FILENO as RawFd);
        libc::dup2(devnull, libc::STDOUT_FILENO as RawFd);
        libc::dup2(devnull, libc::STDERR_FILENO as RawFd);
        if devnull > 2 {
            libc::close(devnull);
        }
    }
    Ok(())
}

fn is_blacklist_target(comm: &str) -> bool {
    BLACK_LIST.iter().any(|s| comm.contains(s))
}

fn get_tid_comm(tid: u32) -> Option<String> {
    let path = format!("/proc/{tid}/comm");
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim_end().to_string())
}

fn get_tid_nice(tid: u32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{tid}/stat")).ok()?;
    let parts: Vec<&str> = stat.split_whitespace().collect();
    parts.get(17)?.parse().ok()
}

fn set_tid_nice(tid: u32, nice: i32) -> std::io::Result<()> {
    fs::write(format!("/proc/{tid}/nice"), nice.to_string())
}

fn read_tid_stat(tid: u32) -> Option<(u64, u64)> {
    let stat = fs::read_to_string(format!("/proc/{tid}/stat")).ok()?;
    let parts: Vec<&str> = stat.split_whitespace().collect();
    let utime: u64 = parts.get(13)?.parse().ok()?;
    let stime: u64 = parts.get(14)?.parse().ok()?;
    Some((utime, stime))
}

fn get_tid_cpu_percent(tid: u32, cache: &mut HashMap<u32, ThreadCpuSample>) -> Option<f32> {
    let (now_utime, now_stime) = read_tid_stat(tid)?;
    let now_instant = Instant::now();

    let entry = cache.entry(tid).or_insert(ThreadCpuSample {
        utime: now_utime,
        stime: now_stime,
        sample_time: now_instant,
    });

    let delta_time_ms = now_instant.duration_since(entry.sample_time).as_millis() as u64;
    if delta_time_ms < 50 {
        return None;
    }

    let delta_utime = now_utime - entry.utime;
    let delta_stime = now_stime - entry.stime;
    let total_delta = delta_utime + delta_stime;

    entry.utime = now_utime;
    entry.stime = now_stime;
    entry.sample_time = now_instant;

    let ticks_per_ms = 0.1;
    let cpu_pct = (total_delta as f32 * 100.0) / (delta_time_ms as f32 * ticks_per_ms);
    Some(cpu_pct.clamp(0.0, 100.0))
}

fn is_process_alive(pid: u32) -> bool {
    fs::metadata(format!("/proc/{pid}")).is_ok()
}

fn scan_game_threads(
    game_pid: u32,
    state_map: &mut HashMap<u32, ThreadState>,
    cpu_cache: &mut HashMap<u32, ThreadCpuSample>,
) {
    let task_path = format!("/proc/{game_pid}/task");
    let dir_entries = match fs::read_dir(task_path) {
        Ok(d) => d,
        Err(_) => return,
    };

    let mut alive_tids = Vec::new();

    const HIGH_THRESHOLD: f32 = 20.0;
    const SAMPLE_HIGH_TRIGGER: u32 = 3;
    const SAMPLE_LOW_RECOVER: u32 = 2;
    const PULSE_WINDOW: u32 = 4;
    const PULSE_HIGH: u32 = 2;

    for entry in dir_entries.flatten() {
        let tid_str = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let tid = match tid_str.parse::<u32>() {
            Ok(n) => n,
            Err(_) => continue,
        };
        alive_tids.push(tid);

        let comm = match get_tid_comm(tid) {
            Some(c) => c,
            None => continue,
        };

        if !is_blacklist_target(&comm) {
            continue;
        }

        let state = state_map.entry(tid).or_insert_with(|| {
            let orig = get_tid_nice(tid).unwrap_or(0);
            ThreadState {
                origin_nice: orig,
                current_nice: orig,
                target_nice: orig,
                high_sample_cnt: 0,
                low_sample_cnt: 0,
                pulse_cnt: 0,
                is_limited: false,
            }
        });

        if comm.contains("ace") {
            state.target_nice = std::cmp::min(state.origin_nice + 5, MAX_NICE);
            state.is_limited = true;
        } else if comm.starts_with("Thread-") {
            let cpu = match get_tid_cpu_percent(tid, cpu_cache) {
                Some(v) => v,
                None => continue,
            };

            if cpu > HIGH_THRESHOLD {
                state.pulse_cnt += 1;
                state.high_sample_cnt += 1;
            }
            if state.pulse_cnt > PULSE_WINDOW {
                state.pulse_cnt = 0;
            }

            if state.high_sample_cnt >= SAMPLE_HIGH_TRIGGER {
                state.target_nice = std::cmp::min(state.origin_nice + 3, MAX_NICE);
                state.is_limited = true;
            } else if state.pulse_cnt >= PULSE_HIGH {
                state.target_nice = std::cmp::min(state.origin_nice + 5, MAX_NICE);
                state.is_limited = true;
            } else {
                state.target_nice = std::cmp::min(state.origin_nice + 1, MAX_NICE);
                state.is_limited = true;
            }

            if cpu <= HIGH_THRESHOLD {
                state.high_sample_cnt = 0;
                if state.is_limited {
                    state.low_sample_cnt += 1;
                    if state.low_sample_cnt >= SAMPLE_LOW_RECOVER {
                        state.target_nice = state.origin_nice;
                    }
                }
            }
        }

        if state.current_nice < state.target_nice {
            state.current_nice += STEP;
            state.current_nice = state.current_nice.min(MAX_NICE);
            let _ = set_tid_nice(tid, state.current_nice);
        } else if state.current_nice > state.target_nice {
            state.current_nice -= STEP;
            let _ = set_tid_nice(tid, state.current_nice);
            if state.current_nice == state.origin_nice {
                state.is_limited = false;
                state.low_sample_cnt = 0;
                state.pulse_cnt = 0;
            }
        }
    }

    state_map.retain(|tid, _| alive_tids.contains(tid));
    cpu_cache.retain(|tid, _| alive_tids.contains(tid));
}

fn restore_all_threads(state_map: &mut HashMap<u32, ThreadState>) {
    for (tid, state) in state_map.iter_mut() {
        state.current_nice = state.origin_nice;
        state.target_nice = state.origin_nice;
        let _ = set_tid_nice(*tid, state.origin_nice);
    }
    state_map.clear();
}

fn find_game_pid(pkg: &str) -> Option<u32> {
    let dir = fs::read_dir("/proc").ok()?;
    for entry in dir.flatten() {
        let filename = entry.file_name();
        let pid_str = match filename.into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let pid: u32 = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let cmdline_path = format!("/proc/{pid}/cmdline");
        let cmdline = match fs::read(cmdline_path) {
            Ok(buf) => buf,
            Err(_) => continue,
        };
        let cmd_str = String::from_utf8_lossy(&cmdline);
        if cmd_str.contains(pkg) {
            return Some(pid);
        }
    }
    None
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // 第一步：单实例检查
    if !check_single_instance() {
        std::process::exit(1);
    }

    // 环境变量 DFM_FOREGROUND=1 前台调试，不daemon
    let foreground = std::env::var("DFM_FOREGROUND").is_ok();
    if !foreground {
        daemonize()?;
    }

    write_pid_file()?;

    use tokio::signal;
    let mut thread_state = HashMap::new();
    let mut cpu_sample_cache = HashMap::new();

    let sig_task = tokio::spawn(async move {
        signal::signal(signal::unix::SignalKind::interrupt()).unwrap().recv().await;
    });

    let res: std::io::Result<()> = async {
        loop {
            let game_pid = loop {
                match find_game_pid(GAME_PACKAGE) {
                    Some(pid) => {
                        if foreground {
                            println!("Detect game start, pid={pid}");
                        }
                        break pid;
                    }
                    None => sleep(Duration::from_secs(1)).await,
                }
            };
            let mut tick = interval(Duration::from_millis(500));
            loop {
                tokio::select! {
                    _ = &sig_task => {
                        restore_all_threads(&mut thread_state);
                        return Ok(());
                    }
                    _ = tick.tick() => {
                        if !is_process_alive(game_pid) {
                            if foreground {
                                println!("Game exited, restore threads, back waiting");
                            }
                            restore_all_threads(&mut thread_state);
                            break;
                        }
                        scan_game_threads(game_pid, &mut thread_state, &mut cpu_sample_cache);
                    }
                }
            }
        }
    }.await;

    restore_all_threads(&mut thread_state);
    clean_pid_file();
    res
}
