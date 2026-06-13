use std::fs;
use std::io::{self, Write, BufRead};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::os::unix::fs::PermissionsExt;
use std::thread;
use anyhow::{Result, Context, bail};

// ==================== 配置区 ====================
const DEFAULT_INTERVAL: u64 = 1;
const DEFAULT_PERM_MODE: u32 = 0o2761;
const RESET_INTERVAL_SECS: u64 = 21600;
const PID_FILE: &str = "/data/adb/anr_cleaner.pid";

const SAFE_CLEAN_PATHS: &[&str] = &[
    "/data/anr",
    "/anr",
    "/data/system/dropbox",
    "/data/tombstones",
    "/data/aee_exp",
    "/data/media/0/Android/media",
];

const FULL_DELETE_PATHS: &[&str] = &[
    "/data/media/0/Documents",
    "/data/media/0/.yuetu_unlock",
    "/data/media/0/.vivocrash",
    "/data/media/0/.vivoBrowser",
    "/data/media/0/.tbs",
    "/data/media/0/.BBKAppStore",
    "/data/media/0/阅图锁屏",
    "/data/media/0/Pictures/Nekogram",
    "/data/media/0/.系统文件，请勿删除",
    "/data/media/0/xinhao",
    "/data/media/0/Download/.TempDragFile",
    "/data/media/0/Download/QQ",
    "/data/media/0/Download/.csj",
];

const PERM_FIX_PATHS: &[&str] = &[
    "/data/media/0/Android/data",
    "/data/media/0/Android/obb",
];

const TIMESTAMP_TARGETS: &[&str] = &[
    "/dev/ptmx",
    "/tmp",
    "/data/adb",
];

// ==================== 配置结构体 ====================
#[derive(Debug, Clone)]
struct Config {
    interval: u64,
    safe_clean_paths: Vec<PathBuf>,
    full_delete_paths: Vec<PathBuf>,
    perm_fix_paths: Vec<PathBuf>,
    perm_mode: u32,
    timestamp_targets: Vec<PathBuf>,
    target_timestamp: SystemTime,
    pid_file: PathBuf,
    reset_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval: DEFAULT_INTERVAL,
            safe_clean_paths: SAFE_CLEAN_PATHS.iter().map(|s| PathBuf::from(*s)).collect(),
            full_delete_paths: FULL_DELETE_PATHS.iter().map(|s| PathBuf::from(*s)).collect(),
            perm_fix_paths: PERM_FIX_PATHS.iter().map(|s| PathBuf::from(*s)).collect(),
            perm_mode: DEFAULT_PERM_MODE,
            timestamp_targets: TIMESTAMP_TARGETS.iter().map(|s| PathBuf::from(*s)).collect(),
            target_timestamp: UNIX_EPOCH,
            pid_file: PathBuf::from(PID_FILE),
            reset_interval: Duration::from_secs(RESET_INTERVAL_SECS),
        }
    }
}

// ==================== PID 管理 (Safe) ====================
struct PidManager {
    pid_file: PathBuf,
}

impl PidManager {
    fn new(pid_file: PathBuf) -> Self {
        PidManager { pid_file }
    }

    fn read_pid(&self) -> Option<u32> {
        fs::read_to_string(&self.pid_file)
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    fn is_running(&self) -> bool {
        if let Some(pid) = self.read_pid() {
            fs::metadata(format!("/proc/{}", pid)).is_ok()
        } else {
            false
        }
    }

    fn write_pid(&self) -> Result<()> {
        let pid = std::process::id();
        fs::write(&self.pid_file, pid.to_string())
            .with_context(|| format!("写入PID文件失败: {}", self.pid_file.display()))?;
        Ok(())
    }

    fn remove_pid(&self) -> Result<()> {
        if self.pid_file.exists() {
            fs::remove_file(&self.pid_file)
                .with_context(|| format!("删除PID文件失败: {}", self.pid_file.display()))?;
        }
        Ok(())
    }

    fn stop(&self) -> Result<bool> {
        if let Some(pid) = self.read_pid() {
            self.remove_pid()?;

            for _ in 0..100 {
                thread::sleep(Duration::from_millis(100));
                if !self.is_running() {
                    return Ok(true);
                }
            }

            let _ = Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();

            thread::sleep(Duration::from_millis(300));
            Ok(!self.is_running())
        } else {
            Ok(false)
        }
    }
}

// ==================== 核心清理引擎 (单线程·无锁·低功耗) ====================
struct Cleaner {
    config: Config,
    last_safe_mtime: Vec<Option<SystemTime>>,
    last_full_mtime: Vec<Option<SystemTime>>,
    last_perm_mode: Vec<Option<u32>>,
    last_time_mtime: Vec<Option<SystemTime>>,
}

impl Cleaner {
    fn new(config: Config) -> Self {
        Cleaner {
            last_safe_mtime: vec![None; config.safe_clean_paths.len()],
            last_full_mtime: vec![None; config.full_delete_paths.len()],
            last_perm_mode: vec![None; config.perm_fix_paths.len()],
            last_time_mtime: vec![None; config.timestamp_targets.len()],
            config,
        }
    }

    fn safe_clean(&mut self) {
        for (idx, path) in self.config.safe_clean_paths.iter().enumerate() {
            if !path.exists() {
                self.last_safe_mtime[idx] = None;
                continue;
            }

            let current_mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
            if current_mtime == self.last_safe_mtime[idx] && current_mtime.is_some() {
                continue;
            }
            self.last_safe_mtime[idx] = current_mtime;

            let entries: Vec<_> = match fs::read_dir(path) {
                Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
                Err(_) => continue,
            };

            for entry in entries {
                let _ = Self::remove_all(&entry.path());
            }
        }
    }

