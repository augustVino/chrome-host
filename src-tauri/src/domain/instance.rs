use serde::Serialize;

use crate::error::AppError;

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

/// 实例名长度上限（按字符数计，CJK 友好；DB 无长度约束，校验在写入前完成）
pub const NAME_MAX_CHARS: usize = 64;

/// 创建入参 name 的规范化与校验（纯函数；service 创建路径唯一消费点）。
/// 规则：None → None（不命名，不参与唯一约束）；Some → 先 trim，
/// 空/纯空白 → 400（「不命名」应省略字段而非传空白）；trim 后 1-64 字符；
/// 含控制字符 → 400（name 会进 CLI 表格与 GUI 列表展示）。
/// 返回 trim 后的规范名。
pub fn normalize_name(raw: Option<&str>) -> Result<Option<String>, AppError> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AppError::invalid_request(
            "实例名不能为空；不命名请省略 name 字段",
        ));
    }
    if trimmed.chars().count() > NAME_MAX_CHARS {
        return Err(AppError::invalid_request(format!(
            "实例名最长 {NAME_MAX_CHARS} 字符（按字符计）",
        )));
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(AppError::invalid_request("实例名不允许包含控制字符"));
    }
    Ok(Some(trimmed.to_string()))
}

/// Instance。DB 存配置与最近状态，实时状态以对账为准。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub id: String,
    /// 可选实例名：同环境（environment_id）内唯一，由 DB partial unique index
    /// 兜底；CLI 引用实例时可按 name 解析。可空（NULL 不参与唯一约束）。
    pub name: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 校验规则逐条：trim、空/纯空白拒绝、长度边界、控制字符拒绝、None 透传。
    /// 表达的是「name 是给人和 CLI 看的锚点」这一约束，而非表面行为。
    #[test]
    fn normalize_name_trims_and_accepts_valid() {
        assert_eq!(normalize_name(None).unwrap(), None, "不命名 = None");
        assert_eq!(normalize_name(Some(" task-1 ")).unwrap().as_deref(), Some("task-1"));
        // CJK 按字符数计：64 个中日韩字符合法
        let cjk_max: String = "任".repeat(NAME_MAX_CHARS);
        assert_eq!(
            normalize_name(Some(&cjk_max)).unwrap().as_deref(),
            Some(cjk_max.as_str())
        );
    }

    #[test]
    fn normalize_name_rejects_blank() {
        for bad in ["", "   ", "\t\n"] {
            let err = normalize_name(Some(bad)).unwrap_err();
            assert_eq!(err.status(), 400);
            assert_eq!(err.code(), "INVALID_REQUEST", "空/纯空白: {bad:?}");
        }
    }

    #[test]
    fn normalize_name_rejects_overlength_and_control_chars() {
        let too_long = "a".repeat(NAME_MAX_CHARS + 1);
        let err = normalize_name(Some(&too_long)).unwrap_err();
        assert_eq!(err.code(), "INVALID_REQUEST");

        let err = normalize_name(Some("a\u{0}b")).unwrap_err();
        assert_eq!(err.code(), "INVALID_REQUEST", "控制字符拒绝");
    }
}
