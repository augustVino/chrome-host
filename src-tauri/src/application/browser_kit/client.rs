//! browser_kit 的 CDP 接缝适配器：把 chrome-host 的 [`SharedPageConn`]
//! （PageCdp trait 对象）+ Watched 事件总线包装成 browser_kit 能力层期望的
//! `CdpClient` 形态——上游能力文件（element/snapshot/interaction/…）只通过
//! `send_command*` 与 `subscribe*` 两个原语族访问连接，本模块是这两个原语族
//! 在本项目连接基础设施上的全部实现。
//!
//! 与上游 cdp/client.rs 的关系：接口面（方法签名）与 `send_command_typed_raw`
//! 的单次反序列化语义（含 lone-surrogate 修复重试）按上游对齐；传输层不复用
//! 上游 WebSocket client——chrome-host 已有 CdpConnection（写帧短锁 + 并发
//! in-flight + 取消安全），本适配器只是把它 dyn 化后的薄包装。
//!
//! 错误形态：上游能力层以 `Result<_, String>` 传递错误（无错误分类），适配器
//! 把 AppError 的 message 抽出为 String；集成层（page_service）负责把 kit 的
//! String 错误映射回 chrome-host 的错误码体系。
//!
//! CHROME-HOST ADAPTATIONS (V1): CdpClient 增加会话级 active_frame 字段。
//!   what: 新增 `active_frame: Option<String>` 字段（构造期经
//!         `with_active_frame` 注入，无 interior mutability）、只读 getter
//!         `active_frame()`；new() 签名不变（默认 None）。
//!   why: 上游把 active_frame 放在 element.rs 的进程级 static
//!        （ACTIVE_FRAME）里、命令前镜像会话态——单 session 单浏览器安全；
//!        chrome-host 多实例并发，进程级全局会跨实例串态，故下沉为 client
//!        实例字段，随连接生命周期隔离。

use std::collections::HashMap;
use std::sync::Mutex;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use super::types::CdpEvent;
use crate::infrastructure::cdp::page_cdp::SharedPageConn;

/// typed_raw 的响应信封：result 直取 R（单次反序列化，不经 Value 中间树）
#[derive(serde::Deserialize)]
struct RawEnvelope<R> {
    result: Option<R>,
    error: Option<RawError>,
}

#[derive(serde::Deserialize)]
struct RawError {
    #[serde(default)]
    code: Value,
    message: String,
}

/// 能力层的 CDP client。上游能力文件的全部 IO 都经由本结构。
pub struct CdpClient {
    conn: SharedPageConn,
    bus: broadcast::Sender<CdpEvent>,
    /// session_subs 的过滤转发任务登记：session_id → 发送端（与
    /// subscribe_session 的通道形态统一为有界 Sender）。持有 clone 供
    /// unsubscribe_session 移除（drop 发送端 → 转发任务在下次 send 失败
    /// 时自然退场；接收端 drop 同理）。
    session_subs: Mutex<HashMap<String, mpsc::Sender<CdpEvent>>>,
    /// 会话级 `frame <sel>` 选择：CSS 选择器解析时优先在该 frame 内解析
    /// （见 element.rs 两个 fallback 分支）。构造期注入后只读——上游在
    /// daemon 里每次命令前镜像 DaemonState.active_frame_id，本项目改为
    /// 按会话在构造期定值，避免进程级全局态跨实例串扰。
    active_frame: Option<String>,
}

impl CdpClient {
    pub fn new(conn: SharedPageConn, bus: broadcast::Sender<CdpEvent>) -> Self {
        CdpClient {
            conn,
            bus,
            session_subs: Mutex::new(HashMap::new()),
            active_frame: None,
        }
    }

    /// 构造器：注入会话级 active_frame（链式，链完即定值）。语义对齐上游
    /// daemon 的 set_active_frame(state.active_frame_id)，但改为构造期
    /// 一次性注入——client 无 interior mutability，多实例并发下天然隔离。
    pub fn with_active_frame(mut self, frame_id: Option<String>) -> Self {
        self.active_frame = frame_id;
        self
    }

    /// 当前会话的 `frame <sel>` 选择（无则 None；见字段注释）。
    pub fn active_frame(&self) -> Option<&str> {
        self.active_frame.as_deref()
    }

