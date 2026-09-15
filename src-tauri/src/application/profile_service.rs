//! Login Profile 生命周期。
//!
//! 登录浏览器**不进 instances 表**（reconciler 不感知它）：
//! - pid 存内存 `Mutex<HashMap<env_id, pid>>`（决策），退出由 reaper 回调驱动；
//! - 应用重启后内存态丢失，靠 `reattach_all` 按 profile_dir 扫描 reattach；
//! - CDP 端口落库：跨重启的端口占用凭证，防实例分配撞端口。
//!
//! 纪律：退出检测走事件回调；"清空"语义用仓储专用 SQL；
//! capture 前置判活用 terminate（内置等待）+ 后置探活复核，不轻信单次读数。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::domain::environment::Environment;
use crate::domain::login_profile::{LoginProfile, LoginProfileStatus};
use crate::error::AppError;
use crate::infrastructure::cdp;
use crate::infrastructure::db::repositories::environment_repository::EnvironmentRepository;
use crate::infrastructure::db::repositories::instance_repository::InstanceRepository;
use crate::infrastructure::db::repositories::login_profile_repository::LoginProfileRepository;
use crate::infrastructure::hosts;
use crate::infrastructure::kernel::KernelManager;
use crate::infrastructure::paths::AppPaths;
use crate::infrastructure::platform;
use crate::infrastructure::process::port_allocator;
use crate::infrastructure::process::{LaunchSpec, ProcessManager};
use crate::infrastructure::profile::copier;

use super::settings_service::SettingsService;

const CDP_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 关窗检测（macOS 关窗不退进程，与 reconciler 参数一致）：
/// 启动缓冲期 10s 内不判定；CDP page 数为 0 需 ≥2.5s 连续双确认才动手（单次读数不可信）
const TAB_CHECK_GRACE_MS: i64 = 10_000;
const TAB_EMPTY_CONFIRM_MS: i64 = 2_500;

/// REST 视图。browserRunning 为派生附加字段：
/// UI 需要它决定"捕获/再次打开"是否应禁用，避免靠 409 试错。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginProfileView {
    pub id: String,
    pub name: String,
    pub status: LoginProfileStatus,
    pub snapshot_version: i64,
    pub instances_using: i64,
    pub last_captured_at: Option<i64>,
    pub browser_running: bool,
    /// 登录浏览器 CDP 端口（运行中才有；附加字段，Agent 自动化登录可用）
    pub cdp_port: Option<u16>,
}

/// Login Profiles 页视图（含环境名）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginProfileRow {
    pub id: String,
    pub environment_id: String,
    pub environment_name: String,
    pub name: String,
    pub status: LoginProfileStatus,
    pub snapshot_version: i64,
    pub instances_using: i64,
    pub last_captured_at: Option<i64>,
    pub browser_running: bool,
}

pub struct ProfileService {
    envs: Arc<EnvironmentRepository>,
    instances: Arc<InstanceRepository>,
    profiles: Arc<LoginProfileRepository>,
    kernel: Arc<KernelManager>,
    process: Arc<dyn ProcessManager>,
    settings: Arc<SettingsService>,
    paths: AppPaths,
    app: AppHandle,
    /// env_id → (登录浏览器 pid, 启动时间戳)。
    /// pid 仅本进程 launch 的浏览器（重启后 reattach 重建）；launched_at 供关窗检测缓冲期用。
    /// Arc 化以便退出回调（reaper 线程）也能清内存态。
    running: Arc<Mutex<HashMap<String, (u32, i64)>>>,
    /// 关窗检测的连续确认状态：env_id → 首次读到 0 page 的时间戳
    window_empty_since: Arc<Mutex<HashMap<String, i64>>>,
    /// 关窗检测武装标记：见过 ≥1 个 page 后才允许“0 page → 关窗”判定，
    /// 替代固定等待—— armed 后关窗回正延迟从最匧18s 降到 ~6s
    window_page_seen: Arc<Mutex<HashMap<String, bool>>>,
    /// 变更操作（launch/capture/reset）串行闸门：同一母本目录上的操作必须互斥，
    /// 否则会出现双开浏览器 / capture 中 clone 半成品等竞态。
    gate: tokio::sync::Mutex<()>,
}

