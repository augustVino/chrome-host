//! CfT 内核管理器。
//!
//! 设计要点：
//! - 下载双源：npmmirror 优先、官方兜底，路径结构一致仅 host 不同
//! - 单飞锁：同一时刻仅一个下载任务，后到者等待后二次检查复用结果
//! - 取消：watch 通道传递信号，下载/解压循环内检查，走正常退出路径保证清理
//! - 原子性：zip 下载到 .tmp，解压到版本目录，校验通过才写 meta.json
//! - 凭证：meta.json（版本/平台匹配 + 二进制存在 = 可用），内核状态不进数据库

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter};
use tokio::sync::{watch, Mutex as TokioMutex};

use super::constants::{
    cft_binary_rel_path, cft_platform, cft_zip_name, CFT_MIRRORS, CFT_VERSION, KERNEL_DOWNLOAD_EVENT,
};

// ---------- 全局下载状态（应用级单例） ----------

static DOWNLOAD_SLOT: Lazy<TokioMutex<()>> = Lazy::new(|| TokioMutex::new(()));
static CANCEL_TX: Lazy<TokioMutex<Option<watch::Sender<bool>>>> = Lazy::new(|| TokioMutex::new(None));
static DOWNLOADING: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
enum InstallError {
    Cancelled,
    Failed(String),
}

impl From<String> for InstallError {
    fn from(e: String) -> Self {
        InstallError::Failed(e)
    }
}

/// meta.json：版本目录的"安装完成"凭证。校验通过才写入。
#[derive(Debug, Serialize, Deserialize)]
struct CftMeta {
    version: String,
    platform: String,
    downloaded_at: String,
}

/// 内核状态（kernel.status 命令 / Settings 页）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CftStatus {
    pub installed: bool,
    pub version: Option<String>,
    pub pinned_version: String,
    pub upgrade_available: bool,
    pub downloading: bool,
    pub binary_path: Option<String>,
}

pub struct KernelManager {
    /// <app_data_dir>/kernel/chrome-for-testing
    root: PathBuf,
}

impl KernelManager {
    pub fn new(root: PathBuf) -> Self {
        KernelManager { root }
    }

    fn version_dir(&self) -> PathBuf {
        self.root.join(CFT_VERSION)
    }

    fn meta_path(&self) -> PathBuf {
        self.root.join("meta.json")
    }

    fn zip_url(mirror: &str) -> String {
        format!("{mirror}/{CFT_VERSION}/{}/{}", cft_platform(), cft_zip_name())
    }

    /// 已安装且版本匹配时返回二进制路径。meta 校验通过才写入，
    /// 因此天然排除"解压到一半"的残缺目录。
    pub fn installed_binary_path(&self) -> Option<PathBuf> {
        let meta: CftMeta = serde_json::from_str(&fs::read_to_string(self.meta_path()).ok()?).ok()?;
        if meta.version != CFT_VERSION || meta.platform != cft_platform() {
            return None;
        }
        let binary = self.version_dir().join(cft_binary_rel_path());
        binary.exists().then_some(binary)
    }

    /// 本地已装版本（任意版本，含待升级旧版）
    fn read_local_version(&self) -> Option<String> {
        let meta: CftMeta = serde_json::from_str(&fs::read_to_string(self.meta_path()).ok()?).ok()?;
        let binary = self.root.join(&meta.version).join(cft_binary_rel_path());
        binary.exists().then_some(meta.version)
    }

    pub fn status(&self) -> CftStatus {
        let local_version = self.read_local_version();
        let installed = self.installed_binary_path();
        let upgrade_available = local_version.as_ref().is_some_and(|v| v != CFT_VERSION);
        CftStatus {
            installed: installed.is_some(),
            version: local_version,
            pinned_version: CFT_VERSION.to_string(),
            upgrade_available,
            downloading: is_downloading(),
            binary_path: installed.map(|p| p.to_string_lossy().to_string()),
        }
    }

