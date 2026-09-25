use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use nix::unistd::{setsid, fork, ForkResult, dup2, getpriority, setpriority, PrioWhich};
use nix::fcntl::open;
use nix::fcntl::O_RDWR;
use nix::sys::stat::Mode;
use signal_hook::{consts::{SIGINT, SIGTERM}, iterator::Signals};

// ========== 配置区 ==========
const SAMPLE_INTERVAL_MS: u64 = 500;
const WINDOW_SEC: u64 = 3;
const AVG_CPU_THRESHOLD: f32 = 3.5;
const BURST_INSTANT_THRESHOLD: f32 = 25.0;
const MAX_NICE: i32 = 19;
const GAME_PKG: &str = "com.tencent.tmgp.dfm";
const BIN_NAME: &[u8] = b"dfm-daemon";

#[derive(Debug,Default)]
struct ThreadCpuSample {
    prev_utime: u64,
    prev_stime: u64,
    total_delta: u64,
    sample_cnt: u64,
}

#[derive(Debug,Copy,Clone)]
struct ThreadState {
    orig_nice: i32,
    adjusted: bool,
}

fn get_pid_via_pidof(pkg: &str) -> Option<u32> {
    let out = Command::new("pidof").arg("-s").arg(pkg).output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.trim().parse::<u32>().ok()
        }
        _ => None
    }
}

