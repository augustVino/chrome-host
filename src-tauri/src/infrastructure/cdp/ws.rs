//! CDP WebSocket 客户端：按需短连接（连上 → 若干命令 → 关闭丢弃）与常驻事件
//! 订阅（连上 → 事件透传 → 断线由调用方重连）两种形态同一实现：无 id 帧是否
//! 有消费方由 [`EventSink`] 决定，命令路径完全一致。
//!
//! 并发模型（P2-F 根治）：`send` 为 `&self`——多条命令可同时 in-flight（写帧
//! 短锁 + id 关联的响应路由）。&mut self 时代「发送+等响应」全程独占连接，
//! in-flight 长命令（如 click 卡在被 alert 阻塞的页面至 15s 超时）会阻塞同
//! 连接上的对话框 handle 等一切并发命令，真机冒烟实测错过对话框黄金窗口。
//!
//! 与 proxy.rs 的帧级桥共用 tokio-tungstenite；短连接面向 chrome-host 侧的
//! 一次性主动命令（Cookie 导出/注入、diagnostics evaluate），常驻形态面向
//! M6 诊断订阅（console/network 事件）。
//!
//! 纪律：
//! - 目标恒为本机回环的 CDP 端口，connect_async 直连 TCP、不经系统代理——
//!   与 client.rs 的 no_proxy 同源约定；
//! - `session_id` 参数服务于常驻 flatten 模式（Target.setAutoAttach）：
//!   Network/Runtime 的命令与事件都是 per-target session 维度（session 是
//!   连接级作用域，跨连接复用 sessionId 必 -32001 Session not found）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::AppError;

/// 单命令超时。CDP 命令通常是毫秒级；放宽到 15s 覆盖大批量 setCookies、
/// 截图等重命令（client.rs 的 1.5s 只够 HTTP 探活，不够 WS 命令）。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>>;
/// 常驻连接的事件出口：无 id 帧（CDP 事件）透传给消费者
pub type EventSink = mpsc::UnboundedSender<serde_json::Value>;

