//! CDP WebSocket 客户端：按需短连接（连上 → 若干命令 → 关闭丢弃）。
//!
//! 与 proxy.rs 的帧级桥共用 tokio-tungstenite；本模块面向 chrome-host 侧的
//! 主动命令调用（Cookie 导出/注入、diagnostics evaluate），常驻事件订阅
//! （console/network 历史）后续在同一连接层上扩展。
//!
//! 纪律：
//! - 目标恒为本机回环的 CDP 端口，connect_async 直连 TCP、不经系统代理——
//!   与 client.rs 的 no_proxy 同源约定；
//! - `session_id` 参数为常驻监听（Target.setAutoAttach flatten 模式）预留：
//!   Network/Runtime 的命令与事件都是 per-target session 维度。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::oneshot;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::AppError;

/// 单命令超时。CDP 命令通常是毫秒级；放宽到 15s 覆盖大批量 setCookies、
/// 截图等重命令（client.rs 的 1.5s 只够 HTTP 探活，不够 WS 命令）。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>>;

/// 一次性 CDP 命令连接。不实现 Clone：调用方独占写半部，用完即弃。
pub struct CdpConnection {
    write: SplitSink<WsStream, tokio_tungstenite::tungstenite::Message>,
    next_id: u64,
    pending: Pending,
    timeout: Duration,
}

impl CdpConnection {
    /// 连接实例的 browser 级 WS（经 /json/version 拿 webSocketDebuggerUrl）。
    /// M2（实例 Cookie 导出）起接入生产路径。
    #[allow(dead_code)]
    pub async fn connect(port: u16) -> Result<Self, AppError> {
        let v = super::client::version(port).await?;
        let url = v.web_socket_url.ok_or_else(|| {
            AppError::business(500, "CDP_CONNECTION_FAILED", "CDP 未提供 webSocketDebuggerUrl")
        })?;
        Self::connect_ws(&url, DEFAULT_TIMEOUT).await
    }

    /// 直连指定 wsUrl（测试入口；生产路径走 [`CdpConnection::connect`]）。
    pub async fn connect_ws(ws_url: &str, timeout: Duration) -> Result<Self, AppError> {
        let (ws, _resp) = tokio_tungstenite::connect_async(ws_url).await.map_err(|e| {
            AppError::business(500, "CDP_CONNECTION_FAILED", format!("CDP WS 连接失败: {e}"))
        })?;
        let (write, read) = ws.split();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(reader_loop(read, Arc::clone(&pending)));
        Ok(Self { write, next_id: 0, pending, timeout })
    }

    /// 发送命令并等待响应。
    ///
    /// - `params` 传 `()` 表示无参数命令（序列化为 null，Chrome 接受）；
    /// - `session_id` 供 flatten 模式的 per-target 命令；browser 级命令传 None；
    /// - Chrome 侧返回 error、命令超时、连接中断统一归为
    ///   `CDP_COMMAND_FAILED`（调用方按可降级路径处理，区分码无消费方）。
    pub async fn send<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: impl Serialize,
        session_id: Option<&str>,
    ) -> Result<T, AppError> {
        let id = self.next_id;
        self.next_id += 1;

        let mut payload = serde_json::json!({
            "id": id,
            "method": method,
            "params": serde_json::to_value(params).unwrap_or(serde_json::Value::Null),
        });
        if let Some(sid) = session_id {
            payload["sessionId"] = serde_json::Value::String(sid.to_string());
        }

        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        self.write
            .send(tokio_tungstenite::tungstenite::Message::Text(payload.to_string().into()))
            .await
            .map_err(|e| {
                self.pending.lock().unwrap().remove(&id);
                AppError::business(500, "CDP_COMMAND_FAILED", format!("CDP 命令发送失败: {e}"))
            })?;

        let outcome = tokio::time::timeout(self.timeout, rx).await;
        match outcome {
            // 超时：清掉 pending 表项防泄漏（reader 只在收到响应时移除）
            Err(_elapsed) => {
                self.pending.lock().unwrap().remove(&id);
                Err(AppError::business(
                    500,
                    "CDP_COMMAND_FAILED",
                    format!("CDP 命令 {method} 超时（{}ms）", self.timeout.as_millis()),
                ))
            }
            // reader 在连接关闭时会把所有 pending 置为 Err
            Ok(Err(_closed)) => Err(AppError::business(
                500,
                "CDP_COMMAND_FAILED",
                "CDP 连接在命令完成前关闭",
            )),
            Ok(Ok(Err(message))) => Err(AppError::business(
                500,
                "CDP_COMMAND_FAILED",
                format!("CDP 命令 {method} 被拒绝: {message}"),
            )),
            Ok(Ok(Ok(result))) => serde_json::from_value(result).map_err(|e| {
                AppError::business(
                    500,
                    "CDP_COMMAND_FAILED",
                    format!("CDP 命令 {method} 响应解析失败: {e}"),
                )
            }),
        }
    }
}

