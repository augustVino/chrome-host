use serde::{Deserialize, Serialize};

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
}
