//! 常驻诊断的数据层：CDP 事件的解析入环 + 查询投影 + HAR 1.2 生成。
//! 纯逻辑无 IO（事件 JSON 由应用层喂入），全部可单测；IO 与重连编排在
//! application/diagnostics_service.rs。
//!
//! 语义（AGENT-API.md 同步披露，见「证据边界契约」）：
//! - console 环 500 条 / network 环 1000 条，进程内态——应用或实例重启即清；
//!   溢出丢最旧并累计 dropped 计数（查询/HAR 响应 meta 透出，绝不静默截断）；
//! - network 条目在请求**完成/失败时**入环（in-flight 不可见，响应 meta 以
//!   `scope: "completed"` 声明）；requestId 按
//!   session 维度去重（多 tab 各自计数，不互相覆盖）；
//! - 事件应用返回 [`ApplyOutcome`]，attach 的 per-session enable 命令由应用层执行
//!   （域层不产 IO）。

use std::collections::{HashMap, VecDeque};

use base64::Engine;
use serde::Serialize;
use serde_json::{json, Value};

use crate::domain::redact;

/// 环形容量（交接契约：console 500 / network 1000）
const CONSOLE_RING: usize = 500;
const NETWORK_RING: usize = 1000;
/// 单条 body 上限（字节）：超限截断 → bodyState "partial"（保留实际字节数）
pub const BODY_ITEM_MAX_BYTES: usize = 256 * 1024;
/// 环级 body 总预算（字节）：超预算从最旧条目驱逐 body（元数据永不驱逐）
pub const BODY_TOTAL_MAX_BYTES: usize = 8 * 1024 * 1024;
/// network 环的采集范围声明（console/network 查询与 HAR `_meta` 的常量值）：
/// 条目在请求完成/失败时入环，**进行中的请求不可见**——响应显式声明而非留白
pub const NETWORK_SCOPE_COMPLETED: &str = "completed";
/// in-flight 悬挂保护：未完成请求的 pending 表上限。超限整体清空——泄漏防护
/// 优先于完整性（无序 HashMap 无从挑选最旧；被清掉的请求完成事件将无处归档）
const PENDING_CAP: usize = 500;

// ---------- bodyState 状态机（冻结契约，从第一天就多态） ----------

/// bodyReason 的契约取值（unavailable 时必填；其余状态为 None）
pub const REASON_MIME: &str = "mime";
pub const REASON_TIMEOUT: &str = "timeout";
pub const REASON_NOT_TEXT: &str = "not-text";
pub const REASON_NO_DATA: &str = "no-data";
/// 请求未完成（loadingFailed）或 body 拉取尚未结算（事件驱动，最终一致）
pub const REASON_NOT_FINISHED: &str = "not-finished";
pub const REASON_REDACT_FAILED: &str = "redact-failed";

/// 响应体可用性状态（冻结契约：available | partial | evicted | unavailable）。
/// 多态从第一天就存在——不先做「有/无」再补状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyState {
    /// 全量可用（预算内未截断）
    Available,
    /// 超单条上限被截断（保留实际字节数；分页读可读已保留部分）
    Partial,
    /// 环级总预算驱逐（元数据保留，body 已回收）
    Evicted,
    /// 不可用（bodyReason 必填：mime/timeout/not-text/no-data/not-finished/redact-failed）
    Unavailable,
}

impl BodyState {
    pub fn as_str(&self) -> &'static str {
        match self {
            BodyState::Available => "available",
            BodyState::Partial => "partial",
            BodyState::Evicted => "evicted",
            BodyState::Unavailable => "unavailable",
        }
    }
}