    /// 确保内核就绪并返回二进制路径（InstanceService launch 前调用）：
    /// 已装 pinned 版本 → 立即返回；未装 → 单飞下载，后到者等待后复用结果。
    pub async fn ensure_ready(&self, app: &AppHandle) -> Result<PathBuf, String> {
        if let Some(p) = self.installed_binary_path() {
            return Ok(p);
        }
        let _slot = DOWNLOAD_SLOT.lock().await;
        // 二次检查：等待期间其他任务可能已完成安装
        if let Some(p) = self.installed_binary_path() {
            return Ok(p);
        }
        self.run_install(app).await
    }

    /// 触发后台下载（Settings 页入口），立即返回；false = 已在下载中。
    pub fn start_download(&self, app: AppHandle) -> bool {
        if is_downloading() {
            return false;
        }
        let this_root = self.root.clone();
        tauri::async_runtime::spawn(async move {
            let manager = KernelManager::new(this_root);
            let _slot = DOWNLOAD_SLOT.lock().await;
            if manager.installed_binary_path().is_some() {
                manager.emit_progress(&app, "done", 100, 0, 0, None);
                return;
            }
            let _ = manager.run_install(&app).await;
        });
        true
    }

    pub async fn cancel_download(&self) -> bool {
        if !is_downloading() {
            return false;
        }
        let guard = CANCEL_TX.lock().await;
        match &*guard {
            Some(tx) => tx.send(true).is_ok(),
            None => false,
        }
    }

    async fn run_install(&self, app: &AppHandle) -> Result<PathBuf, String> {
        fs::create_dir_all(&self.root).map_err(|e| format!("创建内核目录失败: {e}"))?;

        let tmp_zip = self.root.join("download.zip.tmp");
        let _ = fs::remove_file(&tmp_zip);

        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        *CANCEL_TX.lock().await = Some(cancel_tx);
        DOWNLOADING.store(true, Ordering::SeqCst);

        let result = self
            .install_inner(app, &tmp_zip, &mut cancel_rx)
            .await;

        DOWNLOADING.store(false, Ordering::SeqCst);
        *CANCEL_TX.lock().await = None;
        let _ = fs::remove_file(&tmp_zip);

        match result {
            Ok(path) => Ok(path),
            Err(InstallError::Cancelled) => {
                self.emit_progress(app, "cancelled", 0, 0, 0, Some("下载已取消"));
                Err("下载已取消".to_string())
            }
            Err(InstallError::Failed(msg)) => {
                self.emit_progress(app, "error", 0, 0, 0, Some(&msg));
                Err(msg)
            }
        }
    }

    async fn install_inner(
        &self,
        app: &AppHandle,
        tmp_zip: &Path,
        cancel_rx: &mut watch::Receiver<bool>,
    ) -> Result<PathBuf, InstallError> {
        // 1. 下载（双源，进度 0–90）
        self.emit_progress(app, "downloading", 0, 0, 0, None);
        self.download_zip(app, tmp_zip, cancel_rx).await?;

        // 2. 解压（进度 90–99）。残留版本目录整目录删除后重解压。
        let dest = self.version_dir();
        if dest.exists() {
            fs::remove_dir_all(&dest)
                .map_err(|e| InstallError::Failed(format!("清理残留版本目录失败: {e}")))?;
        }
        fs::create_dir_all(&dest)
            .map_err(|e| InstallError::Failed(format!("创建版本目录失败: {e}")))?;
        self.extract_zip(app, tmp_zip, &dest, cancel_rx)?;

        let binary = dest.join(cft_binary_rel_path());

        // 3. 平台后处理
        super::platform::clear_quarantine(&dest);
        super::platform::ensure_executable(&binary);

        // 4. 校验：--version 输出必须包含 pinned 版本
        self.emit_progress(app, "verifying", 99, 0, 0, None);
        verify_binary(&binary).map_err(InstallError::Failed)?;

        // 5. 收尾：写 meta → 清旧版本 → done
        write_meta(&self.root)?;
        cleanup_old_versions(&self.root)?;
        let _ = fs::remove_file(tmp_zip);
        self.emit_progress(app, "done", 100, 0, 0, None);
        Ok(binary)
    }