    fn full_delete(&mut self) {
        for (idx, path) in self.config.full_delete_paths.iter().enumerate() {
            if !path.exists() {
                self.last_full_mtime[idx] = None;
                continue;
            }

            let current_mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
            if current_mtime == self.last_full_mtime[idx] && current_mtime.is_some() {
                continue;
            }
            self.last_full_mtime[idx] = current_mtime;

            let _ = Self::remove_all(path);
        }
    }

    fn remove_all(path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            let entries: Vec<_> = fs::read_dir(path)?
                .filter_map(|e| e.ok())
                .collect();
            for entry in entries {
                let _ = Self::remove_all(&entry.path());
            }
            fs::remove_dir(path)?;
        } else {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn fix_permissions(&mut self) {
        for (idx, path) in self.config.perm_fix_paths.iter().enumerate() {
            if !path.is_dir() {
                continue;
            }

            let current_mode = fs::metadata(path)
                .map(|m| m.permissions().mode() & 0o7777)
                .ok();

            if current_mode == Some(self.config.perm_mode) {
                continue;
            }

            if let Ok(mut permissions) = fs::metadata(path).map(|m| m.permissions()) {
                permissions.set_mode(self.config.perm_mode);
                let _ = fs::set_permissions(path, permissions);
                self.last_perm_mode[idx] = Some(self.config.perm_mode);
            }
        }
    }

    fn fix_timestamp(&mut self) {
        let times = filetime::FileTime::from_system_time(self.config.target_timestamp);
        for (idx, path) in self.config.timestamp_targets.iter().enumerate() {
            if !path.exists() {
                continue;
            }

            let current = fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok());

            if current == Some(self.config.target_timestamp) {
                continue;
            }

            let _ = filetime::set_file_times(path, times, times);
            self.last_time_mtime[idx] = Some(self.config.target_timestamp);
        }
    }

    fn run_cycle(&mut self) {
        let start = Instant::now();
        self.safe_clean();
        self.full_delete();
        self.fix_permissions();
        self.fix_timestamp();
        let _elapsed = start.elapsed();
    }

    fn daemon_loop(&mut self) -> Result<()> {
        let my_pid = std::process::id();
        let pid_mgr = PidManager::new(self.config.pid_file.clone());
        pid_mgr.write_pid()?;

        let mut last_reset = Instant::now();

        loop {
            match pid_mgr.read_pid() {
                Some(pid) if pid == my_pid => {}
                _ => {
                    break;
                }
            }

            let cycle_start = Instant::now();
            self.run_cycle();

            if last_reset.elapsed() >= self.config.reset_interval {
                let _ = pid_mgr.remove_pid();
                return self.self_reset();
            }

            let elapsed = cycle_start.elapsed();
            let sleep_duration = Duration::from_secs(self.config.interval).saturating_sub(elapsed);
            if sleep_duration > Duration::ZERO {
                thread::sleep(sleep_duration);
            }
        }

        let _ = pid_mgr.remove_pid();
        Ok(())
    }

    fn self_reset(&self) -> Result<()> {
        let exe = std::env::current_exe()?;
        let mut cmd = Command::new(&exe);
        for arg in std::env::args().skip(1) {
            cmd.arg(arg);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = cmd.spawn();
        std::process::exit(0);
    }
}

// ==================== 守护进程模式入口 ====================
fn run_daemon(interval: u64) -> Result<()> {
    let my_pid = std::process::id().to_string();

    let _ = Command::new("/system/bin/ionice")
        .args(["-c", "3", "-p", &my_pid])
        .stdout(Stdio::null()).stderr(Stdio::null()).status();
    let _ = Command::new("/system/bin/chrt")
        .args(["-i", "0", "-p", &my_pid])
        .stdout(Stdio::null()).stderr(Stdio::null()).status();
    let _ = Command::new("/system/bin/renice")
        .args(["+19", "-p", &my_pid])
        .stdout(Stdio::null()).stderr(Stdio::null()).status();

    let _ = fs::write("/proc/self/timerslack_ns", "1000000000\n");

    let mut config = Config::default();
    config.interval = interval;

    let mut cleaner = Cleaner::new(config);
    cleaner.daemon_loop()
}

// ==================== 终端交互式菜单 ====================
fn read_line() -> String {
    let mut input = String::new();
    let stdin = io::stdin();
    stdin.lock().read_line(&mut input).unwrap_or(0);
    input.trim().to_string()
}

fn check_root() -> bool {
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("Uid:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[1] == "0" {
                    return true;
                }
            }
        }
    }
    false
}

fn is_running() -> bool {
    PidManager::new(PathBuf::from(PID_FILE)).is_running()
}

fn get_pid() -> Option<u32> {
    PidManager::new(PathBuf::from(PID_FILE)).read_pid()
}

