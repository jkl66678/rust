use std::fs;
use std::io::{self, Write, BufRead};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::thread;
use std::os::unix::fs::PermissionsExt;

// ==================== 配置区 ====================
const DEFAULT_INTERVAL: u64 = 1;
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

const TIMESTAMP_TARGETS: &[&str] = &[
    "/dev/ptmx",
    "/tmp",
    "/data/adb",
];

// obb 权限循环修复目标
// 注意: 0o2000 仅为 setgid 位，无 rwx，会导致目录无法访问。
// Android 常用正确值为 0o771 (drwxrwx--x) 或 0o2771 (drwxrws--x)。
// 如需修正请把下行改为 0o2771 或 0o771。
const OBB_PATH: &str = "/data/media/0/Android/obb";
const OBB_PERM_MODE: u32 = 0o2000;

// ==================== 配置结构体 ====================
#[derive(Debug, Clone)]
struct Config {
    interval: u64,
    safe_clean_paths: Vec<PathBuf>,
    full_delete_paths: Vec<PathBuf>,
    timestamp_targets: Vec<PathBuf>,
    target_timestamp: SystemTime,
    pid_file: PathBuf,
    reset_interval: Duration,
    obb_path: PathBuf,
    obb_perm_mode: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval: DEFAULT_INTERVAL,
            safe_clean_paths: SAFE_CLEAN_PATHS.iter().map(|s| PathBuf::from(*s)).collect(),
            full_delete_paths: FULL_DELETE_PATHS.iter().map(|s| PathBuf::from(*s)).collect(),
            timestamp_targets: TIMESTAMP_TARGETS.iter().map(|s| PathBuf::from(*s)).collect(),
            target_timestamp: UNIX_EPOCH,
            pid_file: PathBuf::from(PID_FILE),
            reset_interval: Duration::from_secs(RESET_INTERVAL_SECS),
            obb_path: PathBuf::from(OBB_PATH),
            obb_perm_mode: OBB_PERM_MODE,
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
    last_obb_mode: Option<u32>,
}

impl Cleaner {
    fn new(config: Config) -> Self {
        Cleaner {
            last_safe_mtime: vec![None; config.safe_clean_paths.len()],
            last_full_mtime: vec![None; config.full_delete_paths.len()],
            last_obb_mode: None,
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

            let entries: Vec<_> = match fs::read_dir(path) {
                Ok(entries) => entries.filter_map(|e| e.ok()).collect(),
                Err(_) => continue,
            };

            for entry in entries {
                let _ = Self::remove_all(&entry.path());
            }

            self.last_safe_mtime[idx] = fs::metadata(path).and_then(|m| m.modified()).ok();
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

            match fs::symlink_metadata(path) {
                Ok(meta) if meta.is_dir() => {
                    if let Ok(entries) = fs::read_dir(path) {
                        let entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
                        for entry in entries {
                            let _ = Self::remove_all(&entry.path());
                        }
                    }
                    self.last_full_mtime[idx] =
                        fs::metadata(path).and_then(|m| m.modified()).ok();
                }
                Ok(_) => {
                    let _ = fs::remove_file(path);
                    self.last_full_mtime[idx] = None;
                }
                Err(_) => {
                    self.last_full_mtime[idx] = None;
                }
            }
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

    // 循环修复 /data/media/0/Android/obb 权限
    fn fix_obb_permissions(&mut self) {
        let path = &self.config.obb_path;
        if !path.is_dir() {
            self.last_obb_mode = None;
            return;
        }

        let current = fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o7777)
            .ok();

        if current == Some(self.config.obb_perm_mode) {
            return;
        }

        if let Ok(mut permissions) = fs::metadata(path).map(|m| m.permissions()) {
            permissions.set_mode(self.config.obb_perm_mode);
            if fs::set_permissions(path, permissions).is_ok() {
                self.last_obb_mode = Some(self.config.obb_perm_mode);
            }
        }
    }

    fn fix_timestamp(&mut self) {
        for path in self.config.timestamp_targets.iter() {
            if !path.exists() {
                continue;
            }

            let already_target = match fs::metadata(path).and_then(|m| m.modified()) {
                Ok(mtime) => match mtime.duration_since(UNIX_EPOCH) {
                    Ok(d) => d.as_secs() == 0,
                    Err(_) => false,
                },
                Err(_) => false,
            };

            if already_target {
                continue;
            }

            let path_str = match path.to_str() {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };

            let mut touched = false;

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
        }
    }

    fn run_cycle(&mut self) {
        let _start = Instant::now();
        self.safe_clean();
        self.full_delete();
        self.fix_obb_permissions();
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

    println!("=== ANR清理+obb权限+时间戳 Safe Rust低功耗版 ===");
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
            println!("📌 obb权限: {} -> {:o}", OBB_PATH, OBB_PERM_MODE);
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
    println!("\n=== ANR清理+obb权限+时间戳 Safe Rust低功耗版 ===");
    println!("⚠️ 特性: 零unsafe代码, 单线程无锁无竞争, 变更检测低功耗, 无日志输出");
    println!("⚠️ 优化: 目录mtime缓存跳过, obb权限mode缓存跳过");
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
