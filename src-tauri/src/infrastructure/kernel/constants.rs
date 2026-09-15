//! CfT 内核常量。仅支持 Chrome for Testing，不支持自定义路径/系统 Chrome。

/// pinned 版本：不自动更新，升级 = 修改此常量发新版
pub const CFT_VERSION: &str = "131.0.6778.204";

/// 下载双源：npmmirror 优先、官方兜底；路径结构一致仅 host 不同。
pub const CFT_MIRRORS: [&str; 2] = [
    "https://cdn.npmmirror.com/binaries/chrome-for-testing",
    "https://storage.googleapis.com/chrome-for-testing-public",
];

/// 下载进度事件名（Tauri 事件名不允许点号）
pub const KERNEL_DOWNLOAD_EVENT: &str = "kernel-download";

/// 当前平台标识（CfT 官方仅发布 linux64，无 linux-arm64）
pub fn cft_platform() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        if cfg!(target_arch = "aarch64") {
            "mac-arm64"
        } else {
            "mac-x64"
        }
    }
    #[cfg(target_os = "windows")]
    {
        "win64"
    }
    #[cfg(target_os = "linux")]
    {
        "linux64"
    }
}

/// zip 包名，如 chrome-mac-arm64.zip
pub fn cft_zip_name() -> String {
    format!("chrome-{}.zip", cft_platform())
}

/// 解压后二进制相对版本目录的路径
pub fn cft_binary_rel_path() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        if cfg!(target_arch = "aarch64") {
            "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"
        } else {
            "chrome-mac-x64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"
        }
    }
    #[cfg(target_os = "windows")]
    {
        "chrome-win64/chrome.exe"
    }
    #[cfg(target_os = "linux")]
    {
        "chrome-linux64/chrome"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_identifiers_consistent() {
        let platform = cft_platform();
        assert!(platform.starts_with("mac") || platform == "win64" || platform == "linux64");
        assert_eq!(cft_zip_name(), format!("chrome-{platform}.zip"));
        assert!(cft_binary_rel_path().contains(platform));
    }

    #[test]
    fn version_is_four_segments() {
        let parts: Vec<&str> = CFT_VERSION.split('.').collect();
        assert_eq!(parts.len(), 4);
        assert!(parts.iter().all(|p| p.parse::<u32>().is_ok()));
    }

    #[test]
    fn mirrors_share_path_structure() {
        let urls: Vec<String> = CFT_MIRRORS
            .iter()
            .map(|m| format!("{m}/{}/{}", CFT_VERSION, cft_platform()))
            .collect();
        let tail = |u: &String| u.splitn(2, "/131").nth(1).unwrap().to_string();
        assert_eq!(tail(&urls[0]), tail(&urls[1]));
    }
}
