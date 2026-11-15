use super::{cdp_send, require_conn};
use crate::application::page_service::eval::eval_json;
use crate::application::page_service::frames::live_frame_session;
use crate::application::page_service::refs::{is_ref_target, resolve_ref_entry};
use crate::application::page_service::state::Watched;
use crate::domain::page_control::PageControlError;
use crate::error::AppError;
use serde_json::{json, Value};
// ============================================================================
// 6a-2. 表格结构化提取（W-ext：收集编排 + 归一化接线）
// ============================================================================

/// extract 工具：目标三路解析（自动发现 / @eN ref（含跨帧）/ CSS 选择器）
/// → 页内收集器（EXTRACT_COLLECT_FN，纯只读事实）→ domain 归一化。
/// 错误映射：目标未命中 404 PAGE_ELEMENT_NOT_FOUND（自动发现无表格同码，
/// message 区分）；命中但非表格 422 PAGE_EXTRACT_NOT_A_TABLE；收集脚本
/// 抛错 500 PAGE_EVAL_FAILED（与 eval 同语义）。
pub(crate) async fn extract_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: Option<&str>,
) -> Result<Value, AppError> {
    use crate::domain::page_control::{
        extract_call_on_fn, extract_discovery_expr, extract_selector_expr,
    };
    use crate::domain::table_extract::{self, TableFacts};

    let conn = require_conn(watched)?;
    let (facts_value, target_desc): (Value, String) = match target {
        None => {
            let v = eval_json(&conn, sid, &extract_discovery_expr(), "extract").await?;
            (v, "(auto-discovered)".to_string())
        }
        Some(t) if is_ref_target(t) => {
            let entry = resolve_ref_entry(watched, tid, t)?;
            // 帧内 ref（W-oopif）：收集在 frame session 上做（press --target 同链）
            let esid = match &entry.frame_target_id {
                None => sid.to_string(),
                Some(fid) => {
                    live_frame_session(watched, tid, fid, entry.frame_session_id.as_deref())
                        .ok_or_else(|| {
                            PageControlError::target_moved(
                                "owning iframe session is gone (frame detached?) — re-snap",
                            )
                        })?
                }
            };
            let resp = cdp_send(
                &conn,
                "DOM.resolveNode",
                json!({ "backendNodeId": entry.backend_node_id }),
                Some(&esid),
            )
            .await
            .map_err(|e| PageControlError::target_moved(&e.to_string()))?;
            let object_id = resp
                .pointer("/object/objectId")
                .and_then(Value::as_str)
                .ok_or_else(|| PageControlError::target_moved("no objectId"))?;
            let resp = cdp_send(
                &conn,
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": extract_call_on_fn(),
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
                Some(&esid),
            )
            .await
            .map_err(|e| PageControlError::target_moved(&e.to_string()))?;
            if let Some(d) = resp.get("exceptionDetails") {
                let text = d
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| d.pointer("/exception/description").and_then(Value::as_str))
                    .unwrap_or("unknown error");
                return Err(AppError::business(
                    500,
                    "PAGE_EVAL_FAILED",
                    text.to_string(),
                ));
            }
            let s = resp
                .pointer("/result/value")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    AppError::internal("extract: 收集器返回缺失（value 非 JSON 字符串）")
                })?;
            let v: Value = serde_json::from_str(s)
                .map_err(|e| AppError::internal(format!("extract: 收集器输出非 JSON: {e}")))?;
            (v, t.to_string())
        }
        Some(sel) => {
            let v = eval_json(&conn, sid, &extract_selector_expr(sel), "extract").await?;
            (v, sel.to_string())
        }
    };

    let facts: TableFacts = serde_json::from_value(facts_value)
        .map_err(|e| AppError::internal(format!("extract: 事实反序列化失败: {e}")))?;
    match facts.reason.as_deref() {
        // 自动发现无表格 / 选择器未命中（选择器路径的 miss 在页内拦）
        Some("no-table") => {
            return Err(PageControlError::element_not_found(
                target.unwrap_or("no <table> in main document"),
            )
            .into())
        }
        Some("not-a-table") => {
            return Err(PageControlError::extract_not_a_table(
                facts.tag.as_deref().unwrap_or("?"),
                facts.role.as_deref(),
            )
            .into())
        }
        _ => {}
    }
    if !facts.found {
        // 收集器契约：found=false 必带 reason；此处防御（不装作空表）
        return Err(AppError::internal(
            "extract: 收集器返回 found=false 但未说明原因",
        ));
    }
    let mut out = table_extract::normalize(&facts);
    out["meta"]["target"] = json!(target_desc);
    Ok(out)
}
