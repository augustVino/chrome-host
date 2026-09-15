use serde::Serialize;

/// Instance 生命周期状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceStatus {
    Created,
    Starting,
    Running,
    Stopping,
    Stopped,
    Error,
    Crashed,
}

impl InstanceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            InstanceStatus::Created => "created",
            InstanceStatus::Starting => "starting",
            InstanceStatus::Running => "running",
            InstanceStatus::Stopping => "stopping",
            InstanceStatus::Stopped => "stopped",
            InstanceStatus::Error => "error",
            InstanceStatus::Crashed => "crashed",
        }
    }

    pub fn from_db(value: &str) -> Self {
        match value {
            "created" => InstanceStatus::Created,
            "starting" => InstanceStatus::Starting,
            "running" => InstanceStatus::Running,
            "stopping" => InstanceStatus::Stopping,
            "stopped" => InstanceStatus::Stopped,
            "error" => InstanceStatus::Error,
            "crashed" => InstanceStatus::Crashed,
            _ => InstanceStatus::Error,
        }
    }

    pub fn is_alive_status(&self) -> bool {
        matches!(self, InstanceStatus::Running | InstanceStatus::Starting)
    }
}

/// Instance。DB 存配置与最近状态，实时状态以对账为准。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub id: String,
    pub environment_id: String,
    pub login_profile_id: Option<String>,
    pub profile_dir: String,
    pub pid: Option<u32>,
    pub cdp_port: Option<u16>,
    pub status: InstanceStatus,
    /// 启动时固化的 hosts 映射快照 JSON
    pub host_rules: Option<String>,
    pub browser_version: Option<String>,
    pub started_at: Option<i64>,
    pub stopped_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 运行时字段更新（动态 SQL，仅更新提供的字段）
#[derive(Debug, Default, Clone)]
pub struct RuntimeUpdate {
    pub pid: Option<u32>,
    pub status: Option<InstanceStatus>,
    pub started_at: Option<i64>,
    pub stopped_at: Option<i64>,
    pub browser_version: Option<String>,
    /// hosts 映射快照 JSON
    pub host_rules: Option<String>,
}
