//! 会话身份层服务：实例即登录源。
//!
//! 职责：
//! - 实例停止路径挂钩：terminate 前经 CDP 导出实例私有 Cookie（尽力而为，不阻塞停止）；
//! - `session_capture` 三档策略（off / first-login / always）决定何时把实例升级为环境身份；
//! - 显式从实例捕获（REST/CLI/MCP 入口）；
//! - 保险库状态查询与回滚。
//!
//! 分层：文件层快照固化复用 ProfileService（gate 串行 + capturing→ready 状态迁移）；
//! Cookie 层（保险库）由本服务持有。两层互补：文件层覆盖磁盘态（IndexedDB 等），
//! Cookie 层覆盖运行时 Cookie（含 Chrome 从不落盘的 session cookie）。

use std::sync::Arc;

use serde::Serialize;
use serde_json::json;

use crate::domain::environment::SessionCapture;
use crate::domain::instance::Instance;
use crate::error::AppError;
use crate::infrastructure::cdp::ws::CdpConnection;
use crate::infrastructure::db::repositories::environment_repository::EnvironmentRepository;
use crate::infrastructure::db::repositories::instance_repository::InstanceRepository;
use crate::infrastructure::paths::AppPaths;
use crate::infrastructure::session_vault::{self, SessionVault};