    async fn download_zip(
        &self,
        app: &AppHandle,
        dest: &Path,
        cancel_rx: &mut watch::Receiver<bool>,
    ) -> Result<(), InstallError> {
        let client = reqwest::Client::builder()
            // 仅约束连接建立；传输阶段不限总时长（进度事件保证可观测）
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| InstallError::Failed(format!("创建 HTTP 客户端失败: {e}")))?;

        let mut last_err = String::new();
        for mirror in CFT_MIRRORS.iter() {
            let url = Self::zip_url(mirror);
            match self.try_download_one(&client, &url, dest, app, cancel_rx).await {
                Ok(()) => return Ok(()),
                Err(InstallError::Cancelled) => return Err(InstallError::Cancelled),
                Err(InstallError::Failed(e)) => {
                    last_err = e;
                    tracing::warn!("[kernel] 下载源失败，尝试下一个: {mirror} ({last_err})");
                }
            }
        }
        Err(InstallError::Failed(format!("所有下载源均失败: {last_err}")))
    }

    async fn try_download_one(
        &self,
        client: &reqwest::Client,
        url: &str,
        dest: &Path,
        app: &AppHandle,
        cancel_rx: &mut watch::Receiver<bool>,
    ) -> Result<(), InstallError> {
        let mut resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| InstallError::Failed(format!("请求失败: {e}")))?
            .error_for_status()
            .map_err(|e| InstallError::Failed(format!("响应异常: {e}")))?;

        let total = resp.content_length().unwrap_or(0);
        let mut file = File::create(dest)
            .map_err(|e| InstallError::Failed(format!("创建临时文件失败: {e}")))?;

        let mut downloaded: u64 = 0;
        let mut last_percent: i64 = -1;
        let mut last_emit = Instant::now();

        loop {
            // 每 chunk 检查一次取消；取消走正常退出路径完成清理
            let chunk = tokio::select! {
                _ = cancel_rx.changed() => {
                    if *cancel_rx.borrow_and_update() {
                        return Err(InstallError::Cancelled);
                    }
                    continue;
                }
                c = resp.chunk() => {
                    c.map_err(|e| InstallError::Failed(format!("下载中断: {e}")))?
                }
            };

            match chunk {
                Some(bytes) => {
                    file.write_all(&bytes)
                        .map_err(|e| InstallError::Failed(format!("写入临时文件失败: {e}")))?;
                    downloaded += bytes.len() as u64;

                    // 节流：百分比变化 或 距上次发射 ≥500ms
                    let percent = if total > 0 { (downloaded * 90 / total) as i64 } else { 0 };
                    if percent != last_percent || last_emit.elapsed() >= Duration::from_millis(500) {
                        last_percent = percent;
                        last_emit = Instant::now();
                        self.emit_progress(app, "downloading", percent as u32, downloaded, total, None);
                    }
                }
                None => break,
            }
        }

        file.flush()
            .map_err(|e| InstallError::Failed(format!("写入临时文件失败: {e}")))?;

        // 反爬返回 HTML 时 content-length 可能为 0，用最小体积阈值兜底
        if total == 0 || downloaded < total / 2 {
            return Err(InstallError::Failed(format!(
                "下载不完整（预期 {total} 字节，实际 {downloaded} 字节）"
            )));
        }
        Ok(())
    }

    /// 解压 zip。同步 CPU/IO 操作，桌面单一下载场景可接受（不引入 spawn_blocking）。
    fn extract_zip(
        &self,
        app: &AppHandle,
        zip_path: &Path,
        dest: &Path,
        cancel_rx: &mut watch::Receiver<bool>,
    ) -> Result<(), InstallError> {
        let file = File::open(zip_path)
            .map_err(|e| InstallError::Failed(format!("打开临时 zip 失败: {e}")))?;
        let mut archive = zip::ZipArchive::new(file)
            .map_err(|e| InstallError::Failed(format!("zip 文件损坏: {e}")))?;

        let total_entries = archive.len() as u64;
        let entry_count = archive.len();
        for i in 0..entry_count {
            if *cancel_rx.borrow_and_update() {
                return Err(InstallError::Cancelled);
            }
            let mut entry = archive
                .by_index(i)
                .map_err(|e| InstallError::Failed(format!("读取 zip 条目失败: {e}")))?;

            // 防 zip-slip：拒绝路径穿越条目
            let Some(rel_path) = entry.enclosed_name() else {
                continue;
            };
            let out_path = dest.join(rel_path);

            if entry.is_dir() {
                fs::create_dir_all(&out_path)
                    .map_err(|e| InstallError::Failed(format!("创建目录失败: {e}")))?;
            } else {
                if let Some(parent) = out_path.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| InstallError::Failed(format!("创建目录失败: {e}")))?;
                }
                let mut out_file = File::create(&out_path)
                    .map_err(|e| InstallError::Failed(format!("创建文件失败: {e}")))?;
                std::io::copy(&mut entry, &mut out_file)
                    .map_err(|e| InstallError::Failed(format!("解压写入失败: {e}")))?;

                #[cfg(unix)]
                if let Some(mode) = entry.unix_mode() {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&out_path, fs::Permissions::from_mode(mode));
                }
            }

            let percent = (90 + (i as u64 + 1) * 9 / total_entries) as u32;
            if i % 50 == 0 || i == entry_count - 1 {
                self.emit_progress(app, "extracting", percent, 0, 0, None);
            }
        }
        Ok(())
    }

    fn emit_progress(
        &self,
        app: &AppHandle,
        stage: &str,
        percent: u32,
        downloaded: u64,
        total: u64,
        message: Option<&str>,
    ) {
        let payload = json!({
            "stage": stage,
            "percent": percent,
            "downloaded": downloaded,
            "total": total,
            "version": CFT_VERSION,
            "message": message,
        });
        if let Err(e) = app.emit(KERNEL_DOWNLOAD_EVENT, payload) {
            tracing::error!("[kernel] 发送进度事件失败: {e}");
        }
    }
}