fn menu_start() {
    if is_running() {
        println!("⚠️ 运行中 (PID:{})", get_pid().unwrap_or(0));
        return;
    }

    print!("间隔(秒, 回车默认{}s): ", DEFAULT_INTERVAL);
    let _ = io::stdout().flush();
    let input = read_line();
    let interval: u64 = input.parse().unwrap_or(DEFAULT_INTERVAL);

    println!("=== ANR清理+权限+时间戳 Safe Rust低功耗版 ===");
    println!("✨ 特性: 零unsafe, 单线程无锁, 变更检测低功耗, 无日志");

    let exe = std::env::current_exe().unwrap_or_default();
    let mut cmd = Command::new(&exe);
    cmd.arg("daemon").arg(interval.to_string());
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    match cmd.spawn() {
        Ok(_child) => {
            for _ in 0..50 {
                thread::sleep(Duration::from_millis(100));
                if is_running() {
                    println!("✅ 启动成功 | PID: {} | 间隔: {}s", get_pid().unwrap_or(0), interval);
                    println!("📌 安全清理: {:?}", SAFE_CLEAN_PATHS);
                    println!("📌 全量删除: {:?}", FULL_DELETE_PATHS);
                    println!("📌 权限目录: {:?}", PERM_FIX_PATHS);
                    println!("📌 权限模式: {:o}", DEFAULT_PERM_MODE);
                    println!("📌 时间戳文件: {:?}", TIMESTAMP_TARGETS);
                    return;
                }
            }
            println!("❌ 启动失败: PID 文件未写入");
        }
        Err(e) => println!("❌ 启动失败: {}", e),
    }
}

fn menu_stop() {
    if !is_running() {
        println!("❌ 未运行");
        return;
    }

    let pid_mgr = PidManager::new(PathBuf::from(PID_FILE));
    match pid_mgr.stop() {
        Ok(true) => println!("✅ 已完全终止"),
        Ok(false) => println!("❌ 终止失败, 手动: kill -9 {}", get_pid().unwrap_or(0)),
        Err(e) => println!("❌ 错误: {}", e),
    }
}

fn menu_status() {
    if is_running() {
        println!("✅ 运行中 (PID:{})", get_pid().unwrap_or(0));
    } else {
        println!("❌ 未运行");
    }
}

fn menu_once() {
    println!("=== 单次执行模式 ===");
    let config = Config::default();
    let mut cleaner = Cleaner::new(config);
    cleaner.run_cycle();
    println!("✅ 单次执行完成");
}

fn show_menu() {
    println!("\n=== ANR清理+权限+时间戳 Safe Rust低功耗版 ===");
    println!("⚠️ 特性: 零unsafe代码, 单线程无锁无竞争, 变更检测低功耗, 无日志输出");
    println!("⚠️ 优化: 目录mtime缓存跳过, 1秒间隔下最小化I/O唤醒");
    println!();
    println!("1. 启动后台进程");
    println!("2. 停止进程");
    println!("3. 查看运行状态");
    println!("4. 单次执行(调试)");
    println!("5. 退出脚本");
    print!("请输入选择(1-5): ");
    let _ = io::stdout().flush();
}

fn main() {
    if !check_root() {
        println!("❌ 必须先执行 su 获取root权限");
        return;
    }

    let exe_path = std::env::current_exe().unwrap_or_default();
    let exe_dir = exe_path.parent().unwrap_or(Path::new("/"));
    if exe_dir != Path::new("/data/adb") {
        println!("⚠️ 警告: 建议将程序移至 /data/adb/ 目录运行");
    }

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "daemon" {
        let interval = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL);
        if let Err(e) = run_daemon(interval) {
            eprintln!("守护进程错误: {}", e);
        }
        return;
    }

    loop {
        show_menu();
        let choice = read_line();

        match choice.as_str() {
            "1" => menu_start(),
            "2" => menu_stop(),
            "3" => menu_status(),
            "4" => menu_once(),
            "5" => {
                println!("👋 已退出, 后台进程仍运行, 可通过菜单2终止");
                break;
            }
            _ => println!("❌ 请输入 1-5 的数字"),
        }
    }
}
    "/data/media/0/Download/QQ",
    "/data/media/0/Download/.csj",

const PERM_FIX_PATHS: &[&str] = &[
    "/data/media/0/Android/data",
    "/data/media/0/Android/obb",
];

const TIMESTAMP_TARGETS: &[&str] = &[
    "/dev/ptmx",
    "/tmp",
    "/data/adb",
];

// ==================== 配置结构体 ====================
#[derive(Debug, Clone)]
struct Config {
    interval: u64,
    safe_clean_paths: Vec<PathBuf>,
    full_delete_paths: Vec<PathBuf>,
    perm_fix_paths: Vec<PathBuf>,
    perm_mode: u32,
    timestamp_targets: Vec<PathBuf>,
    target_timestamp: SystemTime,
    pid_file: PathBuf,
    reset_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval: DEFAULT_INTERVAL,
            safe_clean_paths: SAFE_CLEAN_PATHS.iter().map(|s| PathBuf::from(*s)).collect(),
            full_delete_paths: FULL_DELETE_PATHS.iter().map(|s| PathBuf::from(*s)).collect(),
            perm_fix_paths: PERM_FIX_PATHS.iter().map(|s| PathBuf::from(*s)).collect(),
            perm_mode: DEFAULT_PERM_MODE,
            timestamp_targets: TIMESTAMP_TARGETS.iter().map(|s| PathBuf::from(*s)).collect(),
            target_timestamp: UNIX_EPOCH,
            pid_file: PathBuf::from(PID_FILE),
            reset_interval: Duration::from_secs(RESET_INTERVAL_SECS),
        }
    }
}

// ==================== PID 管理 (Safe) ====================
struct PidManager {
    pid_file: PathBuf,
}

impl PidManager {
    fn new(pid_file: PathBuf) -> Self {
        PidManager { pid_file }
    }

