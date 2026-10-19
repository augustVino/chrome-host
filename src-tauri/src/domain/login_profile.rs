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
    pub created_at: i64,
    pub updated_at: i64,
}

impl LoginProfile {
    /// 文件层身份是否有效：ready 且至少固化过一次。
    /// 身份层两层语义（文件快照 / Cookie 保险库）的文件侧判定——first-login
    /// 判空等场景要求两层任一有效即视为「已有身份」
    pub fn has_effective_snapshot(&self) -> bool {
        self.status == LoginProfileStatus::Ready && self.snapshot_version > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(status: LoginProfileStatus, version: i64) -> LoginProfile {
        LoginProfile {
            id: "lp_x".into(),
            environment_id: "env_x".into(),
            name: "X".into(),
            profile_dir: "/tmp/x".into(),
            snapshot_version: version,
            status,
            last_captured_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    /// 「已有身份」的文件层判定：ready 且 v>0 才算——ready(v0) 是首固中崩溃的
    /// 恢复残留（recover_stale_capturing 会归位 not_configured，但判定自身必须
    /// 防御），capturing/error 一律不算有效身份
    #[test]
    fn effective_snapshot_requires_ready_and_version() {
        assert!(row(LoginProfileStatus::Ready, 1).has_effective_snapshot());
        assert!(!row(LoginProfileStatus::Ready, 0).has_effective_snapshot(), "v0 无快照内容");
        assert!(!row(LoginProfileStatus::NotConfigured, 3).has_effective_snapshot());
        assert!(!row(LoginProfileStatus::Capturing, 3).has_effective_snapshot());
        assert!(!row(LoginProfileStatus::Error, 3).has_effective_snapshot());
    }
}
