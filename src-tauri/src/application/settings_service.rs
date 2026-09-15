//! 应用设置服务：Developer Mode 等开关，存 manager.db app_settings 表。

use rusqlite::params;
use serde::Serialize;

use crate::error::AppError;
use crate::infrastructure::db::client::DbPool;

const KEY_DEVELOPER_MODE: &str = "developer_mode";
const KEY_ENV_LABEL_POSITION: &str = "env_label_position";
const KEY_ENV_LABEL_COLOR: &str = "env_label_color";
const KEY_DEFAULT_START_URL: &str = "default_start_url";

/// 全局默认起始页均未配置时实例/登录浏览器打开的页面
pub const FALLBACK_START_PAGE: &str = "about:blank";

/// 环境标识标签允许的位置/颜色（与扩展 content.js 的映射表一致）
pub const ENV_LABEL_POSITIONS: &[&str] = &["top-left", "top-right", "bottom-left", "bottom-right"];
pub const ENV_LABEL_COLORS: &[&str] = &["red", "blue", "green", "purple"];

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppSettingsView {
    pub developer_mode: bool,
    pub env_label_position: String,
    pub env_label_color: String,
    /// 全局默认起始页（空 = 打开 about:blank）
    pub default_start_url: String,
}

pub struct SettingsService {
    pool: DbPool,
}

impl SettingsService {
    pub fn new(pool: DbPool) -> Self {
        SettingsService { pool }
    }

    fn get_str_with_default(&self, key: &str, default: &str) -> Result<String, AppError> {
        let conn = self.pool.get()?;
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(value.unwrap_or_else(|| default.to_string()))
    }

    fn set_str(&self, key: &str, value: &str) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
            params![key, value, chrono::Utc::now().timestamp_millis()],
        )?;
        Ok(())
    }

    fn get_bool(&self, key: &str) -> Result<bool, AppError> {
        let conn = self.pool.get()?;
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(value.as_deref() == Some("true"))
    }

    fn set_bool(&self, key: &str, value: bool) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
            params![key, value.to_string(), chrono::Utc::now().timestamp_millis()],
        )?;
        Ok(())
    }

    pub fn get(&self) -> Result<AppSettingsView, AppError> {
        Ok(AppSettingsView {
            developer_mode: self.get_bool(KEY_DEVELOPER_MODE)?,
            env_label_position: self
                .get_str_with_default(KEY_ENV_LABEL_POSITION, "bottom-left")?,
            env_label_color: self.get_str_with_default(KEY_ENV_LABEL_COLOR, "red")?,
            default_start_url: self.get_str_with_default(KEY_DEFAULT_START_URL, "")?,
        })
    }

    /// 解析启动页（打开实例/登录浏览器时的初始链接）：全局默认起始页（Settings 页配置）
    /// 非空则用之，否则打开 about:blank。
    /// 由实例/登录浏览器启动流程调用，每次启动现读（改配置对之后的启动生效）。
    pub fn resolve_start_url(&self) -> Result<String, AppError> {
        let url = self.get()?.default_start_url;
        Ok(if url.is_empty() {
            FALLBACK_START_PAGE.to_string()
        } else {
            url
        })
    }

    /// PUT 部分更新：只更新传入字段；枚举值白名单校验，URL 非空时须 http(s)（非法 → 400）
    pub fn update(
        &self,
        developer_mode: Option<bool>,
        env_label_position: Option<String>,
        env_label_color: Option<String>,
        default_start_url: Option<String>,
    ) -> Result<AppSettingsView, AppError> {
        if let Some(on) = developer_mode {
            self.set_bool(KEY_DEVELOPER_MODE, on)?;
        }
        if let Some(pos) = env_label_position {
            if !ENV_LABEL_POSITIONS.contains(&pos.as_str()) {
                return Err(AppError::invalid_request(format!(
                    "envLabelPosition 非法: {pos}（允许 {ENV_LABEL_POSITIONS:?}）"
                )));
            }
            self.set_str(KEY_ENV_LABEL_POSITION, &pos)?;
        }
        if let Some(color) = env_label_color {
            if !ENV_LABEL_COLORS.contains(&color.as_str()) {
                return Err(AppError::invalid_request(format!(
                    "envLabelColor 非法: {color}（允许 {ENV_LABEL_COLORS:?}）"
                )));
            }
            self.set_str(KEY_ENV_LABEL_COLOR, &color)?;
        }
        if let Some(url) = default_start_url {
            let url = validate_start_url(&url)?;
            self.set_str(KEY_DEFAULT_START_URL, &url)?;
        }
        self.get()
    }
}

