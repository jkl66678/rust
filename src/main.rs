use std::collections::HashMap;
use std::fs;
use std::thread;
use std::time::Duration;
use signal_hook::{consts::SIGINT, consts::SIGTERM, iterator::Signals};

// ========== 配置区 ==========
const SAMPLE_INTERVAL_MS: u64 = 500;
const WINDOW_SEC: u64 = 3;
const AVG_CPU_THRESHOLD: f32 = 3.5;
const BURST_INSTANT_THRESHOLD: f32 = 25.0;
const TARGET_NICE: i32 = 4;
const GAME_PKG: &str = "com.tencent.tmgp.dfm";

// 白名单：核心游戏线程，禁止修改nice
const WHITELIST: [&str; 2] = ["GameThread", "RenderThread"];
// 黑名单：需要监控的后台线程
const BLACKLIST: [&str;4] = ["Thread-", "ace", "NativeThread", "TaskGraphNP 0"];

#[derive(Debug, Clone)]
struct ThreadState {
    cpu_samples: Vec<u64>,
    original_nice: Option<i32>,
}

fn is_whitelist(comm: &str) -> bool {
    WHITELIST.iter().any(|&s| comm == s)
}

fn is_blacklist(comm: &str) -> bool {
    BLACKLIST.iter().any(|rule| {
        if rule.ends_with('-') {
            // Thread- 前缀匹配
            comm.starts_with(rule)
        } else {
            comm.contains(rule)
        }
    })
}

fn get_game_pid() -> Option<u32> {
    let dir = fs::read_dir("/proc").ok()?;
    for entry in dir.flatten() {
        let fname = entry.file_name();
        let pid_str = fname.to_string_lossy();
        if let Ok(pid) = pid_str.parse::<u32>() {
            let cmdline = fs::read_to_string(format!("/proc/{pid}/cmdline")).ok()?;
            if cmdline.contains(GAME_PKG) {
                return Some(pid);
            }
        }
    }
    None
}

fn get_all_tid(pid: u32) -> Vec<u32> {
    let mut tids = Vec::new();
    match fs::read_dir(format!("/proc/{pid}/task")) {
        Ok(dir) => {
            for entry in dir.flatten() {
                let tid_name = entry.file_name();
                if let Ok(tid) = tid_name.to_string_lossy().parse::<u32>() {
                    tids.push(tid);
                }
            }
        }
        Err(_) => {}
    }
    tids
}

fn read_tid_stat(tid: u32) -> Option<(u64, String, i32)> {
    let stat_raw = fs::read_to_string(format!("/proc/{tid}/stat")).ok()?;
    let mut parts = stat_raw.split_whitespace();
    let comm = parts.nth(1)?.trim_start_matches('(').trim_end_matches(')').to_string();
    let utime: u64 = parts.nth(11)?.parse().ok()?;
    let stime: u64 = parts.next()?.parse().ok()?;
    let nice: i32 = parts.nth(2)?.parse().ok()?;
    Some((utime + stime, comm, nice))
}

fn set_tid_nice(tid: u32, nice_val: i32) -> bool {
    fs::write(format!("/proc/{tid}/nice"), format!("{}", nice_val)).is_ok()
}

fn main() {
    let mut signals = Signals::new(&[SIGINT, SIGTERM]).unwrap();
    let mut thread_map: HashMap<u32, ThreadState> = HashMap::new();
    let sample_count = (WINDOW_SEC * 1000 / SAMPLE_INTERVAL_MS) as usize;
    let mut restore_list: Vec<(u32, i32)> = Vec::new();

    std::thread::spawn(move || {
        for _ in signals.forever() {
            for (tid, old_nice) in restore_list.iter() {
                let _ = set_tid_nice(*tid, *old_nice);
            }
            std::process::exit(0);
        }
    });

    loop {
        let Some(game_pid) = get_game_pid() else {
            thread::sleep(Duration::from_millis(SAMPLE_INTERVAL_MS));
            thread_map.clear();
            continue;
        };

        let tids = get_all_tid(game_pid);
        let mut new_thread_map = HashMap::new();

        for tid in tids {
            let Some((cpu_total, comm, current_nice)) = read_tid_stat(tid) else {continue;};
            if is_whitelist(&comm) { continue; }
            if !is_blacklist(&comm) { continue; }

            let entry = thread_map.entry(tid).or_insert(ThreadState {
                cpu_samples: Vec::with_capacity(sample_count),
                original_nice: Some(current_nice),
            });
            entry.cpu_samples.push(cpu_total);
            if entry.cpu_samples.len() > sample_count {
                entry.cpu_samples.remove(0);
            }
            new_thread_map.insert(tid, entry.clone());

            // 瞬时负载计算
            if entry.cpu_samples.len() >=2 {
                let delta_cpu = entry.cpu_samples.last().unwrap() - entry.cpu_samples[entry.cpu_samples.len()-2];
                let instant_cpu = delta_cpu as f32 *100.0 / (SAMPLE_INTERVAL_MS as f32 * 10.0);
                let mut trigger = false;

                // 瞬时爆发触发
                if instant_cpu > BURST_INSTANT_THRESHOLD {
                    trigger = true;
                }
                // 3秒平均负载触发
                if entry.cpu_samples.len() >= sample_count {
                    let avg_delta = entry.cpu_samples.last().unwrap() - entry.cpu_samples[0];
                    let avg_cpu = avg_delta as f32 *100.0 / ((WINDOW_SEC *1000) as f32 *10.0);
                    if avg_cpu > AVG_CPU_THRESHOLD {
                        trigger = true;
                    }
                }

                if trigger && current_nice < TARGET_NICE {
                    if set_tid_nice(tid, TARGET_NICE) {
                        if let Some(old) = entry.original_nice {
                            if !restore_list.iter().any(|(t,_)| *t == tid) {
                                restore_list.push((tid, old));
                            }
                        }
                    }
                }
            }
        }
        thread_map = new_thread_map;
        thread::sleep(Duration::from_millis(SAMPLE_INTERVAL_MS));
    }
}
