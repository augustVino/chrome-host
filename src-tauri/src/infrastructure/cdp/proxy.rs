//! CDP 反向代理技术设施（方案 §7.2）：HTTP JSON 端点转发 + webSocketDebuggerUrl
//! 改写 + WS 帧级桥。
//!
//! 帧级桥选型（附录 D.1）：axum ws（server 侧，握手完成后得 Message 帧）↔
//! tokio-tungstenite `connect_async`（client 侧，向 Chrome 完成握手）——两端都说
//! WS 协议，「桥到裸 TcpStream」不可行（axum 已消费握手字节）。
//!
//! 代理端点覆盖（显式边界，方案 §7.2）：GET /json、/json/list、/json/version
//! 与 WS /devtools/*；变更类 HTTP 端点（/json/new 等）不代理——cdp.mjs 全部经
//! browser WS + Target 域完成，远程开新页走 REST open_tab。

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};

use crate::error::AppError;

/// 代理路径分类。纯函数便于单测。
#[derive(Debug, Clone, PartialEq)]
pub enum ProxyKind {
    /// GET /json
    Json,
    /// GET /json/list
    JsonList,
    /// GET /json/version
    JsonVersion,
    /// WS /devtools/...（携带改写后的完整 path，含前导 /）
    DevToolsWs(String),
    /// 非代理面（/json/new 等变更端点或未知路径）
    Rejected,
}

/// 分类 `/cdp/{ins}/{sid}/` 之后的路径段。`rest` 为 axum 通配捕获（无前导 /）。
pub fn classify_proxy_path(rest: &str) -> ProxyKind {
    let r = rest.trim_start_matches('/');
    match r {
        "json" => ProxyKind::Json,
        "json/list" => ProxyKind::JsonList,
        "json/version" => ProxyKind::JsonVersion,
        _ if r.starts_with("devtools/") => ProxyKind::DevToolsWs(format!("/{r}")),
        _ => ProxyKind::Rejected,
    }
}

/// 改写 JSON 值中全部 `webSocketDebuggerUrl`（含 /json/version 的 browser 级）：
/// `ws://127.0.0.1:<port>/devtools/...` → `ws://<host>/cdp/<ins>/<sid>/devtools/...`。
/// host 取请求 Host 头 → 客户端拿到的 URL 自动指向自己可达的地址。
/// devtoolsFrontendUrl 不迁移（客户端不用，方案 §7.2）。纯函数便于单测。
pub fn rewrite_ws_urls(
    value: &mut serde_json::Value,
    instance_port: u16,
    host: &str,
    session_prefix: &str,
) {
    let from = format!("ws://127.0.0.1:{instance_port}/devtools/");
    let to = format!("ws://{host}{session_prefix}/devtools/");
    walk(value, &from, &to);
}

fn walk(value: &mut serde_json::Value, from: &str, to: &str) {
    match value {
        serde_json::Value::String(s) if s.starts_with(from) => {
            *s = format!("{to}{}", &s[from.len()..]);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                walk(item, from, to);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                walk(v, from, to);
            }
        }
        _ => {}
    }
}

/// HTTP JSON 端点代理：请求实例真实端口的同路径 → 改写 WS URL → 返回 JSON。
pub async fn proxy_json(
    instance_port: u16,
    path: &str,
    host_header: &str,
    session_prefix: &str,
) -> Result<serde_json::Value, AppError> {
    let url = format!("http://127.0.0.1:{instance_port}{path}");
    let resp = http_client()
        .get(&url)
        .send()
        .await
        .map_err(|_| {
            AppError::business(
                500,
                "CDP_CONNECTION_FAILED",
                format!("CDP 端口 {instance_port} 不可达（{path}；实例可能刚停止）"),
            )
        })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(AppError::business(
            500,
            "CDP_CONNECTION_FAILED",
            format!("CDP {path} 响应异常: {status}"),
        ));
    }
    let mut value: serde_json::Value = resp.json().await.map_err(|e| {
        AppError::business(500, "CDP_CONNECTION_FAILED", format!("CDP {path} 响应非 JSON: {e}"))
    })?;
    rewrite_ws_urls(&mut value, instance_port, host_header, session_prefix);
    Ok(value)
}

/// 共享 HTTP 客户端（短超时；与 infrastructure/cdp/client.rs 同一纪律：回环直连、
/// 绕过系统代理——环境代理不得劫持本机 CDP）。
fn http_client() -> &'static reqwest::Client {
    static CLIENT: once_cell::sync::Lazy<reqwest::Client> = once_cell::sync::Lazy::new(|| {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default()
    });
    &CLIENT
}

