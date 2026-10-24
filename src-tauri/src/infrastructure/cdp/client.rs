//! CDP 客户端：HTTP 端点（version/list/activate/探活轮询）+ WS 短连接
//! （new/close target，复用 ws.rs 的 [`CdpConnection`]）。
//!
//! 变更类目标操作不走 HTTP 的原因：CfT pinned 131 忽略 /json/new 的 url
//! 参数（返回的 target 停在 about:blank），/json/close 亦不生效——WS 的
//! Target.createTarget/closeTarget 行为确定（cdp.mjs 同路径已验证）。

use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::json;

use super::ws::CdpConnection;
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
            AppError::business(
                500,
                "CDP_CONNECTION_FAILED",
                format!("CDP 目标列表获取失败: {e}"),
            )
        })?;
    let raw: Vec<RawTarget> = resp.json().await?;
    Ok(raw
        .into_iter()
        .map(|t| Target {
            id: t.id,
            target_type: t.target_type,
            title: t.title,
            url: t.url,
        })
        .collect())
}

/// Target.createTarget 响应
#[derive(Debug, Deserialize)]
struct CreateTargetResult {
    #[serde(rename = "targetId")]
    target_id: String,
}

/// Target.getTargetInfo 响应。字段名与 RawTarget（/json/list）不同
/// （targetId 而非 id），故独立定义
#[derive(Debug, Deserialize)]
struct GetTargetInfoResult {
    #[serde(rename = "targetInfo")]
    target_info: TargetInfo,
}

#[derive(Debug, Deserialize)]
struct TargetInfo {
    #[serde(rename = "targetId")]
    target_id: String,
    #[serde(rename = "type")]
    target_type: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
}

/// Target.closeTarget 响应
#[derive(Debug, Deserialize)]
struct CloseTargetResult {
    #[serde(default)]
    success: bool,
}

/// PUT /json/activate/{targetId}：把对应标签页/窗口带到前台
pub async fn activate_target(port: u16, target_id: &str) -> Result<bool, AppError> {
    let resp = CLIENT
        .put(format!("http://127.0.0.1:{port}/json/activate/{target_id}"))
        .send()
        .await?;
    Ok(resp.status().is_success())
}

/// WS Target.createTarget：新建标签页（browser 级短连接，命令毕即弃）。
/// createTarget 携带 url 完成导航；getTargetInfo 补全 type/title/url——
/// 刚建的 target title 为空、url 即请求 url，与旧 HTTP 版返回语义兼容
pub async fn new_target(port: u16, url: &str) -> Result<Target, AppError> {
    let conn = CdpConnection::connect(port).await?;
    let created: CreateTargetResult = conn
        .send("Target.createTarget", json!({ "url": url }), None)
        .await?;
    let info: GetTargetInfoResult = conn
        .send(
            "Target.getTargetInfo",
            json!({ "targetId": created.target_id }),
            None,
        )
        .await?;
    Ok(Target {
        id: info.target_info.target_id,
        target_type: info.target_info.target_type,
        title: info.target_info.title,
        url: info.target_info.url,
    })
}

