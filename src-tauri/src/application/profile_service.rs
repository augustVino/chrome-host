//! Login Profile 生命周期（身份层文件路径）。
//!
//! v0.2.3 起「实例即登录源」：主路径 = 实例里登录 → 关闭实例 → 自动保存
//! （SessionService），本服务收敛为快照本体的文件层操作（capture_files_from /
//! consolidate_from / reset）与查询视图。母本 launch 浏览器子系统已删除
//! （0.3.0 breaking）：launch/reattach/关窗检测/内存判活等 ~400 行进程管理不再存在。
//!
//! 数据安全语义保留：capture/reset/克隆前仍按 ps 扫描检测遗留登录浏览器
//! （旧版本升级场景的孤儿进程）→ 409 / 降级，不 consolidate 一个活着的
//! Chrome 目录（Cookies SQLite 并发写会撕裂）。
//!
//! 纪律："清空"语义用仓储专用 SQL；capture_files_from 的母本互斥由 gate 保证
//!（同一目录上的 consolidate 原子换名不可并发，clone 以 try_lock 降级）。

use std::path::Path;

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::domain::environment::Environment;
use crate::domain::login_profile::{LoginProfile, LoginProfileStatus};
use crate::error::AppError;
use crate::infrastructure::db::repositories::environment_repository::EnvironmentRepository;
use crate::infrastructure::db::repositories::instance_repository::InstanceRepository;
use crate::infrastructure::db::repositories::login_profile_repository::LoginProfileRepository;
use crate::infrastructure::paths::AppPaths;
use crate::infrastructure::platform;
use crate::infrastructure::process::ProcessManager;
use crate::infrastructure::profile::copier;

use std::sync::Arc;

/// REST 视图。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginProfileView {
    pub id: String,
    pub name: String,
    pub status: LoginProfileStatus,
    pub snapshot_version: i64,
    pub instances_using: i64,
    pub last_captured_at: Option<i64>,
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
}

pub struct ProfileService {
    envs: Arc<EnvironmentRepository>,
    instances: Arc<InstanceRepository>,
    profiles: Arc<LoginProfileRepository>,
    process: Arc<dyn ProcessManager>,
    paths: AppPaths,
    app: AppHandle,
    /// 变更操作（capture/reset）串行闸门：同一母本目录上的 consolidate 原子
    /// 换名必须互斥；clone_for_instance 以 try_lock 检测「操作进行中」并降级。
    /// （launch 已删除，gate 的浏览器用途不复存在，母本互斥语义保留）
    gate: tokio::sync::Mutex<()>,
}

impl ProfileService {
    pub fn new(
        envs: Arc<EnvironmentRepository>,
        instances: Arc<InstanceRepository>,
        profiles: Arc<LoginProfileRepository>,
        process: Arc<dyn ProcessManager>,
        paths: AppPaths,
        app: AppHandle,
    ) -> Self {
        ProfileService { envs, instances, profiles, process, paths, app, gate: tokio::sync::Mutex::new(()) }
    }

    // ---------- 查询 ----------

