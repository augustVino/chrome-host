//! 应用设置服务：Developer Mode 等开关，存 manager.db app_settings 表。

use rusqlite::params;
use serde::Serialize;

use crate::error::AppError;
use crate::infrastructure::db::client::DbPool;

const KEY_DEVELOPER_MODE: &str = "developer_mode";
const KEY_ENV_LABEL_POSITION: &str = "env_label_position";
const KEY_ENV_LABEL_COLOR: &str = "env_label_color";
const KEY_DEFAULT_START_URL: &str = "default_start_url";
const KEY_REMOTE_ACCESS_ENABLED: &str = "remote_access_enabled";
const KEY_REMOTE_ACCESS_SSH_TARGET: &str = "remote_access_ssh_target";
const KEY_REMOTE_ACCESS_TOKEN: &str = "remote_access_token";

/// SSH 目标最大长度（user@host / ~/.ssh/config 别名）
const SSH_TARGET_MAX_LEN: usize = 255;

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
    /// 远程接入开关（开启 = 应用守护一条 SSH 反向隧道，仅转发 17890）
    pub remote_access_enabled: bool,
    /// SSH 目标（user@host 或 ~/.ssh/config 别名；enabled=true 时恒非空）
    pub remote_access_ssh_target: String,
    /// 远程访问令牌（首次开启自动生成；17891 监听器 Bearer 校验基准）
    pub remote_access_token: String,
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
            remote_access_enabled: self.get_bool(KEY_REMOTE_ACCESS_ENABLED)?,
            remote_access_ssh_target: self
                .get_str_with_default(KEY_REMOTE_ACCESS_SSH_TARGET, "")?,
            remote_access_token: self.get_str_with_default(KEY_REMOTE_ACCESS_TOKEN, "")?,
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

    /// PUT 部分更新：只更新传入字段；枚举值白名单校验，URL 非空时须 http(s)（非法 → 400）。
    /// 远程接入不变量：`enabled = true ⇒ target 非空`（开启前置校验 + 已开启时禁置空）；
    /// 首次开启自动生成令牌（幂等，已有则不覆盖）。
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &self,
        developer_mode: Option<bool>,
        env_label_position: Option<String>,
        env_label_color: Option<String>,
        default_start_url: Option<String>,
        remote_access_enabled: Option<bool>,
        remote_access_ssh_target: Option<String>,
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
        // ---- 远程接入：前置校验 → 写入（顺序保证失败时不落半套状态）----
        let incoming_target = remote_access_ssh_target
            .as_deref()
            .map(validate_ssh_target)
            .transpose()?;
        // 本次开启（Some(true)），或保持开启状态下的任何更新：目标终值必须非空。
        // 终值语义：显式提交（含置空）即终值；未提交才回落库中值——
        // 否则「已开启时置空目标」会被旧值掩盖而绕过不变量
        let enabled_after = remote_access_enabled
            .unwrap_or_else(|| self.get_bool(KEY_REMOTE_ACCESS_ENABLED).unwrap_or(false));
        if remote_access_enabled == Some(true) || enabled_after {
            let final_target = match incoming_target.as_deref() {
                Some(t) => t.to_string(),
                None => self
                    .get_str_with_default(KEY_REMOTE_ACCESS_SSH_TARGET, "")
                    .unwrap_or_default(),
            };
            if final_target.is_empty() {
                return Err(AppError::business(
                    400,
                    "REMOTE_ACCESS_TARGET_INVALID",
                    "开启远程接入前必须配置 SSH 目标（user@host 或 ~/.ssh/config 别名），已开启时不可置空",
                ));
            }
        }
        if let Some(target) = incoming_target {
            self.set_str(KEY_REMOTE_ACCESS_SSH_TARGET, &target)?;
        }
        if let Some(on) = remote_access_enabled {
            self.set_bool(KEY_REMOTE_ACCESS_ENABLED, on)?;
            if on {
                // 首次开启惰性生成令牌（uuid v4，122bit 熵；不引新依赖）
                if self
                    .get_str_with_default(KEY_REMOTE_ACCESS_TOKEN, "")?
                    .is_empty()
                {
                    self.set_str(KEY_REMOTE_ACCESS_TOKEN, &uuid::Uuid::new_v4().to_string())?;
                }
            }
        }
        self.get()
    }

    /// 轮换远程访问令牌（旧令牌立即失效——比对基准换新）。
    /// 调用点：POST /api/v1/remote-access/token（Settings 页轮换按钮）。
    pub fn rotate_remote_token(&self) -> Result<String, AppError> {
        let token = uuid::Uuid::new_v4().to_string();
        self.set_str(KEY_REMOTE_ACCESS_TOKEN, &token)?;
        Ok(token)
    }

    /// 读取远程访问令牌（17891 auth 中间件的比对基准；空 = 未配置 → 一律拒绝）
    pub fn remote_token(&self) -> Result<String, AppError> {
        self.get_str_with_default(KEY_REMOTE_ACCESS_TOKEN, "")
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

/// 校验 SSH 目标：空合法（= 未配置，置空受 update 不变量约束）；非空时
/// 不可以 - 开头（防被解析为 ssh 选项）、不可含空白（防多参数注入）、长度 ≤ 255。
fn validate_ssh_target(value: &str) -> Result<String, AppError> {
    let v = value.trim();
    if v.is_empty() {
        return Ok(String::new());
    }
    if v.len() > SSH_TARGET_MAX_LEN {
        return Err(AppError::business(
            400,
            "REMOTE_ACCESS_TARGET_INVALID",
            format!("SSH 目标过长（>{SSH_TARGET_MAX_LEN} 字符）"),
        ));
    }
    if v.starts_with('-') || v.chars().any(char::is_whitespace) {
        return Err(AppError::business(
            400,
            "REMOTE_ACCESS_TARGET_INVALID",
            "SSH 目标不能以 - 开头或包含空白字符（形如 user@host 或 ~/.ssh/config 别名）",
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
        s.update(Some(true), None, None, None, None, None).unwrap();
        assert!(s.get().unwrap().developer_mode);
        s.update(Some(false), None, None, None, None, None).unwrap();
        assert!(!s.get().unwrap().developer_mode);
    }

    /// 环境标签位置/颜色：默认值 + 合法值写入 + 白名单拒绝
    #[test]
    fn env_label_position_and_color() {
        let s = setup();
        let view = s.get().unwrap();
        assert_eq!(view.env_label_position, "bottom-left", "默认位置");
        assert_eq!(view.env_label_color, "red", "默认颜色");

        s.update(None, Some("top-right".into()), Some("purple".into()), None, None, None).unwrap();
        let view = s.get().unwrap();
        assert_eq!(view.env_label_position, "top-right");
        assert_eq!(view.env_label_color, "purple");

        let err = s.update(None, Some("middle".into()), None, None, None, None).unwrap_err();
        assert_eq!(err.status(), 400, "非法位置 → 400");
        let err = s.update(None, None, Some("pink".into()), None, None, None).unwrap_err();
        assert_eq!(err.status(), 400, "非法颜色 → 400");
    }

    /// 默认起始页：默认空 + 写入/清除 roundtrip + URL 校验 + 启动页解析
    #[test]
    fn default_start_url_and_resolve() {
        let s = setup();
        assert_eq!(s.get().unwrap().default_start_url, "", "默认为空");
        assert_eq!(s.resolve_start_url().unwrap(), FALLBACK_START_PAGE, "未配置 → about:blank");

        s.update(None, None, None, Some("https://home.example.com".into()), None, None).unwrap();
        assert_eq!(s.get().unwrap().default_start_url, "https://home.example.com");
        assert_eq!(s.resolve_start_url().unwrap(), "https://home.example.com");

        // 空串 = 清除（回退 about:blank）；非法 URL → 400；校验失败不清空已有配置
        s.update(None, None, None, Some("".into()), None, None).unwrap();
        assert_eq!(s.get().unwrap().default_start_url, "");
        s.update(None, None, None, Some("https://a.example.com".into()), None, None).unwrap();
        let err = s.update(None, None, None, Some("not-a-url".into()), None, None).unwrap_err();
        assert_eq!(err.status(), 400);
        assert_eq!(s.get().unwrap().default_start_url, "https://a.example.com");
    }

    /// 远程接入设置生命周期：默认关/空 → 开启前置校验 → 令牌自动生成/保留/轮换 → 不变量
    #[test]
    fn remote_access_settings_lifecycle() {
        let s = setup();
        let view = s.get().unwrap();
        assert!(!view.remote_access_enabled, "默认关闭");
        assert_eq!(view.remote_access_ssh_target, "");
        assert_eq!(view.remote_access_token, "");

        // 未配目标直接开启 → 400，且 enabled 不落库
        let err = s.update(None, None, None, None, Some(true), None).unwrap_err();
        assert_eq!(err.status(), 400);
        assert_eq!(err.code(), "REMOTE_ACCESS_TARGET_INVALID");
        assert!(!s.get().unwrap().remote_access_enabled, "失败不落任何状态");

        // 非法目标（空白 / - 开头 / 超长）→ 400
        for bad in ["a b".to_string(), "-x".to_string(), "a".repeat(256)] {
            let err = s.update(None, None, None, None, None, Some(bad)).unwrap_err();
            assert_eq!(err.status(), 400, "非法目标应被拒绝");
        }

        // 先配目标（未开启）再开启：令牌自动生成
        s.update(None, None, None, None, None, Some("vino@yun".into())).unwrap();
        s.update(None, None, None, None, Some(true), None).unwrap();
        let view = s.get().unwrap();
        assert!(view.remote_access_enabled);
        assert_eq!(view.remote_access_ssh_target, "vino@yun");
        assert!(!view.remote_access_token.is_empty(), "开启时自动生成令牌");

        // 已开启时置空目标 → 拒绝（不变量保护）
        let err = s.update(None, None, None, None, None, Some("".into())).unwrap_err();
        assert_eq!(err.status(), 400);

        // 同一次调用内 target + enable：合法（先校验终值再写入）
        s.update(None, None, None, None, Some(true), Some("other@host".into())).unwrap();
        assert_eq!(s.get().unwrap().remote_access_ssh_target, "other@host");

        // 关闭后允许清空目标；令牌保留，再次开启不换发
        s.update(None, None, None, None, Some(false), None).unwrap();
        s.update(None, None, None, None, None, Some("".into())).unwrap();
        let token_before = s.get().unwrap().remote_access_token;
        assert!(!token_before.is_empty(), "关闭不清令牌");
        s.update(None, None, None, None, Some(true), Some("vino@yun".into())).unwrap();
        assert_eq!(s.get().unwrap().remote_access_token, token_before, "再次开启不换发");

        // 轮换换发，remote_token 同步
        let rotated = s.rotate_remote_token().unwrap();
        assert_ne!(rotated, token_before);
        assert_eq!(s.remote_token().unwrap(), rotated);
    }

}
