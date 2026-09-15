//! Extension 领域模型：一级全局资源，不属于环境/实例。
//! 两类：System（Environment Label，内置锁定：不可删/不可改路径/不可启停）
//! 与 User（用户注册，可启停/可移除注册项）。

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionType {
    System,
    User,
}

impl ExtensionType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExtensionType::System => "system",
            ExtensionType::User => "user",
        }
    }
}

/// 展示层状态：每次读取时根据文件系统现状惰性计算，不落库
/// （注册时已校验过 manifest；missing/invalid 表达"注册后源目录变动"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionStatus {
    Ready,
    Missing,
    Invalid,
}

impl ExtensionStatus {
    pub fn from_str_or_ready(s: &str) -> ExtensionStatus {
        match s {
            "missing" => ExtensionStatus::Missing,
            "invalid" => ExtensionStatus::Invalid,
            _ => ExtensionStatus::Ready,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Extension {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub manifest_version: Option<i64>,
    #[serde(rename = "type")]
    pub extension_type: ExtensionType,
    pub source_path: String,
    pub enabled: bool,
    pub status: ExtensionStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Extension {
    pub fn status_as_str(&self) -> &'static str {
        match self.status {
            ExtensionStatus::Ready => "ready",
            ExtensionStatus::Missing => "missing",
            ExtensionStatus::Invalid => "invalid",
        }
    }
}
