use std::fs;
use std::io::{self, Write, BufRead};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::os::unix::fs::PermissionsExt;
use std::thread;

// ==================== 配置区 ====================
const DEFAULT_INTERVAL: u64 = 1;
const DEFAULT_PERM_MODE: u32 = 0o2761;
const RESET_INTERVAL_SECS: u64 = 21600;
const PID_FILE: &str = "/data/adb/anr_cleaner.pid";

const SAFE_CLEAN_PATHS: &[&str] = &[
    "/data/anr",
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
    "/dev/zero",
    "/dev/tty"
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
            fs::metadata(format!("/proc/{}", pid)).is_ok()
        } else {
            false
        }
    }

    fn write_pid(&self) -> std::io::Result<()> {
        let pid = std::process::id();
        fs::write(&self.pid_file, pid.to_string())
    }

    fn remove_pid(&self) -> std::io::Result<()> {
        if self.pid_file.exists() {
            fs::remove_file(&self.pid_file)?;
        }
        Ok(())
    }

    fn stop(&self) -> Result<bool, std::io::Error> {
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

// ==================== 核心清理引擎 ====================
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

    fn remove_all(path: &Path) -> std::io::Result<()> {
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
        for (idx, path) in self.config.timestamp_targets.iter().enumerate() {
            // 严格检查存在性，不存在则清空缓存
            if !path.exists() {
                self.last_time_mtime[idx] = None;
                continue;
            }

            // 读取当前 mtime
            let current = match fs::metadata(path) {
                Ok(m) => m.modified().ok(),
                Err(_) => None,
            };

            // 已为目标时间则跳过
            if current == Some(self.config.target_timestamp) {
                continue;
            }

            // 路径转字符串，无法转换则跳过
            let path_str = match path.to_str() {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };

            // 逐个路径独立调用 touch，完全隔离
            let mut touched = false;

            // 方法1: 系统 touch -d @0 (GNU 格式)
            if !touched {
                touched = Command::new("/system/bin/touch")
                    .arg("-d")
                    .arg("@0")
                    .arg(path_str)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map_or(false, |s| s.success());
            }

            // 方法2: 系统 touch -t (POSIX 格式)
            if !touched {
                touched = Command::new("/system/bin/touch")
                    .arg("-t")
                    .arg("197001010000.00")
                    .arg(path_str)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map_or(false, |s| s.success());
            }

            // 方法3: busybox touch
            if !touched {
                touched = Command::new("busybox")
                    .arg("touch")
                    .arg("-t")
                    .arg("197001010000.00")
                    .arg(path_str)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map_or(false, |s| s.success());
            }

            // 方法4: toybox touch (Android 新版)
            if !touched {
                touched = Command::new("/system/bin/toybox")
                    .arg("touch")
                    .arg("-t")
                    .arg("197001010000.00")
                    .arg(path_str)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map_or(false, |s| s.success());
            }

            // 成功后更新缓存
            if touched {
                self.last_time_mtime[idx] = Some(self.config.target_timestamp);
            }
        }
    }

    fn run_cycle(&mut self) {
        let _start = Instant::now();
        self.safe_clean();
        self.full_delete();
        self.fix_permissions();
        self.fix_timestamp();
    }

    fn daemon_loop(&mut self) -> std::io::Result<()> {
        let my_pid = std::process::id();
        let pid_mgr = PidManager::new(self.config.pid_file.clone());
        pid_mgr.write_pid()?;

        let mut last_reset = Instant::now();

        loop {
            match pid_mgr.read_pid() {
                Some(pid) if pid == my_pid => {}
                _ => break,
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

    fn self_reset(&self) -> std::io::Result<()> {
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
fn run_daemon(interval: u64) -> std::io::Result<()> {
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
    let exe_str = exe.to_str().unwrap_or("");

    let result = {
        let mut cmd = Command::new("/system/bin/setsid");
        cmd.arg(exe_str)
            .arg("daemon")
            .arg(interval.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.spawn()
    };

    match result {
        Ok(_) => {}
        Err(_) => {
            let mut cmd = Command::new(exe_str);
            cmd.arg("daemon").arg(interval.to_string());
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
            let _ = cmd.spawn();
        }
    }

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