impl ProfileService {
    pub fn new(
        envs: Arc<EnvironmentRepository>,
        instances: Arc<InstanceRepository>,
        profiles: Arc<LoginProfileRepository>,
        kernel: Arc<KernelManager>,
        process: Arc<dyn ProcessManager>,
        settings: Arc<SettingsService>,
        paths: AppPaths,
        app: AppHandle,
    ) -> Self {
        ProfileService {
            envs,
            instances,
            profiles,
            kernel,
            process,
            settings,
            paths,
            app,
            running: Arc::new(Mutex::new(HashMap::new())),
            window_empty_since: Arc::new(Mutex::new(HashMap::new())),
            window_page_seen: Arc::new(Mutex::new(HashMap::new())),
            gate: tokio::sync::Mutex::new(()),
        }
    }

    // ---------- 查询 ----------


/// 全量列表（Login Profiles 页）：仅返回已建行的环境；隐藏窗口 JS 挂起不影响（读路径）
pub async fn list(&self) -> Result<Vec<LoginProfileRow>, AppError> {
    let envs = self.envs.list()?;
    let mut rows = Vec::new();
    for env in &envs {
        let Some(profile) = self.profiles.find_by_env(&env.id)? else { continue };
        rows.push(LoginProfileRow {
            environment_name: env.name.clone(),
            browser_running: self.memory_alive_pid(&profile)?.is_some(),
            instances_using: self.profiles.count_instances_using(&profile.id)?,
            id: profile.id,
            environment_id: profile.environment_id,
            name: profile.name,
            status: profile.status,
            snapshot_version: profile.snapshot_version,
            last_captured_at: profile.last_captured_at,
        });
    }
    Ok(rows)
}

    /// GET 视图：惰性建行（首次访问 login-profile API/UI 时）。
    /// 刻意不走 gate——3s 轮询不得被长操作阻塞。
    pub async fn view(&self, env_id: &str) -> Result<LoginProfileView, AppError> {
        let env = self.envs.get(env_id)?;
        let profile = self.get_or_create(&env)?;
        self.reap_closed_window(&profile).await; // macOS 关窗检测
        self.to_view(&profile)
    }

    fn to_view(&self, profile: &LoginProfile) -> Result<LoginProfileView, AppError> {
        Ok(LoginProfileView {
            id: profile.id.clone(),
            name: profile.name.clone(),
            status: profile.status,
            snapshot_version: profile.snapshot_version,
            instances_using: self.profiles.count_instances_using(&profile.id)?,
            last_captured_at: profile.last_captured_at,
            browser_running: self.memory_alive_pid(profile)?.is_some(),
            cdp_port: profile.cdp_port,
        })
    }

    // ---------- launch----------

    /// 用母本目录启动浏览器供用户登录。hosts 注入与实例同款。
    pub async fn launch(&self, env_id: &str) -> Result<LoginProfileView, AppError> {
        let env = self.envs.get(env_id)?;
        let profile = self.get_or_create(&env)?;
        self.reap_closed_window(&profile).await;
        let _gate = self.gate.lock().await;

        if let Some(pid) = self.find_running_pid(&profile)? {
            return Err(AppError::conflict(
                "PROFILE_IN_USE",
                format!("登录浏览器已在运行（pid {pid}）"),
            ));
        }

        // 端口：实例占用 ∪ 登录浏览器登记端口 的补集中分配
        let mut used = self.profiles.used_cdp_ports()?;
        used.extend(self.instances.used_ports()?);
        let port = port_allocator::allocate(port_allocator::START_PORT, &used).await?;

        let executable = self
            .kernel
            .ensure_ready(&self.app)
            .await
            .map_err(|e| AppError::business(500, "KERNEL_NOT_READY", e))?;

        // hosts 注入（登录内网系统同样依赖域名映射）
        let mut extra_args: Vec<String> = Vec::new();
        if let Some(rules) = hosts::resolver::resolve(env.hosts_source_url.as_deref()).await? {
            extra_args.push(format!("--host-resolver-rules={}", rules.to_resolver_rules()));
            extra_args.push("--no-proxy-server".to_string());
        }
        if let Some(args) = &env.startup_args {
            extra_args.extend(args.split_whitespace().map(str::to_string));
        }
        // 启动页与实例同款：全局默认起始页 → about:blank（登录内网页面同样依赖）
        let start_url = self.settings.resolve_start_url()?;
        extra_args.push(start_url); // 启动页必须最后

        std::fs::create_dir_all(&profile.profile_dir)?;
        let spec = LaunchSpec {
            executable,
            user_data_dir: PathBuf::from(&profile.profile_dir),
            cdp_port: port,
            extra_args,
        };

        // spawn 快（毫秒级），可与内存态写入同临界区，杜绝并发双击双开
        let pid = {
            let mut map = self.lock_running()?;
            if let Some((stale, _)) = map.get(env_id).copied() {
                if self.process.is_alive(stale, Path::new(&profile.profile_dir))? {
                    return Err(AppError::conflict("PROFILE_IN_USE", "登录浏览器已在运行"));
                }
                map.remove(env_id);
            }
            let pid = self.process.launch(&spec)?;
            map.insert(env_id.to_string(), (pid, chrono::Utc::now().timestamp_millis()));
            self.window_page_seen.lock().unwrap().remove(env_id); // 新进程重新武装
            self.profiles.mark_browser_started(&profile.id, port)?;
            self.attach_exit_watcher(env_id, &profile.id, pid);
            pid
        };
        tracing::info!("登录浏览器已启动: env={env_id} pid={pid} cdp={port}");

        // CDP 就绪才算 launch 成功（与实例同款语义）
        if cdp::client::wait_until_ready(port, CDP_READY_TIMEOUT).await.is_err() {
            self.lock_running()?.remove(env_id);
            self.profiles.mark_browser_stopped(&profile.id)?;
            let _ = self.process.terminate(pid);
            return Err(AppError::business(
                500,
                "INSTANCE_START_FAILED",
                "登录浏览器启动失败：CDP 端口未在超时内就绪，已回收进程",
            ));
        }

        self.to_view(&self.profiles.find_by_env(env_id)?.expect("行已存在"))
    }

