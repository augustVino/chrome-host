//! 开机自启 plist 自愈（macOS LaunchAgent）。
//!
//! tauri-plugin-autostart 只在用户显式开启时写 plist（is_enabled = 文件存在）；
//! 历史版本主二进制名为 chrome-host，后来 mainBinaryName 改为 chrome-host-app，
//! 旧 plist 的 ProgramArguments 仍指向已不存在的可执行文件 → 自启静默失效。
//! 每次启动（仅 release）校验指向，失配则重新 enable 重写（幂等）。

/// 从 auto-launch 生成的 plist 文本中提取 ProgramArguments 第一项（可执行文件路径）
#[cfg(target_os = "macos")]
#[cfg_attr(debug_assertions, allow(dead_code))] // 仅 release 由 heal_stale_plist 调用
fn plist_program(plist: &str) -> Option<&str> {
    let idx = plist.find("ProgramArguments")?;
    let rest = &plist[idx..];
    let start = rest.find("<string>")? + "<string>".len();
    let end = rest[start..].find("</string>")? + start;
    Some(rest[start..end].trim())
}

/// 启动时调用：已开启自启且 plist 指向 ≠ 当前二进制 → 重写。
/// plist 重写后下次登录生效（launchd 登录时读取），无需即时 kickstart。
#[cfg(target_os = "macos")]
#[cfg_attr(debug_assertions, allow(dead_code))] // 仅 release 由 main.rs 调用
pub fn heal_stale_plist(app: &tauri::AppHandle) {
    use tauri_plugin_autostart::ManagerExt as _;

    if !app.autolaunch().is_enabled().unwrap_or(false) {
        return; // 未开启或读取失败（视为未开启），不动
    }
    let Some(exe) = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok()) else {
        return;
    };
    // plist 路径须与插件实现一致：~/Library/LaunchAgents/{package_info.name}.plist
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return;
    };
    let plist = home
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", app.package_info().name));
    let Ok(text) = std::fs::read_to_string(&plist) else {
        return;
    };
    let Some(program) = plist_program(&text) else {
        return;
    };
    if program != exe.to_string_lossy().as_ref() {
        match app.autolaunch().enable() {
            Ok(()) => tracing::info!("开机自启 plist 已自愈：{program} → {}", exe.display()),
            Err(e) => tracing::warn!("开机自启 plist 自愈失败（可在设置页关闭后重新开启）: {e}"),
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn heal_stale_plist(_app: &tauri::AppHandle) {}

#[cfg(all(target_os = "macos", test))]
mod tests {
    use super::plist_program;

    #[test]
    fn parses_first_program_argument() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key>
    <string>chrome-host</string>
    <key>ProgramArguments</key>
    <array><string>/Applications/chrome-host.app/Contents/MacOS/chrome-host</string><string>--hidden</string></array>
    <key>RunAtLoad</key>
    <true/>
  </dict>
</plist>"#;
        assert_eq!(
            plist_program(plist),
            Some("/Applications/chrome-host.app/Contents/MacOS/chrome-host")
        );
    }

    #[test]
    fn returns_none_without_program_arguments() {
        assert_eq!(plist_program("<plist><dict/></plist>"), None);
    }
}