    fn read_pid(&self) -> Option<u32> {
        fs::read_to_string(&self.pid_file)
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    fn is_running(&self) -> bool {
        if let Some(pid) = self.read_pid() {
            fs::metadata(format!("/proc/{}", pid)).is_ok()
        } else {
            false
        }
    }

    fn write_pid(&self) -> Result<()> {
        let pid = std::process::id();
        fs::write(&self.pid_file, pid.to_string())
            .with_context(|| format!("写入PID文件失败: {}", self.pid_file.display()))?;
        Ok(())
    }

    fn remove_pid(&self) -> Result<()> {
        if self.pid_file.exists() {
            fs::remove_file(&self.pid_file)
                .with_context(|| format!("删除PID文件失败: {}", self.pid_file.display()))?;
        }
        Ok(())
    }

    fn stop(&self) -> Result<bool> {
        if let Some(pid) = self.read_pid() {
            self.remove_pid()?;

            for _ in 0..100 {
                thread::sleep(Duration::from_millis(100));
                if !self.is_running() {
                    return Ok(true);
                }
            }

            let _ = Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();

            thread::sleep(Duration::from_millis(300));
            Ok(!self.is_running())
        } else {
            Ok(false)
        }
    }
}

// ==================== 核心清理引擎 (单线程·无锁·低功耗) ====================
struct Cleaner {
    config: Config,
    last_safe_mtime: Vec<Option<SystemTime>>,
    last_full_mtime: Vec<Option<SystemTime>>,
    last_perm_mode: Vec<Option<u32>>,
    last_time_mtime: Vec<Option<SystemTime>>,
}

impl Cleaner {
    fn new(config: Config) -> Self {
        Cleaner {
            last_safe_mtime: vec![None; config.safe_clean_paths.len()],
            last_full_mtime: vec![None; config.full_delete_paths.len()],
            last_perm_mode: vec![None; config.perm_fix_paths.len()],
            last_time_mtime: vec![None; config.timestamp_targets.len()],
            config,
        }
    }

    fn safe_clean(&mut self) {
        for (idx, path) in self.config.safe_clean_paths.iter().enumerate() {
            if !path.exists() {
                self.last_safe_mtime[idx] = None;
                continue;
            }

            let current_mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
            if current_mtime == self.last_safe_mtime[idx] && current_mtime.is_some() {
                continue;
            }
            self.last_safe_mtime[idx] = current_mtime;

            let entries: Vec<_> = match fs::read_dir(path) {
                Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
                Err(_) => continue,
            };

            for entry in entries {
                let _ = Self::remove_all(&entry.path());
            }
        }
    }

    fn full_delete(&mut self) {
        for (idx, path) in self.config.full_delete_paths.iter().enumerate() {
            if !path.exists() {
                self.last_full_mtime[idx] = None;
                continue;
            }

            let current_mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
            if current_mtime == self.last_full_mtime[idx] && current_mtime.is_some() {
                continue;
            }
            self.last_full_mtime[idx] = current_mtime;

            let _ = Self::remove_all(path);
        }
    }

    fn remove_all(path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            let entries: Vec<_> = fs::read_dir(path)?
                .filter_map(|e| e.ok())
                .collect();
            for entry in entries {
                let _ = Self::remove_all(&entry.path());
            }
            fs::remove_dir(path)?;
        } else {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn fix_permissions(&mut self) {
        for (idx, path) in self.config.perm_fix_paths.iter().enumerate() {
            if !path.is_dir() {
                continue;
            }

            let current_mode = fs::metadata(path)
                .map(|m| m.permissions().mode() & 0o7777)
                .ok();

            if current_mode == Some(self.config.perm_mode) {
                continue;
            }

            if let Ok(mut permissions) = fs::metadata(path).map(|m| m.permissions()) {
                permissions.set_mode(self.config.perm_mode);
                let _ = fs::set_permissions(path, permissions);
                self.last_perm_mode[idx] = Some(self.config.perm_mode);
            }
        }
    }

    fn fix_timestamp(&mut self) {
        let times = filetime::FileTime::from_system_time(self.config.target_timestamp);
        for (idx, path) in self.config.timestamp_targets.iter().enumerate() {
            if !path.exists() {
                continue;
            }

            let current = fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok());

            if current == Some(self.config.target_timestamp) {
                continue;
            }

            let _ = filetime::set_file_times(path, times, times);
            self.last_time_mtime[idx] = Some(self.config.target_timestamp);
        }
    }

    fn run_cycle(&mut self) {
        let start = Instant::now();
        self.safe_clean();
        self.full_delete();
        self.fix_permissions();
        self.fix_timestamp();
        let _elapsed = start.elapsed();
    }

    fn daemon_loop(&mut self) -> Result<()> {
        let my_pid = std::process::id();
        let pid_mgr = PidManager::new(self.config.pid_file.clone());
        pid_mgr.write_pid()?;

        let mut last_reset = Instant::now();

        loop {
            match pid_mgr.read_pid() {
                Some(pid) if pid == my_pid => {}
                _ => {
                    break;
                }
            }

            let cycle_start = Instant::now();
            self.run_cycle();

            if last_reset.elapsed() >= self.config.reset_interval {
                let _ = pid_mgr.remove_pid();
                return self.self_reset();
            }

            let elapsed = cycle_start.elapsed();
            let sleep_duration = Duration::from_secs(self.config.interval).saturating_sub(elapsed);
            if sleep_duration > Duration::ZERO {
                thread::sleep(sleep_duration);
            }
        }

        let _ = pid_mgr.remove_pid();
        Ok(())
    }

