//! 常驻诊断服务：实例级 watcher 生命周期 + 查询投影。
//!
//! 形态：launch 置 running 后 spawn（不阻塞启动路径）；browser 级常驻 WS +
//! `Target.setAutoAttach(flatten)` —— console/network 事件都是 per-session 维度，
//! 按 target 归属环形缓冲（domain/diagnostics.rs，进程内态重启即清）。
//! 断线指数退避重连（1s 起步、15s 封顶、连上即重置）；实例退出/删除/重启由
//! 代际守卫（AtomicBool）+ 存活检查双路回收，退出即 drop 缓冲防泄漏。
//!
//! 共存纪律：常驻连接只订阅不操作（autoAttach/enable 之外仅截图的
//! Page.captureScreenshot），与 agent 自建的 CDP 会话天然互不干扰。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::domain::diagnostics::{
    ApplyOutcome, BodyPayload, BodyState, DiagBuffer, NetworkBodyView, QueryResult, RingStats,
    NETWORK_SCOPE_COMPLETED, REASON_NO_DATA, REASON_TIMEOUT,
};
use crate::domain::instance::InstanceStatus;
use crate::error::AppError;
use crate::infrastructure::cdp::ws::CdpConnection;
use crate::infrastructure::db::repositories::instance_repository::InstanceRepository;

const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(15);
/// 查询投影的缺省/上限条数（防一次拖全环）
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;

/// body 拉取预算：并发 in-flight ≤4（CdpConnection::send 支持 &self 并发，
/// 常驻事件流不被拉取堵死）；排队上限（溢出即诚实归因 no-data，不堆积）；
/// 单条超时 ~2.5s（远低于命令通道 15s，防慢拉取占住并发槽）
const BODY_FETCH_CONCURRENCY: usize = 4;
const BODY_BACKLOG_CAP: usize = 64;
const BODY_FETCH_TIMEOUT: Duration = Duration::from_millis(2500);
/// body 分页读：offset/limit 以 UTF-16 code unit 计量（JS String 同构）
const BODY_READ_DEFAULT_LIMIT: usize = 8192;
const BODY_READ_MAX_LIMIT: usize = 65536;

fn unavailable(message: impl Into<String>) -> AppError {
    AppError::conflict("DIAGNOSTICS_UNAVAILABLE", message)
}

struct Watched {
    buffer: Arc<Mutex<DiagBuffer>>,
    /// 当前代连接（断线置 None，重连整体替换）；截图查询复用它发命令。
    /// send 已 &self 并发化（P2-F 根治），裸 Arc 直发、无锁包裹。
    conn: Mutex<Option<Arc<CdpConnection>>>,
    /// body 拉取并发闸门（≤4 in-flight；排队 = acquire 等待）
    body_gate: Arc<tokio::sync::Semaphore>,
    /// body 拉取排队计数（含 in-flight；超上限溢出即归因 no-data）
    body_backlog: Arc<AtomicUsize>,
    /// 代际守卫：restart 产生新一代后旧代自行退出（防双 watcher 双缓冲）
    current: AtomicBool,
    /// 本代 watcher 挂载时刻（epoch ms）：响应 meta.since 的数据源——早于该
    /// 时刻的数据从未被采集（诚实声明窗口边界，而非留白让调用方误判为「无数据」）
    since_ms: i64,
}

impl Watched {
    fn new() -> Self {
        Watched {
            buffer: Arc::new(Mutex::new(DiagBuffer::default())),
            conn: Mutex::new(None),
            body_gate: Arc::new(tokio::sync::Semaphore::new(BODY_FETCH_CONCURRENCY)),
            body_backlog: Arc::new(AtomicUsize::new(0)),
            current: AtomicBool::new(true),
            since_ms: now_ms(),
        }
    }
}

pub struct DiagnosticsService {
    registry: Arc<Mutex<HashMap<String, Arc<Watched>>>>,
    repo: Arc<InstanceRepository>,
    version: String,
}

impl DiagnosticsService {
    pub fn new(repo: Arc<InstanceRepository>, version: String) -> Self {
        DiagnosticsService {
            registry: Arc::new(Mutex::new(HashMap::new())),
            repo,
            version,
        }
    }

    /// 实例置 running 后由 InstanceService 调用。同步部分只有登记 + spawn，
    /// 绝不阻塞启动路径。同实例重复调用（restart）触发代际切换。
    pub fn spawn(&self, ins_id: &str, port: u16) {
        let mut map = self.registry.lock().unwrap();
        if let Some(old) = map.get(ins_id) {
            old.current.store(false, Ordering::Relaxed); // 旧代在下一个检查点退出
        }
        let watched = Arc::new(Watched::new());
        map.insert(ins_id.to_string(), Arc::clone(&watched));
        drop(map);

        let repo = Arc::clone(&self.repo);
        let registry = Arc::clone(&self.registry);
        let ins_id = ins_id.to_string();
        // tauri::async_runtime::spawn：调用方含 main 的同步启动段（P0-2 重挂）
        // 与 async 启动路径，tokio::spawn 在同步上下文会 panic（无 reactor）。
        // tauri runtime 全局常驻，任意线程可调度（与 exit watcher 钩子同款）
        tauri::async_runtime::spawn(async move {
            watch_loop(ins_id, port, repo, registry, watched).await
        });
    }

