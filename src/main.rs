use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use signal_hook::{consts::SIGINT, consts::SIGTERM, iterator::Signals};

// ========== 调参区【极致省电优先直接用这套】 ==========
const SAMPLE_INTERVAL_MS: u64 = 600;
const IDLE_SLEEP_MS: u64 = 2500;
const RESCAN_PID_EVERY_N: usize = 8; // 缓存pid，每8轮重新检测一次pid
const WINDOW_SEC: u64 = 3;
const AVG_CPU_THRESHOLD: f32 = 3.5;
const BURST_INSTANT_THRESHOLD: f32 = 25.0;
const TARGET_NICE: i32 = 4;
const GAME_PKG: &str = "com.tencent.tmgp.dfm";

const WHITELIST: [&str; 2] = ["GameThread", "RenderThread"];
const BLACKLIST: [&str;4] = ["Thread-", "ace", "NativeThread", "TaskGraphNP 0"];

#[derive(Debug)]
struct ThreadState {
    cpu_samples: Vec<u64>,
    original_nice: Option<i32>,
}

#[inline(always)]
fn is_whitelist(comm: &str) -> bool {
    WHITELIST.iter().any(|&s| comm == s)
}

#[inline(always)]
fn is_blacklist(comm: &str) -> bool {
    BLACKLIST.iter().any(|rule| {
        if rule.ends_with('-') {
            comm.starts_with(rule)
        } else {
            comm.contains(rule)
        }
    })
}

/// 优先 pidof，失败自动降级扫描/proc
fn scan_game_pid() -> Option<u32> {
    // 优先pidof
    if let Ok(out) = std::process::Command::new("pidof")
        .arg("-s")
        .arg(GAME_PKG)
        .output()
    {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            if let Ok(pid) = s.parse::<u32>() {
                return Some(pid);
            }
        }
    }
    // pidof不可用，降级手动遍历/proc
    let dir = fs::read_dir("/proc").ok()?;
    for entry in dir.flatten() {
        let fname = entry.file_name();
        let pid_str = fname.to_string_lossy();
        let pid = match pid_str.parse::<u32>() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut buf = String::with_capacity(256);
        buf.clear();
        match fs::read_to_string(format!("/proc/{pid}/cmdline")) {
            Ok(s) => buf.push_str(&s),
            Err(_) => continue,
        }
        if buf.contains(GAME_PKG) {
            return Some(pid);
        }
    }
    None
}

#[inline(always)]
fn get_all_tid(pid: u32) -> Vec<u32> {
    let mut tids = Vec::with_capacity(64);
    let dir = match fs::read_dir(format!("/proc/{pid}/task")) {
        Ok(d) => d,
        Err(_) => return tids,
    };
    for entry in dir.flatten() {
        let tid_name = entry.file_name();
        if let Ok(tid) = tid_name.to_string_lossy().parse::<u32>() {
            tids.push(tid);
        }
    }
    tids
}

#[inline(always)]
fn read_tid_stat(tid: u32) -> Option<(u64, String, i32)> {
    let stat_raw = fs::read_to_string(format!("/proc/{tid}/stat")).ok()?;
    let mut parts = stat_raw.split_whitespace();
    let comm = parts.nth(1)?.trim_start_matches('(').trim_end_matches(')').to_string();
    let utime: u64 = parts.nth(11)?.parse().ok()?;
    let stime: u64 = parts.next()?.parse().ok()?;
    let nice: i32 = parts.nth(2)?.parse().ok()?;
    Some((utime + stime, comm, nice))
}

#[inline(always)]
fn set_tid_nice(tid: u32, nice_val: i32) -> bool {
    fs::write(format!("/proc/{tid}/nice"), format!("{}", nice_val)).is_ok()
}

fn main() {
    let mut signals = Signals::new(&[SIGINT, SIGTERM]).unwrap();
    let restore_list: Arc<Mutex<Vec<(u32, i32)>>> = Arc::new(Mutex::new(Vec::new()));
    let restore_clone = Arc::clone(&restore_list);

    // 信号线程独立
    thread::spawn(move || {
        for sig in signals.forever() {
            let mut guard = restore_clone.lock().unwrap();
            for (tid, old_nice) in guard.iter() {
                let _ = set_tid_nice(*tid, *old_nice);
            }
            guard.clear();
            std::process::exit(sig);
        }
    });

    let sample_count = (WINDOW_SEC * 1000 / SAMPLE_INTERVAL_MS) as usize;
    let mut thread_map: HashMap<u32, ThreadState> = HashMap::new();
    let mut cached_pid: Option<u32> = None;
    let mut loop_counter: usize = 0;

    loop {
        loop_counter += 1;
        // 每N轮才重新扫描pid，复用缓存
        if loop_counter % RESCAN_PID_EVERY_N == 0 || cached_pid.is_none() {
            cached_pid = scan_game_pid();
        }

        let Some(game_pid) = cached_pid else {
            thread_map.clear();
            thread::sleep(Duration::from_millis(IDLE_SLEEP_MS));
            continue;
        };

        // 校验pid是否存活，防止缓存失效（进程退出但pid复用）
        if fs::read_to_string(format!("/proc/{game_pid}/cmdline")).is_err() {
            cached_pid = None;
            thread_map.clear();
            continue;
        }

        let tids = get_all_tid(game_pid);
        let mut alive_tids = HashMap::new();

        for tid in tids {
            let Some((cpu_total, comm, current_nice)) = read_tid_stat(tid) else {
                continue;
            };
            if is_whitelist(&comm) {
                continue;
            }
            if !is_blacklist(&comm) {
                continue;
            }

            let state = thread_map.entry(tid).or_insert_with(|| ThreadState {
                cpu_samples: Vec::with_capacity(sample_count),
                original_nice: Some(current_nice),
            });

            state.cpu_samples.push(cpu_total);
            if state.cpu_samples.len() > sample_count {
                state.cpu_samples.remove(0);
            }
            alive_tids.insert(tid, ());

            if state.cpu_samples.len() >= 2 {
                let delta_cpu = state.cpu_samples.last().unwrap() - state.cpu_samples[state.cpu_samples.len() - 2];
                let instant_cpu = delta_cpu as f32 * 100.0 / (SAMPLE_INTERVAL_MS as f32 * 10.0);
                let mut trigger = false;

                if instant_cpu > BURST_INSTANT_THRESHOLD {
                    trigger = true;
                }
                if state.cpu_samples.len() >= sample_count {
                    let avg_delta = state.cpu_samples.last().unwrap() - state.cpu_samples[0];
                    let avg_cpu = avg_delta as f32 *100.0 / ((WINDOW_SEC *1000) as f32 *10.0);
                    if avg_cpu > AVG_CPU_THRESHOLD {
                        trigger = true;
                    }
                }

                if trigger && current_nice < TARGET_NICE {
                    if set_tid_nice(tid, TARGET_NICE) {
                        if let Some(old) = state.original_nice {
                            let mut guard = restore_list.lock().unwrap();
                            if !guard.iter().any(|(t,_)| *t == tid) {
                                guard.push((tid, old));
                            }
                        }
                    }
                }
            }
        }
        thread_map.retain(|tid, _| alive_tids.contains_key(tid));
        thread::sleep(Duration::from_millis(SAMPLE_INTERVAL_MS));
    }
}