pub fn is_downloading() -> bool {
    DOWNLOADING.load(Ordering::SeqCst)
}

fn verify_binary(binary: &Path) -> Result<(), String> {
    if !binary.exists() {
        return Err(format!("内核二进制不存在: {}", binary.display()));
    }
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|e| {
            format!("无法执行内核二进制（Linux 缺依赖库时常见，可用 ldd 排查）: {e}")
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() && stdout.contains(CFT_VERSION) {
        Ok(())
    } else {
        Err(format!(
            "内核校验失败：--version 输出异常（stdout={stdout}, stderr={})",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn write_meta(root: &Path) -> Result<(), String> {
    let meta = CftMeta {
        version: CFT_VERSION.to_string(),
        platform: cft_platform().to_string(),
        downloaded_at: chrono::Utc::now().to_rfc3339(),
    };
    fs::write(
        root.join("meta.json"),
        serde_json::to_string_pretty(&meta).map_err(|e| format!("序列化 meta 失败: {e}"))?,
    )
    .map_err(|e| format!("写入 meta 失败: {e}"))
}

/// 删除 pinned 版本以外的所有版本目录（升级清理）
fn cleanup_old_versions(root: &Path) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(|e| format!("读取内核目录失败: {e}"))? {
        let entry = entry.map_err(|e| format!("读取内核目录失败: {e}"))?;
        let name = entry.file_name();
        if entry.path().is_dir() && name != CFT_VERSION {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_url_contains_version_platform_and_zip_name() {
        let url = KernelManager::zip_url("https://mirror.example.com");
        assert!(url.starts_with("https://mirror.example.com/"));
        assert!(url.contains(CFT_VERSION));
        assert!(url.ends_with(&cft_zip_name()));
    }
}