use super::activity::Activity;
use super::profile_service::ProfileService;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusView {
    pub environment_id: String,
    /// 回写策略（off / first-login / always）
    pub policy: SessionCapture,
    pub generation: i64,
    pub exported_at: Option<i64>,
    pub source_instance_id: Option<String>,
    pub cookie_count: usize,
    pub history: Vec<SessionGenerationView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionGenerationView {
    pub generation: i64,
    pub exported_at: i64,
    pub source_instance_id: Option<String>,
    pub cookie_count: usize,
}

pub struct SessionService {
    envs: Arc<EnvironmentRepository>,
    instances: Arc<InstanceRepository>,
    profiles: Arc<ProfileService>,
    paths: AppPaths,
    activity: Arc<Activity>,
    /// 保险库写互斥（P1）：三条停止路径（stop/stop-all/退出回调/reconciler 关窗）
    /// 并发触发 promote 时串行化 read_current→write_new 全程，保证代际单调不互覆。
    /// 进程内锁足够：单实例应用，vault 文件无跨进程写者
    vault_lock: Arc<tokio::sync::Mutex<()>>,
}

impl SessionService {
    pub fn new(
        envs: Arc<EnvironmentRepository>,
        instances: Arc<InstanceRepository>,
        profiles: Arc<ProfileService>,
        paths: AppPaths,
        activity: Arc<Activity>,
    ) -> Self {
        SessionService {
            envs,
            instances,
            profiles,
            paths,
            activity,
            vault_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// 停止路径挂钩 A：terminate 之前调用（此时 CDP 仍可达），导出实例私有 Cookie。
    /// 尽力而为：任何失败仅告警，绝不阻塞停止流程。
    pub async fn on_instance_stopping(&self, ins: &Instance) {
        let Some(port) = ins.cdp_port else { return };
        match self
            .export_instance_cookies(port, &ins.environment_id, &ins.id)
            .await
        {
            Ok(n) if n > 0 => {
                tracing::info!("[session] 实例 {} 导出 {} 条 Cookie", ins.id, n);
                self.activity.record(
                    "info",
                    "session_cookies_exported",
                    Some(ins.environment_id.as_str()),
                    Some(ins.id.as_str()),
                    &format!("{n} 条"),
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    "[session] 实例 {} Cookie 导出失败（忽略，继续停止）: {e}",
                    ins.id
                );
                // 旧导出文件（上一轮停止遗留）必须删除：promote/显式捕获按文件
                // 存在性判定，残留会让过期 Cookie 被当成本轮新代提升
                let stale = self.paths.instance_cookies(&ins.environment_id, &ins.id);
                match std::fs::remove_file(&stale) {
                    Ok(()) => tracing::info!("[session] 已删除过期导出: {}", stale.display()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        tracing::warn!("[session] 删除过期导出失败: {} - {e}", stale.display())
                    }
                }
            }
        }
    }

    /// 停止路径挂钩 B：实例已置 stopped 后调用，按策略升级身份层。
    /// 自动路径失败仅告警（首登引导不应因一次失败打断停止流程的返回）。
    pub async fn on_instance_stopped(&self, env_id: &str, ins_id: &str) {
        if let Err(e) = self.promote_by_policy(env_id, ins_id).await {
            tracing::warn!("[session] 实例 {} 身份层升级失败（忽略）: {e}", ins_id);
            self.activity.record(
                "warn",
                "session_promote_failed",
                Some(env_id),
                Some(ins_id),
                &e.to_string(),
            );
        }
    }

    async fn promote_by_policy(&self, env_id: &str, ins_id: &str) -> Result<(), AppError> {
        let env_dir = self.paths.environment_dir(env_id);
        let env = self.envs.get(env_id)?;
        match env.session_capture {
            SessionCapture::Off => return Ok(()),
            SessionCapture::FirstLogin | SessionCapture::Always => {}
        }

        // 全程持锁（M1）：FirstLogin 判空与写入同窗口——此前判空在锁外，
        // 并发双 promote（两实例同时首登退出）都判空通过、同一身份连升两代
        let _vault_guard = self.vault_lock.lock().await;
        if env.session_capture == SessionCapture::FirstLogin {
            // 身份层已有有效内容 → 不再自动更新（防登出测试/并行实例污染）。
            // 两层任一有内容即视为已有身份（复审 P2-2）：仅查 Cookie 层时，
            // 「文件层有登录、vault 为空」的环境会被一个零操作实例的停止
            // 占用首登槽位（其导出的持久 Cookie 来自克隆的文件层），
            // 之后的真实首登不再自动捕获
            let vault_has = session_vault::read_current(&env_dir)
                .map(|v| !v.cookies.is_empty())
                .unwrap_or(false);
            let file_has = self.profiles.has_ready_snapshot(env_id)?;
            if vault_has || file_has {
                return Ok(());
            }
        }

        let Some(export) =
            session_vault::read_instance_export(&self.paths.instance_cookies(env_id, ins_id))
        else {
            return Ok(()); // 无导出（崩溃路径/导出失败）：跳过，不算错误
        };
        if export.cookies.is_empty() {
            return Ok(()); // 空 Cookie 不升级（避免把空身份写进保险库）
        }

        self.capture_inner_locked(env_id, ins_id, &export.cookies)
            .await?;
        self.activity.record(
            "info",
            "session_captured_from_instance",
            Some(env_id),
            Some(ins_id),
            if env.session_capture == SessionCapture::Always {
                "策略 always"
            } else {
                "首登引导"
            },
        );
        Ok(())
    }

    /// 显式从已停止实例捕获身份（四入口的捕获动作）。
    /// 实例须属于该环境且已停止（文件层快照要求进程退出）。
    pub async fn capture_from_instance(
        &self,
        env_id: &str,
        ins_id: &str,
    ) -> Result<serde_json::Value, AppError> {
        let ins = self.instances.get(ins_id)?;
        if ins.environment_id != env_id {
            return Err(AppError::not_found(
                "INSTANCE_NOT_FOUND",
                "该实例不属于此环境",
            ));
        }
        if ins.status.is_alive_status() {
            return Err(AppError::conflict(
                "INSTANCE_ALREADY_RUNNING",
                "实例运行中，请先停止再捕获（文件层快照要求进程退出）",
            ));
        }
        let export =
            session_vault::read_instance_export(&self.paths.instance_cookies(env_id, ins_id))
                .ok_or_else(|| {
                    AppError::conflict(
                        "SESSION_CAPTURE_FAILED",
                        "该实例没有已导出的 Cookie（可能异常退出）；先正常停止实例后再捕获",
                    )
                })?;
        if export.cookies.is_empty() {
            return Err(AppError::conflict(
                "SESSION_CAPTURE_FAILED",
                "该实例导出的 Cookie 为空，无法构成有效登录态",
            ));
        }
        let vault = self.capture_inner(env_id, ins_id, &export.cookies).await?;
        Ok(json!({
            "environmentId": env_id,
            "instanceId": ins_id,
            "generation": vault.generation,
            "cookieCount": vault.cookies.len(),
        }))
    }

    /// 两层固化的公共内核：文件层（ProfileService，gate 串行）→ Cookie 层（保险库）
    /// 捕获入口：拿 vault 锁（显式捕获/restore 等锁外调用方使用）。
    /// 已持锁的 promote_by_policy 直接走 [`Self::capture_inner_locked`]
    /// （tokio Mutex 不可重入，重复拿锁会死锁）
    async fn capture_inner(
        &self,
        env_id: &str,
        ins_id: &str,
        cookies: &[session_vault::CookieRecord],
    ) -> Result<SessionVault, AppError> {
        let _vault_guard = self.vault_lock.lock().await;
        self.capture_inner_locked(env_id, ins_id, cookies).await
    }

    /// 锁内实现：调用方保证已持有 vault_lock
    async fn capture_inner_locked(
        &self,
        env_id: &str,
        ins_id: &str,
        cookies: &[session_vault::CookieRecord],
    ) -> Result<SessionVault, AppError> {
        let ins = self.instances.get(ins_id)?;
        let source_dir = std::path::PathBuf::from(&ins.profile_dir);

        // 文件层：白名单固化进母本目录 + 状态机迁移
        self.profiles
            .capture_files_from(env_id, &source_dir)
            .await?;

        // Cookie 层：写保险库（旧版自动入档）。失败时文件层已成功——如实报错，
        // 身份层处于"文件已固化但 Cookie 未更新"的部分成功态，重试捕获即可收敛
        let env_dir = self.paths.environment_dir(env_id);
        session_vault::write_new(&env_dir, cookies.to_vec(), Some(ins_id.to_string()))
    }

    /// GET /environments/:id/session：身份层状态（当前代 + 历史 + 策略）
    pub fn status(&self, env_id: &str) -> Result<SessionStatusView, AppError> {
        let env = self.envs.get(env_id)?;
        let env_dir = self.paths.environment_dir(env_id);
        let current = session_vault::read_current(&env_dir);
        let history = session_vault::list_history(&env_dir)
            .into_iter()
            .map(|v| SessionGenerationView {
                generation: v.generation,
                exported_at: v.exported_at,
                source_instance_id: v.source_instance_id,
                cookie_count: v.cookies.len(),
            })
            .collect();
        Ok(SessionStatusView {
            environment_id: env_id.to_string(),
            policy: env.session_capture,
            generation: current.as_ref().map(|v| v.generation).unwrap_or(0),
            exported_at: current.as_ref().map(|v| v.exported_at),
            source_instance_id: current.as_ref().and_then(|v| v.source_instance_id.clone()),
            cookie_count: current.as_ref().map(|v| v.cookies.len()).unwrap_or(0),
            history,
        })
    }

    /// POST /environments/:id/session/restore：回滚 Cookie 保险库到指定历史代。
    /// 边界（文档披露）：仅回滚 Cookie 层；文件层快照（IndexedDB/localStorage）
    /// 保留当前版本——多数站点的登录态以 Cookie 为主，此覆盖足够恢复典型污染。
    pub async fn restore(
        &self,
        env_id: &str,
        generation: i64,
    ) -> Result<SessionStatusView, AppError> {
        self.envs.get(env_id)?;
        let env_dir = self.paths.environment_dir(env_id);
        let _vault_guard = self.vault_lock.lock().await;
        session_vault::restore(&env_dir, generation)?;
        self.activity.record(
            "warn",
            "session_restored",
            Some(env_id),
            None,
            &format!("回滚到第 {generation} 代 Cookie 保险库"),
        );
        Ok(self.status(env_id)?)
    }

    /// 清空身份层两层：Cookie 保险库（含历史）+ 文件快照母本（回到 not_configured）。
    /// 登录浏览器运行中 → 409，不代杀（由 ProfileService.reset 保证）。
    pub async fn reset(&self, env_id: &str) -> Result<serde_json::Value, AppError> {
        // 前置可失败检查（复审 P1-1）：vault clear 不可逆，404/409 必须发生在任何
        // 变更之前——否则「报错但保险库已清」的部分成功态会让用户误以为未重置，
        // 下个实例静默空登录态启动。预检与 reset 内部检查同源，双保险
        self.envs.get(env_id)?;
        if self.profiles.has_orphan_login_browser(env_id)? {
            return Err(AppError::conflict(
                "PROFILE_IN_USE",
                "遗留登录浏览器运行中（旧版本 launch 遗留），请先关闭该浏览器窗口",
            ));
        }
        let env_dir = self.paths.environment_dir(env_id);
        // 持锁（M2）：clear 与并发 promote 的 write_new 互斥，防清空后被迟到的
        // promote 回写旧身份
        let _vault_guard = self.vault_lock.lock().await;
        session_vault::clear(&env_dir)?;
        self.activity.record(
            "warn",
            "session_reset",
            Some(env_id),
            None,
            "身份层已清空（文件快照 + Cookie 保险库）",
        );
        Ok(json!(self.profiles.reset(env_id).await?))
    }

    async fn export_instance_cookies(
        &self,
        port: u16,
        env_id: &str,
        ins_id: &str,
    ) -> Result<usize, AppError> {
        let conn = CdpConnection::connect(port).await?;
        let result: serde_json::Value = conn.send("Storage.getCookies", json!({}), None).await?;
        let cookies = session_vault::parse_cdp_cookies(&result);
        let path = self.paths.instance_cookies(env_id, ins_id);
        session_vault::write_instance_export(&path, ins_id, &cookies)?;
        Ok(cookies.len())
    }
}