/// WS Target.closeTarget：关闭标签页，成功返回 true。目标可能已被用户关闭：
/// Chrome 对不存在的 targetId 回协议错误，归一为 false 不报错（旧 HTTP 版
/// 对非 2xx 同样返回 false 的宽容语义）
pub async fn close_target(port: u16, target_id: &str) -> Result<bool, AppError> {
    let conn = CdpConnection::connect(port).await?;
    match conn
        .send::<CloseTargetResult>("Target.closeTarget", json!({ "targetId": target_id }), None)
        .await
    {
        Ok(r) => Ok(r.success),
        Err(_) => Ok(false),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use axum::routing::get;
    use axum::Router;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    /// 收到的 WS 命令帧（含 id/method/params），供序列断言
    type CommandLog = Arc<Mutex<Vec<Value>>>;

    /// mock CDP server 的 WS 侧：Target.createTarget/getTargetInfo/closeTarget
    /// 最小语义。getTargetInfo 回本连接上一次 createTarget 的 url——与真实
    /// Chrome「创建即导航」一致；closeTarget 对未知 id 回协议错误（真实
    /// Chrome 对已关 target 的行为）
    async fn cdp_socket(mut socket: WebSocket, log: CommandLog) {
        let mut created_url = "about:blank".to_string();
        while let Some(Ok(msg)) = socket.recv().await {
            let Message::Text(text) = msg else { continue };
            let v: Value = serde_json::from_str(&text).unwrap();
            let Some(id) = v.get("id").and_then(|x| x.as_u64()) else {
                continue;
            };
            log.lock().unwrap().push(v.clone());
            let reply = match v["method"].as_str() {
                Some("Target.createTarget") => {
                    created_url = v["params"]["url"].as_str().unwrap_or_default().to_string();
                    json!({ "id": id, "result": { "targetId": "T1" } })
                }
                Some("Target.getTargetInfo") => json!({ "id": id, "result": {
                    "targetInfo": {
                        "targetId": "T1", "type": "page", "title": "", "url": created_url,
                    }
                }}),
                Some("Target.closeTarget") => {
                    if v["params"]["targetId"] == "T1" {
                        json!({ "id": id, "result": { "success": true } })
                    } else {
                        json!({ "id": id, "error": {
                            "code": -32000, "message": "No target with given id found",
                        }})
                    }
                }
                _ => continue,
            };
            let _ = socket.send(Message::Text(reply.to_string().into())).await;
        }
    }

    /// 同端口起 HTTP /json/version（指向同端口 /ws）+ WS /ws：CdpConnection::connect
    /// 的完整生产路径（HTTP 探 url → WS 连接）全链路可测
    async fn spawn_mock_cdp() -> (u16, CommandLog) {
        let log: CommandLog = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log_ws = Arc::clone(&log);
        let app = Router::new()
            .route(
                "/json/version",
                get(move || async move {
                    axum::Json(json!({
                        "Browser": "Chrome/131",
                        "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}/ws"),
                    }))
                }),
            )
            .route(
                "/ws",
                get(move |ws: WebSocketUpgrade| async move {
                    ws.on_upgrade(move |s| cdp_socket(s, log_ws))
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (port, log)
    }

    /// 修复主断言：新建标签页必须经 WS createTarget 携带 url 导航（HTTP
    /// /json/new 被 CfT 131 忽略 url 的替代路径），返回值映射自 getTargetInfo
    #[tokio::test]
    async fn new_target_navigates_via_ws_create_target() {
        let (port, log) = spawn_mock_cdp().await;
        let t = new_target(port, "https://example.com/").await.unwrap();

        assert_eq!(t.id, "T1");
        assert_eq!(t.target_type, "page");
        assert_eq!(t.url, "https://example.com/");
        assert_eq!(
            t.title, "",
            "刚建的 target title 为空，与旧 HTTP 版语义一致"
        );

        let cmds = log.lock().unwrap().clone();
        assert_eq!(cmds.len(), 2, "恰好两条命令：createTarget + getTargetInfo");
        assert_eq!(cmds[0]["method"], "Target.createTarget");
        assert_eq!(cmds[0]["params"]["url"], "https://example.com/");
        assert!(
            cmds[0].get("sessionId").is_none(),
            "browser 级命令不带 session"
        );
        assert_eq!(cmds[1]["method"], "Target.getTargetInfo");
        assert_eq!(cmds[1]["params"]["targetId"], "T1");
    }

    /// 关闭标签页走 WS closeTarget 并透传 success
    #[tokio::test]
    async fn close_target_sends_ws_close_target() {
        let (port, log) = spawn_mock_cdp().await;
        assert!(close_target(port, "T1").await.unwrap());

        let cmds = log.lock().unwrap().clone();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0]["method"], "Target.closeTarget");
        assert_eq!(cmds[0]["params"]["targetId"], "T1");
    }

    /// 目标已被用户关闭（Chrome 回协议错误）时归一为 false，不向上报错——
    /// navigate 流程不能因旧 tab 先一步被关而整体失败
    #[tokio::test]
    async fn close_target_tolerates_already_closed_target() {
        let (port, _log) = spawn_mock_cdp().await;
        assert!(!close_target(port, "gone").await.unwrap());
    }
}