/// store_body 的输入（app 层拉取结果 → 域层结算终态）
#[derive(Debug)]
pub enum BodyPayload {
    /// getResponseBody 原始返回（base64 与否由 CDP 响应告知；解码/校验/脱敏/
    /// 截断在域层 [`settle_body`] 完成——app 层在 DiagBuffer Mutex **之外**执行后
    /// 以 [`SettledBody`] 提交。「落环前脱敏」单一冲口不变：未脱敏内容
    /// 不出现在提交路径）
    Raw { body: String, base64_encoded: bool },
    /// 拉取失败（timeout = 2.5s 预算耗尽；no-data = 排队溢出/连接断/CDP 拒绝/
    /// 空响应；not-text = 解码后非 UTF-8 文本——具体归因在 app 层，域层只存）
    Unavailable { reason: &'static str },
}

/// [`settle_body`] 的产物：锁外重活管线完成后的终态。app 层两段式结算 =
/// 锁内 mime 快照（[`DiagBuffer::pending_body_mime`]）→ 锁外 [`settle_body`]
/// → 锁内提交（[`DiagBuffer::store_body`]）。
#[derive(Debug)]
pub enum SettledBody {
    /// 已脱敏、已按单条上限截断的最终文本。partial = 触发过
    /// [`BODY_ITEM_MAX_BYTES`] 截断（→ bodyState "partial"）
    Text { body: String, partial: bool },
    /// 终态归因（timeout/no-data/not-text/redact-failed）
    Unavailable { reason: &'static str },
}

/// body 结算重活管线（纯函数：无 `&mut self`、无锁，可在任意线程执行）：
/// base64 解码 → UTF-8 校验 → 脱敏 → 单条上限截断。mime 取目标条目快照
/// （决定 JSON tokenizer / 文本模式）。此前整条管线内联在 [`DiagBuffer::store_body`]
/// 的 Mutex 临界区内——大 body 的解码/脱敏会串行卡死事件泵 `apply()` 与
/// console/network/har 全部查询（W-fix2 外移；锁内只剩提交入账与预算驱逐）。
pub fn settle_body(mime: Option<&str>, payload: BodyPayload) -> SettledBody {
    match payload {
        BodyPayload::Unavailable { reason } => SettledBody::Unavailable { reason },
        BodyPayload::Raw {
            body,
            base64_encoded,
        } => {
            let bytes = if base64_encoded {
                match base64::engine::general_purpose::STANDARD.decode(body.as_bytes()) {
                    Ok(b) => b,
                    // base64 非法：CDP 回了不可解码的字节序列，按二进制伪装归 not-text
                    Err(_) => {
                        return SettledBody::Unavailable {
                            reason: REASON_NOT_TEXT,
                        }
                    }
                }
            } else {
                body.into_bytes()
            };
            if bytes.is_empty() {
                return SettledBody::Unavailable {
                    reason: REASON_NO_DATA,
                };
            }
            let Ok(text) = String::from_utf8(bytes) else {
                return SettledBody::Unavailable {
                    reason: REASON_NOT_TEXT,
                };
            };
            let redacted = match redact::redact_body(mime, &text) {
                Ok(r) => r,
                Err(_reject) => {
                    // 拒绝语义优先于部分保留（宁少证据不漏密钥）；具体拒绝型
                    // （Unparseable/TooDeep/Uncertain）统一落 redact-failed，
                    // 诊断级分钟级排障足够
                    return SettledBody::Unavailable {
                        reason: REASON_REDACT_FAILED,
                    };
                }
            };
            if redacted.len() > BODY_ITEM_MAX_BYTES {
                // 字符边界截断（不跨 UTF-8 序列）；保留实际字节数入账
                let mut cut = BODY_ITEM_MAX_BYTES;
                while !redacted.is_char_boundary(cut) {
                    cut -= 1;
                }
                SettledBody::Text {
                    body: redacted[..cut].to_string(),
                    partial: true,
                }
            } else {
                SettledBody::Text {
                    body: redacted,
                    partial: false,
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleEntry {
    pub ts: i64,
    /// error | warning | info | debug | log（Runtime.consoleAPICalled type /
    /// Log.entryAdded level，原样归一小写）
    pub level: String,
    pub text: String,
    /// 来源标记：console（JS console API）或 Log.entryAdded 的 source（如 network）
    pub source: String,
    /// 归属页面 URL（**已脱敏**：session 登记值或 Log 条目 url 冲口脱敏）
    pub target_url: String,
    /// 归属 tab 的 targetId（attach 时登记；None = 归属未知——未 attach/迟到的
    /// console 帧，tab 过滤对这类条目降级不过滤）
    pub target_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkEntry {
    /// 请求开始时刻（wallTime，epoch ms）
    pub ts: i64,
    pub request_id: String,
    pub method: String,
    /// **脱敏后**的 URL（query 敏感参数值已替换；原始 URL 不落进程内存）
    pub url: String,
    pub status: Option<i64>,
    pub mime_type: Option<String>,
    pub failed: bool,
    /// CDP errorText（loadingFailed 时）
    pub error: Option<String>,
    pub duration_ms: Option<i64>,
    /// 归属页面 URL（**已脱敏**：session 登记值经 record_session 冲口）
    pub target_url: String,
    /// 归属 tab 的 targetId（requestWillBeSent 有 session 门卫，必有归属）
    pub target_id: Option<String>,
    /// 响应体可用性（冻结契约，见 [`BodyState`]）
    pub body_state: BodyState,
    /// unavailable 时的原因（其余状态 None，不序列化）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_reason: Option<&'static str>,
    /// 已保留的脱敏 body（不随条目序列化——list 响应只出状态，内容走 body 端点）
    #[serde(skip)]
    pub body: Option<String>,
}

/// 环的证据边界（查询响应/HAR `_meta` 的数据源）：固定容量 + 累计溢出丢弃数。
/// 「诚实证据」契约的一环——调用方必须能看见截断，而非静默少数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RingStats {
    /// 环固定容量
    pub capacity: usize,
    /// 自缓冲创建以来因溢出被丢弃的条目总数（只增不清零）
    pub dropped: u64,
}

/// tab 过滤的执行情况（查询响应 meta 的来源；仅请求了 tab 过滤时出现）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TabFilterMeta {
    /// 请求过滤的 tab（targetId）
    pub filtered_by_tab: String,
    /// true = 环中存在无归属条目，过滤降级为全量（禁止臆造归属）
    pub tab_filter_unavailable: bool,
    /// tab 过滤前的匹配条数（level/failedOnly 过滤之后）
    pub before: usize,
    /// tab 过滤后的匹配条数（limit 截断之前）
    pub after: usize,
}

/// 查询投影结果：过滤后的条目 + 可选的 tab 过滤 meta
pub struct QueryResult<T> {
    pub entries: Vec<T>,
    pub meta: Option<TabFilterMeta>,
}

/// body 分页读取投影（service 层据此组装响应）
#[derive(Debug)]
pub struct NetworkBodyView {
    pub state: BodyState,
    pub reason: Option<&'static str>,
    pub slice: Utf16Slice,
    /// 环内已保留字节数（partial = 截断后实际保留量）
    pub stored_bytes: usize,
}

/// UTF-16 code unit 计量的切片结果（不劈代理对：边界落入代理对时向外取整，
/// 返回量可能比请求 limit 多 1 个 unit）
#[derive(Debug, Default)]
pub struct Utf16Slice {
    pub text: String,
    /// 实际返回的 UTF-16 单位数
    pub returned: usize,
    /// 全文 UTF-16 单位总长
    pub total: usize,
}

impl Utf16Slice {
    fn empty() -> Self {
        Utf16Slice {
            text: String::new(),
            returned: 0,
            total: 0,
        }
    }
}

/// UTF-16 窗口切片：取与 [offset, offset+limit) **有交集**的字符（区间重叠
/// 规则 = 起止边界同时向外取整，代理对永不劈开；limit=0 或 offset 出界 → 空）
pub fn slice_utf16(text: &str, offset: usize, limit: usize) -> Utf16Slice {
    let mut total = 0usize;
    let mut out = String::new();
    let mut returned = 0usize;
    let mut pos = 0usize;
    for c in text.chars() {
        let len = c.len_utf16();
        let end = pos + len;
        // 字符的 [pos, end) 与窗口 [offset, offset+limit) 有交集则包含
        if limit > 0 && pos < offset + limit && end > offset {
            out.push(c);
            returned += len;
        }
        pos = end;
        total += len;
    }
    Utf16Slice {
        text: out,
        returned,
        total,
    }
}

/// attach/detach 的信号（应用层据此发送 per-session enable 命令）；
/// BodyFetch = 该请求应拉响应体（文本 mime 且成功完成，并发/超时预算在应用层）
#[derive(Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    None,
    Attach(String),
    Detach(String),
    BodyFetch {
        session_id: String,
        request_id: String,
    },
}

/// attach 登记的 session 归属：tab 的 targetId + attach 时的页面 URL
/// （**已脱敏**——record_session 冲口统一处理，所有派生消费面免信任）
#[derive(Debug, Clone)]
struct SessionInfo {
    target_id: String,
    target_url: String,
}

#[derive(Default)]
pub struct DiagBuffer {
    console: VecDeque<ConsoleEntry>,
    network: VecDeque<NetworkEntry>,
    /// 环溢出累计丢弃数（锁内累加；查询时随 meta 透出）
    console_dropped: u64,
    network_dropped: u64,
    /// session_id → 归属信息（attach 时登记，detach 移除；条目留档）
    sessions: HashMap<String, SessionInfo>,
    /// (session_id, request_id) → 进行中的请求；完成/失败时结算入环
    pending: HashMap<(String, String), PendingRequest>,
    /// 环内已保留 body 的字节总量（分层驱逐的预算账本）
    body_bytes: usize,
}

#[derive(Debug)]
struct PendingRequest {
    entry: NetworkEntry,
    /// CDP 单调时钟秒（requestWillBeSent.timestamp），与 loadingFinished.timestamp
    /// 同源，差值即总时长
    mono_start: f64,
}

impl DiagBuffer {
    pub fn record_session(&mut self, session_id: &str, target_id: &str, target_url: &str) {
        self.sessions.insert(
            session_id.to_string(),
            SessionInfo {
                target_id: target_id.to_string(),
                // 落环前脱敏（与 requestWillBeSent 同一契约）：attach 时刻的
                // 页面/OOPIF iframe URL 常带 SSO token、OAuth code——session
                // URL 派生出 console 条目 targetUrl、network 条目 targetUrl
                // 与 HAR _targetUrl，原始值一旦入环会随查询/HAR 反复外泄
                target_url: redact::redact_url(target_url),
            },
        );
    }

    pub fn drop_session(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
        self.pending.retain(|(sid, _), _| sid != session_id);
    }

    fn session_of(&self, session_id: &str) -> Option<&SessionInfo> {
        self.sessions.get(session_id)
    }

    fn push_console(&mut self, entry: ConsoleEntry) {
        self.console.push_back(entry);
        if self.console.len() > CONSOLE_RING {
            self.console.pop_front();
            self.console_dropped += 1;
        }
    }

    /// 应用一帧 CDP 事件。`now_ms` 为应用层接收时刻（console 条目的 wall clock）。
    pub fn apply(&mut self, event: &Value, now_ms: i64) -> ApplyOutcome {
        let method = event.get("method").and_then(Value::as_str).unwrap_or("");
        let session_id = event
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = event.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "Target.attachedToTarget" => {
                // flatten 模式：嵌套 attach 的 sessionId 在 params 内
                let sid = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let info = params.get("targetInfo").cloned().unwrap_or(Value::Null);
                let url = info
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                // tab 级过滤的归属键：targetId 在 tab 生命周期内稳定（导航不变），
                // 与 tabs/navigate 的对外 tab id 同一标识
                let tid = info
                    .get("targetId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if sid.is_empty() {
                    return ApplyOutcome::None;
                }
                self.record_session(&sid, &tid, &url);
                ApplyOutcome::Attach(sid)
            }
            "Target.detachedFromTarget" => {
                let sid = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if sid.is_empty() {
                    return ApplyOutcome::None;
                }
                self.drop_session(&sid);
                ApplyOutcome::Detach(sid)
            }
            "Runtime.consoleAPICalled" => {
                let level = params.get("type").and_then(Value::as_str).unwrap_or("log");
                let text = params
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|args| args.iter().map(format_arg).collect::<Vec<_>>().join(" "))
                    .unwrap_or_default();
                let session = self.session_of(&session_id);
                self.push_console(ConsoleEntry {
                    ts: now_ms,
                    level: level.to_lowercase(),
                    text,
                    source: "console".into(),
                    target_url: session.map(|s| s.target_url.clone()).unwrap_or_default(),
                    target_id: session.map(|s| s.target_id.clone()),
                });
                ApplyOutcome::None
            }
            "Log.entryAdded" => {
                let entry = params.get("entry").cloned().unwrap_or(Value::Null);
                let level = entry.get("level").and_then(Value::as_str).unwrap_or("info");
                let text = entry
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let source = entry.get("source").and_then(Value::as_str).unwrap_or("log");
                let session = self.session_of(&session_id);
                self.push_console(ConsoleEntry {
                    ts: now_ms,
                    level: level.to_lowercase(),
                    text: text.to_string(),
                    source: source.to_string(),
                    // 落环前脱敏：CDP 网络资源加载失败日志的 url 携完整
                    // query（签名 CDN、token 化接口极常见），与请求 URL 同权
                    target_url: entry
                        .get("url")
                        .and_then(Value::as_str)
                        .map(redact::redact_url)
                        .unwrap_or_else(|| {
                            session.map(|s| s.target_url.clone()).unwrap_or_default()
                        }),
                    target_id: session.map(|s| s.target_id.clone()),
                });
                ApplyOutcome::None
            }
            "Network.requestWillBeSent" => {
                let request_id = params
                    .get("requestId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(request) = params.get("request") else {
                    return ApplyOutcome::None;
                };
                // 未知 session（尚未 attach 或已 detach 的迟到帧）不归档，防悬挂
                if request_id.is_empty()
                    || !self.sessions.contains_key(&session_id)
                    || self.pending.len() >= PENDING_CAP
                {
                    return ApplyOutcome::None;
                }
                let ts = (params
                    .get("wallTime")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    * 1000.0) as i64;
                let session = self.session_of(&session_id);
                self.pending.insert(
                    (session_id.clone(), request_id.to_string()),
                    PendingRequest {
                        entry: NetworkEntry {
                            ts,
                            request_id: request_id.to_string(),
                            method: request
                                .get("method")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            // 落环前脱敏：原始 URL 不进进程内存（常驻环会被
                            // 查询/HAR 反复外泄，单向脱敏让查得到的一定安全）
                            url: redact::redact_url(
                                request
                                    .get("url")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default(),
                            ),
                            status: None,
                            mime_type: None,
                            failed: false,
                            error: None,
                            duration_ms: None,
                            target_url: session.map(|s| s.target_url.clone()).unwrap_or_default(),
                            target_id: session.map(|s| s.target_id.clone()),
                            body_state: BodyState::Unavailable,
                            body_reason: Some(REASON_NOT_FINISHED),
                            body: None,
                        },
                        mono_start: params
                            .get("timestamp")
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0),
                    },
                );
                ApplyOutcome::None
            }
            "Network.responseReceived" => {
                if let Some(mut req) = self.take_pending(&session_id, &params) {
                    let response = params.get("response").cloned().unwrap_or(Value::Null);
                    req.entry.status = response.get("status").and_then(Value::as_i64);
                    req.entry.mime_type = response
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    // 回填 pending（响应到达≠请求结束，仍待 finished/failed 结算）
                    self.pending
                        .insert((session_id, req.entry.request_id.clone()), req);
                }
                ApplyOutcome::None
            }
            "Network.loadingFinished" => {
                if let Some(mut req) = self.take_pending(&session_id, &params) {
                    let end = params
                        .get("timestamp")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    req.entry.duration_ms =
                        Some(((end - req.mono_start).max(0.0) * 1000.0).round() as i64);
                    // 初始 bodyState：文本 mime → not-finished（事件驱动拉取中，
                    // 最终一致）；非文本/未知 mime → mime（永不拉取）。多态从
                    // 第一天就存在——查询看到的就是这四个状态之一
                    let text_mime = req
                        .entry
                        .mime_type
                        .as_deref()
                        .map(redact::is_text_mime)
                        .unwrap_or(false);
                    if text_mime {
                        req.entry.body_state = BodyState::Unavailable;
                        req.entry.body_reason = Some(REASON_NOT_FINISHED);
                    } else {
                        req.entry.body_state = BodyState::Unavailable;
                        req.entry.body_reason = Some(REASON_MIME);
                    }
                    let outcome = if text_mime {
                        ApplyOutcome::BodyFetch {
                            session_id: session_id.clone(),
                            request_id: req.entry.request_id.clone(),
                        }
                    } else {
                        ApplyOutcome::None
                    };
                    self.settle(req.entry);
                    outcome
                } else {
                    ApplyOutcome::None
                }
            }
            "Network.loadingFailed" => {
                if let Some(mut req) = self.take_pending(&session_id, &params) {
                    req.entry.failed = true;
                    req.entry.error = params
                        .get("errorText")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let end = params
                        .get("timestamp")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    req.entry.duration_ms =
                        Some(((end - req.mono_start).max(0.0) * 1000.0).round() as i64);
                    // 请求未完成：响应体永不可得
                    req.entry.body_state = BodyState::Unavailable;
                    req.entry.body_reason = Some(REASON_NOT_FINISHED);
                    self.settle(req.entry);
                }
                ApplyOutcome::None
            }
            _ => ApplyOutcome::None,
        }
    }

    fn take_pending(&mut self, session_id: &str, params: &Value) -> Option<PendingRequest> {
        let request_id = params.get("requestId").and_then(Value::as_str)?;
        self.pending
            .remove(&(session_id.to_string(), request_id.to_string()))
    }

    fn settle(&mut self, entry: NetworkEntry) {
        self.network.push_back(entry);
        while self.network.len() > NETWORK_RING {
            if let Some(dropped) = self.network.pop_front() {
                if let Some(body) = &dropped.body {
                    self.body_bytes = self.body_bytes.saturating_sub(body.len());
                }
                self.network_dropped += 1;
            }
        }
    }

    /// 锁外结算的前置快照：目标条目（最新 not-finished 同名 requestId，与
    /// [`store_body`] 同一定位规则）的 mime。外层 None = 无待结算条目（已
    /// 结算/被环溢出挤出），调用方跳过重活与提交。mime 决定脱敏走 JSON
    /// tokenizer 还是文本模式。注：跨 session requestId 碰撞的极小窗口内
    /// 快照与提交目标可能错位（先到先结算本身即任意序），按快照 mime 结算。
    pub fn pending_body_mime(&self, request_id: &str) -> Option<Option<String>> {
        self.network
            .iter()
            .rev()
            .find(|e| {
                e.request_id == request_id
                    && e.body_state == BodyState::Unavailable
                    && e.body_reason == Some(REASON_NOT_FINISHED)
            })
            .map(|e| e.mime_type.clone())
    }

    /// body 结算提交（锁内轻活，与锁外 [`settle_body`] 两段式）：定位目标
    /// 条目 = 环内最新一个该 requestId 且仍处 not-finished 待结算态
    /// （requestId 理论上可跨 session 碰撞，先到先结算；已被环溢出挤出的条目
    /// 找不到则丢弃——诚实丢弃，无处可写）→ 终态写入 → 字节入账 → 预算
    /// 驱逐。驱逐决策基于锁内最新 body_bytes 重验（快照-计算-回锁提交期间
    /// 账本可能被并发结算推进），最旧先驱语义不变。
    pub fn store_body(&mut self, request_id: &str, settled: SettledBody) {
        let Some(entry) = self.network.iter_mut().rev().find(|e| {
            e.request_id == request_id
                && e.body_state == BodyState::Unavailable
                && e.body_reason == Some(REASON_NOT_FINISHED)
        }) else {
            return;
        };
        match settled {
            SettledBody::Unavailable { reason } => {
                entry.body_reason = Some(reason);
            }
            SettledBody::Text { body, partial } => {
                self.body_bytes += body.len();
                entry.body = Some(body);
                entry.body_state = if partial {
                    BodyState::Partial
                } else {
                    BodyState::Available
                };
                entry.body_reason = None;
                self.enforce_body_budget();
            }
        }
    }

    /// 分层驱逐：body 总预算超限时从**最旧**条目开始回收 body（元数据永不
    /// 驱逐，bodyState → evicted）。与环容量驱逐并行起作用：环驱条目，
    /// 预算驱 body——结构证据完整，重的 body 可回收。
    fn enforce_body_budget(&mut self) {
        while self.body_bytes > BODY_TOTAL_MAX_BYTES {
            let (idx, len) = match self
                .network
                .iter()
                .enumerate()
                .find(|(_, e)| e.body.is_some())
            {
                Some((i, e)) => (i, e.body.as_ref().map_or(0, |b| b.len())),
                // body_bytes 超限却无 body 可驱：账本与环不一致（不可达——
                // 入账与回收同锁同点），归零防御防死循环
                None => {
                    self.body_bytes = 0;
                    return;
                }
            };
            let e = &mut self.network[idx];
            e.body = None;
            e.body_state = BodyState::Evicted;
            e.body_reason = None;
            self.body_bytes = self.body_bytes.saturating_sub(len);
        }
    }

    /// body 读取投影：requestId 最新命中条目（跨 session 同名 id 时取最新，
    /// 诚实语义已在 AGENT-API 声明）。返回 None = requestId 不在环内。
    pub fn network_body(
        &self,
        request_id: &str,
        offset: usize,
        limit: usize,
    ) -> Option<NetworkBodyView> {
        let entry = self
            .network
            .iter()
            .rev()
            .find(|e| e.request_id == request_id)?;
        let stored_bytes = entry.body.as_ref().map_or(0, |b| b.len());
        let slice = match &entry.body {
            Some(text) => slice_utf16(text, offset, limit),
            None => Utf16Slice::empty(),
        };
        Some(NetworkBodyView {
            state: entry.body_state,
            reason: entry.body_reason,
            slice,
            stored_bytes,
        })
    }

    /// console 环的证据边界（响应 meta 数据源）
    pub fn console_stats(&self) -> RingStats {
        RingStats {
            capacity: CONSOLE_RING,
            dropped: self.console_dropped,
        }
    }

    /// network 环的证据边界（响应 meta / HAR `_meta` 数据源）
    pub fn network_stats(&self) -> RingStats {
        RingStats {
            capacity: NETWORK_RING,
            dropped: self.network_dropped,
        }
    }

    /// 截图目标选择：第一个 http(s) 页面 session（与 M5 查询端点同语义）
    pub fn pick_page_session(&self) -> Option<String> {
        self.sessions
            .iter()
            .find(|(_, info)| {
                info.target_url.starts_with("http://") || info.target_url.starts_with("https://")
            })
            .map(|(sid, _)| sid.clone())
    }

    /// console 查询投影：level 精确过滤（None = 全量）+ tab 归属过滤，
    /// 时间正序取尾部 limit 条。先过滤后切片（环上限 500，收集成本可忽略；
    /// `rev().take().rev()` 要求 DoubleEnded 而 Filter 不满足）
    pub fn console_entries(
        &self,
        level: Option<&str>,
        tab: Option<&str>,
        limit: usize,
    ) -> QueryResult<ConsoleEntry> {
        let matched: Vec<&ConsoleEntry> = self
            .console
            .iter()
            .filter(|e| level.is_none_or(|l| e.level == l))
            .collect();
        project(matched, tab, limit, |e| e.target_id.as_deref())
    }

    /// network 查询投影：failedOnly = 失败请求或 HTTP ≥400 + tab 归属过滤，
    /// 时间正序取尾部 limit 条
    pub fn network_entries(
        &self,
        failed_only: bool,
        tab: Option<&str>,
        limit: usize,
    ) -> QueryResult<NetworkEntry> {
        let matched: Vec<&NetworkEntry> = self
            .network
            .iter()
            .filter(|e| !failed_only || e.failed || e.status.unwrap_or(0) >= 400)
            .collect();
        project(matched, tab, limit, |e| e.target_id.as_deref())
    }

    /// HAR 构建的锁内快照（轻：条目克隆即返回）。HAR 组装与 base64 编码由
    /// 调用方持快照在锁外经 [`build_har`] 完成——与 body 结算同纪律：重活
    /// 不进 DiagBuffer Mutex（事件泵与查询不被 HAR 生成阻塞）。快照与
    /// [`Self::network_stats`] 同锁获取即为一致的点时视图（此后到达的结算
    /// 不出现在本次 HAR 中）。
    pub fn har_snapshot(&self) -> Vec<NetworkEntry> {
        self.network.iter().cloned().collect()
    }
}

/// HAR 1.2 组装（纯函数，锁外执行；har-recorder 的正式落地点）。未捕获的
/// 字段按规范以 -1/空集占位；`_targetUrl` 为自定义字段标注条目归属的页面
/// 目标（CDP session 归属）；body 可用（available/partial）时
/// `response.content.text` 填**脱敏后**内容（含非 ASCII → base64 + encoding，
/// HAR 惯例），否则不填 text（HAR 兼容占位，bodyState 语义见 network 查询）。
pub fn build_har(entries: &[NetworkEntry], creator_version: &str) -> Value {
    let entries: Vec<Value> = entries
        .iter()
        .map(|e| {
            let mut content = json!({
                "size": e.body.as_ref().map_or(0, |b| b.len()),
                "mimeType": e.mime_type.clone().unwrap_or_default(),
            });
            if let Some(body) = &e.body {
                if body.is_ascii() {
                    content["text"] = json!(body);
                } else {
                    content["text"] = json!(
                        base64::engine::general_purpose::STANDARD.encode(body.as_bytes())
                    );
                    content["encoding"] = json!("base64");
                }
            }
            json!({
                "startedDateTime": iso_ms(e.ts),
                "time": e.duration_ms.unwrap_or(0),
                "request": {
                    "method": e.method,
                    "url": e.url,
                    "httpVersion": "unknown",
                    "headers": [],
                    "queryString": [],
                    "cookies": [],
                    "headersSize": -1,
                    "bodySize": -1,
                },
                "response": {
                    "status": e.status.unwrap_or(0),
                    "statusText": if e.failed { e.error.clone().unwrap_or_default() } else { String::new() },
                    "httpVersion": "unknown",
                    "headers": [],
                    "content": content,
                    "redirectURL": "",
                    "headersSize": -1,
                    "bodySize": -1,
                },
                "cache": {},
                "timings": {
                    "send": -1, "wait": -1, "receive": e.duration_ms.unwrap_or(-1),
                    "dns": -1, "connect": -1, "ssl": -1,
                    "comment": "chrome-host 常驻订阅仅记录总时长，分阶段耗时未采集",
                },
                "_targetUrl": e.target_url,
            })
        })
        .collect();
    json!({
        "log": {
            "version": "1.2",
            "creator": { "name": "chrome-host", "version": creator_version },
            "pages": [],
            "entries": entries,
        }
    })
}

/// tab 过滤投影的公共尾部：归属完整 → 精确过滤；环中存在无归属条目 → 降级
/// 全量并标注 `tabFilterUnavailable`（禁止臆造归属）；未请求 tab → meta 为 None。
fn project<E: Clone>(
    matched: Vec<&E>,
    tab: Option<&str>,
    limit: usize,
    tab_of: impl Fn(&E) -> Option<&str>,
) -> QueryResult<E> {
    let Some(tab) = tab else {
        return QueryResult {
            entries: tail(&matched, limit),
            meta: None,
        };
    };
    if matched.iter().any(|e| tab_of(e).is_none()) {
        let n = matched.len();
        return QueryResult {
            entries: tail(&matched, limit),
            meta: Some(TabFilterMeta {
                filtered_by_tab: tab.to_string(),
                tab_filter_unavailable: true,
                before: n,
                after: n,
            }),
        };
    }
    let before = matched.len();
    let kept: Vec<&E> = matched
        .into_iter()
        .filter(|e| tab_of(e) == Some(tab))
        .collect();
    let after = kept.len();
    QueryResult {
        entries: tail(&kept, limit),
        meta: Some(TabFilterMeta {
            filtered_by_tab: tab.to_string(),
            tab_filter_unavailable: false,
            before,
            after,
        }),
    }
}

/// 尾部 limit 条切片（时间正序）
fn tail<E: Clone>(matched: &[&E], limit: usize) -> Vec<E> {
    let start = matched.len().saturating_sub(limit);
    matched[start..].iter().map(|e| (*e).clone()).collect()
}

/// console 参数值格式化：原始值优先，对象/函数取 description，其余以类型占位
fn format_arg(arg: &Value) -> String {
    if let Some(v) = arg.get("value") {
        match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    } else {
        arg.get("description")
            .and_then(Value::as_str)
            .or_else(|| arg.get("subtype").and_then(Value::as_str))
            .unwrap_or_else(|| arg.get("type").and_then(Value::as_str).unwrap_or("?"))
            .to_string()
    }
}

fn iso_ms(ts: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESS: &str = "sess-1";
    const TAB: &str = "tab-1";

    fn attached(url: &str) -> Value {
        json!({
            "method": "Target.attachedToTarget",
            "params": { "sessionId": SESS, "targetInfo": { "targetId": TAB, "type": "page", "url": url } }
        })
    }

    /// 指定 session/targetId 的 attach（多 tab 场景）
    fn attached_as(sess: &str, tab: &str, url: &str) -> Value {
        json!({
            "method": "Target.attachedToTarget",
            "params": { "sessionId": sess, "targetInfo": { "targetId": tab, "type": "page", "url": url } }
        })
    }

    fn req(request_id: &str, url: &str) -> Value {
        json!({
            "method": "Network.requestWillBeSent",
            "sessionId": SESS,
            "params": {
                "requestId": request_id,
                "wallTime": 1710000000.5,
                "timestamp": 100.0,
                "request": { "method": "GET", "url": url }
            }
        })
    }

    /// 全链路：attach → console → 请求 200 完成 / 404 完成 / 失败 → 查询投影 + HAR
    #[test]
    fn full_lifecycle_console_and_network_rings() {
        let mut buf = DiagBuffer::default();
        assert_eq!(
            buf.apply(&attached("https://app.example.com/"), 1),
            ApplyOutcome::Attach(SESS.into())
        );

        buf.apply(
            &json!({
                "method": "Runtime.consoleAPICalled", "sessionId": SESS,
                "params": { "type": "error", "args": [
                    { "type": "string", "value": "boom" },
                    { "type": "object", "description": "{a: 1}" }
                ] }
            }),
            42,
        );
        buf.apply(
            &json!({
                "method": "Log.entryAdded", "sessionId": SESS,
                "params": { "entry": { "level": "error", "source": "network", "text": "Failed to load resource: 404", "url": "https://app.example.com/x" } }
            }),
            43,
        );

        // 三个请求：正常 / 404 / 失败（errorText）
        buf.apply(&req("r1", "https://a/api/ok"), 1);
        buf.apply(&json!({
            "method": "Network.responseReceived", "sessionId": SESS,
            "params": { "requestId": "r1", "response": { "status": 200, "mimeType": "application/json" } }
        }), 1);
        buf.apply(
            &json!({
                "method": "Network.loadingFinished", "sessionId": SESS,
                "params": { "requestId": "r1", "timestamp": 100.25 }
            }),
            1,
        );

        buf.apply(&req("r2", "https://a/missing"), 1);
        buf.apply(&json!({
            "method": "Network.responseReceived", "sessionId": SESS,
            "params": { "requestId": "r2", "response": { "status": 404, "mimeType": "text/plain" } }
        }), 1);
        buf.apply(
            &json!({
                "method": "Network.loadingFinished", "sessionId": SESS,
                "params": { "requestId": "r2", "timestamp": 101.0 }
            }),
            1,
        );

        buf.apply(&req("r3", "https://a/dead"), 1);
        buf.apply(&json!({
            "method": "Network.loadingFailed", "sessionId": SESS,
            "params": { "requestId": "r3", "errorText": "net::ERR_CONNECTION_REFUSED", "timestamp": 102.0 }
        }), 1);

        // console：error 过滤命中两条（console API + Log 网络错误）；target 归属正确
        let errs = buf.console_entries(Some("error"), None, 100).entries;
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert_eq!(
            errs[0].text, "boom {a: 1}",
            "args 拼接：原始值 + description"
        );
        assert_eq!(errs[0].source, "console");
        assert_eq!(
            errs[0].target_id.as_deref(),
            Some(TAB),
            "归属 = attach 登记的 targetId"
        );
        assert_eq!(errs[1].source, "network");
        assert!(errs[1].target_url.contains("app.example.com"));
        assert!(buf
            .console_entries(Some("info"), None, 100)
            .entries
            .is_empty());

        // network：全量 3 条；failedOnly 命中 404 + 失败
        let all = buf.network_entries(false, None, 100).entries;
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].status, Some(200));
        assert_eq!(all[0].duration_ms, Some(250), "100.25-100.0 = 250ms");
        assert_eq!(all[0].target_url, "https://app.example.com/");
        assert_eq!(all[0].target_id.as_deref(), Some(TAB));
        let failed = buf.network_entries(true, None, 100).entries;
        assert_eq!(failed.len(), 2);
        assert!(failed
            .iter()
            .all(|e| e.url.ends_with("missing") || e.url.ends_with("dead")));
        assert_eq!(
            failed[1].error.as_deref(),
            Some("net::ERR_CONNECTION_REFUSED")
        );

