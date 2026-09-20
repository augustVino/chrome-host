use std::path::{Path, PathBuf};

use crate::error::AppError;

/// 启动规格：调用方（InstanceService）组装，ProcessManager 只负责进程语义。
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub executable: PathBuf,
    pub user_data_dir: PathBuf,
    pub cdp_port: u16,
    /// 附加参数（hosts 注入 / startupArgs / 启动页 URL），按序追加在标准参数之后
    pub extra_args: Vec<String>,
}

/// 组装 Chrome 启动参数。独立纯函数便于单测。
pub fn build_args(spec: &LaunchSpec) -> Vec<String> {
    let mut args = vec![
        format!("--user-data-dir={}", spec.user_data_dir.display()),
        format!("--remote-debugging-port={}", spec.cdp_port),
        // Chrome 111+ 默认拒绝跨源 CDP 连接，自动化必须放开
        "--remote-allow-origins=*".to_string(),
        // 抑制“不受支持的命令行标记”警告条（host-resolver-rules 等自定义 flag 会触发）
        "--test-type".to_string(),
        // 抑制 CfT 自带的“仅适用于自动测试”提示条（--test-type 管不住它，M131 实测有效）
        "--disable-infobars".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
    ];
    args.extend(spec.extra_args.iter().cloned());
    args
}

/// 进程退出回调：在 reaper 线程中执行（进程被 reap 后触发）。
/// 用于事件驱动的退出检测，替代轮询猜测。
pub type OnExit = Box<dyn FnOnce(Option<i32>) + Send + Sync>;

pub trait ProcessManager: Send + Sync {
    /// detached 启动，返回 pid。不持有 Child 句柄——Chrome 独立于应用存活，
    /// 后续管理一律经 pid（探活/终止），应用重启后可重新关联。
    fn launch(&self, spec: &LaunchSpec) -> Result<u32, AppError>;

    /// 注册退出监听：起 reaper 线程 wait() 该 pid 的子进程，
    /// 退出时回收（防僵尸）并触发回调。须在 launch 后尽快调用。
    fn attach_exit_watcher(&self, pid: u32, on_exit: OnExit) -> Result<(), AppError>;

    /// SIGTERM → 3s 未退出 → SIGKILL
    fn terminate(&self, pid: u32) -> Result<(), AppError>;

    /// 探活：进程存在 且 命令行包含本实例 profile_dir（防 pid 复用误判）
    fn is_alive(&self, pid: u32, profile_dir: &Path) -> Result<bool, AppError>;
}

/// 已 spawn 但尚未挂 watcher 的 Child 暂存。
/// attach_exit_watcher 从这里取走 Child 交给 reaper 线程；无人取的极端情况由 Drop 兜底。
static PENDING_CHILDREN: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<u32, std::process::Child>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

pub struct DefaultProcessManager;

impl DefaultProcessManager {
    pub fn new() -> Self {
        DefaultProcessManager
    }
}

impl Default for DefaultProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessManager for DefaultProcessManager {
    fn launch(&self, spec: &LaunchSpec) -> Result<u32, AppError> {
        let args = build_args(spec);
        let pid = launch_platform(&spec.executable, &args).map_err(|e| {
            AppError::business(500, "INSTANCE_START_FAILED", format!("Chrome 启动失败: {e}"))
        })?;
        // 把 Child 从暂存区留给 watcher；此处只返回 pid
        Ok(pid)
    }

    fn attach_exit_watcher(&self, pid: u32, on_exit: OnExit) -> Result<(), AppError> {
        let child = PENDING_CHILDREN
            .lock()
            .map_err(|e| AppError::internal(e.to_string()))?
            .remove(&pid);
        match child {
            Some(mut child) => {
                std::thread::Builder::new()
                    .name(format!("reaper-{pid}"))
                    .spawn(move || {
                        let status = child.wait().ok().and_then(|s| s.code());
                        tracing::debug!("[process] pid={pid} 退出（已回收）status={status:?}");
                        on_exit(status);
                    })
                    .map_err(|e| AppError::internal(e.to_string()))?;
                Ok(())
            }
            None => Err(AppError::internal(format!("pid {pid} 无可挂起的子进程"))),
        }
    }

    fn terminate(&self, pid: u32) -> Result<(), AppError> {
        terminate_platform(pid)
    }

    fn is_alive(&self, pid: u32, profile_dir: &Path) -> Result<bool, AppError> {
        is_alive_platform(pid, profile_dir)
    }
}