    fn alive(&self, ins_id: &str) -> bool {
        self.repo
            .get(ins_id)
            .map(|i| i.status != InstanceStatus::Stopped && i.status != InstanceStatus::Error)
            .unwrap_or(false)
    }

    /// 查询前置：实例活着（否则 409）；watcher 尚未起来/刚重启 → 空数据而非错误
    fn require(&self, ins_id: &str) -> Result<Option<Arc<Watched>>, AppError> {
        if !self.alive(ins_id) {
            return Err(unavailable(format!(
                "实例 {ins_id} 未在运行，常驻诊断仅对运行中实例可用"
            )));
        }
        Ok(self.registry.lock().unwrap().get(ins_id).cloned())
    }

    /// GET console?level=&limit=&tab=：level 精确匹配（error/warning/info/debug/log）；
    /// tab 按 targetId 归属过滤（环中存在无归属条目时降级全量，meta 标注）。
    /// 响应 meta 恒带证据边界（dropped/capacity/since/live，见 query_response）
    pub fn console(
        &self,
        ins_id: &str,
        level: Option<&str>,
        tab: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Value, AppError> {
        let watched = self.require(ins_id)?;
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let (result, stats, since, live) = match watched {
            Some(w) => {
                let buf = w.buffer.lock().unwrap();
                (
                    buf.console_entries(level, tab, limit),
                    buf.console_stats(),
                    Some(w.since_ms),
                    true,
                )
            }
            // 无 watcher = 空投影，但 meta 必须如实声明（since:null + live:false）
            None => (
                empty_result(),
                DiagBuffer::default().console_stats(),
                None,
                false,
            ),
        };
        Ok(query_response(ins_id, result, &stats, since, live, None))
    }

    /// GET network?failedOnly=&limit=&tab=：failedOnly = 失败请求或 HTTP ≥400；
    /// tab 按 targetId 归属过滤（同上）。meta 另带 scope:"completed"——
    /// 进行中的请求不可见（完成/失败时才入环）
    pub fn network(
        &self,
        ins_id: &str,
        failed_only: bool,
        tab: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Value, AppError> {
        let watched = self.require(ins_id)?;
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let (result, stats, since, live) = match watched {
            Some(w) => {
                let buf = w.buffer.lock().unwrap();
                (
                    buf.network_entries(failed_only, tab, limit),
                    buf.network_stats(),
                    Some(w.since_ms),
                    true,
                )
            }
            None => (
                empty_result(),
                DiagBuffer::default().network_stats(),
                None,
                false,
            ),
        };
        Ok(query_response(
            ins_id,
            result,
            &stats,
            since,
            live,
            Some(NETWORK_SCOPE_COMPLETED),
        ))
    }

    /// GET har：network 环生成 HAR 1.2 文档原样返回（har-recorder 落地点）。
    /// 证据边界走 HAR 顶层 `_meta` 扩展键（HAR 规范允许 `_` 前缀自定义字段；
    /// 不用 {har, meta} 包裹——会破坏「API 返回 HAR 文档原样」的既有消费方）
    pub fn har(&self, ins_id: &str) -> Result<Value, AppError> {
        let watched = self.require(ins_id)?;
        let (mut har, stats, since, live) = match watched {
            Some(w) => {
                let buf = w.buffer.lock().unwrap();
                (
                    buf.to_har(&self.version),
                    buf.network_stats(),
                    Some(w.since_ms),
                    true,
                )
            }
            None => {
                let buf = DiagBuffer::default();
                (buf.to_har(&self.version), buf.network_stats(), None, false)
            }
        };
        har["_meta"] = json!({
            "since": since,
            "live": live,
            "dropped": stats.dropped,
            "capacity": stats.capacity,
            "scope": NETWORK_SCOPE_COMPLETED,
            // 未采集字段的占位语义（HAR 1.2：-1 = not available；空集 = 未采集非确认为空）
            "uncollected": [
                "request/response 的 headersSize、bodySize = -1：未采集（HAR 规范 not available 语义）",
                "timings 的 send/wait/dns/connect/ssl = -1：仅记录总时长（receive），分阶段耗时未采集",
                "headers/queryString/cookies = 空数组：未采集，非确认为空",
                "request body 未采集：仅捕获响应体（文本类 mime 的 response.content.text，脱敏后嵌入）",
                "response.content.text：仅 bodyState available/partial 的条目携带（partial = 超 256KiB 截断；含非 ASCII 时 base64 + encoding）；evicted/unavailable 条目无 text——bodyState/bodyReason 见 network 查询",
            ],
        });
        Ok(har)
    }

    /// GET network/{requestId}/body?offset=&limit=：分页读响应体（UTF-16
    /// code unit 计量，不劈代理对）。仅环内 available/partial 条目可读；
    /// requestId 不在环 → 404 NETWORK_BODY_NOT_FOUND（环为进程内态，重启即清）；
    /// 状态不可读 → 409 NETWORK_BODY_UNAVAILABLE（message 携带 bodyState/bodyReason）。
    pub fn network_body(
        &self,
        ins_id: &str,
        request_id: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<Value, AppError> {
        let offset = offset.unwrap_or(0);
        let limit = limit.unwrap_or(BODY_READ_DEFAULT_LIMIT).min(BODY_READ_MAX_LIMIT);
        let Some(watched) = self.require(ins_id)? else {
            return Err(AppError::not_found(
                "NETWORK_BODY_NOT_FOUND",
                format!("请求 {request_id} 不在诊断环内（常驻诊断未就绪：实例刚启动或已重启，环内无数据）"),
            ));
        };
        let view = {
            let buf = watched.buffer.lock().unwrap();
            buf.network_body(request_id, offset, limit)
        };
        let Some(view) = view else {
            return Err(AppError::not_found(
                "NETWORK_BODY_NOT_FOUND",
                format!(
                    "请求 {request_id} 不在诊断环内（可能已被环容量挤出，或实例/应用重启；\
                     requestId 以 network 查询为准）"
                ),
            ));
        };
        if !matches!(view.state, BodyState::Available | BodyState::Partial) {
            let reason = view
                .reason
                .map(|r| format!("，bodyReason: {r}"))
                .unwrap_or_default();
            return Err(AppError::conflict(
                "NETWORK_BODY_UNAVAILABLE",
                format!(
                    "请求 {request_id} 的 body 不可读（bodyState: {}{}）",
                    view.state.as_str(),
                    reason
                ),
            ));
        }
        Ok(body_response(ins_id, request_id, offset, limit, &view))
    }

    /// GET screenshot：复用常驻连接对第一个 http(s) 页面 session 截图（png base64）。
    /// 连接未就绪/无页面 → 409 DIAGNOSTICS_UNAVAILABLE（与 M5 同码同语义）。
    pub async fn screenshot(&self, ins_id: &str) -> Result<Value, AppError> {
        let watched = self
            .require(ins_id)?
            .ok_or_else(|| unavailable("常驻诊断连接未就绪（实例刚启动，稍后重试）"))?;
        let shared = watched
            .conn
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| unavailable("常驻诊断连接断开重连中，稍后重试"))?;
        let session_id = watched
            .buffer
            .lock()
            .unwrap()
            .pick_page_session()
            .ok_or_else(|| unavailable("无可截图的 http(s) 页面目标"))?;
        let resp: Value = shared
            .send(
                "Page.captureScreenshot",
                json!({ "format": "png" }),
                Some(&session_id),
            )
            .await?;
        let data = resp
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::internal("截图响应缺少 data 字段"))?
            .to_string();
        Ok(json!({ "instanceId": ins_id, "format": "png", "data": data }))
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// body 响应组装：truncated = 窗口未覆盖全文（offset+returned < total），
/// 分页继续读用 offset += returned。bodyReason 仅 unavailable 态携带。
fn body_response(
    ins_id: &str,
    request_id: &str,
    offset: usize,
    limit: usize,
    view: &NetworkBodyView,
) -> Value {
    let mut resp = json!({
        "instanceId": ins_id,
        "requestId": request_id,
        "bodyState": view.state,
        "body": view.slice.text,
        "offset": offset,
        "limit": limit,
        "returned": view.slice.returned,
        "totalLength": view.slice.total,
        "bodyBytes": view.stored_bytes,
        "truncated": offset + view.slice.returned < view.slice.total,
    });
    if let Some(reason) = view.reason {
        resp["bodyReason"] = json!(reason);
    }
    resp
}

/// body 拉取调度：排队预算（防事件洪峰堆积；溢出即诚实归因 no-data）→
/// 并发闸门（≤4 in-flight）。连接断开（重连窗口）同样归因 no-data，
/// 条目不停畆在悬挂态。
fn dispatch_body_fetch(watched: &Arc<Watched>, session_id: String, request_id: String) {
    if watched.body_backlog.load(Ordering::Relaxed) >= BODY_BACKLOG_CAP {
        watched.buffer.lock().unwrap().store_body(
            &request_id,
            BodyPayload::Unavailable {
                reason: REASON_NO_DATA,
            },
        );
        return;
    }
    watched.body_backlog.fetch_add(1, Ordering::Relaxed);
    let gate = Arc::clone(&watched.body_gate);
    let backlog = Arc::clone(&watched.body_backlog);
    let buffer = Arc::clone(&watched.buffer);
    let conn = watched.conn.lock().unwrap().clone();
    // 拉取完全异步：事件泵只登记不等待（诊断连接是常驻事件流，不能被 body 拉取堵死）
    tauri::async_runtime::spawn(async move {
        // 排队等待 permit（≤4 并发）；拿到 permit 即退出排队计数
        let _permit = gate.acquire().await;
        backlog.fetch_sub(1, Ordering::Relaxed);
        let payload = match conn {
            Some(conn) => fetch_response_body(&conn, &session_id, &request_id, BODY_FETCH_TIMEOUT).await,
            None => BodyPayload::Unavailable {
                reason: REASON_NO_DATA,
            },
        };
        buffer.lock().unwrap().store_body(&request_id, payload);
    });
}

/// 单条拉取：getResponseBody（per-session）+ 2.5s 超时预算 → 归因映射。
/// timeout 参数化仅为单测提速（生产恒为 [`BODY_FETCH_TIMEOUT`]）。
async fn fetch_response_body(
    conn: &CdpConnection,
    session_id: &str,
    request_id: &str,
    timeout: Duration,
) -> BodyPayload {
    let fut = conn.send::<Value>(
        "Network.getResponseBody",
        json!({ "requestId": request_id }),
        Some(session_id),
    );
    match tokio::time::timeout(timeout, fut).await {
        Err(_elapsed) => BodyPayload::Unavailable {
            reason: REASON_TIMEOUT,
        },
        Ok(Err(_cdp)) => BodyPayload::Unavailable {
            // CDP 拒绝（如 No resource with given identifier）/连接中断：无数据
            reason: REASON_NO_DATA,
        },
        Ok(Ok(v)) => match v.get("body").and_then(Value::as_str) {
            None | Some("") => BodyPayload::Unavailable {
                reason: REASON_NO_DATA,
            },
            Some(body) => BodyPayload::Raw {
                body: body.to_string(),
                base64_encoded: v
                    .get("base64Encoded")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            },
        },
    }
}

/// watcher 未就绪时的空投影（空环过滤任意 tab 结果仍为空，无需降级标注）
fn empty_result<T>() -> QueryResult<T> {
    QueryResult {
        entries: Vec::new(),
        meta: None,
    }
}

/// 查询响应组装：meta 恒在（证据边界契约）——dropped/capacity/since/live 必有，
/// network 另有 scope；请求了 tab 过滤时 TabFilterMeta 字段并入同一对象
fn query_response<T: Serialize>(
    ins_id: &str,
    result: QueryResult<T>,
    stats: &RingStats,
    since: Option<i64>,
    live: bool,
    scope: Option<&str>,
) -> Value {
    let QueryResult { entries, meta } = result;
    let mut m = json!({
        "dropped": stats.dropped,
        "capacity": stats.capacity,
        "since": since,
        "live": live,
    });
    if let Some(scope) = scope {
        m["scope"] = json!(scope);
    }
    if let Some(tf) = meta {
        // TabFilterMeta 为纯数据结构，序列化不会失败
        if let Value::Object(fields) = serde_json::to_value(tf).unwrap_or(Value::Null) {
            for (k, v) in fields {
                m[k] = v;
            }
        }
    }
    json!({ "instanceId": ins_id, "entries": entries, "meta": m })
}

/// watcher 主循环：连接 → 泵事件 → 死亡后退避重连；退出条件（代际失效或实例
/// 不再存活）满足后从登记表移除（仅当仍是当前代，防旧代误删新代条目）
async fn watch_loop(
    ins_id: String,
    port: u16,
    repo: Arc<InstanceRepository>,
    registry: Arc<Mutex<HashMap<String, Arc<Watched>>>>,
    watched: Arc<Watched>,
) {
    let mut backoff = BACKOFF_START;
    loop {
        if !watched.current.load(Ordering::Relaxed) {
            break;
        }
        if !alive(&repo, &ins_id) {
            break;
        }
        match start_session(port, &watched).await {
            Ok(mut rx) => {
                backoff = BACKOFF_START;
                while let Some(event) = rx.recv().await {
                    if !watched.current.load(Ordering::Relaxed) {
                        break;
                    }
                    let outcome = watched.buffer.lock().unwrap().apply(&event, now_ms());
                    match outcome {
                        ApplyOutcome::Attach(sid) => enable_session(&watched, &sid).await,
                        ApplyOutcome::BodyFetch { session_id, request_id } => {
                            dispatch_body_fetch(&watched, session_id, request_id)
                        }
                        ApplyOutcome::Detach(_) | ApplyOutcome::None => {}
                    }
                }
                // 通道关闭 = reader 结束 = 连接已死；清空连接引用后进入退避重连
                *watched.conn.lock().unwrap() = None;
            }
            Err(e) => {
                tracing::warn!("[diagnostics] 实例 {ins_id} 常驻连接建立失败（退避重试）: {e}");
            }
        }
        if !watched.current.load(Ordering::Relaxed) || !alive(&repo, &ins_id) {
            break;
        }
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff * 2, BACKOFF_MAX);
    }
    let mut map = registry.lock().unwrap();
    if map
        .get(&ins_id)
        .is_some_and(|cur| Arc::ptr_eq(cur, &watched))
    {
        map.remove(&ins_id);
    }
}

fn alive(repo: &InstanceRepository, ins_id: &str) -> bool {
    repo.get(ins_id)
        .map(|i| i.status != InstanceStatus::Stopped && i.status != InstanceStatus::Error)
        .unwrap_or(false)
}

/// 建立常驻连接并挂 autoAttach；事件通道由 reader 侧持有（连接死 → 通道 drop）
async fn start_session(
    port: u16,
    watched: &Watched,
) -> Result<tokio::sync::mpsc::UnboundedReceiver<Value>, AppError> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let conn = CdpConnection::connect_eventing(port, tx).await?;
    let shared = Arc::new(conn);
    *watched.conn.lock().unwrap() = Some(Arc::clone(&shared));
    // waitForDebuggerOnStart 在 CfT 131 为必填参数（缺省即 -32602 Invalid parameters，
    // 实测确认）；false = attach 不暂停目标页，符合「只订阅不操作」纪律
    let _: Value = shared
        .send(
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "flatten": true, "waitForDebuggerOnStart": false }),
            None,
        )
        .await?;
    Ok(rx)
}

