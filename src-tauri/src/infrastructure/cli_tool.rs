//! chrome-host CLI 的打包发现与本机安装（设置页「命令行工具」卡片）。
//!
//! CLI 二进制经 Tauri externalBin 随 .app 打包（与主程序同目录，Contents/MacOS/chrome-host），
//! 「安装」= 在 PATH 目录创建指向它的符号链接（VS Code `code` 命令同款模式）。
//! CLI 随应用发布天然同版本，无需独立分发渠道；应用升级换包后链接指向不变，
//! 失效场景由 `heal_stale_link` / 设置页「修复」兜底。

use std::path::{Path, PathBuf};

use serde::Serialize;

/// PATH 上的命令名（用户在终端敲的名字）
fn link_file_name() -> &'static str {
    if cfg!(windows) {
        "chrome-host.exe"
    } else {
        "chrome-host"
    }
}

/// externalBin 产物在 .app 内的文件名（与 Cargo 包名区分开，Tauri 禁止同名）
fn sidecar_file_name() -> &'static str {
    if cfg!(windows) {
        "chrome-host-cli.exe"
    } else {
        "chrome-host-cli"
    }
}

/// CLI 随 .app 打包后的实际路径（与主程序同目录）。未打包（脚本漏跑/异常分发）返回 None。
pub fn sidecar_path() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let path = dir.join(sidecar_file_name());
    path.exists().then_some(path)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// 安装/探测候选目录。/usr/local/bin 是安装目标（macOS 默认 PATH 成员），
