use super::{cdp_send, nav_failed, nav_no_history, require_conn};
use crate::application::page_service::eval::eval_raw;
use crate::application::page_service::state::Watched;
use crate::error::AppError;
use crate::infrastructure::cdp::page_cdp::SharedPageConn;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
/// 导航命令族（navigate/back/forward/reload）的加载等待窗口：CDP 导航命令返回
/// ≠ 加载完成，命令后在同 session 上等待 Page.loadEventFired 至此窗口。
/// 窗口耗尽 → 返回 loaded:false（诚实证据哲学：导航已开始但加载未证实，
/// 不编造成功也不无限等死）。取 25s 而非 30s：给 CDP send（内建 15s）与响应
/// 组装留余量，使「加载未确认」优先于工具预算 504 呈现；SPA 路由不触发 load
/// 的场景见各工具注释与 AGENT-API。
pub(crate) const NAV_LOAD_TIMEOUT: Duration = Duration::from_secs(25);
/// back/forward 的提交确认轮询间隔（历史索引 currentIndex 到位即提交；与
/// wait 轮询同量级，无需参数化）。
const NAV_POLL_INTERVAL: Duration = Duration::from_millis(250);

// ============================================================================
// 6c. 导航命令族（navigate/back/forward/reload）
// ============================================================================

/// load 等待登记：必须在导航命令发送**之前**调用——事件经 watch_loop 异步
/// 分派，若先发送后登记，极速加载（磁盘缓存）的 load 事件可能在登记前到达，
/// 造成永久等不到（假 loaded:false）。登记后发送即无错过窗口（命令响应与
/// 事件同连接保序，load 必要晚于 Page.navigate 响应才可能发生）。
pub(crate) fn register_load_waiter(watched: &Watched, tid: &str) -> oneshot::Receiver<()> {
    let (tx, rx) = oneshot::channel();
    watched
        .load_waiters
        .lock()
        .unwrap()
        .entry(tid.to_string())
        .or_default()
        .push(tx);
    rx
}

/// 等待 load 事件至截止线：true = 窗口内观察到 Page.loadEventFired。
/// 已知边界（诚实披露而非修复）：loadEventFired 不携带 loaderId，若命令
/// 发出时该 tab 恰有一个更早的在途加载，其 load 事件可能被计入（puppeteer
/// 同类时序靠 loaderId 匹配根治，超出本层事件面）。
async fn await_load(rx: oneshot::Receiver<()>, deadline: Instant) -> bool {
    matches!(
        tokio::time::timeout_at(deadline.into(), rx).await,
        Ok(Ok(()))
    )
}

/// navigate <url>：Page.navigate 直发到 page session。
/// 等待语义：CDP 导航命令返回 ≠ 加载完成，本工具等待
/// Page.loadEventFired（窗口 NAV_LOAD_TIMEOUT）后返回：
/// - 窗口内观察到 → loaded:true；
/// - 窗口耗尽 → loaded:false（导航已开始、加载未证实——不编造成功，也不
///   无限等死；调用方用 snap/wait/eval 观察实际状态）；
/// - 同文档导航（#fragment / pushState 型）：响应无新 loader
///   （sameDocumentNavigation / loaderId 缺失）→ 不等待，loaded:true（DOM
///   同步替换，无 load 事件可等）。
///
/// SPA 路由说明：页面内已加载 SPA 的路由切换（pushState/replaceState）不经
/// 过本命令，自然也不触发 load 事件——观察 SPA 路由后的 DOM 用 snap/wait。
pub(crate) async fn navigate_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    url: &str,
    load_timeout: Duration,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let load = register_load_waiter(watched, tid);
    let resp = cdp_send(&conn, "Page.navigate", json!({ "url": url }), Some(sid)).await?;
    // errorText（net::ERR_* 等）= 导航未发生 → 失败而非 loaded:false
    if let Some(err) = resp
        .get("errorText")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Err(nav_failed(err));
    }
    let same_doc = resp.get("sameDocumentNavigation").and_then(Value::as_bool) == Some(true)
        || resp.get("loaderId").is_none_or(Value::is_null);
    let deadline = Instant::now() + load_timeout;
    let (loaded, note) = if same_doc {
        (true, "same-document navigation")
    } else if await_load(load, deadline).await {
        (true, "load complete")
    } else {
        (
            false,
            "load not confirmed within the window — page may still be loading",
        )
    };
    Ok(json!({
        "result": format!("Navigated to {url} ({note})"),
        "url": url,
        "loaded": loaded,
    }))
}