    fn self_reset(&self) -> Result<()> {
        let exe = std::env::current_exe()?;
        let mut cmd = Command::new(&exe);
        for arg in std::env::args().skip(1) {
            cmd.arg(arg);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = cmd.spawn();
        std::process::exit(0);
    }
}

// ==================== 守护进程模式入口 ====================
fn run_daemon(interval: u64) -> Result<()> {
    let my_pid = std::process::id().to_string();

    let _ = Command::new("/system/bin/ionice")
        .args(["-c", "3", "-p", &my_pid])
        .stdout(Stdio::null()).stderr(Stdio::null()).status();
    let _ = Command::new("/system/bin/chrt")
        .args(["-i", "0", "-p", &my_pid])
        .stdout(Stdio::null()).stderr(Stdio::null()).status();
    let _ = Command::new("/system/bin/renice")
        .args(["+19", "-p", &my_pid])
        .stdout(Stdio::null()).stderr(Stdio::null()).status();

    let _ = fs::write("/proc/self/timerslack_ns", "1000000000\n");

    let mut config = Config::default();
    config.interval = interval;

    let mut cleaner = Cleaner::new(config);
    cleaner.daemon_loop()
}

// ==================== 终端交互式菜单 ====================
fn read_line() -> String {
    let mut input = String::new();
    let stdin = io::stdin();
    stdin.lock().read_line(&mut input).unwrap_or(0);
    input.trim().to_string()
}

fn check_root() -> bool {
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("Uid:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[1] == "0" {
                    return true;
                }
            }
        }
    }
    false
}

fn is_running() -> bool {
    PidManager::new(PathBuf::from(PID_FILE)).is_running()
}

fn get_pid() -> Option<u32> {
    PidManager::new(PathBuf::from(PID_FILE)).read_pid()
}

fn menu_start() {
    if is_running() {
        println!("⚠️ 运行中 (PID:{})", get_pid().unwrap_or(0));
        return;
    }

    print!("间隔(秒, 回车默认{}s): ", DEFAULT_INTERVAL);
    let _ = io::stdout().flush();
    let input = read_line();
    let interval: u64 = input.parse().unwrap_or(DEFAULT_INTERVAL);

    println!("=== ANR清理+权限+时间戳 Safe Rust低功耗版 ===");
    println!("✨ 特性: 零unsafe, 单线程无锁, 变更检测低功耗, 无日志");

    let exe = std::env::current_exe().unwrap_or_default();
    let mut cmd = Command::new(&exe);
    cmd.arg("daemon").arg(interval.to_string());
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    match cmd.spawn() {
        Ok(_child) => {
            for _ in 0..50 {
                thread::sleep(Duration::from_millis(100));
                if is_running() {
                    println!("✅ 启动成功 | PID: {} | 间隔: {}s", get_pid().unwrap_or(0), interval);
                    println!("📌 安全清理: {:?}", SAFE_CLEAN_PATHS);
                    println!("📌 全量删除: {:?}", FULL_DELETE_PATHS);
                    println!("📌 权限目录: {:?}", PERM_FIX_PATHS);
                    println!("📌 权限模式: {:o}", DEFAULT_PERM_MODE);
                    println!("📌 时间戳文件: {:?}", TIMESTAMP_TARGETS);
                    return;
                }
            }
            println!("❌ 启动失败: PID 文件未写入");
        }
        Err(e) => println!("❌ 启动失败: {}", e),
    }
}

fn menu_stop() {
    if !is_running() {
        println!("❌ 未运行");
        return;
    }

    let pid_mgr = PidManager::new(PathBuf::from(PID_FILE));
    match pid_mgr.stop() {
        Ok(true) => println!("✅ 已完全终止"),
        Ok(false) => println!("❌ 终止失败, 手动: kill -9 {}", get_pid().unwrap_or(0)),
        Err(e) => println!("❌ 错误: {}", e),
    }
}

fn menu_status() {
    if is_running() {
        println!("✅ 运行中 (PID:{})", get_pid().unwrap_or(0));
    } else {
        println!("❌ 未运行");
    }
}

fn menu_once() {
    println!("=== 单次执行模式 ===");
    let config = Config::default();
    let mut cleaner = Cleaner::new(config);
    cleaner.run_cycle();
    println!("✅ 单次执行完成");
}

fn show_menu() {
    println!("\n=== ANR清理+权限+时间戳 Safe Rust低功耗版 ===");
    println!("⚠️ 特性: 零unsafe代码, 单线程无锁无竞争, 变更检测低功耗, 无日志输出");
    println!("⚠️ 优化: 目录mtime缓存跳过, 1秒间隔下最小化I/O唤醒");
    println!();
    println!("1. 启动后台进程");
    println!("2. 停止进程");
    println!("3. 查看运行状态");
    println!("4. 单次执行(调试)");
    println!("5. 退出脚本");
    print!("请输入选择(1-5): ");
    let _ = io::stdout().flush();
}