/// 平台 spawn：成功后 Child 存入 PENDING_CHILDREN（attach_exit_watcher 取走 reap）。
fn register_child(child: std::process::Child) -> u32 {
    let pid = child.id();
    if let Ok(mut map) = PENDING_CHILDREN.lock() {
        map.insert(pid, child);
    }
    pid
}

// ---------- 平台实现 ----------

#[cfg(target_os = "linux")]
fn spawn_platform(executable: &Path, args: &[String], extra: &[&str]) -> std::io::Result<u32> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let mut child = std::process::Command::new(executable)
        .args(args.iter().map(|s| s.as_str()).chain(extra.iter().copied()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    // stderr 排水线程：接管管道持续读取丢弃，防管道写满阻塞 Chrome
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut s = stderr;
            let _ = std::io::copy(&mut s, &mut std::io::sink());
        });
    }
    Ok(register_child(child))
}

#[cfg(target_os = "linux")]
fn launch_platform(executable: &Path, args: &[String]) -> std::io::Result<u32> {
    // Linux 云桌面内核常禁 unprivileged userns，沙箱失败表现为启动后立即退出：
    // 800ms 探测窗口检测，自动追加 --no-sandbox 重试一次（兜底而非默认）
    use std::time::Duration;

    let first = spawn_platform(executable, args, &[])?;
    std::thread::sleep(Duration::from_millis(800));
    if process_alive_by_kill0(first) {
        return Ok(first);
    }
    tracing::warn!("[process] Linux 启动后立即退出，尝试 --no-sandbox 重试");
    reap_pending(first);
    let retry = spawn_platform(executable, args, &["--no-sandbox"])?;
    std::thread::sleep(Duration::from_millis(800));
    if process_alive_by_kill0(retry) {
        Ok(retry)
    } else {
        reap_pending(retry);
        Err(std::io::Error::other("已尝试 --no-sandbox 仍立即退出"))
    }
}

#[cfg(not(target_os = "linux"))]
fn launch_platform(executable: &Path, args: &[String]) -> std::io::Result<u32> {
    use std::process::{Command, Stdio};

    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    #[cfg(target_os = "windows")]
    use std::os::windows::process::CommandExt;

    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    command.process_group(0); // 脱离终端进程组
    #[cfg(target_os = "windows")]
    {
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    let child = command.spawn()?;
    Ok(register_child(child))
}

/// 清理沙箱重试产生的已死子进程（防僵尸）
#[cfg(target_os = "linux")]
fn reap_pending(pid: u32) {
    if let Ok(mut map) = PENDING_CHILDREN.lock() {
        if let Some(mut child) = map.remove(&pid) {
            let _ = child.wait();
        }
    }
}

/// 轻量探活（kill -0 / 平台等价）：进程是否存活。
/// 注意：父进程未 reap 的僵尸会返回 true——必须配合 reaper 使用。
#[cfg(unix)]
fn process_alive_by_kill0(pid: u32) -> bool {
    // libc::kill(pid, 0) 系统调用，零 spawn——外部 kill 进程在高负载/进程数触顶时
    // spawn 失败会造成误报死亡（真实故障源）
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(unix)]
fn wait_exit_polling(pid: u32, timeout_ms: u64) -> bool {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if !process_alive_by_kill0(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !process_alive_by_kill0(pid)
}

#[cfg(unix)]
fn terminate_platform(pid: u32) -> Result<(), AppError> {
    let sig = |flag: &str| {
        let _ = std::process::Command::new("kill")
            .args([flag, &pid.to_string()])
            .status();
    };

    sig("-TERM");
    if wait_exit_polling(pid, 3_000) {
        return Ok(());
    }
    tracing::warn!("[process] SIGTERM 3s 未退出，SIGKILL: pid={pid}");
    sig("-KILL");
    wait_exit_polling(pid, 1_000);
    Ok(())
}

#[cfg(target_os = "windows")]
fn terminate_platform(pid: u32) -> Result<(), AppError> {
    let status = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()?;
    if !status.success() {
        tracing::warn!("[process] taskkill 非零退出（进程可能已退出）: pid={pid}");
    }
    Ok(())
}

#[cfg(not(any(unix, target_os = "windows")))]
fn terminate_platform(_pid: u32) -> Result<(), AppError> {
    Err(AppError::internal("当前平台不支持进程终止"))
}

#[cfg(unix)]
fn is_alive_platform(pid: u32, profile_dir: &Path) -> Result<bool, AppError> {
    // 先 kill -0 快速判死（配合 reaper，僵尸已被回收，此检查准确）
    if !process_alive_by_kill0(pid) {
        return Ok(false);
    }
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()?;
    if !output.status.success() {
        return Ok(false);
    }
    let command = String::from_utf8_lossy(&output.stdout);
    Ok(command.contains(profile_dir.to_string_lossy().as_ref()))
}

#[cfg(target_os = "windows")]
fn is_alive_platform(pid: u32, profile_dir: &Path) -> Result<bool, AppError> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "(Get-CimInstance Win32_Process -Filter \"ProcessId={pid}\").CommandLine"
            ),
        ])
        .output()?;
    if !output.status.success() {
        return Ok(false);
    }
    let command = String::from_utf8_lossy(&output.stdout);
    Ok(command.contains(profile_dir.to_string_lossy().as_ref()))
}

