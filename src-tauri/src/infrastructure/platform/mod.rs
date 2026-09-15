//! 平台差异：窗口唤出（focus 的 CDP 失败失败回退路径）
//!          + 进程重关联扫描（reconcile 判死前的 pid 漂移修正）
//!          + 开机自启 plist 自愈（autostart）

mod autostart;

#[cfg_attr(debug_assertions, allow(unused_imports))] // 仅 release 由 main.rs 使用
pub use autostart::heal_stale_plist;

use std::path::Path;

use crate::error::AppError;

/// 把 pid 对应进程的主窗口带到前台（focus 的 CDP 失败回退路径）。
/// macOS 用 AXRaise；Windows/Linux 为 best-effort，失败仅报错不 panic。
pub fn bring_to_front(pid: u32) -> Result<(), AppError> {
    #[cfg(target_os = "macos")]
    {
        // AXRaise 可把最小化窗口弹回，且不需要辅助功能写权限；逐窗 try，失败静默
        let script = format!(
            r#"tell application "System Events"
  set p to first process whose unix id is {pid}
  repeat with w in windows of p
    try
      perform action "AXRaise" of w
    end try
  end repeat
  set frontmost of p to true
end tell"#
        );
        let output = std::process::Command::new("osascript")
            .args(["-e", &script])
            .output()?;
        if !output.status.success() {
            return Err(AppError::internal(format!(
                "唤出窗口失败: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        // WScript.Shell AppActivate 按 pid 激活：系统自带组件，无需额外依赖
        let script = format!("(New-Object -ComObject WScript.Shell).AppActivate({pid})");
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()?;
        if !output.status.success() {
            return Err(AppError::internal("唤出窗口失败（AppActivate）"));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        // 优先 wmctrl，回退 xdotool（两者均为 X11 工具，Wayland 下可能不可用）
        for cmd in [
            vec!["wmctrl".to_string(), "-i".to_string(), "-a".to_string(), pid.to_string()],
            vec!["xdotool".to_string(), "windowactivate".to_string(), pid.to_string()],
        ] {
            if let Ok(output) = std::process::Command::new(&cmd[0]).args(&cmd[1..]).output() {
                if output.status.success() {
                    return Ok(());
                }
            }
        }
        Err(AppError::internal(
            "唤出窗口失败（需要 wmctrl 或 xdotool，Wayland 下暂不支持）",
        ))
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = pid;
        Err(AppError::internal("当前平台暂不支持窗口唤出"))
    }
}

/// 全进程扫描：找命令行包含指定 user-data-dir 的 Chrome 主进程 pid。
/// 用于 pid 漂移重关联（如 Chrome relaunch 换 pid 后，以 profile_dir 重新找到它）。
#[cfg(unix)]
pub fn scan_pid_by_profile(profile_dir: &Path) -> Option<u32> {
    let needle = profile_dir.to_string_lossy().to_string();
    let output = std::process::Command::new("ps")
        .args(["-ax", "-o", "pid=,command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let trimmed = line.trim();
        // 主进程特征：含 profile_dir 与 remote-debugging-port，排除 Helper/renderer
        if trimmed.contains(&needle)
            && trimmed.contains("remote-debugging-port")
            && !trimmed.contains("Helper")
            && !trimmed.contains("--type=")
        {
            let pid_str = trimmed.split_whitespace().next()?;
            return pid_str.parse::<u32>().ok();
        }
    }
    None
}

/// Windows 同语义实现：PowerShell CIM 查询命令行，
/// 过滤特征与 unix 版一致（含 profile_dir + remote-debugging-port，排除 --type= 的子进程）。
#[cfg(target_os = "windows")]
pub fn scan_pid_by_profile(profile_dir: &Path) -> Option<u32> {
    let needle = profile_dir.to_string_lossy().replace('\\', "\\\\");
    let script = format!(
        "Get-CimInstance Win32_Process -Filter \"Name like '%Chrome%'\" | \
         Where-Object {{ $_.CommandLine -like '*{needle}*' -and $_.CommandLine -like '*remote-debugging-port*' \
         -and $_.CommandLine -notlike '*--type=*' }} | Select-Object -First 1 -ExpandProperty ProcessId"
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse::<u32>().ok()
}