/// per-session 域启用（console/network/log 三源）。best-effort：失败仅该 session
/// 缺数据，重连后自愈；不因单个 attach 失败拖垮 watcher
async fn enable_session(watched: &Watched, session_id: &str) {
    let Some(shared) = watched.conn.lock().unwrap().clone() else {
        return;
    };
    for method in ["Runtime.enable", "Network.enable", "Log.enable"] {
        if let Err(e) = shared.send::<Value>(method, (), Some(session_id)).await {
            tracing::debug!("[diagnostics] session {session_id} {method} 失败: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::instance::Instance;
    use crate::infrastructure::db::client::init_pool;
    use crate::infrastructure::db::schema::init_schema;

    fn setup() -> (Arc<InstanceRepository>, Instance) {
        let dir = std::env::temp_dir().join(format!("cem-diag-svc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = init_pool(&dir.join("test.db")).unwrap();
        init_schema(&pool).unwrap();
        // instances.environment_id 有 FK，先落一条环境行
        let now = chrono::Utc::now().timestamp_millis();
        pool.get()
            .unwrap()
            .execute(
                "INSERT INTO environments (id, name, keep_alive, created_at, updated_at) VALUES ('env_x', 'x', 0, ?1, ?1)",
                rusqlite::params![now],
            )
            .unwrap();
        let repo = Arc::new(InstanceRepository::new(pool));
        let ins = Instance {
            id: "ins_diag".into(),
            name: None,
            environment_id: "env_x".into(),
            login_profile_id: None,
            profile_dir: "/tmp/x".into(),
            pid: None,
            cdp_port: Some(9333),
            status: InstanceStatus::Running,
            host_rules: None,
            browser_version: None,
            started_at: None,
            stopped_at: None,
            created_at: now,
            updated_at: now,
        };
        repo.insert(&ins).unwrap();
        (repo, ins)
    }

    /// 查询门卫：未运行 → 409；运行中但 watcher 未起（刚启动）→ 空数据而非错误
    #[test]
    fn require_gates_on_liveness_and_treats_missing_watcher_as_empty() {
        let (repo, ins) = setup();
        let svc = DiagnosticsService::new(repo.clone(), "0.2.3".into());

        let console = svc.console(&ins.id, None, None, None).unwrap();
        assert_eq!(
            console["entries"].as_array().unwrap().len(),
            0,
            "无 watcher = 空数据"
        );
        assert_eq!(console["meta"]["since"], Value::Null, "无 watcher 无窗口起点");
        assert_eq!(console["meta"]["live"], false, "无 watcher 必须声明非活采集");
        assert_eq!(console["meta"]["dropped"], 0);
        assert_eq!(console["meta"]["capacity"], 500);
        let network = svc.network(&ins.id, false, None, None).unwrap();
        assert_eq!(network["meta"]["scope"], "completed");
        let har = svc.har(&ins.id).unwrap();
        assert_eq!(har["log"]["entries"].as_array().unwrap().len(), 0);
        assert_eq!(har["_meta"]["since"], Value::Null);
        assert_eq!(har["_meta"]["live"], false);
        assert_eq!(har["_meta"]["scope"], "completed");
        assert!(har["_meta"]["uncollected"].as_array().unwrap().len() >= 3);

        repo.update_runtime(
            &ins.id,
            crate::domain::instance::RuntimeUpdate {
                status: Some(InstanceStatus::Stopped),
                ..Default::default()
            },
        )
        .unwrap();
        let err = svc.console(&ins.id, None, None, None).unwrap_err();
        assert_eq!(err.status(), 409);
        assert_eq!(err.code(), "DIAGNOSTICS_UNAVAILABLE");
    }

    /// 代际守卫的登记语义：spawn 同实例两次，旧代标记失效且登记表指向新代。
    /// watcher 内含 tokio::spawn，需在 runtime 内驱动（连接必然失败进入退避循环，
    /// 测试结束时随 runtime 一同丢弃）
    #[tokio::test]
    async fn spawn_replaces_generation() {
        let (repo, ins) = setup();
        let svc = DiagnosticsService::new(repo, "0.2.3".into());
        svc.spawn(&ins.id, 9333); // 无 CDP 可连：watcher 会退避循环，不影响登记语义
        let first = svc.registry.lock().unwrap().get(&ins.id).cloned().unwrap();
        svc.spawn(&ins.id, 9333);
        let second = svc.registry.lock().unwrap().get(&ins.id).cloned().unwrap();
        assert!(!Arc::ptr_eq(&first, &second), "登记表已指向新代");
        assert!(!first.current.load(Ordering::Relaxed), "旧代已失效");
        assert!(second.current.load(Ordering::Relaxed));
        // 实例停止后 watcher 自行退出并清理登记（这里直接驱动其退出路径）
        second.current.store(false, Ordering::Relaxed);
    }

    /// tab 过滤透传：直接登记 Watched 并喂事件（绕过 spawn 的网络路径），
    /// 验证 service 层的 tab 参数透传与 meta 组装（证据边界恒在 +
    /// filteredByTab/tabFilterUnavailable 并入同一 meta 对象）
    #[test]
    fn console_network_tab_filter_meta_surfaced() {
        let (repo, ins) = setup();
        let svc = DiagnosticsService::new(repo, "0.2.3".into());
        let watched = Arc::new(Watched::new());
        svc.registry
            .lock()
            .unwrap()
            .insert(ins.id.clone(), Arc::clone(&watched));

        let mut buffer = watched.buffer.lock().unwrap();
        buffer.apply(
            &json!({ "method": "Target.attachedToTarget",
                "params": { "sessionId": "s1", "targetInfo": { "targetId": "tab-1", "type": "page", "url": "https://a/" } } }),
            1,
        );
        buffer.apply(
            &json!({ "method": "Runtime.consoleAPICalled", "sessionId": "s1",
                "params": { "type": "error", "args": [{ "type": "string", "value": "boom" }] } }),
            2,
        );
        // 归属缺失样本：未登记 session 的迟到 console 帧
        buffer.apply(
            &json!({ "method": "Runtime.consoleAPICalled", "sessionId": "ghost",
                "params": { "type": "error", "args": [{ "type": "string", "value": "late" }] } }),
            3,
        );
        drop(buffer);

        // 带 tab 查询：降级全量 + meta 标注（domain 语义在 service 响应中如实透出）
        let resp = svc
            .console(&ins.id, Some("error"), Some("tab-1"), None)
            .unwrap();
        assert_eq!(resp["entries"].as_array().unwrap().len(), 2);
        assert_eq!(resp["meta"]["filteredByTab"], "tab-1");
        assert_eq!(resp["meta"]["tabFilterUnavailable"], true);
        assert_eq!(resp["meta"]["before"], 2);
        assert_eq!(resp["meta"]["after"], 2);
        // 证据边界与 tab 过滤字段同层：tab 过滤信息不得挤掉窗口/溢出声明
        assert_eq!(resp["meta"]["dropped"], 0);
        assert_eq!(resp["meta"]["capacity"], 500);
        assert!(resp["meta"]["since"].as_i64().is_some(), "watcher 在 = 有窗口起点");
        assert_eq!(resp["meta"]["live"], true);
        assert!(resp["meta"].get("scope").is_none(), "console 无 scope 语义");

        // network 透传：空环 + tab 过滤 → 空 entries，meta 正常（不降级）+ scope 声明
        let resp = svc.network(&ins.id, false, Some("tab-1"), None).unwrap();
        assert_eq!(resp["entries"].as_array().unwrap().len(), 0);
        assert_eq!(resp["meta"]["tabFilterUnavailable"], false);
        assert_eq!(resp["meta"]["scope"], "completed");
        assert_eq!(resp["meta"]["capacity"], 1000);

        // 未请求 tab：meta 恒在（证据边界契约），只是无 tab 过滤字段
        let resp = svc.console(&ins.id, None, None, None).unwrap();
        assert_eq!(resp["entries"].as_array().unwrap().len(), 2);
        assert_eq!(resp["meta"]["capacity"], 500);
        assert!(resp["meta"].get("filteredByTab").is_none());

        // har：HAR 本体不动，顶层 _meta 带同一套证据边界 + uncollected 占位语义
        let har = svc.har(&ins.id).unwrap();
        assert!(har["_meta"]["since"].as_i64().is_some());
        assert_eq!(har["_meta"]["live"], true);
        assert_eq!(har["_meta"]["dropped"], 0);
        assert_eq!(har["_meta"]["capacity"], 1000);
        assert_eq!(har["_meta"]["scope"], "completed");
        assert!(har["_meta"]["uncollected"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.as_str().unwrap_or("").contains("timings")));
    }

    // ---------- body 读取端点与拉取编排 ----------

    /// 事件喂入工具：完整走完一个请求（指定 mime）并返回 requestId
    fn finish_request(buf: &mut std::sync::MutexGuard<'_, DiagBuffer>, rid: &str, mime: &str) {
        buf.apply(
            &json!({ "method": "Network.requestWillBeSent", "sessionId": "s1",
                "params": { "requestId": rid, "wallTime": 1.0, "timestamp": 1.0,
                    "request": { "method": "GET", "url": format!("https://a/{rid}") } } }),
            0,
        );
        buf.apply(
            &json!({ "method": "Network.responseReceived", "sessionId": "s1",
                "params": { "requestId": rid, "response": { "status": 200, "mimeType": mime } } }),
            0,
        );
        buf.apply(
            &json!({ "method": "Network.loadingFinished", "sessionId": "s1",
                "params": { "requestId": rid, "timestamp": 1.0 } }),
            0,
        );
    }

    /// body 端点语义与契约字段：available 可读（分页 + truncated）、
    /// 不可读状态 → 409 NETWORK_BODY_UNAVAILABLE、不在环 → 404、
    /// network 列表条目携带 bodyState/bodyReason（meta 契约不变）
    #[test]
    fn network_body_endpoint_semantics() {
        let (repo, ins) = setup();
        let svc = DiagnosticsService::new(Arc::clone(&repo), "0.6.0".into());
        let watched = Arc::new(Watched::new());
        svc.registry
            .lock()
            .unwrap()
            .insert(ins.id.clone(), Arc::clone(&watched));
        watched.buffer.lock().unwrap().apply(
            &json!({ "method": "Target.attachedToTarget",
                "params": { "sessionId": "s1", "targetInfo": { "targetId": "tab-1", "type": "page", "url": "https://a/" } } }),
            0,
        );
        {
            let mut buf = watched.buffer.lock().unwrap();
            finish_request(&mut buf, "r1", "application/json");
            finish_request(&mut buf, "r2", "image/png");
            finish_request(&mut buf, "r3", "application/json");
            buf.store_body(
                "r1",
                BodyPayload::Raw { body: "{\"token\":\"x\",\"data\":[1,2,3]}".into(), base64_encoded: false },
            );
            // r3 超时终态
            buf.store_body("r3", BodyPayload::Unavailable { reason: crate::domain::diagnostics::REASON_TIMEOUT });
        }

        // 列表契约：条目携带 bodyState/bodyReason；meta 结构不变（无新增键语义）
        let list = svc.network(&ins.id, false, None, None).unwrap();
        let e0 = &list["entries"][0];
        assert_eq!(e0["bodyState"], "available");
        assert!(e0.get("bodyReason").is_none(), "非 unavailable 无 bodyReason");
        assert_eq!(list["entries"][1]["bodyState"], "unavailable");
        assert_eq!(list["entries"][1]["bodyReason"], "mime");
        assert_eq!(list["meta"]["scope"], "completed");

        // 可读：全文一次返回
        let body = svc.network_body(&ins.id, "r1", None, None).unwrap();
        assert_eq!(body["bodyState"], "available");
        assert_eq!(body["body"], "{\"token\":\"[redacted]\",\"data\":[1,2,3]}");
        assert_eq!(body["returned"], 37);
        assert_eq!(body["totalLength"], 37);
        assert_eq!(body["bodyBytes"], 37);
        assert_eq!(body["truncated"], false);

        // 分页：limit=10 → 前半 + truncated=true；续读 offset=10
        let page = svc.network_body(&ins.id, "r1", Some(0), Some(10)).unwrap();
        assert_eq!(page["returned"], 10);
        assert_eq!(page["truncated"], true);
        assert_eq!(page["offset"], 0);
        let next = svc.network_body(&ins.id, "r1", Some(10), Some(10)).unwrap();
        assert_eq!(next["offset"], 10);

        // 不可读（mime）→ 409 NETWORK_BODY_UNAVAILABLE，message 携带归因
        let err = svc.network_body(&ins.id, "r2", None, None).unwrap_err();
        assert_eq!((err.status(), err.code()), (409, "NETWORK_BODY_UNAVAILABLE"));
        assert!(err.to_string().contains("mime"));
        let err = svc.network_body(&ins.id, "r3", None, None).unwrap_err();
        assert!(err.to_string().contains("timeout"));

        // 不在环 → 404 NETWORK_BODY_NOT_FOUND
        let err = svc.network_body(&ins.id, "ghost", None, None).unwrap_err();
        assert_eq!((err.status(), err.code()), (404, "NETWORK_BODY_NOT_FOUND"));

        // 实例停止 → 409 DIAGNOSTICS_UNAVAILABLE（与 network 查询同门卫）
        repo.update_runtime(
            &ins.id,
            crate::domain::instance::RuntimeUpdate {
                status: Some(InstanceStatus::Stopped),
                ..Default::default()
            },
        )
        .unwrap();
        let err = svc.network_body(&ins.id, "r1", None, None).unwrap_err();
        assert_eq!(err.code(), "DIAGNOSTICS_UNAVAILABLE");
    }

    /// 排队溢出：backlog 打满时新拉取同步归因 no-data（不停畆在 not-finished）
    #[test]
    fn body_fetch_dispatch_sheds_when_backlog_full() {
        let watched = Arc::new(Watched::new());
        watched
            .buffer
            .lock()
            .unwrap()
            .apply(
                &json!({ "method": "Target.attachedToTarget",
                    "params": { "sessionId": "s1", "targetInfo": { "targetId": "t", "type": "page", "url": "https://a/" } } }),
                0,
            );
        let mut buf = watched.buffer.lock().unwrap();
        finish_request(&mut buf, "r1", "application/json");
        drop(buf);
        watched
            .body_backlog
            .store(BODY_BACKLOG_CAP, Ordering::Relaxed);
        dispatch_body_fetch(&watched, "s1".into(), "r1".into()); // 同步溢出路径，不 spawn
        let e = &watched
            .buffer
            .lock()
            .unwrap()
            .network_entries(false, None, 10)
            .entries[0];
        assert_eq!(
            (e.body_state, e.body_reason),
            (BodyState::Unavailable, Some(crate::domain::diagnostics::REASON_NO_DATA))
        );
    }

    /// 拉取路径（FakeCdp）：mock ws server 脚本化 getResponseBody 三种应答——
    /// 正常文本（base64 与否）、CDP 拒绝、悬挂（超时）。事件驱动链路的 IO 段
    /// 在此验证；mock 形态与 infrastructure/cdp/ws.rs 测试同源。
    #[tokio::test]
    async fn fetch_response_body_against_mock_cdp() {
        use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
        use axum::response::Response;
        use axum::routing::get;
        use axum::Router;

        async fn mock_socket(mut socket: WebSocket) {
            while let Some(Ok(msg)) = socket.recv().await {
                let Message::Text(text) = msg else { continue };
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                let Some(id) = v.get("id").and_then(|x| x.as_u64()) else { continue };
                let reply = match v["params"]["requestId"].as_str() {
                    Some("r-ok") => serde_json::json!({
                        "id": id,
                        "result": { "body": "Bearer sec.ret", "base64Encoded": false }
                    }),
                    Some("r-b64") => serde_json::json!({
                        "id": id,
                        "result": {
                            "body": base64::Engine::encode(
                                &base64::engine::general_purpose::STANDARD, "plain text"),
                            "base64Encoded": true
                        }
                    }),
                    Some("r-deny") => serde_json::json!({
                        "id": id,
                        "error": { "code": -32000, "message": "No resource with given identifier" }
                    }),
                    Some("r-hang") => continue, // 不应答：超时路径
                    _ => continue,
                };
                let _ = socket
                    .send(Message::Text(reply.to_string().into()))
                    .await;
            }
        }
        let app = Router::new().route(
            "/ws",
            get(|ws: WebSocketUpgrade| async move { ws.on_upgrade(mock_socket) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let conn = CdpConnection::connect_ws(
            &format!("ws://127.0.0.1:{}/ws", addr.port()),
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        // 正常：文本（脱敏在域层，此处只验取回）
        match fetch_response_body(&conn, "sess", "r-ok", Duration::from_secs(2)).await {
            BodyPayload::Raw { body, base64_encoded } => {
                assert_eq!(body, "Bearer sec.ret");
                assert!(!base64_encoded);
            }
            other => panic!("应为 Raw: {other:?}"),
        }
        // base64 形态透传（解码在域层 store_body）
        match fetch_response_body(&conn, "sess", "r-b64", Duration::from_secs(2)).await {
            BodyPayload::Raw { body: _, base64_encoded } => assert!(base64_encoded),
            other => panic!("应为 Raw: {other:?}"),
        }
        // CDP 拒绝 → no-data
        match fetch_response_body(&conn, "sess", "r-deny", Duration::from_secs(2)).await {
            BodyPayload::Unavailable { reason } => {
                assert_eq!(reason, crate::domain::diagnostics::REASON_NO_DATA)
            }
            other => panic!("应为 Unavailable: {other:?}"),
        }
        // 悬挂 → 超时（预算 300ms，验证不等待命令通道 15s）
        let start = std::time::Instant::now();
        match fetch_response_body(&conn, "sess", "r-hang", Duration::from_millis(300)).await {
            BodyPayload::Unavailable { reason } => {
                assert_eq!(reason, crate::domain::diagnostics::REASON_TIMEOUT)
            }
            other => panic!("应为 Unavailable: {other:?}"),
        }
        assert!(start.elapsed() < Duration::from_secs(2), "超时预算生效，非命令通道 15s");
    }
}
