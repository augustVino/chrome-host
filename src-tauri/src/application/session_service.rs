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
}

impl SessionService {
    pub fn new(
        envs: Arc<EnvironmentRepository>,
        instances: Arc<InstanceRepository>,
        profiles: Arc<ProfileService>,
        paths: AppPaths,
        activity: Arc<Activity>,
    ) -> Self {
        SessionService { envs, instances, profiles, paths, activity }
    }

    /// 停止路径挂钩 A：terminate 之前调用（此时 CDP 仍可达），导出实例私有 Cookie。
    /// 尽力而为：任何失败仅告警，绝不阻塞停止流程。
    pub async fn on_instance_stopping(&self, ins: &Instance) {
        let Some(port) = ins.cdp_port else { return };
        match self.export_instance_cookies(port, &ins.environment_id, &ins.id).await {
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
                tracing::warn!("[session] 实例 {} Cookie 导出失败（忽略，继续停止）: {e}", ins.id)
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
            SessionCapture::FirstLogin => {
                // 身份层已有有效内容 → 不再自动更新（防登出测试/并行实例污染）
                let has = session_vault::read_current(&env_dir)
                    .map(|v| !v.cookies.is_empty())
                    .unwrap_or(false);
                if has {
                    return Ok(());
                }
            }
            SessionCapture::Always => {}
        }

        let Some(export) = session_vault::read_instance_export(
            &self.paths.instance_cookies(env_id, ins_id),
        ) else {
            return Ok(()); // 无导出（崩溃路径/导出失败）：跳过，不算错误
        };
        if export.cookies.is_empty() {
            return Ok(()); // 空 Cookie 不升级（避免把空身份写进保险库）
        }

        self.capture_inner(env_id, ins_id, &export.cookies).await?;
        self.activity.record(
            "info",
            "session_captured_from_instance",
            Some(env_id),
            Some(ins_id),
            if env.session_capture == SessionCapture::Always { "策略 always" } else { "首登引导" },
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
            return Err(AppError::not_found("INSTANCE_NOT_FOUND", "该实例不属于此环境"));
        }
        if ins.status.is_alive_status() {
            return Err(AppError::conflict(
                "INSTANCE_ALREADY_RUNNING",
                "实例运行中，请先停止再捕获（文件层快照要求进程退出）",
            ));
        }
        let export = session_vault::read_instance_export(
            &self.paths.instance_cookies(env_id, ins_id),
        )
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
    async fn capture_inner(
        &self,
        env_id: &str,
        ins_id: &str,
        cookies: &[session_vault::CookieRecord],
    ) -> Result<SessionVault, AppError> {
        let ins = self.instances.get(ins_id)?;
        let source_dir = std::path::PathBuf::from(&ins.profile_dir);

        // 文件层：白名单固化进母本目录 + 状态机迁移
        self.profiles.capture_files_from(env_id, &source_dir).await?;

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
    pub async fn restore(&self, env_id: &str, generation: i64) -> Result<SessionStatusView, AppError> {
        self.envs.get(env_id)?;
        let env_dir = self.paths.environment_dir(env_id);
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

    async fn export_instance_cookies(
        &self,
        port: u16,
        env_id: &str,
        ins_id: &str,
    ) -> Result<usize, AppError> {
        let mut conn = CdpConnection::connect(port).await?;
        let result: serde_json::Value =
            conn.send("Storage.getCookies", json!({}), None).await?;
        let cookies = session_vault::parse_cdp_cookies(&result);
        let path = self.paths.instance_cookies(env_id, ins_id);
        session_vault::write_instance_export(&path, ins_id, &cookies)?;
        Ok(cookies.len())
    }
}