fn main() {
    if !check_root() {
        println!("❌ 必须先执行 su 获取root权限");
        return;
    }

    let exe_path = std::env::current_exe().unwrap_or_default();
    let exe_dir = exe_path.parent().unwrap_or(Path::new("/"));
    if exe_dir != Path::new("/data/adb") {
        println!("⚠️ 警告: 建议将程序移至 /data/adb/ 目录运行");
    }

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "daemon" {
        let interval = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL);
        if let Err(e) = run_daemon(interval) {
            eprintln!("守护进程错误: {}", e);
        }
        return;
    }

    loop {
        show_menu();
        let choice = read_line();

        match choice.as_str() {
            "1" => menu_start(),
            "2" => menu_stop(),
            "3" => menu_status(),
            "4" => menu_once(),
            "5" => {
                println!("👋 已退出, 后台进程仍运行, 可通过菜单2终止");
                break;
            }
            _ => println!("❌ 请输入 1-5 的数字"),
        }
    }
}
    "/data/media/0/阅图锁屏",
    "/data/media/0/Pictures/Nekogram",
    "/data/media/0/.系统文件，请勿删除",
    "/data/media/0/xinhao",
    "/data/media/0/Download/.TempDragFile",
    "/data/media/0/Download/QQ",
    "/data/media/0/Download/.csj",
];

const PERM_FIX_PATHS: &[&str] = &[
    "/data/media/0/Android/data",
    "/data/media/0/Android/obb",
];

const TIMESTAMP_TARGETS: &[&str] = &[
    "/dev/ptmx",
    "/tmp",
];

// ==================== 配置结构体 ====================
#[derive(Debug, Clone)]
struct Config {
    interval: u64,
    safe_clean_paths: Vec<PathBuf>,
    full_delete_paths: Vec<PathBuf>,
    perm_fix_paths: Vec<PathBuf>,
    perm_mode: u32,
    timestamp_targets: Vec<PathBuf>,
    target_timestamp: SystemTime,
    pid_file: PathBuf,
    reset_interval: Duration,
    worker_threads: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval: DEFAULT_INTERVAL,
            safe_clean_paths: SAFE_CLEAN_PATHS.iter().map(|s| PathBuf::from(*s)).collect(),
            full_delete_paths: FULL_DELETE_PATHS.iter().map(|s| PathBuf::from(*s)).collect(),
            perm_fix_paths: PERM_FIX_PATHS.iter().map(|s| PathBuf::from(*s)).collect(),
            perm_mode: DEFAULT_PERM_MODE,
            timestamp_targets: TIMESTAMP_TARGETS.iter().map(|s| PathBuf::from(*s)).collect(),
            target_timestamp: UNIX_EPOCH,
            pid_file: PathBuf::from(PID_FILE),
            reset_interval: Duration::from_secs(RESET_INTERVAL_SECS),
            worker_threads: 0,
        }
    }
}

// ==================== PID 管理 ====================
struct PidManager {
    pid_file: PathBuf,
}

impl PidManager {
    fn new(pid_file: PathBuf) -> Self {
        PidManager { pid_file }
    }

