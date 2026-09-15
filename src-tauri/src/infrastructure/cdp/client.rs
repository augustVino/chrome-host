//! CDP HTTP 客户端：MVP 仅用 Chrome HTTP 端点，不引入 WebSocket 客户端。

use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde::Deserialize;

use crate::error::AppError;

static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    // no_proxy：CDP 目标是本机回环地址，不能被终端代理环境变量劫持
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(1500))
        .build()
        .expect("构建 CDP HTTP 客户端失败")
});

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    pub browser: String,
    /// /json/version 的 webSocketDebuggerUrl，透传给 Agent
    pub web_socket_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawVersion {
    #[serde(rename = "Browser")]
    browser: Option<String>,
    #[serde(rename = "webSocketDebuggerUrl")]
    web_socket_debugger_url: Option<String>,
}

/// 探测版本；不可达时返回错误（1.5s 超时）
pub async fn version(port: u16) -> Result<VersionInfo, AppError> {
    let resp = CLIENT
        .get(format!("http://127.0.0.1:{port}/json/version"))
        .send()
        .await?
        .error_for_status()?;
    let raw: RawVersion = resp.json().await?;
    Ok(VersionInfo {
        browser: raw.browser.unwrap_or_default(),
        web_socket_url: raw.web_socket_debugger_url,
    })
}

/// Target / Tab
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub id: String,
    #[serde(rename = "type")]
    pub target_type: String,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Deserialize)]
struct RawTarget {
    id: String,
    #[serde(rename = "type")]
    target_type: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
}

/// GET /json/list
pub async fn list_targets(port: u16) -> Result<Vec<Target>, AppError> {
    let resp = CLIENT
        .get(format!("http://127.0.0.1:{port}/json/list"))
        .send()
        .await?
        .error_for_status()
        .map_err(|e| {
            AppError::business(500, "CDP_CONNECTION_FAILED", format!("CDP 目标列表获取失败: {e}"))
        })?;
    let raw: Vec<RawTarget> = resp.json().await?;
    Ok(raw
        .into_iter()
        .map(|t| Target { id: t.id, target_type: t.target_type, title: t.title, url: t.url })
        .collect())
}

/// PUT /json/activate/{targetId}：把对应标签页/窗口带到前台
pub async fn activate_target(port: u16, target_id: &str) -> Result<bool, AppError> {
    let resp = CLIENT
        .put(format!("http://127.0.0.1:{port}/json/activate/{target_id}"))
        .send()
        .await?;
    Ok(resp.status().is_success())
}

/// PUT /json/new?url=：新建标签页。
/// Chrome 111+ 起该端点必须为 PUT（GET 被 CDP 拒绝）；CfT pinned 131 行为确定
pub async fn new_target(port: u16, url: &str) -> Result<Target, AppError> {
    let resp = CLIENT
        .put(format!("http://127.0.0.1:{port}/json/new"))
        .query(&[("url", url)])
        .send()
        .await?
        .error_for_status()
        .map_err(|e| {
            AppError::business(500, "CDP_CONNECTION_FAILED", format!("新建标签页失败: {e}"))
        })?;
    let raw: RawTarget = resp.json().await?;
    Ok(Target { id: raw.id, target_type: raw.target_type, title: raw.title, url: raw.url })
}

/// GET /json/close/{targetId}：关闭标签页。目标可能已被用户关闭（非错误），返回是否成功
pub async fn close_target(port: u16, target_id: &str) -> Result<bool, AppError> {
    let resp = CLIENT
        .get(format!("http://127.0.0.1:{port}/json/close/{target_id}"))
        .send()
        .await?;
    Ok(resp.status().is_success())
}

/// 轮询直到 CDP 就绪或超时
pub async fn wait_until_ready(port: u16, timeout: Duration) -> Result<VersionInfo, AppError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(v) = version(port).await {
            return Ok(v);
        }
        if Instant::now() >= deadline {
            return Err(AppError::business(
                500,
                "CDP_NOT_READY",
                format!("CDP 端口 {port} 在超时时间内未就绪"),
            ));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}
