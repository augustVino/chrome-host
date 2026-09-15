use serde::Serialize;

/// Login Profile 快照状态（Not Configured → Ready →（Capturing）→ Ready，↘ Error）。
/// 注意：登录浏览器"运行中"不属于本状态机——它是派生运行态（pid 内存态 + 探活），
/// 不落库、不影响快照状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginProfileStatus {
    NotConfigured,
    Ready,
    Capturing,
    Error,
}

impl LoginProfileStatus {
    #[allow(dead_code)] // 与 InstanceStatus::as_str 对齐保留；序列化走 serde rename_all
    pub fn as_str(&self) -> &'static str {
        match self {
            LoginProfileStatus::NotConfigured => "not_configured",
            LoginProfileStatus::Ready => "ready",
            LoginProfileStatus::Capturing => "capturing",
            LoginProfileStatus::Error => "error",
        }
    }

    pub fn from_db(value: &str) -> Self {
        match value {
            "ready" => LoginProfileStatus::Ready,
            "capturing" => LoginProfileStatus::Capturing,
            "error" => LoginProfileStatus::Error,
            _ => LoginProfileStatus::NotConfigured,
        }
    }
}

/// Login Profile。pid 不落库——登录浏览器由 ProfileService 持内存态，
/// 应用重启后按 profile_dir 扫描 reattach（决策）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginProfile {
    pub id: String,
    pub environment_id: String,
    pub name: String,
    pub profile_dir: String,
    /// 0 = 尚无快照
    pub snapshot_version: i64,
    pub status: LoginProfileStatus,
    pub last_captured_at: Option<i64>,
    /// 登录浏览器当前占用的 CDP 端口（跨重启的端口占用凭证；退出时专用 SQL 清空）
    pub cdp_port: Option<u16>,
    pub created_at: i64,
    pub updated_at: i64,
}