/// 校验全局默认起始页：空值合法（= 未配置，启动时打开 about:blank）；非空须 http(s) URL。
fn validate_start_url(value: &str) -> Result<String, AppError> {
    let v = value.trim();
    if v.is_empty() {
        return Ok(String::new());
    }
    if !v.starts_with("http://") && !v.starts_with("https://") {
        return Err(AppError::invalid_request(
            "默认起始页必须为 http(s):// 开头的合法 URL",
        ));
    }
    Ok(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> SettingsService {
        let dir = std::env::temp_dir().join(format!("cem-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::infrastructure::db::client::init_pool(&dir.join("test.db")).unwrap();
        crate::infrastructure::db::schema::init_schema(&pool).unwrap();
        SettingsService::new(pool)
    }

    #[test]
    fn developer_mode_roundtrip() {
        let s = setup();
        assert!(!s.get().unwrap().developer_mode, "默认关闭");
        s.update(Some(true), None, None, None).unwrap();
        assert!(s.get().unwrap().developer_mode);
        s.update(Some(false), None, None, None).unwrap();
        assert!(!s.get().unwrap().developer_mode);
    }

    /// 环境标签位置/颜色：默认值 + 合法值写入 + 白名单拒绝
    #[test]
    fn env_label_position_and_color() {
        let s = setup();
        let view = s.get().unwrap();
        assert_eq!(view.env_label_position, "bottom-left", "默认位置");
        assert_eq!(view.env_label_color, "red", "默认颜色");

        s.update(None, Some("top-right".into()), Some("purple".into()), None).unwrap();
        let view = s.get().unwrap();
        assert_eq!(view.env_label_position, "top-right");
        assert_eq!(view.env_label_color, "purple");

        let err = s.update(None, Some("middle".into()), None, None).unwrap_err();
        assert_eq!(err.status(), 400, "非法位置 → 400");
        let err = s.update(None, None, Some("pink".into()), None).unwrap_err();
        assert_eq!(err.status(), 400, "非法颜色 → 400");
    }

    /// 默认起始页：默认空 + 写入/清除 roundtrip + URL 校验 + 启动页解析
    #[test]
    fn default_start_url_and_resolve() {
        let s = setup();
        assert_eq!(s.get().unwrap().default_start_url, "", "默认为空");
        assert_eq!(s.resolve_start_url().unwrap(), FALLBACK_START_PAGE, "未配置 → about:blank");

        s.update(None, None, None, Some("https://home.example.com".into())).unwrap();
        assert_eq!(s.get().unwrap().default_start_url, "https://home.example.com");
        assert_eq!(s.resolve_start_url().unwrap(), "https://home.example.com");

        // 空串 = 清除（回退 about:blank）；非法 URL → 400；校验失败不清空已有配置
        s.update(None, None, None, Some("".into())).unwrap();
        assert_eq!(s.get().unwrap().default_start_url, "");
        s.update(None, None, None, Some("https://a.example.com".into())).unwrap();
        let err = s.update(None, None, None, Some("not-a-url".into())).unwrap_err();
        assert_eq!(err.status(), 400);
        assert_eq!(s.get().unwrap().default_start_url, "https://a.example.com");
    }
}