        // HAR：版本/creator/条目映射/未知量占位
        let har = build_har(&buf.har_snapshot(), "0.2.3");
        assert_eq!(har["log"]["version"], "1.2");
        assert_eq!(har["log"]["creator"]["name"], "chrome-host");
        assert_eq!(har["log"]["entries"].as_array().unwrap().len(), 3);
        let e0 = &har["log"]["entries"][0];
        assert_eq!(e0["request"]["method"], "GET");
        assert_eq!(e0["response"]["status"], 200);
        assert_eq!(e0["response"]["content"]["mimeType"], "application/json");
        assert_eq!(e0["time"], 250);
        assert_eq!(e0["startedDateTime"], "2024-03-09T16:00:00.500Z");
        assert_eq!(e0["request"]["headersSize"], -1, "未采集字段按规范占位");
        assert_eq!(
            har["log"]["entries"][2]["response"]["status"], 0,
            "失败请求无响应状态"
        );

        // detach：session 移除，已入环条目保留
        buf.apply(
            &json!({ "method": "Target.detachedFromTarget", "params": { "sessionId": SESS } }),
            2,
        );
        assert_eq!(
            buf.network_entries(false, None, 100).entries.len(),
            3,
            "历史条目留档"
        );
        buf.apply(&req("r4", "https://a/after-detach"), 3);
        buf.apply(
            &json!({ "method": "Network.loadingFinished", "sessionId": SESS, "params": { "requestId": "r4", "timestamp": 1.0 } }),
            3,
        );
        assert_eq!(
            buf.network_entries(false, None, 100).entries.len(),
            3,
            "detach 后无 session 归属，pending 已随 detach 清理"
        );
    }