/// WS 帧级桥：axum WebSocket（客户端侧）↔ Chrome 实例 CDP 端口（tokio-tungstenite）。
/// 任一方向结束（断开/错误）→ 桥关闭，两侧随 drop 断开。
/// Text↔Text、Binary↔Binary 透传；Ping/Pong 原样转发（两端各自应答）。
pub async fn bridge_ws(socket: WebSocket, instance_port: u16, devtools_path: String) {
    let url = format!("ws://127.0.0.1:{instance_port}{devtools_path}");
    let (chrome_ws, _resp) = match tokio_tungstenite::connect_async(url.as_str()).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!("[cdp-proxy] 连接实例 CDP 失败 {url}: {e}");
            // 直接关闭客户端连接（客户端看到连接关闭 → 重新开会话/实例检查）
            return;
        }
    };

    let (mut client_tx, mut client_rx) = socket.split();
    let (mut chrome_tx, mut chrome_rx) = chrome_ws.split();

    let client_to_chrome = async move {
        while let Some(Ok(msg)) = client_rx.next().await {
            if chrome_tx.send(to_tungstenite(msg)).await.is_err() {
                break;
            }
        }
    };
    let chrome_to_client = async move {
        while let Some(Ok(msg)) = chrome_rx.next().await {
            if client_tx.send(to_axum(msg)).await.is_err() {
                break;
            }
        }
    };
    // 任一方向完成即结束（另一 future 被 drop → 对应连接随 drop 关闭）
    tokio::select! {
        _ = client_to_chrome => {},
        _ = chrome_to_client => {},
    }
}

/// axum ws Message → tungstenite Message
fn to_tungstenite(msg: Message) -> tokio_tungstenite::tungstenite::Message {
    use tokio_tungstenite::tungstenite::Message as T;
    match msg {
        Message::Text(t) => T::Text(t.as_str().into()),
        Message::Binary(b) => T::Binary(b),
        Message::Ping(p) => T::Ping(p),
        Message::Pong(p) => T::Pong(p),
        Message::Close(_) => T::Close(None),
    }
}

/// tungstenite Message → axum ws Message
fn to_axum(msg: tokio_tungstenite::tungstenite::Message) -> Message {
    use tokio_tungstenite::tungstenite::Message as T;
    match msg {
        T::Text(t) => Message::Text(t.as_str().into()),
        T::Binary(b) => Message::Binary(b),
        T::Ping(p) => Message::Ping(p),
        T::Pong(p) => Message::Pong(p),
        T::Close(_) => Message::Close(None),
        // 原始 Frame 透传场景不存在于对等桥（握手后无原始帧）
        T::Frame(_) => Message::Text(String::new().into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classify_whitelist_only() {
        assert_eq!(classify_proxy_path("json"), ProxyKind::Json);
        assert_eq!(classify_proxy_path("/json/list"), ProxyKind::JsonList);
        assert_eq!(classify_proxy_path("json/version"), ProxyKind::JsonVersion);
        assert_eq!(
            classify_proxy_path("devtools/page/ABC123"),
            ProxyKind::DevToolsWs("/devtools/page/ABC123".into())
        );
        assert_eq!(
            classify_proxy_path("devtools/browser/xyz"),
            ProxyKind::DevToolsWs("/devtools/browser/xyz".into())
        );
        // 变更类端点与未知路径一律拒绝（方案 §7.2 显式边界）
        assert_eq!(classify_proxy_path("json/new"), ProxyKind::Rejected);
        assert_eq!(classify_proxy_path("json/activate/ABC"), ProxyKind::Rejected);
        assert_eq!(classify_proxy_path("json/close/ABC"), ProxyKind::Rejected);
        assert_eq!(classify_proxy_path("json/protocol"), ProxyKind::Rejected);
        assert_eq!(classify_proxy_path(""), ProxyKind::Rejected);
    }

    #[test]
    fn rewrite_targets_and_browser_urls() {
        // /json/list 形状：页面级 WS URL
        let mut list = json!([{
            "id": "ABC",
            "webSocketDebuggerUrl": "ws://127.0.0.1:29223/devtools/page/ABC",
            "url": "https://example.com", "title": "t"
        }]);
        rewrite_ws_urls(&mut list, 29223, "127.0.0.1:17890", "/cdp/ins_x/sid_y");
        assert_eq!(
            list[0]["webSocketDebuggerUrl"],
            "ws://127.0.0.1:17890/cdp/ins_x/sid_y/devtools/page/ABC"
        );
        // 非目标端口 / 普通字段不动
        assert_eq!(list[0]["url"], "https://example.com");

        // /json/version 形状：browser 级 WS URL
        let mut version = json!({
            "Browser": "Chrome/131",
            "webSocketDebuggerUrl": "ws://127.0.0.1:29223/devtools/browser/uuid-1"
        });
        rewrite_ws_urls(&mut version, 29223, "yun-host:17890", "/cdp/ins_x/sid_y");
        assert_eq!(
            version["webSocketDebuggerUrl"],
            "ws://yun-host:17890/cdp/ins_x/sid_y/devtools/browser/uuid-1"
        );

        // 其他实例端口的 URL 不改写（多实例串扰防护）
        let mut mixed = json!({ "ws": "ws://127.0.0.1:29224/devtools/page/X" });
        rewrite_ws_urls(&mut mixed, 29223, "h", "/cdp/i/s");
        assert_eq!(mixed["ws"], "ws://127.0.0.1:29224/devtools/page/X");
    }
}