    fn read_pid(&self) -> Option<u32> {
        fs::read_to_string(&self.pid_file)
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    fn is_running(&self) -> bool {
        if let Some(pid) = self.read_pid() {
            unsafe { libc::kill(pid as i32, 0) == 0 }
        } else {
            false
        }
    }

    fn write_pid(&self) -> Result<()> {
        let pid = std::process::id();
        fs::write(&self.pid_file, pid.to_string())
            .with_context(|| format!("写入PID文件失败: {}", self.pid_file.display()))?;
        Ok(())
    }

    fn remove_pid(&self) -> Result<()> {
        if self.pid_file.exists() {
            fs::remove_file(&self.pid_file)
                .with_context(|| format!("删除PID文件失败: {}", self.pid_file.display()))?;
        }
        Ok(())
    }

    fn stop(&self) -> Result<bool> {
        if let Some(pid) = self.read_pid() {
            println!("终止 PID: {}", pid);
            unsafe { libc::kill(pid as i32, libc::SIGTERM); }
            thread::sleep(Duration::from_millis(300));
            if self.is_running() {
                unsafe { libc::kill(pid as i32, libc::SIGKILL); }
                thread::sleep(Duration::from_millis(100));
            }
            let still_running = self.is_running();
            let _ = self.remove_pid();
            Ok(!still_running)
        } else {
            Ok(false)
        }
    }
}

// ==================== 核心清理引擎 ====================
struct Cleaner {
    config: Config,
    running: Arc<AtomicBool>,
}

impl Cleaner {
    fn new(config: Config) -> Self {
        Cleaner {
            config,
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    fn init_thread_pool(&self) {
        let threads = if self.config.worker_threads == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        } else {
            self.config.worker_threads
        };

        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global();

        info!("线程池初始化完成: {} 线程", threads);
    }

    /// 安全清理：保留目录，删除内部所有内容（多线程并行）
    fn safe_clean(&self) {
        let paths: Vec<_> = self.config.safe_clean_paths
            .iter()
            .filter(|p| p.exists())
            .cloned()
            .collect();

        if paths.is_empty() { return; }

        paths.par_iter().for_each(|path| {
            debug!("安全清理: {}", path.display());
            let entries: Vec<_> = match fs::read_dir(path) {
                Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
                Err(_) => return,
            };
            entries.par_iter().for_each(|entry| {
                let _ = Self::remove_all(&entry.path());
            });
        });
    }

    /// 全量删除：删除整个目录树（多线程并行）
    fn full_delete(&self) {
        let paths: Vec<_> = self.config.full_delete_paths
            .iter()
            .filter(|p| p.exists())
            .cloned()
            .collect();

        if paths.is_empty() { return; }

        paths.par_iter().for_each(|path| {
            debug!("全量删除: {}", path.display());
            let _ = Self::remove_all(path);
        });
    }

    /// 递归删除文件或目录
    fn remove_all(path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            let entries: Vec<_> = fs::read_dir(path)?
                .filter_map(|e| e.ok())
                .collect();
            entries.par_iter().for_each(|entry| {
                let _ = Self::remove_all(&entry.path());
            });
            fs::remove_dir(path)?;
        } else {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// 权限修复：并行修复目录权限
    fn fix_permissions(&self) {
        let paths: Vec<_> = self.config.perm_fix_paths
            .iter()
            .filter(|p| p.is_dir())
            .cloned()
            .collect();

        if paths.is_empty() { return; }

        let mode = self.config.perm_mode;
        paths.par_iter().for_each(|path| {
            debug!("修复权限: {} -> {:o}", path.display(), mode);
            if let Ok(mut permissions) = fs::metadata(path).map(|m| m.permissions()) {
                let current_mode = permissions.mode() & 0o7777;
                if current_mode != mode {
                    permissions.set_mode(mode);
                    let _ = fs::set_permissions(path, permissions);
                }
            }
        });
    }

    /// 时间戳修改：并行修改目标文件时间
    fn fix_timestamp(&self) {
        let paths: Vec<_> = self.config.timestamp_targets
            .iter()
            .filter(|p| p.exists())
            .cloned()
            .collect();

        if paths.is_empty() { return; }

        let times = filetime::FileTime::from_system_time(self.config.target_timestamp);
        paths.par_iter().for_each(|path| {
            debug!("修改时间戳: {}", path.display());
            let _ = filetime::set_file_times(path, times, times);
        });
    }

    /// 执行完整清理周期
    fn run_cycle(&self) {
        let start = Instant::now();

        thread::scope(|s| {
            s.spawn(|| self.safe_clean());
            s.spawn(|| self.full_delete());
            s.spawn(|| self.fix_permissions());
            s.spawn(|| self.fix_timestamp());
        });

        info!("清理周期完成，耗时: {:?}", start.elapsed());
    }

    /// 守护进程主循环
    fn daemon_loop(&self) -> Result<()> {
        self.init_thread_pool();
        let mut last_reset = Instant::now();

        while self.running.load(Ordering::Relaxed) {
            let cycle_start = Instant::now();
            self.run_cycle();

            if last_reset.elapsed() >= self.config.reset_interval {
                info!("执行6小时自我重置...");
                self.self_reset()?;
                last_reset = Instant::now();
            }

            let elapsed = cycle_start.elapsed();
            let sleep_duration = Duration::from_secs(self.config.interval).saturating_sub(elapsed);
            if sleep_duration > Duration::ZERO {
                thread::sleep(sleep_duration);
            }
        }
        Ok(())
    }

    /// 自我重置：fork + exec
    fn self_reset(&self) -> Result<()> {
        use nix::unistd::{fork, ForkResult, execv};
        use std::ffi::CString;

        match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                let args: Vec<CString> = std::env::args()
                    .map(|a| CString::new(a).unwrap())
                    .collect();
                let arg_ptrs: Vec<&std::ffi::CStr> = args.iter().map(|s| s.as_c_str()).collect();
                let program = CString::new(std::env::current_exe()?.to_str().unwrap())?;
                let _ = execv(&program, &arg_ptrs);
                unreachable!()
            }
            Ok(ForkResult::Parent { .. }) => {
                std::process::exit(0);
            }
            Err(e) => bail!("fork失败: {}", e),
        }
    }
}

// ==================== 守护进程模式入口 ====================
fn run_daemon(interval: u64, threads: usize) -> Result<()> {
    use nix::unistd::{fork, setsid, ForkResult};

    // 第一次fork
    match unsafe { fork() } {
        Ok(ForkResult::Parent { .. }) => std::process::exit(0),
        Ok(ForkResult::Child) => {}
        Err(e) => bail!("第一次fork失败: {}", e),
    }

    setsid().map_err(|e| anyhow::anyhow!("setsid失败: {}", e))?;

    // 第二次fork
    match unsafe { fork() } {
        Ok(ForkResult::Parent { .. }) => std::process::exit(0),
        Ok(ForkResult::Child) => {}
        Err(e) => bail!("第二次fork失败: {}", e),
    }

    // 重定向标准IO
    let dev_null = fs::OpenOptions::new().read(true).write(true).open("/dev/null")?;
    let fd = dev_null.into_raw_fd();
    unsafe {
        libc::dup2(fd, libc::STDIN_FILENO);
        libc::dup2(fd, libc::STDOUT_FILENO);
        libc::dup2(fd, libc::STDERR_FILENO);
        if fd > 2 { libc::close(fd); }
    }

    // 设置最低优先级
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19); }
    let _ = Command::new("/system/bin/ionice")
        .args(["-c", "3", "-p", &std::process::id().to_string()])
        .stdout(Stdio::null()).stderr(Stdio::null()).status();
    let _ = Command::new("/system/bin/chrt")
        .args(["-i", "0", "-p", &std::process::id().to_string()])
        .stdout(Stdio::null()).stderr(Stdio::null()).status();
    let _ = fs::write("/proc/self/timerslack_ns", "1000000000\n");

    // 写入PID
    let pid_mgr = PidManager::new(PathBuf::from(PID_FILE));
    pid_mgr.write_pid()?;

    // 信号处理
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    let mut signals = Signals::new(&[SIGTERM, libc::SIGINT])?;
    thread::spawn(move || {
        for sig in signals.forever() {
            match sig {
                SIGTERM | libc::SIGINT => {
                    running_clone.store(false, Ordering::Relaxed);
                    break;
                }
                _ => {}
            }
        }
    });

    // 退出清理
    let pid_file = PathBuf::from(PID_FILE);
    let _ = ctrlc::set_handler(move || {
        let _ = fs::remove_file(&pid_file);
        std::process::exit(0);
    });

    let mut config = Config::default();
    config.interval = interval;
    config.worker_threads = threads;

    let cleaner = Cleaner::new(config);
    cleaner.daemon_loop()?;
    let _ = pid_mgr.remove_pid();
    Ok(())
}

