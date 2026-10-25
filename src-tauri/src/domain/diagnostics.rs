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

use serde::Serialize;
use serde_json::{json, Value};

/// 环形容量（交接契约：console 500 / network 1000）
const CONSOLE_RING: usize = 500;
const NETWORK_RING: usize = 1000;
/// network 环的采集范围声明（console/network 查询与 HAR `_meta` 的常量值）：
/// 条目在请求完成/失败时入环，**进行中的请求不可见**——响应显式声明而非留白
pub const NETWORK_SCOPE_COMPLETED: &str = "completed";
/// in-flight 悬挂保护：未完成请求的 pending 表上限。超限整体清空——泄漏防护
/// 优先于完整性（无序 HashMap 无从挑选最旧；被清掉的请求完成事件将无处归档）
const PENDING_CAP: usize = 500;

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
    pub url: String,
    pub status: Option<i64>,
    pub mime_type: Option<String>,
    pub failed: bool,
    /// CDP errorText（loadingFailed 时）
    pub error: Option<String>,
    pub duration_ms: Option<i64>,
    pub target_url: String,
    /// 归属 tab 的 targetId（requestWillBeSent 有 session 门卫，必有归属）
    pub target_id: Option<String>,
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

/// attach/detach 的信号（应用层据此发送 per-session enable 命令）
#[derive(Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    None,
    Attach(String),
    Detach(String),
}

/// attach 登记的 session 归属：tab 的 targetId + attach 时的页面 URL
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
                target_url: target_url.to_string(),
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
                    target_url: entry
                        .get("url")
                        .and_then(Value::as_str)
                        .map(str::to_string)
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
                            url: request
                                .get("url")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            status: None,
                            mime_type: None,
                            failed: false,
                            error: None,
                            duration_ms: None,
                            target_url: session.map(|s| s.target_url.clone()).unwrap_or_default(),
                            target_id: session.map(|s| s.target_id.clone()),
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
                    self.settle(req.entry);
                }
                ApplyOutcome::None
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
        if self.network.len() > NETWORK_RING {
            self.network.pop_front();
            self.network_dropped += 1;
        }
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

    /// HAR 1.2（har-recorder 的正式落地点）。未捕获的字段按规范以 -1/空集占位；
    /// `_targetUrl` 为自定义字段标注条目归属的页面目标（CDP session 归属）。
    pub fn to_har(&self, creator_version: &str) -> Value {
        let entries: Vec<Value> = self
            .network
            .iter()
            .map(|e| {
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
                        "content": { "size": 0, "mimeType": e.mime_type.clone().unwrap_or_default() },
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
        let har = buf.to_har("0.2.3");
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
            RingStats { capacity: 500, dropped: 0 },
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
            buf.console_stats().dropped, 100,
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
            RingStats { capacity: 1000, dropped: 100 },
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
}