    /// 测试构造器（vendored 测试模块用）：直连 ws URL 建真实 CdpConnection
    ///（无事件消费方），包成本适配器。上游测试用 CdpClient::connect(url)，
    /// 本项目传输层不同，此为最小等价物。
    #[cfg(test)]
    pub async fn connect_test(ws_url: &str) -> Self {
        let conn = crate::infrastructure::cdp::ws::CdpConnection::connect_ws(
            ws_url,
            std::time::Duration::from_secs(15),
        )
        .await
        .expect("mock CDP server 连接失败");
        let bus = broadcast::channel(64).0;
        CdpClient::new(std::sync::Arc::new(conn), bus)
    }

    /// 事件总线的广播端（Watched::event_bus 持有；watch_loop 逐事件转发）。
    pub fn bus(&self) -> &broadcast::Sender<CdpEvent> {
        &self.bus
    }

    pub async fn send_command(
        &self,
        method: &str,
        params: Option<Value>,
        session_id: Option<&str>,
    ) -> Result<Value, String> {
        self.conn
            .send_value(method, params.unwrap_or(Value::Null), session_id)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn send_command_typed<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
        session_id: Option<&str>,
    ) -> Result<R, String> {
        let params_value =
            serde_json::to_value(params).map_err(|e| format!("Failed to serialize params: {e}"))?;
        let result = self
            .send_command(method, Some(params_value), session_id)
            .await?;
        serde_json::from_value(result)
            .map_err(|e| format!("Failed to deserialize CDP response for {method}: {e}"))
    }

    pub async fn send_command_no_params(
        &self,
        method: &str,
        session_id: Option<&str>,
    ) -> Result<Value, String> {
        self.send_command(method, None, session_id).await
    }

