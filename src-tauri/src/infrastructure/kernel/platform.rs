//! 内核安装的平台后处理

use std::path::Path;

/// macOS：防御性清除 quarantine 标记（幂等），防 Gatekeeper "已损坏" 报错。
#[cfg(target_os = "macos")]
pub fn clear_quarantine(dir: &Path) {
    let _ = std::process::Command::new("xattr").args(["-cr"]).arg(dir).output();
}

#[cfg(not(target_os = "macos"))]
pub fn clear_quarantine(_dir: &Path) {}

/// Linux：对主二进制显式补可执行权限（zip 条目可能缺失权限信息，兜底）。
#[cfg(target_os = "linux")]
pub fn ensure_executable(binary: &Path) {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(binary) {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o755);
        let _ = fs::set_permissions(binary, perms);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn ensure_executable(_binary: &Path) {}