/// 取导航历史并做边界检查：返回 (target index, entryId, url)。
/// 响应形状并带 → INTERNAL_ERROR（防御，CDP 正常不会发垃圾）。
async fn nav_history_target(
    conn: &SharedPageConn,
    sid: &str,
    delta: i64,
) -> Result<(i64, Value, String), AppError> {
    let hist = cdp_send(conn, "Page.getNavigationHistory", json!({}), Some(sid)).await?;
    let current = hist
        .get("currentIndex")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            AppError::internal("unexpected Page.getNavigationHistory response: no currentIndex")
        })?;
    let entries = hist
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            AppError::internal("unexpected Page.getNavigationHistory response: no entries")
        })?;
    let target = current + delta;
    if target < 0 || target >= entries.len() as i64 {
        let direction = if delta < 0 {
            "earlier (back)"
        } else {
            "later (forward)"
        };
        return Err(nav_no_history(direction));
    }
    let entry = &entries[target as usize];
    let id = entry
        .get("id")
        .cloned()
        .ok_or_else(|| AppError::internal("navigation history entry missing id"))?;
    let url = entry
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok((target, id, url))
}

/// back/forward 共用（delta = -1/+1）：取历史 → 边界检查（409
/// PAGE_NAV_NO_HISTORY）→ navigateToHistoryEntry → 等待完成。
/// 完成信号双轨（历史条目分两类，事件面无法提前区分）：
/// - 新文档条目：load 事件（navigate/reload 同款等待）；
/// - 同文档条目（SPA pushState 产生）：不触发 load 事件——以历史索引提交
///   为信号；提交确认后 readyState 已 complete 则立即返回（快路径，不等满
///   窗口）。readyState=complete 与 load 事件触发是同一时刻（HTML 规范：
///   readyState 在 load 触发时变为 complete），loaded:true 证据等价。
pub(crate) async fn history_step_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    delta: i64,
    load_timeout: Duration,
) -> Result<Value, AppError> {
    let direction = if delta < 0 { "back" } else { "forward" };
    let conn = require_conn(watched)?;
    let (target, entry_id, url) = nav_history_target(&conn, sid, delta).await?;
    let load = register_load_waiter(watched, tid);
    cdp_send(
        &conn,
        "Page.navigateToHistoryEntry",
        json!({ "entryId": entry_id }),
        Some(sid),
    )
    .await?;
    let deadline = Instant::now() + load_timeout;
    // 提交确认轮询：currentIndex 到位 = 目标条目已激活（同文档条目同步到位，
    // 新文档条目在提交时到位）
    loop {
        let committed =
            match cdp_send(&conn, "Page.getNavigationHistory", json!({}), Some(sid)).await {
                Ok(h) => h.get("currentIndex").and_then(Value::as_i64) == Some(target),
                Err(_) => false, // 轮询期连接抖动按未提交继续（要么恢复要么超时）
            };
        if committed {
            let ready_complete = matches!(
                eval_raw(&conn, sid, "document.readyState").await,
                Ok(v) if v.as_str() == Some("complete")
            );
            if ready_complete {
                return Ok(json!({
                    "result": format!(
                        "Navigated {direction} to entry {target}: {url} (ready — same-document entry or already loaded)"
                    ),
                    "url": url,
                    "index": target,
                    "loaded": true,
                }));
            }
            break; // 新文档已提交仍在加载 → 转 load 事件等待
        }
        if Instant::now() >= deadline {
            return Ok(json!({
                "result": format!(
                    "Navigated {direction} to entry {target}: {url} (commit not confirmed within the window — navigation may still be pending)"
                ),
                "url": url,
                "index": target,
                "loaded": false,
            }));
        }
        tokio::time::sleep(
            NAV_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
        )
        .await;
    }
    let loaded = await_load(load, deadline).await;
    let note = if loaded {
        "load complete"
    } else {
        "committed, load not confirmed within the window — page may still be loading"
    };
    Ok(json!({
        "result": format!("Navigated {direction} to entry {target}: {url} ({note})"),
        "url": url,
        "index": target,
        "loaded": loaded,
    }))
}

/// reload [--ignore-cache]：Page.reload 无响应负载（错误走 CDP error 通道），
/// 恒为新文档加载 → 一律等待 load 事件（窗口内未见 → loaded:false）。
pub(crate) async fn reload_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    ignore_cache: bool,
    load_timeout: Duration,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let load = register_load_waiter(watched, tid);
    cdp_send(
        &conn,
        "Page.reload",
        json!({ "ignoreCache": ignore_cache }),
        Some(sid),
    )
    .await?;
    let loaded = await_load(load, Instant::now() + load_timeout).await;
    let note = if loaded {
        "load complete"
    } else {
        "load not confirmed within the window — page may still be loading"
    };
    Ok(json!({
        "result": format!("Reloaded ({note})"),
        "loaded": loaded,
    }))
}