/// pending 表项的取消安全守卫：send future 在任意 await 点被外层 drop
/// （如 diagnostics body 拉取的 2.5s 预算超时）时，由 Drop 即时移除表项。
/// 此前只有内层超时（15s）或响应到达才清理——被 drop 的悬挂请求会滞留
/// pending 表直到 Chrome 迟来响应被 reader 路由丢弃或连接关闭统一 drain。
/// 正常出口（响应/拒绝/内层超时/写失败）Drop 同样执行 remove，幂等无害：
/// id 单调不复用，reader 抢先移除时 remove 落空即 no-op。
struct PendingGuard {
    pending: Pending,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

/// 一次性 CDP 命令连接（可共享：`Arc<CdpConnection>` 直接发命令，无锁包裹）。
pub struct CdpConnection {
    /// 写半部短锁：只覆盖「写一帧」，不等响应——若把等响应也留在锁内，
    /// in-flight 长命令会重新串行化整条连接（P2-F 根治的关键点）。
    write: tokio::sync::Mutex<SplitSink<WsStream, tokio_tungstenite::tungstenite::Message>>,
    next_id: AtomicU64,
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
            AppError::business(
                500,
                "CDP_CONNECTION_FAILED",
                "CDP 未提供 webSocketDebuggerUrl",
            )
        })?;
        Self::connect_ws(&url, DEFAULT_TIMEOUT).await
    }

    /// 直连指定 wsUrl（测试入口；生产路径走 [`CdpConnection::connect`]）。
    pub async fn connect_ws(ws_url: &str, timeout: Duration) -> Result<Self, AppError> {
        Self::connect_ws_eventing(ws_url, timeout, None).await
    }

    /// 连接实例 browser 级 WS 并把事件帧透传给 `events`（常驻订阅形态，M6 起
    /// 接入生产路径）：无 id 帧路由到事件通道，命令路径与短连接完全一致。
    pub async fn connect_eventing(port: u16, events: EventSink) -> Result<Self, AppError> {
        let v = super::client::version(port).await?;
        let url = v.web_socket_url.ok_or_else(|| {
            AppError::business(
                500,
                "CDP_CONNECTION_FAILED",
                "CDP 未提供 webSocketDebuggerUrl",
            )
        })?;
        Self::connect_ws_eventing(&url, DEFAULT_TIMEOUT, Some(events)).await
    }

    pub async fn connect_ws_eventing(
        ws_url: &str,
        timeout: Duration,
        events: Option<EventSink>,
    ) -> Result<Self, AppError> {
        let (ws, _resp) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| {
                AppError::business(
                    500,
                    "CDP_CONNECTION_FAILED",
                    format!("CDP WS 连接失败: {e}"),
                )
            })?;
        let (write, read) = ws.split();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(reader_loop(read, Arc::clone(&pending), events));
        Ok(Self {
            write: tokio::sync::Mutex::new(write),
            next_id: AtomicU64::new(0),
            pending,
            timeout,
        })
    }

    /// 发送命令并等待响应（`&self`：多条命令可并发 in-flight，见模块注释）。
    ///
    /// - `params` 传 `()` 表示无参数命令（序列化为 null，Chrome 接受）；
    /// - `session_id` 供 flatten 模式的 per-target 命令；browser 级命令传 None；
    /// - Chrome 侧返回 error、命令超时、连接中断统一归为
    ///   `CDP_COMMAND_FAILED`（调用方按可降级路径处理，区分码无消费方）；
    /// - 取消安全（W-fix2）：future 在任意 await 点被 drop 时 pending 表项由
    ///   [`PendingGuard`] 即时移除——外层包 timeout 取消不再依赖内层超时兜底。
    pub async fn send<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
        session_id: Option<&str>,
    ) -> Result<T, AppError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

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
        // 插入即布防：本函数所有出口（写帧失败/响应/拒绝/内层超时/被外层
        // drop）统一经 Drop 清理 pending——取代原先散落的手动 remove
        let _guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            id,
        };

        {
            // 写帧短锁：发送完成即释放，等响应不持锁（并发 in-flight 的前提）
            let mut write = self.write.lock().await;
            write
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    payload.to_string().into(),
                ))
                .await
                .map_err(|e| {
                    AppError::business(500, "CDP_COMMAND_FAILED", format!("CDP 命令发送失败: {e}"))
                })?;
        }

        let outcome = tokio::time::timeout(self.timeout, rx).await;
        match outcome {
            // 超时：表项由 _guard Drop 移除（reader 只在收到响应时移除）
            Err(_elapsed) => Err(AppError::business(
                500,
                "CDP_COMMAND_FAILED",
                format!("CDP 命令 {method} 超时（{}ms）", self.timeout.as_millis()),
            )),
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

/// 读循环：响应（带 id）路由回 pending 等待者；事件（无 id）——短连接无消费方
/// 丢弃，常驻连接（M6 起）透传给事件通道。事件通道随 reader 结束而 drop，
/// 消费侧 recv 返回 None 即连接已死的信号。
async fn reader_loop(read: SplitStream<WsStream>, pending: Pending, events: Option<EventSink>) {
    let mut read = read;
    while let Some(msg) = read.next().await {
        match msg {
            Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let Some(id) = v.get("id").and_then(|x| x.as_u64()) else {
                    if let Some(tx) = &events {
                        // unbounded 是权衡后的选择（复审 P3-3）：诊断消费端是纯内存
                        // 解析 + 定长环形，速率远高于事件到达；连接断开 reader 结束、
                        // 通道整体 drop，无累积泄漏路径。若未来接重消费者再改
                        // bounded + try_send 丢弃计数（诊断数据可丢，连接保活优先）
                        let _ = tx.send(v);
                    }
                    continue;
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
            let Some(id) = v.get("id").and_then(|x| x.as_u64()) else {
                continue;
            };
            let reply = match v["method"].as_str() {
                Some("Echo.ok") => json!({ "id": id, "result": v["params"] }),
                Some("Echo.session") => json!({
                    "id": id,
                    "result": { "sessionId": v.get("sessionId").cloned().unwrap_or(Value::Null) }
                }),
                Some("Echo.fail") => {
                    json!({ "id": id, "error": { "code": -32000, "message": "boom" } })
                }
                Some("Echo.close") => break, // 直接断开：验证等待者立即失败而非等到超时
                _ => continue,               // Echo.hang 等：不响应
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
        let conn = CdpConnection::connect_ws(&url, DEFAULT_TIMEOUT)
            .await
            .unwrap();
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
        let conn = CdpConnection::connect_ws(&url, DEFAULT_TIMEOUT)
            .await
            .unwrap();
        let a: Value = conn
            .send("Echo.ok", json!({ "seq": 1 }), None)
            .await
            .unwrap();
        let b: Value = conn
            .send("Echo.ok", json!({ "seq": 2 }), None)
            .await
            .unwrap();
        assert_eq!(a["seq"], 1);
        assert_eq!(b["seq"], 2);
    }

    #[tokio::test]
    async fn chrome_error_maps_to_command_failed() {
        let url = spawn_mock().await;
        let conn = CdpConnection::connect_ws(&url, DEFAULT_TIMEOUT)
            .await
            .unwrap();
        let err = conn.send::<Value>("Echo.fail", (), None).await.unwrap_err();
        assert_eq!(err.code(), "CDP_COMMAND_FAILED");
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn timeout_maps_to_command_failed() {
        let url = spawn_mock().await;
        let conn = CdpConnection::connect_ws(&url, Duration::from_millis(300))
            .await
            .unwrap();
        let err = conn.send::<Value>("Echo.hang", (), None).await.unwrap_err();
        assert_eq!(err.code(), "CDP_COMMAND_FAILED");
        assert!(err.to_string().contains("超时"));
    }

    /// 取消安全回归（W-fix2）：外层 timeout drop 掉 send future 时，pending
    /// 表项必须被 Drop guard 即时移除——此前只有内层超时或响应到达才清理，
    /// 悬挂请求会滞留 pending 表直到连接关闭
    #[tokio::test]
    async fn dropped_send_removes_pending_entry() {
        let url = spawn_mock().await;
        // 内层超时 10s：若 drop 不清理，表项将滞留至 10s 内层超时——直接断言 0
        let conn = CdpConnection::connect_ws(&url, Duration::from_secs(10))
            .await
            .unwrap();
        let _ = tokio::time::timeout(
            Duration::from_millis(100),
            conn.send::<Value>("Echo.hang", (), None),
        )
        .await; // Elapsed：send future 在此被 drop
        assert_eq!(
            conn.pending.lock().unwrap().len(),
            0,
            "drop 后 pending 表项必须即时清理"
        );
        // 后续命令不受残留表项影响（id 递增不冲突）
        let out: Value = conn.send("Echo.ok", json!({ "n": 1 }), None).await.unwrap();
        assert_eq!(out["n"], 1);
    }

    #[tokio::test]
    async fn session_id_passthrough() {
        let url = spawn_mock().await;
        let conn = CdpConnection::connect_ws(&url, DEFAULT_TIMEOUT)
            .await
            .unwrap();
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
        let conn = CdpConnection::connect_ws(&url, Duration::from_secs(10))
            .await
            .unwrap();
        let err = conn
            .send::<Value>("Echo.close", (), None)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "CDP_COMMAND_FAILED");
        assert!(err.to_string().contains("关闭"));
    }

    /// P2-F 根治回归：send 并发 in-flight——gated 命令响应被服务端扣住期间，
    /// 同连接另一条命令必须能正常完成。&mut self 时代（或任何把「等响应」
    /// 留在锁内的实现）第二条要等第一条超时（15s），500ms 预算直接判负。
    #[tokio::test]
    async fn concurrent_sends_do_not_serialize_on_inflight_response() {
        // mock：Echo.gated 扣住响应；Echo.release 到达后补发 gated 的响应
        async fn gated_socket(mut socket: WebSocket) {
            let mut stashed: Option<u64> = None;
            while let Some(Ok(msg)) = socket.recv().await {
                let Message::Text(text) = msg else { continue };
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                let Some(id) = v.get("id").and_then(|x| x.as_u64()) else {
                    continue;
                };
                match v["method"].as_str() {
                    Some("Echo.gated") => stashed = Some(id),
                    Some("Echo.release") => {
                        if let Some(gid) = stashed.take() {
                            let _ = socket
                                .send(Message::Text(
                                    json!({ "id": gid, "result": { "released": true } })
                                        .to_string()
                                        .into(),
                                ))
                                .await;
                        }
                        let _ = socket
                            .send(Message::Text(
                                json!({ "id": id, "result": {} }).to_string().into(),
                            ))
                            .await;
                    }
                    _ => {
                        let _ = socket
                            .send(Message::Text(
                                json!({ "id": id, "result": v["params"].clone() })
                                    .to_string()
                                    .into(),
                            ))
                            .await;
                    }
                }
            }
        }
        let app = Router::new().route(
            "/ws",
            get(|ws: WebSocketUpgrade| async move { ws.on_upgrade(gated_socket) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("ws://127.0.0.1:{}/ws", addr.port());

        let conn = Arc::new(
            CdpConnection::connect_ws(&url, DEFAULT_TIMEOUT)
                .await
                .unwrap(),
        );
        // gated 后台 in-flight：spawn 后留一拍（current_thread runtime 下 sleep
        // 让任务跑到等响应处），确保它已发出且响应被扣住
        let c = Arc::clone(&conn);
        let gated = tokio::spawn(async move {
            c.send::<Value>("Echo.gated", json!({ "slow": true }), None)
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let ok = tokio::time::timeout(
            Duration::from_millis(500),
            conn.send::<Value>("Echo.ok", json!({ "n": 7 }), None),
        )
        .await
        .expect("in-flight 命令不得阻塞同连接的其他 send")
        .unwrap();
        assert_eq!(ok["n"], 7);

        // 放行：gated 收到补发响应（id 路由正确性）
        conn.send::<Value>("Echo.release", (), None).await.unwrap();
        let gated_out = tokio::time::timeout(Duration::from_secs(2), gated)
            .await
            .expect("release 后 gated 应收到补发响应")
            .unwrap()
            .unwrap();
        assert_eq!(gated_out["released"], true);
    }

    /// 常驻形态：事件帧（无 id）透传给 EventSink，命令路径不变；
    /// reader 结束时通道 drop → 消费侧 recv None 即「连接已死」信号
    #[tokio::test]
    async fn eventing_routes_events_and_signals_close() {
        // mock：收到 Echo.event 命令后先推一个事件帧（无 id）再回命令响应；
        // Echo.close 断开连接
        async fn event_socket(mut socket: WebSocket) {
            while let Some(Ok(msg)) = socket.recv().await {
                let Message::Text(text) = msg else { continue };
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                let Some(id) = v.get("id").and_then(|x| x.as_u64()) else {
                    continue;
                };
                match v["method"].as_str() {
                    Some("Echo.event") => {
                        let event = json!({ "method": "Test.event", "params": { "n": 7 }, "sessionId": "sess-1" });
                        let _ = socket.send(Message::Text(event.to_string().into())).await;
                        let _ = socket
                            .send(Message::Text(
                                json!({ "id": id, "result": {} }).to_string().into(),
                            ))
                            .await;
                    }
                    Some("Echo.close") => break,
                    _ => {}
                }
            }
        }
        let app = Router::new().route(
            "/ws",
            get(|ws: WebSocketUpgrade| async move { ws.on_upgrade(event_socket) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("ws://127.0.0.1:{}/ws", addr.port());

        let (tx, mut rx) = mpsc::unbounded_channel();
        let conn = CdpConnection::connect_ws_eventing(&url, DEFAULT_TIMEOUT, Some(tx))
            .await
            .unwrap();

        // 命令仍可用（同一连接双通道）
        conn.send::<Value>("Echo.event", (), None).await.unwrap();
        let event = rx.recv().await.expect("事件帧应透传");
        assert_eq!(event["method"], "Test.event");
        assert_eq!(event["params"]["n"], 7);
        assert_eq!(
            event["sessionId"], "sess-1",
            "session 维度保留（常驻订阅核心）"
        );

        // 连接关闭 → 通道 drop → recv None（重连循环的退出信号）
        let _ = conn.send::<Value>("Echo.close", (), None).await;
        let mut none_count = 0;
        for _ in 0..3 {
            if rx.recv().await.is_none() {
                none_count += 1;
                break;
            }
        }
        assert_eq!(none_count, 1, "连接死后通道应终止");
    }
}
