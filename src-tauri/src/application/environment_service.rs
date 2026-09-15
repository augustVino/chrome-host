use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use uuid::Uuid;

use crate::domain::environment::{CreateEnvironmentInput, Environment, UpdateEnvironmentInput};
use crate::domain::hosts;
use crate::error::AppError;
use crate::infrastructure::db::repositories::environment_repository::EnvironmentRepository;
use crate::infrastructure::db::repositories::instance_repository::InstanceRepository;

use super::activity::Activity;
use super::keep_alive::KeepAliveWatcher;
use super::profile_service::ProfileService;
use super::reconciler::Reconciler;

/// 环境列表视图：附带对账后的运行时摘要（运行摘要）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentSummary {
    #[serde(flatten)]
    pub environment: Environment,
    pub running_instances: i64,
    pub total_instances: i64,
}

pub struct EnvironmentService {
    envs: Arc<EnvironmentRepository>,
    instances: Arc<InstanceRepository>,
    reconciler: Arc<Reconciler>,
    profiles: Arc<ProfileService>,
    activity: Arc<Activity>,
    keep_alive: Arc<KeepAliveWatcher>,
}

impl EnvironmentService {
    pub fn new(
        envs: Arc<EnvironmentRepository>,
        instances: Arc<InstanceRepository>,
        reconciler: Arc<Reconciler>,
        profiles: Arc<ProfileService>,
        activity: Arc<Activity>,
        keep_alive: Arc<KeepAliveWatcher>,
    ) -> Self {
        EnvironmentService { envs, instances, reconciler, profiles, activity, keep_alive }
    }

    pub fn create(&self, input: CreateEnvironmentInput) -> Result<Environment, AppError> {
        let name = input.name.trim();
        if name.is_empty() {
            return Err(AppError::invalid_request("环境名称不能为空"));
        }
        let hosts_source_url = normalize_optional(&input.hosts_source_url);
        if let Some(url) = &hosts_source_url {
            if !hosts::is_valid_source_url(url) {
                return Err(source_url_invalid(url));
            }
        }

        let now = chrono::Utc::now().timestamp_millis();
        let env = Environment {
            id: format!("env_{}", Uuid::new_v4()),
            name: name.to_string(),
            hosts_source_url,
            icon: input.icon,
            startup_args: None,
            keep_alive: false,
            created_at: now,
            updated_at: now,
        };
        self.envs.insert(&env)?;
        self.emit("environment-created", &env.id);
        tracing::info!("环境创建成功: {} ({})", env.name, env.id);
        Ok(env)
    }

    /// PATCH 部分更新。hostsSourceUrl 变更仅影响之后启动的 Instance。
    pub fn update(&self, id: &str, input: UpdateEnvironmentInput) -> Result<Environment, AppError> {
        let mut env = self.envs.get(id)?;

        if let Some(name) = input.name {
            let name = name.trim();
            if name.is_empty() {
                return Err(AppError::invalid_request("环境名称不能为空"));
            }
            env.name = name.to_string();
        }
        if let Some(hs) = input.hosts_source_url {
            if let Some(url) = &hs {
                if !hosts::is_valid_source_url(url) {
                    return Err(source_url_invalid(url));
                }
            }
            env.hosts_source_url = hs;
        }
        if let Some(icon) = input.icon {
            env.icon = icon;
        }
        if let Some(args) = input.startup_args {
            env.startup_args = args;
        }
        if let Some(on) = input.keep_alive {
            env.keep_alive = on;
        }
        env.updated_at = chrono::Utc::now().timestamp_millis();

        self.envs.update(&env)?;

        // keepAlive 开关联动：开 → 为运行中实例注册监控；关 → 全部取消
        let running: Vec<String> = self
            .instances
            .list_by_env(id)?
            .into_iter()
            .filter(|i| i.status.is_alive_status())
            .map(|i| i.id)
            .collect();
        if env.keep_alive {
            for ins_id in &running {
                self.keep_alive.register(ins_id);
            }
        } else {
            self.keep_alive.cancel_all(&running);
        }

        self.emit("environment-updated", id);
        Ok(env)
    }

    pub fn get(&self, id: &str) -> Result<Environment, AppError> {
        self.envs.get(id)
    }

    /// 列表 + 对账后的运行时摘要
    pub async fn list_summary(&self) -> Result<Vec<EnvironmentSummary>, AppError> {
        let envs = self.envs.list()?;
        let mut counts: HashMap<String, (i64, i64)> = HashMap::new();
        for ins in self.instances.list_all()? {
            let ins = self.reconciler.reconcile(&ins).await;
            let entry = counts.entry(ins.environment_id.clone()).or_insert((0, 0));
            entry.1 += 1;
            if ins.status.is_alive_status() {
                entry.0 += 1;
            }
        }
        Ok(envs
            .into_iter()
            .map(|environment| {
                let (running_instances, total_instances) =
                    counts.get(&environment.id).copied().unwrap_or((0, 0));
                EnvironmentSummary { environment, running_instances, total_instances }
            })
            .collect())
    }

    /// 删除：有运行中实例 → 409；否则级联删除实例记录与 profile 目录。
    pub fn delete(&self, id: &str) -> Result<(), AppError> {
        self.envs.get(id)?;

        if self.instances.count_running_by_env(id)? > 0 {
            return Err(AppError::conflict(
                "ENVIRONMENT_HAS_RUNNING_INSTANCES",
                "环境仍有运行中的实例，请先停止",
            ));
        }

        // 级联清理：先删 profile 目录（best effort），再删记录
        for ins in self.instances.list_by_env(id)? {
            let dir = std::path::Path::new(&ins.profile_dir);
            if let Err(e) = std::fs::remove_dir_all(dir) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("清理实例 profile 失败（继续）: {} - {e}", ins.profile_dir);
                }
            }
        }
        self.instances.delete_by_env(id)?;
        // 级联清理登录 profile（浏览器运行中 → 409；否则清目录 + 删行）
        self.profiles.delete_for_env(id)?;

        self.envs.delete(id)?;
        self.emit("environment-deleted", id);
        tracing::info!("环境已删除: {id}");
        Ok(())
    }

    fn emit(&self, event: &str, id: &str) {
        self.activity.record("info", event, Some(id), Some(id), "");
    }
}

fn normalize_optional(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn source_url_invalid(url: &str) -> AppError {
    AppError::business(
        400,
        "HOSTS_SOURCE_INVALID",
        format!(
            "hostsSourceUrl 必须为 http(s):// 开头的合法 URL，实际: {url}"
        ),
    )
}