/// 其余用于状态探测：用户可能装在 homebrew 或用户级目录。
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = ["/usr/local/bin", "/opt/homebrew/bin"]
        .iter()
        .map(PathBuf::from)
        .collect();
    if let Some(home) = home_dir() {
        dirs.push(home.join(".local/bin"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for p in std::env::split_paths(&path) {
            if !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }
    dirs
}

/// 默认安装目标：/usr/local/bin/chrome-host（仅 unix 调用方会真正使用；
/// Windows 上 install/uninstall 在触碰该路径前已提前返回，保留定义以保证跨平台编译）
fn default_link_path() -> PathBuf {
    PathBuf::from("/usr/local/bin").join(link_file_name())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliToolStatus {
    /// CLI 是否已随当前应用打包
    pub bundled: bool,
    /// 打包内 CLI 路径
    pub sidecar_path: Option<String>,
    /// 当前平台是否支持应用内一键安装（Windows 需改 PATH 注册表，暂未实现）
    pub install_supported: bool,
    /// PATH 上已存在的 chrome-host（符号链接或独立安装的二进制，如 cargo install）
    pub installed_path: Option<String>,
    /// installed_path 为符号链接时的指向原文（未解析）
    pub symlink_target: Option<String>,
    /// 已安装且符号链接指向当前应用的 sidecar（cargo install 等独立安装为 false）
    pub up_to_date: bool,
}

/// 符号链接是否指向 src（原值或解析后相等，兼容历史链接的相对/规范化差异）
fn symlink_targets(link: &Path, src: &Path) -> bool {
    let Ok(t) = std::fs::read_link(link) else {
        return false;
    };
    if t == src {
        return true;
    }
    match (t.canonicalize(), src.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub fn status() -> CliToolStatus {
    let sidecar = sidecar_path();
    let mut installed_path: Option<PathBuf> = None;
    let mut symlink_target: Option<String> = None;
    for dir in candidate_dirs() {
        let p = dir.join(link_file_name());
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            symlink_target = std::fs::read_link(&p)
                .ok()
                .map(|t| t.to_string_lossy().into_owned());
            installed_path = Some(p);
            break;
        }
        if meta.is_file() {
            installed_path = Some(p); // 独立二进制（非本应用符号链接），如实上报
            break;
        }
    }
    let up_to_date = installed_path
        .as_deref()
        .zip(sidecar.as_deref())
        .map(|(link, src)| symlink_targets(link, src))
        .unwrap_or(false);
    CliToolStatus {
        bundled: sidecar.is_some(),
        sidecar_path: sidecar.map(|p| p.to_string_lossy().into_owned()),
        install_supported: !cfg!(windows),
        installed_path: installed_path.map(|p| p.to_string_lossy().into_owned()),
        symlink_target,
        up_to_date,
    }
}

fn is_permission(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied
}

/// shell 单引号转义（osascript do shell script 的内层命令用）
fn shq(s: &Path) -> String {
    format!("'{}'", s.to_string_lossy().replace('\'', r"'\''"))
}

/// macOS 下经 osascript 申请管理员权限重建符号链接（VS Code 同款流程）。
/// 用户在系统弹窗取消 → 返回可识别的取消文案，不视为故障。
fn elevate_link(src: &Path, dst: &Path) -> Result<(), String> {
    let dir = dst.parent().unwrap_or(Path::new("/usr/local/bin"));
    let inner = format!(
        "mkdir -p {} && ln -sf {} {}",
        shq(dir),
        shq(src),
        shq(dst)
    );
    let script = format!(
        "do shell script \"{}\" with administrator privileges",
        inner.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .map_err(|e| format!("无法启动 osascript：{e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("-128") || stderr.to_lowercase().contains("user canceled") {
        return Err("已取消管理员授权，未做任何更改".into());
    }
    Err(format!("管理员授权安装失败：{}", stderr.trim()))
}

/// 原子替换符号链接：先建临时链接再 rename，失败不留「旧链接已删、新链接未建」的中间态。
/// 权限不足且 allow_elevate 时走提权；heal 等后台路径传 false，静默失败不打扰用户。
fn replace_symlink(src: &Path, dst: &Path, allow_elevate: bool) -> Result<(), String> {
    let tmp = dst.with_file_name(format!(
        ".{}.tmp-{}",
        link_file_name(),
        std::process::id()
    ));
    let result = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(src, &tmp)?;
            std::fs::rename(&tmp, dst)?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            if is_permission(&e) {
                if allow_elevate && cfg!(target_os = "macos") {
                    return elevate_link(src, dst);
                }
                return Err(format!(
                    "无权限写入 {}，请以管理员权限重试或手动执行：ln -sf '{}' '{}'",
                    dst.display(),
                    src.display(),
                    dst.display()
                ));
            }
            Err(format!("替换符号链接 {} 失败：{e}", dst.display()))
        }
    }
}

/// 在 dst 创建指向 src 的符号链接（dst 已存在且指向 src 时幂等成功）。
fn install_link(src: &Path, dst: &Path, allow_elevate: bool) -> Result<(), String> {
    match std::fs::symlink_metadata(dst) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if symlink_targets(dst, src) {
                return Ok(());
            }
            replace_symlink(src, dst, allow_elevate)
        }
        // 普通文件/目录：可能是用户自装的同名工具，拒绝覆盖
        Ok(_) => Err(format!(
            "{} 已存在且不是符号链接，请手动处理后重试",
            dst.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = dst.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    if is_permission(&e) {
                        if allow_elevate && cfg!(target_os = "macos") {
                            return elevate_link(src, dst);
                        }
                        return Err(format!(
                            "无权限创建 {}，请手动执行：sudo mkdir -p {} && sudo ln -sf '{}' '{}'",
                            parent.display(),
                            parent.display(),
                            src.display(),
                            dst.display()
                        ));
                    }
                    return Err(format!("创建目录 {} 失败：{e}", parent.display()));
                }
            }
            #[cfg(unix)]
            {
                match std::os::unix::fs::symlink(src, dst) {
                    Ok(()) => Ok(()),
                    Err(e) if is_permission(&e) => {
                        if allow_elevate && cfg!(target_os = "macos") {
                            elevate_link(src, dst)
                        } else {
                            Err(format!(
                                "无权限写入 {}，请手动执行：sudo ln -sf '{}' '{}'",
                                dst.display(),
                                src.display(),
                                dst.display()
                            ))
                        }
                    }
                    Err(e) => Err(format!("创建符号链接 {} 失败：{e}", dst.display())),
                }
            }
            #[cfg(not(unix))]
            {
                Ok(())
            }
        }
        Err(e) => Err(format!("探测 {} 失败：{e}", dst.display())),
    }
}

pub fn install() -> Result<CliToolStatus, String> {
    if cfg!(windows) {
        return Err("Windows 暂不支持应用内自动安装，请手动将 CLI 所在目录加入 PATH".into());
    }
    let src = sidecar_path().ok_or("CLI 未随当前应用打包，无法安装")?;
    install_link(&src, &default_link_path(), true)?;
    Ok(status())
}

/// 仅删除「指向 chrome-host 应用内 / 构建产物路径」的符号链接；
/// 外部目标（用户自己的同名工具）拒绝触碰。
fn is_owned_link_target(target: &Path) -> bool {
    let s = target.to_string_lossy();
    // .app 内链接名为 chrome-host-cli；target/ 下为 cargo 产物 chrome-host
    let name = target
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    (s.contains(".app/Contents/MacOS/") || s.contains("/target/"))
        && ["chrome-host", "chrome-host-cli"].contains(&name.as_str())
}

pub fn uninstall() -> Result<CliToolStatus, String> {
    if cfg!(windows) {
        return Err("Windows 暂不支持应用内自动卸载".into());
    }
    let dst = default_link_path();
    match std::fs::symlink_metadata(&dst) {
        // 未安装 → 幂等成功
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Ok(meta) if meta.file_type().is_symlink() => {
            let target = std::fs::read_link(&dst).unwrap_or_default();
            if !is_owned_link_target(&target) {
                return Err(format!(
                    "{} 指向外部目标（{}），已拒绝删除",
                    dst.display(),
                    target.display()
                ));
            }
            if let Err(e) = std::fs::remove_file(&dst) {
                if is_permission(&e) && cfg!(target_os = "macos") {
                    elevate_remove(&dst)?;
                } else {
                    return Err(format!("删除 {} 失败：{e}", dst.display()));
                }
            }
        }
        Ok(_) => {
            return Err(format!("{} 不是符号链接，已拒绝删除", dst.display()));
        }
        Err(e) => return Err(format!("探测 {} 失败：{e}", dst.display())),
    }
    Ok(status())
}

fn elevate_remove(dst: &Path) -> Result<(), String> {
    let script = format!(
        "do shell script \"rm -f {}\" with administrator privileges",
        shq(dst)
    );
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .map_err(|e| format!("无法启动 osascript：{e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("-128") || stderr.to_lowercase().contains("user canceled") {
        return Err("已取消管理员授权，未做任何更改".into());
    }
    Err(format!("管理员授权删除失败：{}", stderr.trim()))
}

/// 启动自愈（仅 release 调用）：应用升级换包后，PATH 上指向旧 .app 内 CLI 的
/// 符号链接已失效。发现「指向 chrome-host 旧 bundle / 构建产物」的链接时尝试
/// 静默重链（无权限不提权）仅记日志，等用户在设置页修复。
#[cfg_attr(debug_assertions, allow(dead_code))] // 仅 release 由 main.rs 调用
pub fn heal_stale_link() {
    let Some(src) = sidecar_path() else {
        return;
    };
    for dir in candidate_dirs() {
        let p = dir.join(link_file_name());
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if !meta.file_type().is_symlink() || symlink_targets(&p, &src) {
            continue;
        }
        let target = std::fs::read_link(&p).unwrap_or_default();
        if !is_owned_link_target(&target) {
            continue;
        }
        match replace_symlink(&src, &p, false) {
            Ok(()) => tracing::info!("CLI 符号链接已自愈：{} → {}", p.display(), src.display()),
            Err(e) => tracing::info!("CLI 符号链接 {} 已失效，等待用户在设置页修复：{e}", p.display()),
        }
    }
}

// ---- Tauri commands（GUI 设置页调用；本机机器级操作，不走 Agent API 公开契约）----

#[tauri::command]
pub fn cli_tool_status() -> CliToolStatus {
    status()
}

/// osascript 提权会阻塞等待用户响应，放 blocking 线程避免卡 async runtime
#[tauri::command]
pub async fn cli_tool_install() -> Result<CliToolStatus, String> {
    tauri::async_runtime::spawn_blocking(install)
        .await
        .map_err(|e| format!("安装任务执行失败：{e}"))?
}

#[tauri::command]
pub async fn cli_tool_uninstall() -> Result<CliToolStatus, String> {
    tauri::async_runtime::spawn_blocking(uninstall)
        .await
        .map_err(|e| format!("卸载任务执行失败：{e}"))?
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "ch-cli-tool-{}-{tag}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fake_cli(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join("chrome-host");
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        p
    }

    #[test]
    fn install_creates_symlink_and_is_idempotent() {
        let tmp = TempDir::new("install");
        let src = fake_cli(tmp.path());
        let dst = tmp.path().join("bin/chrome-host");
        install_link(&src, &dst, false).unwrap();
        assert!(symlink_targets(&dst, &src), "链接应指向 sidecar");
        // 幂等：重复安装不报错不重建
        install_link(&src, &dst, false).unwrap();
        assert_eq!(std::fs::read_link(&dst).unwrap(), src);
    }

    #[test]
    fn install_replaces_stale_owned_symlink() {
        let tmp = TempDir::new("replace");
        let old = fake_cli(&tmp.path().join("old-app"));
        let new = fake_cli(&tmp.path().join("new-app"));
        let dst = tmp.path().join("bin/chrome-host");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&old, &dst).unwrap();
        install_link(&new, &dst, false).unwrap();
        assert!(symlink_targets(&dst, &new), "旧链接应被原子替换");
    }

    #[test]
    fn install_refuses_existing_regular_file() {
        let tmp = TempDir::new("refuse");
        let src = fake_cli(tmp.path());
        let dst = tmp.path().join("chrome-host");
        std::fs::write(&dst, "user tool").unwrap();
        let err = install_link(&src, &dst, false).unwrap_err();
        assert!(err.contains("不是符号链接"), "应拒绝覆盖普通文件：{err}");
    }

    #[test]
    fn uninstall_removes_owned_link_but_keeps_foreign() {
        let tmp = TempDir::new("uninstall");
        // 模拟 .app 内路径：ownership 启发式按路径模式识别自有链接
        let src = fake_cli(&tmp.path().join("chrome-host.app/Contents/MacOS"));
        let dst = tmp.path().join("bin/chrome-host");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&src, &dst).unwrap();
        assert!(is_owned_link_target(&std::fs::read_link(&dst).unwrap()));
        std::fs::remove_file(&dst).unwrap();
        assert!(!dst.exists());

        // 外部目标（如用户自装的别的工具）不可判为自有
        let foreign = tmp.path().join("other-tool/chrome-host");
        std::fs::create_dir_all(foreign.parent().unwrap()).unwrap();
        assert!(!is_owned_link_target(&foreign));
    }
}