    /// 环形上限：console 500 / network 1000，超限丢最旧；溢出计数随 meta 透出
    #[test]
    fn rings_are_bounded() {
        let mut buf = DiagBuffer::default();
        assert_eq!(
            buf.console_stats(),
            RingStats {
                capacity: 500,
                dropped: 0
            },
            "新缓冲无丢弃"
        );
        buf.apply(&attached("https://a/"), 0);
        for i in 0..600 {
            buf.apply(
                &json!({ "method": "Runtime.consoleAPICalled", "sessionId": SESS,
                    "params": { "type": "log", "args": [{ "type": "string", "value": format!("m{i}") }] } }),
                i,
            );
        }
        assert_eq!(buf.console_entries(None, None, 1000).entries.len(), 500);
        let oldest = buf.console_entries(None, None, 1000).entries[0]
            .text
            .clone();
        assert_eq!(oldest, "m100", "最旧被挤出的正是环形起点: {oldest}");
        assert_eq!(
            buf.console_stats().dropped,
            100,
            "600 条入 500 环，累计丢弃 100"
        );

        for i in 0..1100 {
            buf.apply(&req(&format!("r{i}"), &format!("https://a/{i}")), 0);
            buf.apply(
                &json!({ "method": "Network.loadingFinished", "sessionId": SESS,
                    "params": { "requestId": format!("r{i}"), "timestamp": 1.0 } }),
                0,
            );
        }
        assert_eq!(buf.network_entries(false, None, 2000).entries.len(), 1000);
        assert_eq!(
            buf.network_stats(),
            RingStats {
                capacity: 1000,
                dropped: 100
            },
            "1100 条入 1000 环，累计丢弃 100"
        );
        // limit 投影：尾部 N 条
        assert_eq!(buf.console_entries(None, None, 7).entries.len(), 7);
        // 丢弃计数不影响存活条目语义：最旧存活请求正是被挤出后的环形起点
        assert!(buf.network_entries(false, None, 1).entries[0]
            .url
            .ends_with("a/1099"));
    }