// ==================== 终端交互式菜单 ====================
fn read_line() -> String {
    let mut input = String::new();
    let stdin = io::stdin();
    stdin.lock().read_line(&mut input).unwrap();
    input.trim().to_string()
}

fn check_root() -> bool {
    unsafe { libc::getuid() == 0 }
}

fn is_running() -> bool {
    PidManager::new(PathBuf::from(PID_FILE)).is_running()
}

fn get_pid() -> Option<u32> {
    PidManager::new(PathBuf::from(PID_FILE)).read_pid()
}

fn menu_start() {
    if is_running() {
        println!("⚠️ 运行中 (PID:{})", get_pid().unwrap_or(0));
        return;
    }

    print!("间隔(秒, 回车默认{}s): ", DEFAULT_INTERVAL);
    let _ = io::stdout().flush();
    let input = read_line();
    let interval: u64 = input.parse().unwrap_or(DEFAULT_INTERVAL);

    print!("工作线程数(回车默认自动检测): ");
    let _ = io::stdout().flush();
    let input = read_line();
    let threads: usize = input.parse().unwrap_or(0);

    println!("=== ANR清理+权限+时间戳 Rust多线程优化版 ===");
    println!("✨ 优化: 零分配循环, 并行清理, 自适应线程池");

    // 启动daemon（当前进程fork后退出，daemon在后台运行）
    match run_daemon(interval, threads) {
        Ok(_) => {}
        Err(e) => println!("❌ 启动失败: {}", e),
    }

    // 等待PID文件写入
    for _ in 0..50 {
        thread::sleep(Duration::from_millis(100));
        if is_running() {
            println!("✅ 启动成功 | PID: {} | 间隔: {}s", get_pid().unwrap_or(0), interval);
            println!("📌 安全清理: {:?}", SAFE_CLEAN_PATHS);
            println!("📌 全量删除: {:?}", FULL_DELETE_PATHS);
            println!("📌 权限目录: {:?}", PERM_FIX_PATHS);
            println!("📌 权限模式: {:o}", DEFAULT_PERM_MODE);
            println!("📌 时间戳文件: {:?}", TIMESTAMP_TARGETS);
            return;
        }
    }
    println!("❌ 启动失败");
}

fn menu_stop() {
    if !is_running() {
        println!("❌ 未运行");
        return;
    }

    let pid_mgr = PidManager::new(PathBuf::from(PID_FILE));
    match pid_mgr.stop() {
        Ok(true) => println!("✅ 已完全终止"),
        Ok(false) => println!("❌ 失败, 手动: kill -9 {}", get_pid().unwrap_or(0)),
        Err(e) => println!("❌ 错误: {}", e),
    }
}

fn menu_status() {
    if is_running() {
        println!("✅ 运行中 (PID:{})", get_pid().unwrap_or(0));
    } else {
        println!("❌ 未运行");
    }
}

fn menu_once() {
    println!("=== 单次执行模式 ===");
    let config = Config::default();
    let cleaner = Cleaner::new(config);
    cleaner.init_thread_pool();
    cleaner.run_cycle();
    println!("✅ 单次执行完成");
}

fn show_menu() {
    println!("\n=== ANR清理+权限+时间戳 Rust多线程优化版 ===");
    println!("⚠️ 修复: 零分配循环, 并行清理, 自适应线程池");
    println!("⚠️ 资源: 循环内零分配, 6小时exec重置RES");
    println!();
    println!("1. 启动后台进程");
    println!("2. 停止进程");
    println!("3. 查看运行状态");
    println!("4. 单次执行(调试)");
    println!("5. 退出脚本");
    print!("请输入选择(1-5): ");
    let _ = io::stdout().flush();
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .init();

    // 检查root权限
    if !check_root() {
        println!("❌ 必须先执行 su 获取root权限");
        return;
    }

    // 检查运行目录
    let exe_path = std::env::current_exe().unwrap_or_default();
    let exe_dir = exe_path.parent().unwrap_or(Path::new("/"));
    if exe_dir != Path::new("/data/adb") {
        println!("⚠️ 警告: 建议将程序移至 /data/adb/ 目录运行");
    }

    // 如果带参数 "daemon" 则直接进入守护模式（内部使用）
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "daemon" {
        let interval = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL);
        let threads = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
        let _ = run_daemon(interval, threads);
        return;
    }

    // 交互式菜单
    loop {
        show_menu();
        let choice = read_line();

        match choice.as_str() {
            "1" => menu_start(),
            "2" => menu_stop(),
            "3" => menu_status(),
            "4" => menu_once(),
            "5" => {
                println!("👋 已退出, 后台进程仍运行, 可通过菜单2终止");
                break;
            }
            _ => println!("❌ 请输入 1-5 的数字"),
        }
    }
}

use std::os::unix::io::IntoRawFd;