    /// 发送命令并把响应帧文本单次反序列化为 R（大响应防双份内存：
    /// getFullAXTree 的 MB 级 AX 树）。语义对齐上游 client.rs（含
    /// lone-surrogate 修复重试一次）。
    pub async fn send_command_typed_raw<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
        session_id: Option<&str>,
    ) -> Result<R, String> {
        let params_value =
            serde_json::to_value(params).map_err(|e| format!("Failed to serialize params: {e}"))?;
        let text = self
            .conn
            .send_raw(method, params_value, session_id)
            .await
            .map_err(|e| e.to_string())?;
        let parse = |text: &str| -> Result<RawEnvelope<R>, String> {
            serde_json::from_str(text)
                .map_err(|e| format!("Failed to deserialize CDP response for {method}: {e}"))
        };
        let response = parse(&text).or_else(|error| match sanitize_lone_surrogates(&text) {
            Some(repaired) => parse(&repaired),
            None => Err(error),
        })?;
        if let Some(err) = response.error {
            return Err(format!(
                "CDP error ({method}): {} {}",
                err.code, err.message
            ));
        }
        response.result.ok_or_else(|| {
            format!("Failed to deserialize CDP response for {method}: missing result")
        })
    }

    /// 发送命令不等响应（recording 的 screencastFrameAck：高频 ack 不占
    /// 等待路径）。响应到达后由丢弃任务消化（保持 pending 表卫生）。
    pub async fn send_command_no_wait(
        &self,
        method: &str,
        params: Option<Value>,
        session_id: Option<&str>,
    ) -> Result<(), String> {
        let conn = self.conn.clone();
        let method = method.to_string();
        let sid = session_id.map(str::to_string);
        tokio::spawn(async move {
            let _ = conn
                .send_value(&method, params.unwrap_or(Value::Null), sid.as_deref())
                .await;
        });
        Ok(())
    }

    /// 订阅全部 CDP 事件（broadcast：多订阅者、慢订阅者丢旧帧收 Lagged）。
    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.bus.subscribe()
    }

    /// 订阅单个 session 的事件（recording 的 screencast 帧流）。
    /// 实现为 broadcast 消费者上的 session 过滤转发任务；unsubscribe 或
    /// 接收端 drop 后任务自然退场。有界（512）：背压时丢帧而非无限堆积
    ///（screencast 帧流容忍丢帧，上游同语义）。
    pub fn subscribe_session(&self, session_id: &str) -> mpsc::Receiver<CdpEvent> {
        let (tx, rx) = mpsc::channel(512);
        let mut broadcast_rx = self.bus.subscribe();
        let sid = session_id.to_string();
        self.session_subs
            .lock()
            .unwrap()
            .insert(session_id.to_string(), tx.clone());
        tokio::spawn(async move {
            loop {
                match broadcast_rx.recv().await {
                    Ok(ev) => {
                        if ev.session_id.as_deref() == Some(sid.as_str()) {
                            match tx.try_send(ev) {
                                // 满丢帧：screencast 容忍；接收端 drop：退场
                                Err(mpsc::error::TrySendError::Full(_)) => continue,
                                Err(mpsc::error::TrySendError::Closed(_)) => break,
                                Ok(()) => {}
                            }
                        }
                    }
                    // Lagged：慢消费丢帧——screencast 帧流容忍丢帧（上游同语义）
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    /// 撤销 subscribe_session 建立的订阅（移除登记 → 转发任务在下次 send
    /// 失败时退场）。
    pub fn unsubscribe_session(&self, session_id: &str) {
        self.session_subs.lock().unwrap().remove(session_id);
    }
}

/// 把 ws.rs 透传的原始事件帧（Value）转成能力层的 CdpEvent。
/// 无 method 字段的帧（协议噪声）返回 None 被丢弃。
pub fn cdp_event_from_frame(v: &Value) -> Option<CdpEvent> {
    Some(CdpEvent {
        method: v.get("method")?.as_str()?.to_string(),
        params: v.get("params").cloned().unwrap_or(Value::Null),
        session_id: v
            .get("sessionId")
            .and_then(|s| s.as_str())
            .map(str::to_string),
    })
}

/// 修复 Chrome 可能吐出的孤立 UTF-16 代理对。原样搬自上游
/// cdp/client.rs `sanitize_lone_surrogates`（字节级扫描：合法代理对保持
/// 原样，孤立高位/低位代理替换为 U+FFFD，保证重试 parse 可过）。
fn sanitize_lone_surrogates(msg: &str) -> Option<String> {
    const ESCAPE_LEN: usize = 6;
    const HIGH_START: u32 = 0xD800;
    const LOW_START: u32 = 0xDC00;
    const SURROGATE_END: u32 = 0xE000;

    let read_escape = |bytes: &[u8], at: usize| -> Option<u32> {
        let end = at.checked_add(ESCAPE_LEN)?;
        if end > bytes.len() || bytes[at] != b'\\' || bytes[at + 1] != b'u' {
            return None;
        }
        let hex = &bytes[at + 2..end];
        if !hex.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()
    };

    let bytes = msg.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] != b'u' {
            i += 2;
            continue;
        }

        let Some(unit) = read_escape(bytes, i) else {
            i += 1;
            continue;
        };

        let is_high = (HIGH_START..LOW_START).contains(&unit);
        let is_low = (LOW_START..SURROGATE_END).contains(&unit);
        let paired = is_high
            && read_escape(bytes, i + ESCAPE_LEN)
                .is_some_and(|next| (LOW_START..SURROGATE_END).contains(&next));

        if paired {
            i += ESCAPE_LEN * 2;
            continue;
        }

        if is_high || is_low {
            let buf = out.get_or_insert_with(String::new);
            buf.push_str(&msg[copied..i]);
            buf.push(char::REPLACEMENT_CHARACTER);
            copied = i + ESCAPE_LEN;
        }
        i += ESCAPE_LEN;
    }

    if let Some(buf) = out.as_mut() {
        buf.push_str(&msg[copied..]);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lone_surrogate_escape_is_repaired() {
        // "\ud83d" 孤立高位代理 → U+FFFD 替换后可被 serde 解析
        let raw = r#"{"result":{"name":"\ud83d"}}"#;
        assert!(serde_json::from_str::<serde_json::Value>(raw).is_err());
        let repaired = sanitize_lone_surrogates(raw).expect("应产生修复文本");
        let v: serde_json::Value = serde_json::from_str(&repaired).expect("修复后应可解析");
        assert_eq!(v["result"]["name"].as_str(), Some("\u{FFFD}"));
    }

    #[test]
    fn clean_text_is_untouched() {
        let raw = r#"{"result":{"ok":true}}"#;
        assert_eq!(sanitize_lone_surrogates(raw), None);
    }

    #[test]
    fn event_from_frame_maps_fields() {
        let frame = serde_json::json!({
            "method": "Page.screencastFrame",
            "params": {"data": "abc"},
            "sessionId": "sess-9"
        });
        let ev = cdp_event_from_frame(&frame).unwrap();
        assert_eq!(ev.method, "Page.screencastFrame");
        assert_eq!(ev.params["data"], "abc");
        assert_eq!(ev.session_id.as_deref(), Some("sess-9"));
        assert!(cdp_event_from_frame(&serde_json::json!({"id": 1})).is_none());
    }
}