    /// tab 级过滤：多 tab 各自 attach，条目按 targetId 精确过滤，meta 带 before/after
    #[test]
    fn tab_filter_selects_entries_by_target_id() {
        let mut buf = DiagBuffer::default();
        // tab-a 用 SESS（复用 req 帮手），tab-b 另开 session
        buf.apply(&attached_as(SESS, "tab-a", "https://a.example.com/"), 0);
        buf.apply(&attached_as("sess-b", "tab-b", "https://b.example.com/"), 0);
        buf.apply(
            &json!({ "method": "Runtime.consoleAPICalled", "sessionId": SESS,
                "params": { "type": "log", "args": [{ "type": "string", "value": "from a" }] } }),
            1,
        );
        buf.apply(
            &json!({ "method": "Runtime.consoleAPICalled", "sessionId": "sess-b",
                "params": { "type": "log", "args": [{ "type": "string", "value": "from b" }] } }),
            1,
        );
        // 每 tab 各一个请求并完成
        buf.apply(&req("r1", "https://a/api"), 1);
        buf.apply(
            &json!({ "method": "Network.loadingFinished", "sessionId": SESS, "params": { "requestId": "r1", "timestamp": 1.0 } }),
            1,
        );
        buf.apply(
            &json!({ "method": "Network.requestWillBeSent", "sessionId": "sess-b",
                "params": { "requestId": "r9", "wallTime": 1.0, "timestamp": 1.0, "request": { "method": "GET", "url": "https://b/api" } } }),
            1,
        );
        buf.apply(
            &json!({ "method": "Network.loadingFinished", "sessionId": "sess-b", "params": { "requestId": "r9", "timestamp": 1.0 } }),
            1,
        );

        // console 按 tab-a 过滤：只回 tab-a 的条目，before=2 after=1
        let result = buf.console_entries(None, Some("tab-a"), 100);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].text, "from a");
        let meta = result.meta.expect("请求了 tab 过滤则必有 meta");
        assert_eq!(meta.filtered_by_tab, "tab-a");
        assert!(!meta.tab_filter_unavailable);
        assert_eq!((meta.before, meta.after), (2, 1));

        // network 按 tab-b 过滤：只回 tab-b 的请求
        let result = buf.network_entries(false, Some("tab-b"), 100);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].url, "https://b/api");
        assert_eq!(result.meta.as_ref().expect("meta").after, 1);

        // 未知 tab：正常过滤（结果为空），不降级
        let result = buf.console_entries(None, Some("tab-zzz"), 100);
        assert!(result.entries.is_empty());
        assert_eq!(result.meta.unwrap().after, 0);

        // 不带 tab：meta 为 None，返回全量
        let result = buf.console_entries(None, None, 100);
        assert_eq!(result.entries.len(), 2);
        assert!(result.meta.is_none());
    }

    /// 归属缺失降级：未 attach 的 console 帧入环无归属（target_id=None）→
    /// 带 tab 查询不过滤、返回全量并标注 tabFilterUnavailable；network 侧因
    /// requestWillBeSent 有 session 门卫，无归属帧直接丢弃，不存在降级路径
    #[test]
    fn tab_filter_degrades_when_attribution_missing() {
        let mut buf = DiagBuffer::default();
        buf.apply(&attached_as("sess-a", "tab-a", "https://a.example.com/"), 0);
        buf.apply(
            &json!({ "method": "Runtime.consoleAPICalled", "sessionId": "sess-a",
                "params": { "type": "log", "args": [{ "type": "string", "value": "有归属" }] } }),
            1,
        );
        // 归属缺失路径：session 未登记（迟到/未 attach）的 console 帧仍入环
        buf.apply(
            &json!({ "method": "Runtime.consoleAPICalled", "sessionId": "ghost",
                "params": { "type": "log", "args": [{ "type": "string", "value": "无归属" }] } }),
            2,
        );
        let all = buf.console_entries(None, None, 100).entries;
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].target_id.as_deref(), Some("tab-a"));
        assert_eq!(all[1].target_id, None, "未登记 session 的条目无归属");

        // 带 tab 查询：降级全量 + 标注，禁止臆造归属把无归属条目静默丢弃
        let result = buf.console_entries(None, Some("tab-a"), 100);
        assert_eq!(result.entries.len(), 2, "降级返回全部");
        let meta = result.meta.unwrap();
        assert!(meta.tab_filter_unavailable);
        assert_eq!((meta.before, meta.after), (2, 2));

        // network：无 session 的 requestWillBeSent 被门卫丢弃，环不为无归属条目所累
        buf.apply(
            &json!({ "method": "Network.requestWillBeSent", "sessionId": "ghost",
                "params": { "requestId": "rx", "wallTime": 1.0, "timestamp": 1.0, "request": { "method": "GET", "url": "https://x/" } } }),
            3,
        );
        let result = buf.network_entries(false, Some("tab-a"), 100);
        assert!(result.entries.is_empty());
        assert!(
            !result.meta.unwrap().tab_filter_unavailable,
            "network 环全归属，不降级"
        );
    }

    // ---------- 环内 URL 全量脱敏 ----------

    /// 环内非请求 URL 一并脱敏：attach 页面 URL（targetUrl——含 OOPIF iframe
    /// 的 SSO token 场景）与 Log.entryAdded 的 entry.url（签名 CDN 查询串）
    /// 都在落环前过 redact_url；console 回填、network targetUrl、HAR
    /// _targetUrl 全部派生自已脱敏的 session 登记
    #[test]
    fn ring_urls_beyond_requests_are_redacted() {
        let mut buf = DiagBuffer::default();
        buf.apply(
            &attached("https://sso.example.com/frame?token=sec1"),
            1,
        );
        // console 条目回填的 targetUrl 来自 session 登记：已脱敏
        buf.apply(
            &json!({ "method": "Runtime.consoleAPICalled", "sessionId": SESS,
                "params": { "type": "log", "args": [{ "type": "string", "value": "hi" }] } }),
            2,
        );
        let c = &buf.console_entries(None, None, 10).entries[0];
        assert_eq!(c.target_url, "https://sso.example.com/frame?token=[redacted]");

        // Log.entryAdded 自带 url：脱敏后落环（敏感参数替换，非敏感参数原样）
        buf.apply(
            &json!({
                "method": "Log.entryAdded", "sessionId": SESS,
                "params": { "entry": { "level": "error", "source": "network",
                    "text": "Failed to load resource: 404",
                    "url": "https://cdn.example.com/signed.js?Signature=abc123&v=2" } }
            }),
            3,
        );
        let c = &buf.console_entries(None, None, 10).entries[1];
        assert_eq!(c.target_url, "https://cdn.example.com/signed.js?Signature=[redacted]&v=2");

        // network 条目 targetUrl 与 HAR _targetUrl：同一脱敏源（session 登记）
        buf.apply(&req("r1", "https://a/api?token=z"), 4);
        buf.apply(
            &json!({ "method": "Network.loadingFinished", "sessionId": SESS,
                "params": { "requestId": "r1", "timestamp": 1.0 } }),
            4,
        );
        let e = &buf.network_entries(false, None, 10).entries[0];
        assert_eq!(e.target_url, "https://sso.example.com/frame?token=[redacted]");
        let har = build_har(&buf.network_entries(false, None, 100).entries, "t");
        assert_eq!(
            har["log"]["entries"][0]["_targetUrl"],
            "https://sso.example.com/frame?token=[redacted]"
        );
    }

    // ---------- body 捕获与 bodyState 状态机 ----------

    /// 两段式结算帮手（生产路径同构）：锁内 mime 快照 → 锁外结算 → 锁内提交
    fn settle_and_store(buf: &mut DiagBuffer, rid: &str, payload: BodyPayload) {
        let mime = buf.pending_body_mime(rid).expect("条目应处待结算态");
        buf.store_body(rid, settle_body(mime.as_deref(), payload));
    }

    /// 全链路事件喂入：文本 mime 完成 → BodyFetch 信号 + not-finished 初态；
    /// 非文本 mime / 失败请求 → 无信号 + mime/not-finished 终态（永不拉取）
    #[test]
    fn body_fetch_signal_and_initial_states() {
        let mut buf = DiagBuffer::default();
        buf.apply(&attached("https://a/"), 0);

        // json mime：完成 → 信号
        buf.apply(&req("r1", "https://a/api"), 0);
        buf.apply(
            &json!({ "method": "Network.responseReceived", "sessionId": SESS,
                "params": { "requestId": "r1", "response": { "status": 200, "mimeType": "application/json" } } }),
            0,
        );
        assert_eq!(
            buf.apply(
                &json!({ "method": "Network.loadingFinished", "sessionId": SESS,
                    "params": { "requestId": "r1", "timestamp": 1.0 } }),
                0,
            ),
            ApplyOutcome::BodyFetch {
                session_id: SESS.into(),
                request_id: "r1".into()
            }
        );
        let e = &buf.network_entries(false, None, 10).entries[0];
        assert_eq!(
            (e.body_state, e.body_reason),
            (BodyState::Unavailable, Some(REASON_NOT_FINISHED))
        );

        // 非文本 mime：无信号 + mime 终态
        buf.apply(&req("r2", "https://a/img"), 0);
        buf.apply(
            &json!({ "method": "Network.responseReceived", "sessionId": SESS,
                "params": { "requestId": "r2", "response": { "status": 200, "mimeType": "image/png" } } }),
            0,
        );
        assert_eq!(
            buf.apply(
                &json!({ "method": "Network.loadingFinished", "sessionId": SESS,
                    "params": { "requestId": "r2", "timestamp": 1.0 } }),
                0,
            ),
            ApplyOutcome::None
        );
        let e = &buf.network_entries(false, None, 10).entries[1];
        assert_eq!(
            (e.body_state, e.body_reason),
            (BodyState::Unavailable, Some(REASON_MIME))
        );

        // 失败请求：无信号 + not-finished 终态
        buf.apply(&req("r3", "https://a/dead"), 0);
        assert_eq!(
            buf.apply(
                &json!({ "method": "Network.loadingFailed", "sessionId": SESS,
                    "params": { "requestId": "r3", "errorText": "net::ERR", "timestamp": 1.0 } }),
                0,
            ),
            ApplyOutcome::None
        );
        let e = &buf.network_entries(false, None, 10).entries[2];
        assert_eq!(
            (e.body_state, e.body_reason),
            (BodyState::Unavailable, Some(REASON_NOT_FINISHED))
        );
        assert!(e.failed);
    }

    /// store_body 结算管线：成功 → available（脱敏后落环）；CDP 失败归因 →
    /// unavailable+reason；redact 拒绝 → redact-failed；未知 requestId → no-op
    #[test]
    fn store_body_settles_states_and_redacts() {
        let mut buf = DiagBuffer::default();
        buf.apply(&attached("https://a/"), 0);
        for (rid, mime) in [
            ("r1", "application/json"),
            ("r2", "application/json"),
            ("r3", "text/plain"),
        ] {
            buf.apply(&req(rid, &format!("https://a/{rid}?token=secret1")), 0);
            buf.apply(
                &json!({ "method": "Network.responseReceived", "sessionId": SESS,
                    "params": { "requestId": rid, "response": { "status": 200, "mimeType": mime } } }),
                0,
            );
            buf.apply(
                &json!({ "method": "Network.loadingFinished", "sessionId": SESS,
                    "params": { "requestId": rid, "timestamp": 1.0 } }),
                0,
            );
        }

        // 入环 URL 已脱敏（原始 URL 不落进程内存）
        let e = &buf.network_entries(false, None, 10).entries[0];
        assert_eq!(e.url, "https://a/r1?token=[redacted]");

        // 成功：body 脱敏后 available（两段式：快照 → 锁外结算 → 提交）
        settle_and_store(
            &mut buf,
            "r1",
            BodyPayload::Raw {
                body: r#"{"token":"abc","ok":true}"#.into(),
                base64_encoded: false,
            },
        );
        let e = &buf.network_entries(false, None, 10).entries[0];
        assert_eq!((e.body_state, e.body_reason), (BodyState::Available, None));
        assert_eq!(
            e.body.as_deref(),
            Some(r#"{"token":"[redacted]","ok":true}"#)
        );

        // 超时归因
        settle_and_store(
            &mut buf,
            "r2",
            BodyPayload::Unavailable {
                reason: REASON_TIMEOUT,
            },
        );
        let e = &buf.network_entries(false, None, 10).entries[1];
        assert_eq!(
            (e.body_state, e.body_reason),
            (BodyState::Unavailable, Some(REASON_TIMEOUT))
        );

        // redact 拒绝（text mime + JSON 形态 → Uncertain → redact-failed）
        settle_and_store(
            &mut buf,
            "r3",
            BodyPayload::Raw {
                body: "{broken".into(),
                base64_encoded: false,
            },
        );
        let e = &buf.network_entries(false, None, 10).entries[2];
        assert_eq!(
            (e.body_state, e.body_reason),
            (BodyState::Unavailable, Some(REASON_REDACT_FAILED))
        );

        // 未知 requestId：诚实丢弃（无处理写）——直接提交无快照路径
        buf.store_body(
            "ghost",
            settle_body(
                None,
                BodyPayload::Raw {
                    body: "x".into(),
                    base64_encoded: false,
                },
            ),
        );
        assert_eq!(buf.network_entries(false, None, 10).entries.len(), 3);
    }

    /// base64 形态与 not-text / no-data / 重复结算（已结算条目不再被后续同名
    /// store 命中——not-finished 待结算态匹配）
    #[test]
    fn store_body_handles_base64_and_terminal_reasons() {
        let mut buf = DiagBuffer::default();
        buf.apply(&attached("https://a/"), 0);
        for rid in ["rb", "rn", "rd"] {
            buf.apply(&req(rid, "https://a/x"), 0);
            buf.apply(
                &json!({ "method": "Network.responseReceived", "sessionId": SESS,
                    "params": { "requestId": rid, "response": { "status": 200, "mimeType": "text/plain" } } }),
                0,
            );
            buf.apply(
                &json!({ "method": "Network.loadingFinished", "sessionId": SESS,
                    "params": { "requestId": rid, "timestamp": 1.0 } }),
                0,
            );
        }
        // base64："Bearer abc" 的 base64 → 解码后脱敏
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "Bearer abc");
        settle_and_store(
            &mut buf,
            "rb",
            BodyPayload::Raw {
                body: b64,
                base64_encoded: true,
            },
        );
        let e = &buf.network_entries(false, None, 10).entries[0];
        assert_eq!(e.body.as_deref(), Some("Bearer [redacted]"));

        // base64 非法（二进制伪装）→ not-text；空 body → no-data
        settle_and_store(
            &mut buf,
            "rn",
            BodyPayload::Raw {
                body: "!!!not-base64".into(),
                base64_encoded: true,
            },
        );
        settle_and_store(
            &mut buf,
            "rd",
            BodyPayload::Unavailable {
                reason: REASON_NO_DATA,
            },
        );
        let es = buf.network_entries(false, None, 10).entries;
        assert_eq!(es[1].body_reason, Some(REASON_NOT_TEXT));
        assert_eq!(es[2].body_reason, Some(REASON_NO_DATA));

        // 已结算（available）的条目不再被同名 store 命中
        buf.store_body(
            "rb",
            SettledBody::Unavailable {
                reason: REASON_TIMEOUT,
            },
        );
        let e = &buf.network_entries(false, None, 10).entries[0];
        assert_eq!(e.body_state, BodyState::Available, "终态不可被覆盖");
    }

    /// 单条 256 KiB 截断 → partial（字符边界，保留实际字节数入账）
    #[test]
    fn store_body_truncates_to_item_max() {
        let mut buf = DiagBuffer::default();
        buf.apply(&attached("https://a/"), 0);
        buf.apply(&req("r1", "https://a/big"), 0);
        buf.apply(
            &json!({ "method": "Network.responseReceived", "sessionId": SESS,
                "params": { "requestId": "r1", "response": { "status": 200, "mimeType": "text/plain" } } }),
            0,
        );
        buf.apply(
            &json!({ "method": "Network.loadingFinished", "sessionId": SESS,
                "params": { "requestId": "r1", "timestamp": 1.0 } }),
            0,
        );
        // 多字节字符混合：截断不得跨 UTF-8 序列
        let mut big = "密钥无关正文".repeat(60_000); // ≈ 480KB
        big.push_str(&"a".repeat(BODY_ITEM_MAX_BYTES));
        settle_and_store(
            &mut buf,
            "r1",
            BodyPayload::Raw {
                body: big,
                base64_encoded: false,
            },
        );
        let e = &buf.network_entries(false, None, 10).entries[0];
        assert_eq!(e.body_state, BodyState::Partial);
        let stored_len = e.body.as_ref().unwrap().len();
        assert!(
            stored_len <= BODY_ITEM_MAX_BYTES,
            "不超单条上限: {stored_len}"
        );
        assert!(
            stored_len >= BODY_ITEM_MAX_BYTES - 3,
            "贴着上限截（字符边界最多回退 3B）"
        );
        assert_eq!(e.body_reason, None);
    }

    /// 环级 8 MiB 总预算：从最旧条目驱逐 body，元数据保留（分层驱逐）。
    /// 33 × 256KiB = 8.25MiB > 8MiB → 驱逐最旧 1 条；条目本身仍在环内。
    #[test]
    fn body_budget_evicts_oldest_bodies_keeps_metadata() {
        let mut buf = DiagBuffer::default();
        buf.apply(&attached("https://a/"), 0);
        let n = BODY_TOTAL_MAX_BYTES / BODY_ITEM_MAX_BYTES + 1; // 33
        for i in 0..n {
            let rid = format!("r{i}");
            buf.apply(&req(&rid, &format!("https://a/{i}")), 0);
            buf.apply(
                &json!({ "method": "Network.responseReceived", "sessionId": SESS,
                    "params": { "requestId": rid.clone(), "response": { "status": 200, "mimeType": "text/plain" } } }),
                0,
            );
            buf.apply(
                &json!({ "method": "Network.loadingFinished", "sessionId": SESS,
                    "params": { "requestId": rid, "timestamp": 1.0 } }),
                0,
            );
            let item = "x".repeat(BODY_ITEM_MAX_BYTES);
            settle_and_store(
                &mut buf,
                &format!("r{i}"),
                BodyPayload::Raw {
                    body: item,
                    base64_encoded: false,
                },
            );
        }
        let es = buf.network_entries(false, None, 100).entries;
        assert_eq!(es.len(), n, "条目（元数据）永不驱逐");
        assert_eq!(es[0].body_state, BodyState::Evicted, "最旧 body 被预算驱逐");
        assert_eq!(es[0].body_reason, None);
        assert_eq!(es[0].status, Some(200), "元数据完整保留");
        for e in &es[1..] {
            assert_eq!(e.body_state, BodyState::Available);
        }
        // 账本收敛：驱到 ≤ 8MiB 为止
        let kept: usize = es[1..]
            .iter()
            .map(|e| e.body.as_ref().map_or(0, |b| b.len()))
            .sum();
        assert!(kept <= BODY_TOTAL_MAX_BYTES);
    }

    /// 锁外结算管线（纯函数 settle_body）与前置快照（pending_body_mime）：
    /// 归因透传、脱敏/截断标记 partial、非 UTF-8 → not-text；快照定位与提交
    /// 定位同规则（最新 not-finished），终态后不再命中
    #[test]
    fn settle_body_pipeline_and_mime_snapshot() {
        // Unavailable 透传
        assert!(matches!(
            settle_body(
                None,
                BodyPayload::Unavailable {
                    reason: REASON_TIMEOUT
                }
            ),
            SettledBody::Unavailable {
                reason: REASON_TIMEOUT
            }
        ));
        // 正常文本：脱敏 + partial=false
        let SettledBody::Text { body, partial } = settle_body(
            Some("text/plain"),
            BodyPayload::Raw {
                body: "Bearer xyz".into(),
                base64_encoded: false,
            },
        ) else {
            panic!("应为 Text");
        };
        assert_eq!(body, "Bearer [redacted]");
        assert!(!partial);
        // 超 256KiB 截断 → partial=true（纯 ASCII 贴着字节上限）
        let SettledBody::Text { body, partial } = settle_body(
            Some("text/plain"),
            BodyPayload::Raw {
                body: "a".repeat(BODY_ITEM_MAX_BYTES + 1),
                base64_encoded: false,
            },
        ) else {
            panic!("应为 Text");
        };
        assert_eq!(body.len(), BODY_ITEM_MAX_BYTES);
        assert!(partial);
        // 非 UTF-8（base64 形态）→ not-text
        let bad = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            [0xffu8, 0xfe, 0xfd, 0xfc],
        );
        assert!(matches!(
            settle_body(
                None,
                BodyPayload::Raw {
                    body: bad,
                    base64_encoded: true
                }
            ),
            SettledBody::Unavailable {
                reason: REASON_NOT_TEXT
            }
        ));

        // 快照定位：最新 not-finished 同名条目的 mime；结算后不再命中
        let mut buf = DiagBuffer::default();
        buf.apply(&attached("https://a/"), 0);
        buf.apply(&req("r1", "https://a/x"), 0);
        buf.apply(
            &json!({ "method": "Network.responseReceived", "sessionId": SESS,
                "params": { "requestId": "r1", "response": { "status": 200, "mimeType": "application/json" } } }),
            0,
        );
        buf.apply(
            &json!({ "method": "Network.loadingFinished", "sessionId": SESS,
                "params": { "requestId": "r1", "timestamp": 1.0 } }),
            0,
        );
        assert_eq!(
            buf.pending_body_mime("r1"),
            Some(Some("application/json".into()))
        );
        settle_and_store(
            &mut buf,
            "r1",
            BodyPayload::Raw {
                body: "{}".into(),
                base64_encoded: false,
            },
        );
        assert_eq!(buf.pending_body_mime("r1"), None, "终态不再被同名快照命中");
        // 未知 requestId：快照即无目标
        assert_eq!(buf.pending_body_mime("ghost"), None);
    }

    /// 两段式竞态下的预算语义：锁外算好的终态在提交时才入账——提交序（而非
    /// 计算序）驱动账本，驱逐仍最旧先驱且基于锁内最新 body_bytes 重验
    /// （快照-计算-回锁提交的核心保证：B 批全部算完才提交，仍驱最旧的 A 批）
    #[test]
    fn settled_commits_account_in_commit_order_oldest_first_eviction() {
        let mut buf = DiagBuffer::default();
        buf.apply(&attached("https://a/"), 0);
        let mut stashed_b: Vec<(String, SettledBody)> = Vec::new();
        for batch in ["a", "b"] {
            for i in 0..17 {
                let rid = format!("{batch}{i}");
                buf.apply(&req(&rid, &format!("https://a/{rid}")), 0);
                buf.apply(
                    &json!({ "method": "Network.responseReceived", "sessionId": SESS,
                        "params": { "requestId": rid.clone(), "response": { "status": 200, "mimeType": "text/plain" } } }),
                    0,
                );
                buf.apply(
                    &json!({ "method": "Network.loadingFinished", "sessionId": SESS,
                        "params": { "requestId": rid.clone(), "timestamp": 1.0 } }),
                    0,
                );
                // 锁外计算（每条恰 256KiB，不触发单条截断）
                let settled = settle_body(
                    Some("text/plain"),
                    BodyPayload::Raw {
                        body: "x".repeat(BODY_ITEM_MAX_BYTES),
                        base64_encoded: false,
                    },
                );
                if batch == "a" {
                    buf.store_body(&rid, settled); // A 批：即算即提交
                } else {
                    stashed_b.push((rid, settled)); // B 批：全部算完后才提交
                }
            }
        }
        for (rid, settled) in stashed_b {
            buf.store_body(&rid, settled);
        }
        let es = buf.network_entries(false, None, 100).entries;
        assert_eq!(es.len(), 34, "元数据永不驱逐");
        // 34 × 256KiB = 8.5MiB > 8MiB → 驱逐最旧 2 条（a0/a1），与提交序无关
        assert_eq!(es[0].body_state, BodyState::Evicted, "最旧 a0 被预算驱逐");
        assert_eq!(es[1].body_state, BodyState::Evicted, "次旧 a1 被预算驱逐");
        assert!(es[2..].iter().all(|e| e.body_state == BodyState::Available));
        let kept: usize = es
            .iter()
            .map(|e| e.body.as_ref().map_or(0, |b| b.len()))
            .sum();
        assert!(kept <= BODY_TOTAL_MAX_BYTES, "账本收敛 ≤ 8MiB: {kept}");
    }

    /// body 分页读：UTF-16 code unit 计量、代理对不劈、出界与 limit=0、
    /// evicted/unavailable 条目无内容但有总长语义（空切片）
    #[test]
    fn network_body_slices_utf16_without_splitting_pairs() {
        let mut buf = DiagBuffer::default();
        buf.apply(&attached("https://a/"), 0);
        buf.apply(&req("r1", "https://a/t"), 0);
        buf.apply(
            &json!({ "method": "Network.responseReceived", "sessionId": SESS,
                "params": { "requestId": "r1", "response": { "status": 200, "mimeType": "text/plain" } } }),
            0,
        );
        buf.apply(
            &json!({ "method": "Network.loadingFinished", "sessionId": SESS,
                "params": { "requestId": "r1", "timestamp": 1.0 } }),
            0,
        );
        // 4 字符：A(1u) 𝕏(2u 代理对) B(1u) C(1u) → total 5 units
        settle_and_store(
            &mut buf,
            "r1",
            BodyPayload::Raw {
                body: "A𝕏BC".into(),
                base64_encoded: false,
            },
        );

        let v = buf.network_body("r1", 0, 5).unwrap();
        assert_eq!((v.slice.returned, v.slice.total), (5, 5));
        assert_eq!(v.slice.text, "A𝕏BC");
        assert_eq!(v.state, BodyState::Available);
        assert_eq!(v.stored_bytes, 7); // A=1B 𝕏=4B B/C 各 1B

        // offset=2 落在代理对中间 → 向外取整（含 𝕏 整字符）
        let v = buf.network_body("r1", 2, 2).unwrap();
        assert_eq!(v.slice.text, "𝕏B");
        assert_eq!(v.slice.returned, 3, "代理对整体保留（2+1 units）");
        // limit 落在代理对中间同理向外
        let v = buf.network_body("r1", 0, 2).unwrap();
        assert_eq!(v.slice.text, "A𝕏");
        // 出界 / limit=0
        assert_eq!(buf.network_body("r1", 99, 5).unwrap().slice.text, "");
        assert_eq!(buf.network_body("r1", 0, 0).unwrap().slice.returned, 0);
        // 不存在的 requestId → None（service 层转 404）
        assert!(buf.network_body("ghost", 0, 5).is_none());
    }

    /// HAR：body 可用条目填脱敏 text（ASCII 原样 / 非 ASCII base64+encoding），
    /// 不可用条目不填 text（HAR 兼容占位）
    #[test]
    fn har_embeds_redacted_body_text() {
        let mut buf = DiagBuffer::default();
        buf.apply(&attached("https://a/"), 0);
        for (rid, mime) in [("r1", "application/json"), ("r2", "image/png")] {
            buf.apply(&req(rid, "https://a/x"), 0);
            buf.apply(
                &json!({ "method": "Network.responseReceived", "sessionId": SESS,
                    "params": { "requestId": rid, "response": { "status": 200, "mimeType": mime } } }),
                0,
            );
            buf.apply(
                &json!({ "method": "Network.loadingFinished", "sessionId": SESS,
                    "params": { "requestId": rid, "timestamp": 1.0 } }),
                0,
            );
        }
        settle_and_store(
            &mut buf,
            "r1",
            BodyPayload::Raw {
                body: "{\"msg\":\"中文\"}".into(),
                base64_encoded: false,
            },
        );
        let har = build_har(&buf.har_snapshot(), "0.6.0");
        let e0 = &har["log"]["entries"][0];
        assert_eq!(
            e0["response"]["content"]["encoding"], "base64",
            "非 ASCII → base64"
        );
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            e0["response"]["content"]["text"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "{\"msg\":\"中文\"}");
        assert_eq!(e0["response"]["content"]["size"], 16); // {"msg":"中文"} 字节数（中文各 3B）
        let e1 = &har["log"]["entries"][1];
        assert!(
            e1["response"]["content"].get("text").is_none(),
            "不可用条目无 text"
        );
    }
}
