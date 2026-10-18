use serde::{Deserialize, Serialize};

/// 实例登录态回写环境身份层的策略（session_capture）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SessionCapture {
    /// 全手动：实例关闭不回写身份层（跑登出测试/多账号轮换的环境）
    Off,
    /// 默认：身份层为空时，第一个带有效 Cookie 关闭的实例自动升级为身份
    #[default]
    FirstLogin,
    /// 每次实例关闭都回写（单账号、单人使用、明确知道自己在干嘛的环境）
    Always,
}

impl SessionCapture {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "first-login" => Some(Self::FirstLogin),
            "always" => Some(Self::Always),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::FirstLogin => "first-login",
            Self::Always => "always",
        }
    }
    /// DB 脏数据兼容：未知值回退默认档
    pub fn from_db(s: &str) -> Self {
        Self::parse(s).unwrap_or_default()
    }
}

/// Environment
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    pub id: String,
    pub name: String,
    /// 动态 hosts 配置源（hosts 格式文本的 URL），可空
    pub hosts_source_url: Option<String>,
    pub icon: Option<String>,
    /// 附加启动参数（高级逃生口）
    pub startup_args: Option<String>,
    pub keep_alive: bool,
    /// 登录态回写策略（见 SessionCapture）
    pub session_capture: SessionCapture,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateEnvironmentInput {
    pub name: String,
    #[serde(default)]
    pub hosts_source_url: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    /// 登录态回写策略（off / first-login / always；缺省 first-login）。
    /// 以字符串接收、服务层白名单校验，保证错误进统一 ErrorBody 而非 axum 反序列化 422
    #[serde(default)]
    pub session_capture: Option<String>,
}

/// 支持 `"x" | null`（缺省不参与更新；null 显式置空）
macro_rules! deserialize_nullable_string {
    () => {
        fn deserialize_nullable_string<'de, D>(
            deserializer: D,
        ) -> Result<Option<Option<String>>, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let v: Option<String> = Option::deserialize(deserializer)?;
            Ok(Some(v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())))
        }
    };
}
deserialize_nullable_string!();

/// PATCH 部分更新输入：只更新传入字段；`Some(None)` = 显式置空
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpdateEnvironmentInput {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub hosts_source_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub icon: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub startup_args: Option<Option<String>>,
    #[serde(default)]
    pub keep_alive: Option<bool>,
    #[serde(default)]
    pub session_capture: Option<String>,
}