#[cfg(not(any(unix, target_os = "windows")))]
fn is_alive_platform(_pid: u32, _profile_dir: &Path) -> Result<bool, AppError> {
    Err(AppError::internal("当前平台不支持进程探活"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(extra: Vec<String>) -> LaunchSpec {
        LaunchSpec {
            executable: PathBuf::from("/Applications/Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"),
            user_data_dir: PathBuf::from("/tmp/env/env_a/instances/ins_1"),
            cdp_port: 9223,
            extra_args: extra,
        }
    }

    #[test]
    fn args_include_standard_set_and_keep_extra_order() {
        let args = build_args(&spec(vec![
            "--host-resolver-rules=MAP a.example.com 10.0.0.1".into(),
            "--no-proxy-server".into(),
            "https://staging.example.com".into(),
        ]));

        assert!(args.contains(&"--user-data-dir=/tmp/env/env_a/instances/ins_1".to_string()));
        assert!(args.contains(&"--remote-debugging-port=9223".to_string()));
        assert!(args.contains(&"--remote-allow-origins=*".to_string()));
        assert!(args.contains(&"--disable-infobars".to_string()));
        assert!(args.contains(&"--no-first-run".to_string()));
        assert!(args.contains(&"--no-default-browser-check".to_string()));
        // extra 保持顺序且位于标准参数之后
        let extra_start = args
            .iter()
            .position(|a| a.starts_with("--host-resolver-rules"))
            .unwrap();
        assert_eq!(args[extra_start + 1], "--no-proxy-server");
        assert_eq!(args[extra_start + 2], "https://staging.example.com");
        assert_eq!(args.last().unwrap(), "https://staging.example.com", "启动页必须在最后");
    }

    #[test]
    fn args_without_extra_have_no_proxy_flags() {
        let args = build_args(&spec(vec![]));
        assert!(!args.iter().any(|a| a.contains("proxy")));
        assert!(!args.iter().any(|a| a.contains("host-resolver")));
    }

    /// 真实进程验证：spawn → watcher reap → terminate 秒级退出、无僵尸残留
    #[cfg(unix)]
    #[test]
    fn terminate_kills_real_process_and_reaps() {
        use std::os::unix::process::CommandExt;
    use std::sync::mpsc;
        use std::time::Duration;

        let manager = DefaultProcessManager::new();
        let mut command = std::process::Command::new("sleep");
        command.arg("30").process_group(0);
        let child = command.spawn().unwrap();
        let pid = register_child(child);

        let (tx, rx) = mpsc::channel();
        manager
            .attach_exit_watcher(
                pid,
                Box::new(move |_| {
                    let _ = tx.send(());
                }),
            )
            .unwrap();

        let t0 = std::time::Instant::now();
        manager.terminate(pid).unwrap();
        let elapsed = t0.elapsed();
        assert!(elapsed < Duration::from_secs(4), "terminate 应秒级完成，实际 {elapsed:?}");
        assert!(!process_alive_by_kill0(pid), "进程应已死且被 reap（kill -0 失败）");
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok(), "退出回调应触发");
    }

    /// watcher 自然退出路径：短命进程退出后回调触发且无僵尸
    #[cfg(unix)]
    #[test]
    fn watcher_fires_on_natural_exit() {
        use std::os::unix::process::CommandExt;
    use std::sync::mpsc;
        use std::time::Duration;

        let manager = DefaultProcessManager::new();
        let mut command = std::process::Command::new("sleep");
        command.arg("0").process_group(0);
        let child = command.spawn().unwrap();
        let pid = register_child(child);

        let (tx, rx) = mpsc::channel();
        manager
            .attach_exit_watcher(
                pid,
                Box::new(move |_| {
                    let _ = tx.send(());
                }),
            )
            .unwrap();

        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok(), "自然退出应触发回调");
        std::thread::sleep(Duration::from_millis(200));
        assert!(!process_alive_by_kill0(pid), "退出并 reap 后 kill -0 应失败（无僵尸）");
    }
}