    /// 全量列表（Login Profiles 页）：仅返回已建行的环境
    pub async fn list(&self) -> Result<Vec<LoginProfileRow>, AppError> {
        let envs = self.envs.list()?;
        let mut rows = Vec::new();
        for env in &envs {
            let Some(profile) = self.profiles.find_by_env(&env.id)? else { continue };
            rows.push(LoginProfileRow {
                environment_name: env.name.clone(),
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
        })
    }

    /// 启动对账恢复（P1/H1）：Capturing 卡死行按快照版本分流（v0 →
    /// not_configured 防 clone 放行空登录态；v>0 → ready 按旧版本号）；
    /// 并对全部母本目录执行换名残留处置（.old 是唯一母本时恢复回位，
    /// 不必等下次 capture 才发现）。main 启动编排调用
    pub fn recover_stale_capturing(&self) {
        match self.profiles.recover_stale_capturing() {
            Ok(0) => {}
            Ok(n) => tracing::warn!("[profile] 启动恢复：{n} 个 Capturing 卡死状态已按版本分流恢复"),
            Err(e) => tracing::warn!("[profile] Capturing 卡死恢复失败（不阻断启动）: {e}"),
        }
        match self.profiles.list_all() {
            Ok(rows) => {
                for row in rows {
                    let dir = std::path::Path::new(&row.profile_dir);
                    match copier::resolve_stale_swap(dir) {
                        Ok(true) => tracing::warn!(
                            "[profile] 启动恢复：母本目录从换名残留回位: {}",
                            dir.display()
                        ),
                        Ok(false) => {}
                        Err(e) => {
                            tracing::warn!("[profile] 换名残留处置失败（继续）: {} - {e}", dir.display())
                        }
                    }
                }
            }
            Err(e) => tracing::warn!("[profile] 读取 profile 行失败，跳过换名残留处置: {e}"),
        }
    }

    // ---------- capture----------

    /// 停机捕获快照：遗留登录浏览器运行中 → 409；否则终止孤儿 →
    /// 原地固化（母本目录收敛为白名单文件，version+1）→ ready。
    pub async fn capture(&self, env_id: &str) -> Result<LoginProfileView, AppError> {
        let env = self.envs.get(env_id)?;
        let profile = self.get_or_create(&env)?;
        let _gate = self.gate.lock().await;

        if profile.status == LoginProfileStatus::Capturing {
            return Err(AppError::conflict("PROFILE_IN_USE", "快照捕获已在进行中"));
        }

        // 遗留登录浏览器（旧版本 launch 子系统升级前遗留）→ 代为终止。
        // terminate 内置 SIGTERM→SIGKILL+等待；返回后探活复核（不轻信单次读数）
        let dir = Path::new(&profile.profile_dir);
        if let Some(pid) = self.orphan_pid(dir) {
            tracing::info!("capture 前终止遗留登录浏览器: pid={pid}");
            self.process.terminate(pid)?;
            if self.process.is_alive(pid, dir)? {
                return Err(AppError::business(
                    500,
                    "PROFILE_SNAPSHOT_FAILED",
                    format!("遗留登录浏览器（pid {pid}）未能退出，快照未捕获"),
                ));
            }
        }

        self.profiles.mark_capturing(&profile.id)?;
        self.emit(env_id);

        match self.consolidate_from(Path::new(&profile.profile_dir), &profile) {
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

    /// 从指定来源目录固化文件层快照并迁移状态（gate 串行 + capturing→ready）。
    /// 来源可以是母本自身（传统捕获）或已停止实例的 profile 目录（实例即登录源）。
    /// 供 SessionService 调用；失败置 error 态（与显式捕获同语义，失败是准确状态）。
    pub async fn capture_files_from(
        &self,
        env_id: &str,
        source_dir: &Path,
    ) -> Result<LoginProfileView, AppError> {
        let env = self.envs.get(env_id)?;
        let profile = self.get_or_create(&env)?;
        let _gate = self.gate.lock().await;

        if profile.status == LoginProfileStatus::Capturing {
            return Err(AppError::conflict("PROFILE_IN_USE", "快照捕获已在进行中"));
        }
        // 母本上有遗留登录浏览器在写 → 文件层不可变约定，与 capture 一致拒做
        if self.orphan_pid(Path::new(&profile.profile_dir)).is_some() {
            return Err(AppError::conflict(
                "PROFILE_IN_USE",
                "遗留登录浏览器运行中（旧版本 launch 遗留），请先关闭该浏览器窗口",
            ));
        }

        self.profiles.mark_capturing(&profile.id)?;
        self.emit(env_id);
        match self.consolidate_from(source_dir, &profile) {
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
    /// 白名单文件从来源目录拷入同级临时目录 → 原子换名替换母本 → 删除旧目录。
    /// 效果：缓存等可再生内容出清（体积可控），失败可回滚不丢母本。
    fn consolidate_from(&self, source: &Path, profile: &LoginProfile) -> Result<(), AppError> {
        let dir = Path::new(&profile.profile_dir);
        if !source.exists() {
            // 来源不存在（从未打开过浏览器）：固化即建空母本目录
            std::fs::create_dir_all(dir)?;
            return Ok(());
        }

        let plan = copier::build(source);
        let file_name = dir.file_name().expect("profile_dir 有文件名").to_string_lossy().to_string();
        let parent = dir.parent().expect("profile_dir 有父目录");
        let tmp = parent.join(format!(".{file_name}.capturing"));
        let old = parent.join(format!(".{file_name}.old"));
        // 换名区间崩溃残留处置（copier::resolve_stale_swap：old 是唯一母本时
        // 恢复回位，否则清除；此前开头无条件删 old 会把唯一母本删掉）
        if copier::resolve_stale_swap(dir)? {
            tracing::warn!("[profile] 发现上次固化中断残留，母本目录已恢复: {}", dir.display());
        }
        std::fs::create_dir_all(&tmp)?;

        let bytes = copier::execute(&plan, source, &tmp)?;

        // 目标（母本）不存在（新环境首次从实例捕获）：无旧目录可归档，直接换名落位
        let archived = dir.exists();
        if archived {
            std::fs::rename(dir, &old)?;
        }
        if let Err(e) = std::fs::rename(&tmp, dir) {
            // 回滚：旧目录原位，母本维持 capture 前状态
            if archived {
                let _ = std::fs::rename(&old, dir);
            }
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

    /// 清空母本回到 Not Configured。遗留浏览器运行中 → 409，不代杀。
    pub async fn reset(&self, env_id: &str) -> Result<LoginProfileView, AppError> {
        let env = self.envs.get(env_id)?;
        let profile = self.get_or_create(&env)?;
        let _gate = self.gate.lock().await;

        if self.orphan_pid(Path::new(&profile.profile_dir)).is_some() {
            return Err(AppError::conflict(
                "PROFILE_IN_USE",
                "遗留登录浏览器运行中（旧版本 launch 遗留），请先关闭该浏览器窗口",
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
    /// 遗留登录浏览器在写时不克隆——Chrome 正在写 Cookies 等 SQLite，
    /// 并发复制可能产出撕裂的登录库（比空登录态更糟，且用户无法排查）。
    pub fn clone_for_instance(&self, env_id: &str, ins_id: &str) -> Result<Option<String>, AppError> {
        // 有变更操作在跑（capture/reset）→ 母本目录不稳定，降级
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
        if self.orphan_pid(Path::new(&profile.profile_dir)).is_some() {
            tracing::info!("[clone] 遗留登录浏览器运行中，实例 {ins_id} 以空登录态创建");
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

    /// 环境删除时联动：遗留登录浏览器运行中 → 409（与"运行中实例先停止"同语义）；
    /// 否则清理母本目录 + 删行。
    pub fn delete_for_env(&self, env_id: &str) -> Result<(), AppError> {
        let Some(profile) = self.profiles.find_by_env(env_id)? else {
            return Ok(());
        };
        if self.gate.try_lock().is_err() {
            return Err(AppError::conflict("PROFILE_IN_USE", "登录 profile 操作进行中，稍后重试"));
        }
        if self.orphan_pid(Path::new(&profile.profile_dir)).is_some() {
            return Err(AppError::conflict(
                "PROFILE_IN_USE",
                "遗留登录浏览器运行中（旧版本 launch 遗留），请先关闭该浏览器窗口",
            ));
        }
        let dir = Path::new(&profile.profile_dir);
        if let Err(e) = std::fs::remove_dir_all(dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("清理登录 profile 目录失败（继续删行）: {} - {e}", profile.profile_dir);
            }
        }
        self.profiles.delete_by_env(env_id)?;
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

    /// 遗留登录浏览器探活（旧版本 launch 子系统升级前遗留的孤儿进程）：
    /// ps 按母本目录扫主进程 pid。无内存态、无收养、无 watcher——
    /// 只做数据安全判定（活目录不 consolidate/不克隆），不做生命周期管理。
    fn orphan_pid(&self, dir: &Path) -> Option<u32> {
        platform::scan_pid_by_profile(dir)
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