    /// 应用启动全量 reattach。
    /// 孤儿登录浏览器（app 崩溃遗留）：ps 按母本目录重找主进程 pid，恢复内存态。
    /// 此时进程非本进程子进程，无法挂 watcher——退出检测退化为 find_running_pid 的探活路径。
    pub fn reattach_all(&self) {
        let rows = match self.profiles.list_all() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!("登录 profile 读取失败（跳过 reattach）: {e}");
                return;
            }
        };
        let mut map = match self.lock_running() {
            Ok(m) => m,
            Err(_) => return,
        };
        for profile in rows {
            if map.contains_key(&profile.environment_id) {
                continue;
            }
            if let Some(pid) = platform::scan_pid_by_profile(Path::new(&profile.profile_dir)) {
                map.insert(
                    profile.environment_id.clone(),
                    (pid, chrono::Utc::now().timestamp_millis()),
                );
                tracing::info!(
                    "登录浏览器 reattach: env={} pid={} cdp={:?}",
                    profile.environment_id,
                    pid,
                    profile.cdp_port
                );
            }
        }
    }

    // ---------- capture----------

    /// 停机捕获快照：浏览器运行中 → 409；否则终止孤儿 →
    /// 原地固化（母本目录收敛为白名单文件，version+1）→ ready。
    pub async fn capture(&self, env_id: &str) -> Result<LoginProfileView, AppError> {
        let env = self.envs.get(env_id)?;
        let profile = self.get_or_create(&env)?;
        self.reap_closed_window(&profile).await;
        let _gate = self.gate.lock().await;

        if profile.status == LoginProfileStatus::Capturing {
            return Err(AppError::conflict("PROFILE_IN_USE", "快照捕获已在进行中"));
        }

        // 母本浏览器运行中 → 409（不代杀用户开着的窗口）
        if let Some(pid) = self.memory_alive_pid(&profile)? {
            return Err(AppError::conflict(
                "PROFILE_IN_USE",
                format!("登录浏览器运行中（pid {pid}），请先关闭再捕获"),
            ));
        }
        // 遗留孤儿（非本进程追踪，app 重启前遗留且 reattach 未收养）→ 代为终止。
        // terminate 内置 SIGTERM→SIGKILL+等待；返回后探活复核（不轻信单次读数）
        let dir = Path::new(&profile.profile_dir);
        if let Some(pid) = platform::scan_pid_by_profile(dir) {
            tracing::info!("capture 前终止遗留登录浏览器: pid={pid}");
            self.process.terminate(pid)?;
            if self.process.is_alive(pid, dir)? {
                return Err(AppError::business(
                    500,
                    "PROFILE_SNAPSHOT_FAILED",
                    format!("遗留登录浏览器（pid {pid}）未能退出，快照未捕获"),
                ));
            }
            self.lock_running()?.remove(env_id);
            self.profiles.mark_browser_stopped(&profile.id)?;
        }

        self.profiles.mark_capturing(&profile.id)?;
        self.emit(env_id);

        match self.consolidate(&profile) {
            Ok(()) => {
                self.profiles.mark_ready(&profile.id, profile.snapshot_version + 1)?;
            }
            Err(e) => {
                self.profiles.mark_error(&profile.id)?;
                self.emit(env_id);
                return Err(e);
            }
        }
        self.emit(env_id);
        self.to_view(&self.profiles.find_by_env(env_id)?.expect("行已存在"))
    }

    /// 原地固化：母本目录 = 快照本体（简化决策）。
    /// 白名单文件拷入同级临时目录 → 原子换名替换母本 → 删除旧目录。
    /// 效果：缓存等可再生内容出清（体积可控），失败可回滚不丢母本。
    fn consolidate(&self, profile: &LoginProfile) -> Result<(), AppError> {
        let dir = Path::new(&profile.profile_dir);
        if !dir.exists() {
            std::fs::create_dir_all(dir)?;
            return Ok(()); // 从未打开过浏览器的空母本：固化即建目录
        }

        let plan = copier::build(dir);
        let file_name = dir.file_name().expect("profile_dir 有文件名").to_string_lossy().to_string();
        let parent = dir.parent().expect("profile_dir 有父目录");
        let tmp = parent.join(format!(".{file_name}.capturing"));
        let old = parent.join(format!(".{file_name}.old"));
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&old);
        std::fs::create_dir_all(&tmp)?;

        let bytes = copier::execute(&plan, dir, &tmp)?;

        std::fs::rename(dir, &old)?;
        if let Err(e) = std::fs::rename(&tmp, dir) {
            // 回滚：旧目录原位，母本维持 capture 前状态
            let _ = std::fs::rename(&old, dir);
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(AppError::internal(format!("快照固化失败（已回滚）: {e}")));
        }
        let _ = std::fs::remove_dir_all(&old);
        tracing::info!(
            "快照固化完成: env={} {} 个文件 {} 字节（version → {}）",
            profile.environment_id,
            plan.len(),
            bytes,
            profile.snapshot_version + 1
        );
        Ok(())
    }

    // ---------- reset----------

    /// 清空母本回到 Not Configured。运行中 → 409，不代杀。
    pub async fn reset(&self, env_id: &str) -> Result<LoginProfileView, AppError> {
        let env = self.envs.get(env_id)?;
        let profile = self.get_or_create(&env)?;
        self.reap_closed_window(&profile).await;
        let _gate = self.gate.lock().await;

        if let Some(pid) = self.find_running_pid(&profile)? {
            return Err(AppError::conflict(
                "PROFILE_IN_USE",
                format!("登录浏览器运行中（pid {pid}），请先关闭再重置"),
            ));
        }

        let dir = Path::new(&profile.profile_dir);
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        std::fs::create_dir_all(dir)?;
        self.profiles.mark_reset(&profile.id)?;
        self.emit(env_id);
        tracing::info!("登录快照已重置: env={env_id}");
        self.to_view(&self.profiles.find_by_env(env_id)?.expect("行已存在"))
    }

    // ---------- clone----------

    /// 实例创建流程接入点：ready 时从母本选择性复制到实例 profile 目录。
    /// 返回 Ok(Some(profile_id)) = 已克隆（实例记录 login_profile_id）；Ok(None) = 空登录态。
    ///
    /// 降级而非报错：保证实例永远能创建。
    /// 比计划更严格的一档：浏览器运行中也不克隆——Chrome 正在写 Cookies 等 SQLite，
    /// 并发复制可能产出撕裂的登录库（比空登录态更糟，且用户无法排查）。
    pub fn clone_for_instance(&self, env_id: &str, ins_id: &str) -> Result<Option<String>, AppError> {
        // 有变更操作在跑（launch/capture/reset）→ 母本目录不稳定，降级
        if self.gate.try_lock().is_err() {
            tracing::info!("[clone] profile 操作进行中，实例 {ins_id} 以空登录态创建");
            return Ok(None);
        }
        let Some(profile) = self.profiles.find_by_env(env_id)? else {
            return Ok(None); // 实例创建不触发惰性建行（仅 login-profile API/UI 触发）
        };
        if profile.status != LoginProfileStatus::Ready {
            return Ok(None);
        }
        if self.find_running_pid(&profile)?.is_some() {
            tracing::info!("[clone] 登录浏览器运行中，实例 {ins_id} 以空登录态创建");
            return Ok(None);
        }

        let src = Path::new(&profile.profile_dir);
        let dest = self.paths.instance_profile(env_id, ins_id);
        std::fs::create_dir_all(&dest)?;
        let plan = copier::build(src);
        let bytes = copier::execute(&plan, src, &dest)?;
        tracing::info!(
            "[clone] 快照 v{} 已克隆至实例 {ins_id}: {} 个文件 {} 字节",
            profile.snapshot_version,
            plan.len(),
            bytes
        );
        Ok(Some(profile.id))
    }

    // ---------- 环境级联删除----------

    /// 环境删除时联动：登录浏览器运行中（含孤儿）→ 409（与"运行中实例先停止"同语义）；
    /// 否则清理母本目录 + 删行。
    pub fn delete_for_env(&self, env_id: &str) -> Result<(), AppError> {
        let Some(profile) = self.profiles.find_by_env(env_id)? else {
            return Ok(());
        };
        if self.gate.try_lock().is_err() {
            return Err(AppError::conflict("PROFILE_IN_USE", "登录 profile 操作进行中，稍后重试"));
        }
        if let Some(pid) = self.find_running_pid(&profile)? {
            return Err(AppError::conflict(
                "PROFILE_IN_USE",
                format!("登录浏览器运行中（pid {pid}），请先关闭再删除环境"),
            ));
        }
        let dir = Path::new(&profile.profile_dir);
        if let Err(e) = std::fs::remove_dir_all(dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("清理登录 profile 目录失败（继续删行）: {} - {e}", profile.profile_dir);
            }
        }
        self.profiles.delete_by_env(env_id)?;
        self.lock_running()?.remove(env_id);
        Ok(())
    }

    // ---------- 内部 ----------

    fn get_or_create(&self, env: &Environment) -> Result<LoginProfile, AppError> {
        self.profiles.get_or_create(
            &env.id,
            &format!("{} Login", env.name),
            &self.paths.login_profile_dir(&env.id).to_string_lossy(),
        )
    }

    /// 仅内存态判活（GET 视图用，零 spawn 开销）。
    /// 探活失败做迟到清理（reattach 后无 watcher，退出全靠此处回收）。
    fn memory_alive_pid(&self, profile: &LoginProfile) -> Result<Option<u32>, AppError> {
        let dir = Path::new(&profile.profile_dir);
        let env_id = &profile.environment_id;
        let map = self.lock_running()?;
        if let Some((pid, _launched_at)) = map.get(env_id).copied() {
            if self.process.is_alive(pid, dir)? {
                return Ok(Some(pid));
            }
            drop(map);
            if self.cleanup_dead(env_id, pid).is_some() {
                self.profiles.mark_browser_stopped(&profile.id)?;
            }
        }
        Ok(None)
    }

    /// 判定登录浏览器是否在运行（变更路径用），返回主进程 pid。
    /// 证据链：内存 pid → libc 探活 + ps 命令行含母本目录；
    /// 无内存态 → ps 扫描收养孤儿。探活出错向上传播（宁报错不误判死亡）。
    fn find_running_pid(&self, profile: &LoginProfile) -> Result<Option<u32>, AppError> {
        if let Some(pid) = self.memory_alive_pid(profile)? {
            return Ok(Some(pid));
        }
        let dir = Path::new(&profile.profile_dir);
        let env_id = &profile.environment_id;
        match platform::scan_pid_by_profile(dir) {
            Some(pid) => {
                self.lock_running()?.insert(
                    env_id.clone(),
                    (pid, chrono::Utc::now().timestamp_millis()),
                );
                tracing::info!("发现遗留登录浏览器并收养: env={env_id} pid={pid}");
                Ok(Some(pid))
            }
            None => Ok(None),
        }
    }

    /// 清理已死浏览器的内存态（reattach 后无 watcher，迟到退出全靠此处回收）。
    fn cleanup_dead(&self, env_id: &str, pid: u32) -> Option<u32> {
        let mut map = self.lock_running().ok()?;
        match map.get(env_id) {
            Some((p, _)) if *p == pid => map.remove(env_id).map(|(p, _)| p),
            _ => None,
        }
    }

    /// 退出回调（reaper 线程执行）：清内存态 + 显式清 DB 端口（专用 SQL）+ 事件。
    /// 登录浏览器被用户关闭 = 进入"可捕获"状态，正是期望行为，无额外语义。
    fn attach_exit_watcher(&self, env_id: &str, profile_id: &str, pid: u32) {
        let profiles = self.profiles.clone();
        let running = self.running.clone();
        let window_page_seen = self.window_page_seen.clone();
        let env_id = env_id.to_string();
        let profile_id = profile_id.to_string();
        let app = self.app.clone();
        let result = self.process.attach_exit_watcher(
            pid,
            Box::new(move |_code| {
                if let Ok(mut map) = running.lock() {
                    if map.get(&env_id).map(|(p, _)| *p) == Some(pid) {
                        map.remove(&env_id);
                    }
                }
                if let Ok(mut seen) = window_page_seen.lock() {
                    seen.remove(&env_id);
                }
                if let Err(e) = profiles.mark_browser_stopped(&profile_id) {
                    tracing::error!("登录浏览器退出清理失败: {e}");
                }
                if let Err(e) = app.emit("login-profile-changed", serde_json::json!({})) {
                    tracing::error!("发送事件 login-profile-changed 失败: {e}");
                }
                tracing::info!("登录浏览器进程退出: profile={profile_id} pid={pid}");
            }),
        );
        if let Err(e) = result {
            tracing::error!("登录浏览器退出监听挂载失败（退化为探活路径）: {e}");
        }
    }

    fn lock_running(&self) -> Result<MutexGuard<'_, HashMap<String, (u32, i64)>>, AppError> {
        self.running.lock().map_err(|e| AppError::internal(e.to_string()))
    }

    /// macOS 关窗检测（用户实测暴露：关窗后进程不退，browserRunning 恒 true、按钮恒灰）。
    /// CDP page 数为 0 且过缓冲期 + 双确认 → 用户已关窗 = 可捕获态，代为收尾（terminate + 清态）。
    /// CDP 不可达 → 保守跳过不动作（单次读数不可信）。由 view/launch/capture/reset 驱动
    /// （登录浏览器无 reconciler，3s 轮询即触发器）。
    async fn reap_closed_window(&self, profile: &LoginProfile) {
        let env_id = &profile.environment_id;
        let (pid, launched_at) = {
            let Ok(map) = self.running.lock() else { return };
            match map.get(env_id) {
                Some(entry) => *entry,
                None => return,
            }
        };
        let dir = Path::new(&profile.profile_dir);
        if !self.process.is_alive(pid, dir).unwrap_or(false) {
            if self.cleanup_dead(env_id, pid).is_some() {
                let _ = self.profiles.mark_browser_stopped(&profile.id);
            }
            return;
        }
        let now = chrono::Utc::now().timestamp_millis();
        let armed = {
            let seen = self.window_page_seen.lock().unwrap();
            seen.get(env_id).copied().unwrap_or(false)
        } || now - launched_at >= TAB_CHECK_GRACE_MS;
        if !armed {
            self.window_empty_since.lock().unwrap().remove(env_id);
            return;
        }
        let Some(port) = profile.cdp_port else { return };
        let pages = match cdp::client::list_targets(port).await {
            Ok(targets) => targets.iter().filter(|t| t.target_type == "page").count(),
            Err(_) => return, // 不可达不代表关窗（Chrome 忙/瞬时异常），交给进程退出路径
        };
        if pages > 0 {
            self.window_page_seen.lock().unwrap().insert(env_id.clone(), true);
            self.window_empty_since.lock().unwrap().remove(env_id);
            return;
        }
        let confirmed = {
            let mut since = self.window_empty_since.lock().unwrap();
            match since.get(env_id) {
                None => {
                    since.insert(env_id.clone(), now);
                    false
                }
                Some(first) if now - first < TAB_EMPTY_CONFIRM_MS => false,
                Some(_) => {
                    since.remove(env_id);
                    true
                }
            }
        };
        if !confirmed {
            return;
        }
        tracing::info!("登录浏览器窗口已全部关闭（双确认），优雅终止进程: env={env_id} pid={pid}");
        let _ = self.process.terminate(pid);
        if self.cleanup_dead(env_id, pid).is_some() {
            let _ = self.profiles.mark_browser_stopped(&profile.id);
        }
        self.window_empty_since.lock().unwrap().remove(env_id);
        self.window_page_seen.lock().unwrap().remove(env_id);
        // 退出回调通常也会发事件；此处主动补发保证 UI 即时回正
        if let Err(e) =
            self.app.emit("login-profile-changed", serde_json::json!({ "environmentId": env_id }))
        {
            tracing::error!("发送事件 login-profile-changed 失败: {e}");
        }
    }

    fn emit(&self, env_id: &str) {
        if let Err(e) = self
            .app
            .emit("login-profile-changed", serde_json::json!({ "environmentId": env_id }))
        {
            tracing::error!("发送事件 login-profile-changed 失败: {e}");
        }
    }
}