/// 读循环：响应（带 id）路由回 pending 等待者；事件（无 id）当前无消费方，
/// 丢弃即可——常驻监听落地时在此处接入事件通道，不影响本模块的调用方。
async fn reader_loop(read: SplitStream<WsStream>, pending: Pending) {
    let mut read = read;
    while let Some(msg) = read.next().await {
        match msg {
            Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let Some(id) = v.get("id").and_then(|x| x.as_u64()) else {
                    continue; // 事件帧：M1 无消费方
                };
                let tx = pending.lock().unwrap().remove(&id);
                if let Some(tx) = tx {
                    let result = match v.get("error") {
                        Some(err) => Err(format!(
                            "{} {}",
                            err.get("code").map(|c| c.to_string()).unwrap_or_default(),
                            err.get("message").and_then(|m| m.as_str()).unwrap_or("")
                        )),
                        None => Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null)),
                    };
                    let _ = tx.send(result);
                }
            }
            // Ping/Pong 由 tungstenite 读循环内部应答；Binary/Close 不属于命令通道
            Ok(_) => {}
            Err(_) => break,
        }
    }
    // 连接结束：唤醒全部等待者，避免悬挂到超时
    let stranded: Vec<_> = pending.lock().unwrap().drain().collect();
    for (_, tx) in stranded {
        let _ = tx.send(Err("CDP 连接已关闭".to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use axum::response::Response;
    use axum::routing::get;
    use axum::Router;
    use serde_json::json;

    /// mock CDP server：Echo.ok 原样回传 params；Echo.fail 回 error；
    /// Echo.hang 永不响应（测超时）；Echo.session 回传请求里的 sessionId。
    async fn mock_socket(mut socket: WebSocket) {
        while let Some(Ok(msg)) = socket.recv().await {
            let Message::Text(text) = msg else { continue };
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            let Some(id) = v.get("id").and_then(|x| x.as_u64()) else { continue };
            let reply = match v["method"].as_str() {
                Some("Echo.ok") => json!({ "id": id, "result": v["params"] }),
                Some("Echo.session") => json!({
                    "id": id,
                    "result": { "sessionId": v.get("sessionId").cloned().unwrap_or(Value::Null) }
                }),
                Some("Echo.fail") => json!({ "id": id, "error": { "code": -32000, "message": "boom" } }),
                Some("Echo.close") => break, // 直接断开：验证等待者立即失败而非等到超时
                _ => continue, // Echo.hang 等：不响应
            };
            let _ = socket.send(Message::Text(reply.to_string().into())).await;
        }
    }

    async fn mock_handler(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(mock_socket)
    }

    use serde_json::Value;

    async fn spawn_mock() -> String {
        let app = Router::new().route("/ws", get(mock_handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("ws://127.0.0.1:{}/ws", addr.port())
    }

    #[tokio::test]
    async fn command_roundtrip_deserializes_response() {
        let url = spawn_mock().await;
        let mut conn = CdpConnection::connect_ws(&url, DEFAULT_TIMEOUT).await.unwrap();
        let out: Value = conn
            .send("Echo.ok", json!({ "hello": "world", "n": 42 }), None)
            .await
            .unwrap();
        assert_eq!(out["hello"], "world");
        assert_eq!(out["n"], 42);
    }

    #[tokio::test]
    async fn sequential_commands_no_cross_talk() {
        let url = spawn_mock().await;
        let mut conn = CdpConnection::connect_ws(&url, DEFAULT_TIMEOUT).await.unwrap();
        let a: Value = conn.send("Echo.ok", json!({ "seq": 1 }), None).await.unwrap();
        let b: Value = conn.send("Echo.ok", json!({ "seq": 2 }), None).await.unwrap();
        assert_eq!(a["seq"], 1);
        assert_eq!(b["seq"], 2);
    }

    #[tokio::test]
    async fn chrome_error_maps_to_command_failed() {
        let url = spawn_mock().await;
        let mut conn = CdpConnection::connect_ws(&url, DEFAULT_TIMEOUT).await.unwrap();
        let err = conn.send::<Value>("Echo.fail", (), None).await.unwrap_err();
        assert_eq!(err.code(), "CDP_COMMAND_FAILED");
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn timeout_maps_to_command_failed() {
        let url = spawn_mock().await;
        let mut conn = CdpConnection::connect_ws(&url, Duration::from_millis(300)).await.unwrap();
        let err = conn.send::<Value>("Echo.hang", (), None).await.unwrap_err();
        assert_eq!(err.code(), "CDP_COMMAND_FAILED");
        assert!(err.to_string().contains("超时"));
    }

    #[tokio::test]
    async fn session_id_passthrough() {
        let url = spawn_mock().await;
        let mut conn = CdpConnection::connect_ws(&url, DEFAULT_TIMEOUT).await.unwrap();
        let out: Value = conn
            .send("Echo.session", (), Some("target-session-1"))
            .await
            .unwrap();
        assert_eq!(out["sessionId"], "target-session-1");
    }

    #[tokio::test]
    async fn early_disconnect_fails_waiters_immediately() {
        let url = spawn_mock().await;
        // 超时设 10s：若断开路径失效，本测试会挂满 10s 才超时失败——断言快速失败即证明
        // reader 的 pending 清理逻辑生效
        let mut conn = CdpConnection::connect_ws(&url, Duration::from_secs(10)).await.unwrap();
        let err = conn.send::<Value>("Echo.close", (), None).await.unwrap_err();
        assert_eq!(err.code(), "CDP_COMMAND_FAILED");
        assert!(err.to_string().contains("关闭"));
    }
}
