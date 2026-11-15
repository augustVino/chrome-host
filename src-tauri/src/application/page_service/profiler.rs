//! page profiler：kit tracing.rs 的 Tracing.start/end 编排。
//!
//! 常驻任务特例（同 download_wait 模式，不走 command() 骨架）：
//! - **不占实例串行锁**（ensure 后 drop(serial)）：tracing 常驻期间其他
//!   page 命令必须能继续执行；
//! - **无 30s 命令预算**：stop 的事件收集（kit 内部 30s deadline）+ 落盘
//!   可能超预算——外层预算会在 kit 自身的收集窗口中途误杀（数据丢失且
//!   状态悬置），故由 kit 内部 deadline 兜底；
//! - dialogs_seen 不附（Tracing 期间对话框事件与 profiler 语义无关，
//!   page dialogs 可独立查询）。
//!
//! 状态：`Watched.profiler`（实例级单条——Chrome tracing 是实例级单会话
//! 资源）。start 带 TAB 路由会话；stop 无 TAB 入参，按 start 锚点路由。
//! 生命周期：establish 整清（连接死 → tracing session 失效）；锚点 tab
//! 关闭时废弃（drop_tab，见 state.rs 注释）。

use super::state::{ProfilerState, Watched};
use super::{kit_client, PageService};
use crate::application::browser_kit::tracing::{profiler_start, profiler_stop, TracingState};
use crate::error::AppError;
use serde_json::{json, Value};

impl PageService {
    /// page profiler start <tab> [--categories "a,b"]：kit profiler_start
    ///（Tracing.start ReportEvents 模式 + enableSampling）。categories 缺省
    /// = kit DEFAULT_PROFILER_CATEGORIES（devtools.timeline / v8 / blink 全家）。
    pub async fn profiler_start(
        &self,
        ins_id: &str,
        tab: &str,
        categories: Option<Vec<String>>,
    ) -> Result<Value, AppError> {
        let (watched, serial) = self.ensure(ins_id).await?;
        let (tid, sid) = super::resolve_target(&watched.tabs.lock().unwrap(), tab)?;
        // 常驻任务不占实例串行锁（download_wait 特例，见模块注释）
        drop(serial);
        let mut resp = profiler_start_tool(&watched, &tid, &sid, categories).await?;
        resp["instanceId"] = json!(ins_id);
        resp["targetId"] = json!(tid);
        Ok(resp)
    }

    /// page profiler stop [--out <f.json>]：kit profiler_stop（Tracing.end →
    /// 事件收集 → traceEvents JSON 落盘）。out 缺省 = `<实例id>-trace.json`
    ///（服务端 cwd，对齐 screenshot --full 的 `<id>-full.png` 命名风格；
    /// CLI 层另拼本地 cwd 缺省名后透传）。
    pub async fn profiler_stop(
        &self,
        ins_id: &str,
        out: Option<String>,
    ) -> Result<Value, AppError> {
        let (watched, serial) = self.ensure(ins_id).await?;
        drop(serial);
        let out = out.unwrap_or_else(|| format!("{ins_id}-trace.json"));
        let mut resp = profiler_stop_tool(&watched, &out).await?;
        resp["instanceId"] = json!(ins_id);
        Ok(resp)
    }
}

/// start 工具：活跃互斥检查（锁内同步，无 await）→ kit profiler_start 用
/// 本地新 state → 成功才写回锚点（干净失败 = 状态区不残留）。并发双 start
/// 的竞态由 Chrome 兜底（第二次 Tracing.start 报 already started → 409）。
pub(crate) async fn profiler_start_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    categories: Option<Vec<String>>,
) -> Result<Value, AppError> {
    if watched
        .profiler
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|p| p.tracing.active)
    {
        return Err(AppError::conflict(
            "PAGE_PROFILER_ALREADY_ACTIVE",
            "profiler 已在运行（先 profiler stop 落盘再重新 start）",
        ));
    }
    let client = kit_client(watched)?;
    let mut tracing_state = TracingState::new();
    let cats_desc = categories
        .as_ref()
        .map(|c| c.join(","))
        .unwrap_or_else(|| "kit defaults".to_string());
    profiler_start(&client, sid, &mut tracing_state, categories)
        .await
        .map_err(|e| map_profiler_error(&e, "start"))?;
    *watched.profiler.lock().unwrap() = Some(ProfilerState {
        tracing: tracing_state,
        tid: tid.to_string(),
        sid: sid.to_string(),
    });
    Ok(json!({ "result": format!("profiler started (categories: {cats_desc})") }))
}

/// stop 工具：take-out 模式（锁内不 await，跨 await 持有 owned 状态）→
/// kit profiler_stop → 成功 = 终结（不回填）；失败（如 30s 收集超时）=
/// 回填原状态，调用方可重试 stop（tracing 仍 active）。
pub(crate) async fn profiler_stop_tool(watched: &Watched, out: &str) -> Result<Value, AppError> {
    let taken = watched.profiler.lock().unwrap().take();
    let Some(mut state) = taken else {
        return Err(AppError::conflict(
            "PAGE_PROFILER_NOT_RUNNING",
            "无运行中的 profiler（从未 start / 已 stop / 连接重建或 profiler 所在 tab 已关闭致运行态废弃）",
        ));
    };
    let client = kit_client(watched)?;
    match profiler_stop(&client, &state.sid, &mut state.tracing, Some(out)).await {
        Ok(v) => Ok(json!({
            "result": "profiler stopped",
            "path": v.get("path").cloned().unwrap_or(Value::Null),
            "eventCount": v.get("eventCount").cloned().unwrap_or(Value::Null),
        })),
        Err(e) => {
            // 回填：kit 超时路径 active 保持 true，重试 stop 可再走完整收集
            watched.profiler.lock().unwrap().replace(state);
            Err(map_profiler_error(&e, "stop"))
        }
    }
}

/// kit tracing 的 String 错误 → chrome-host 错误码（集成层契约；不复用
/// refs::map_kit_error——其 not found/visible 规则针对元素链文案，会误伤
/// tracing 文案）。
fn map_profiler_error(raw: &str, op: &str) -> AppError {
    let m = raw.to_lowercase();
    if m.contains("already active") {
        AppError::conflict("PAGE_PROFILER_ALREADY_ACTIVE", raw.to_string())
    } else if m.contains("no profiling in progress") {
        AppError::conflict("PAGE_PROFILER_NOT_RUNNING", raw.to_string())
    } else if m.contains("timed out") {
        AppError::business(
            504,
            "PAGE_TOOL_TIMEOUT",
            format!("profiler {op} {raw}（可重试；tracing 仍 active）"),
        )
    } else {
        crate::domain::page_control::PageControlError::action_failed(format!(
            "profiler {op}: {raw}"
        ))
        .into()
    }
}