fn get_pid_by_proc_scan(pkg: &str) -> Option<u32> {
    let dir_iter = match fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return None,
    };
    let mut buf = [0u8;512];
    for entry in dir_iter.flatten() {
        let name = entry.file_name();
        let pid_str = match name.to_str() {
            Some(s) if s.chars().all(|c| c.is_ascii_digit()) => s,
            _ => continue,
        };
        let pid = pid_str.parse::<u32>().ok()?;
        let proc_path = format!("/proc/{pid}/cmdline");
        let mut f = match fs::File::open(proc_path) { Ok(f)=>f, Err(_)=>continue };
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

fn read_tid_stat(tid: u32) -> Option<(u64,u64)> {
    let stat_path = format!("/proc/{tid}/stat");
    let mut stack_buf = [0u8;1024];
    let mut fd = fs::File::open(stat_path).ok()?;
    let read_len = fd.read(&mut stack_buf).ok()?;
    if read_len == 0 { return None; }
    let content = std::str::from_utf8(&stack_buf[0..read_len]).ok()?;

    let mut tokens = content.split_whitespace();
    let mut idx = 0usize;
    let mut utime:Option<u64> = None;
    let mut stime:Option<u64> = None;
    for tok in tokens {
        idx +=1;
        match idx {
            14 => utime = tok.parse().ok(),
            15 => stime = tok.parse().ok(),
            _ => {}
        }
        if utime.is_some() && stime.is_some() { break; }
    }
    Some((utime?, stime?))
}

fn set_thread_nice(tid: u32, nice: i32) -> bool {
    setpriority(PrioWhich::Process(tid as i32), nice).is_ok()
}

fn get_thread_nice(tid: u32) -> Option<i32> {
    getpriority(PrioWhich::Process(tid as i32)).ok()
}

fn daemonize_self() {
    match fork() {
        Ok(ForkResult::Child) => {},
        Ok(ForkResult::Parent{..}) => std::process::exit(0),
        Err(_) => return,
    }
    let _ = setsid();
    match fork() {
        Ok(ForkResult::Child) => {},
        Ok(ForkResult::Parent{..}) => std::process::exit(0),
        Err(_) => return,
    }
    if let Ok(devnull) = open("/dev/null", O_RDWR, Mode::empty()) {
        let _ = dup2(devnull, 0);
        let _ = dup2(devnull, 1);
        let _ = dup2(devnull, 2);
    }
}

fn single_instance_check() -> bool {
    let dir_iter = match fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return true,
    };
    let mut read_buf = [0u8;1024];

    for entry in dir_iter.flatten() {
        let fname = entry.file_name();
        let pid_str = match fname.to_str() {
            Some(s) if s.chars().all(|c| c.is_ascii_digit()) => s,
            _ => continue,
        };
        let pid = match pid_str.parse::<u32>() {Ok(p)=>p,Err(_)=>continue};

        let cmd_path = format!("/proc/{pid}/cmdline");
        let mut f = match fs::File::open(cmd_path) {Ok(f)=>f,Err(_)=>continue};
        let n = f.read(&mut read_buf).unwrap_or(0);
        if !read_buf[0..n].windows(BIN_NAME.len()).any(|w| w == BIN_NAME) {
            continue;
        }

        let stat_path = format!("/proc/{pid}/stat");
        let mut sf = match fs::File::open(stat_path) {Ok(f)=>f,Err(_)=>continue};
        let rn = sf.read(&mut read_buf).unwrap_or(0);
        let s = match std::str::from_utf8(&read_buf[0..rn]) {Ok(v)=>v,Err(_)=>continue};
        let mut t = s.split_whitespace();
        let mut idx = 0;
        let mut ppid_opt:Option<u32> = None;
        for token in t {
            idx +=1;
            if idx ==4 {
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

fn restore_all(restore_list:&mut Vec<(u32,i32)>) {
    for (tid,old) in restore_list.iter() {
        let _ = set_thread_nice(*tid,*old);
    }
    restore_list.clear();
}

fn main() -> io::Result<()> {
    if !single_instance_check() {
        eprintln!("daemon already running");
        std::process::exit(1);
    }
    daemonize_self();

    let restore_list: Arc<Mutex<Vec<(u32,i32)>>> = Arc::new(Mutex::new(Vec::new()));
    let rl_clone = Arc::clone(&restore_list);

    thread::spawn(move ||{
        let mut signals = Signals::new(&[SIGINT,SIGTERM]).unwrap();
        for _ in signals.forever() {
            let mut guard = rl_clone.lock().unwrap();
            restore_all(&mut guard);
            std::process::exit(0);
        }
    });

    let mut thread_state_map: HashMap<u32, ThreadState> = HashMap::new();
    let mut cpu_history: HashMap<u32, ThreadCpuSample> = HashMap::new();
    let mut tids_buf: Vec<u32> = Vec::with_capacity(128);

    loop {
        let Some(game_pid) = find_game_pid() else {
            let mut guard = restore_list.lock().unwrap();
            restore_all(&mut guard);
            thread_state_map.clear();
            cpu_history.clear();
            thread::sleep(Duration::from_millis(SAMPLE_INTERVAL_MS));
            continue;
        };

        tids_buf.clear();
        let task_path = format!("/proc/{game_pid}/task");
        let dir_iter = match fs::read_dir(task_path) {
            Ok(d)=>d,
            Err(_)=>{
                thread::sleep(Duration::from_millis(SAMPLE_INTERVAL_MS));
                continue;
            }
        };
        for e in dir_iter.flatten() {
            let fname = e.file_name();
            let s = match fname.to_str() { Some(v)=>v, None=>continue };
            let tid = match s.parse::<u32>() { Ok(v)=>v, Err(_)=>continue };
            tids_buf.push(tid);
        }

        // ========== 每轮清理死亡TID ==========
        let alive_set = std::collections::HashSet::from_iter(tids_buf.iter().copied());
        let mut dead_tids = Vec::new();
        for tid in thread_state_map.keys() {
            if !alive_set.contains(tid) {
                dead_tids.push(*tid);
            }
        }
        for tid in dead_tids {
            if let Some(state) = thread_state_map.remove(&tid) {
                if state.adjusted {
                    let mut g = restore_list.lock().unwrap();
                    g.retain(|&(t,_)| t != tid);
                    // 线程已消亡，不必调用set_thread_nice
                }
            }
            cpu_history.remove(&tid);
        }

        for &tid in tids_buf.iter() {
            let Some((utime, stime)) = read_tid_stat(tid) else { continue };
            let entry = cpu_history.entry(tid).or_default();
            let delta_u = utime.saturating_sub(entry.prev_utime);
            let delta_s = stime.saturating_sub(entry.prev_stime);
            let delta_total = delta_u + delta_s;

            entry.prev_utime = utime;
            entry.prev_stime = stime;
            entry.total_delta += delta_total;
            entry.sample_cnt +=1;

            let jiff_per_sec = 100u64;
            let window_jiff = WINDOW_SEC * jiff_per_sec;
            if entry.total_delta > window_jiff {
                entry.total_delta = window_jiff;
            }

            let avg_cpu_pct = (entry.total_delta as f32) / (WINDOW_SEC as f32);
            let instant_pct = (delta_total as f32) * 100.0 / jiff_per_sec as f32;

            let state = thread_state_map.entry(tid).or_insert(ThreadState{orig_nice:0, adjusted:false});

            if (avg_cpu_pct > AVG_CPU_THRESHOLD || instant_pct > BURST_INSTANT_THRESHOLD) && !state.adjusted {
                if let Some(cur_nice) = get_thread_nice(tid) {
                    state.orig_nice = cur_nice;
                    state.adjusted = true;
                    restore_list.lock().unwrap().push((tid,cur_nice));
                    set_thread_nice(tid, MAX_NICE);
                }
            }
        }

        thread::sleep(Duration::from_millis(SAMPLE_INTERVAL_MS));
    }
}
