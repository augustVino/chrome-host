use super::{cdp_send, require_conn};
use crate::application::page_service::state::Watched;
use crate::domain::page_control::PageControlError;
use crate::error::AppError;
use crate::infrastructure::cdp::page_cdp::SharedPageConn;
use serde_json::{json, Value};
/// JS Number 的 toString 形态（150.0 → "150"）：结果文案里的坐标格式基准。
pub(crate) fn js_num(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// 截断：40 字符 + '…'（fill 回读摘要用）
pub(crate) fn truncate40(s: &str) -> String {
    if s.chars().count() > 40 {
        let mut t: String = s.chars().take(40).collect();
        t.push('…');
        t
    } else {
        s.to_string()
    }
}

// ============================================================================
// 4. 页内 eval 基件
// ============================================================================

/// Runtime.evaluate 的完整 result RemoteObject（eval 取值 / eval 工具序列化 /
/// html 共用基件；每次先 Runtime.enable（幂等），
/// returnByValue + awaitPromise）。页面异常 → 500 PAGE_EVAL_FAILED。
async fn eval_remote_object(
    conn: &SharedPageConn,
    sid: &str,
    expression: &str,
) -> Result<Value, AppError> {
    let _: Value = cdp_send(conn, "Runtime.enable", json!({}), Some(sid)).await?;
    let resp = cdp_send(
        conn,
        "Runtime.evaluate",
        json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
        Some(sid),
    )
    .await?;
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
    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
}

/// eval + 值提取：returnByValue 下 undefined 与 null 都坍缩为 Value::Null
/// （需要区分二者的工具直接用 [`eval_remote_object`] 读 result.type）。
pub(crate) async fn eval_raw(
    conn: &SharedPageConn,
    sid: &str,
    expression: &str,
) -> Result<Value, AppError> {
    Ok(eval_remote_object(conn, sid, expression)
        .await?
        .get("value")
        .cloned()
        .unwrap_or(Value::Null))
}

/// eval + JSON.parse：页内脚本约定返回 JSON 字符串；
/// 垃圾返回 → 500（wait 把一切 eval 错误都当 detached 继续轮询）。
pub(crate) async fn eval_json(
    conn: &SharedPageConn,
    sid: &str,
    expression: &str,
    tool: &str,
) -> Result<Value, AppError> {
    let raw = eval_raw(conn, sid, expression).await?;
    let s = match raw.as_str() {
        Some(s) => s.to_string(),
        None => raw.to_string(),
    };
    let brief = s.chars().take(200).collect::<String>();
    serde_json::from_str(&s).map_err(|_| {
        AppError::business(
            500,
            "PAGE_EVAL_FAILED",
            format!("{tool}: unexpected eval result: {brief}"),
        )
    })
}

/// Runtime.callFunctionOn（this = objectId 的页内函数调用，fill/几何回退用）。
/// 页面异常 → 500 PAGE_EVAL_FAILED（消息带 fill 前缀）。
pub(crate) async fn call_fn(
    conn: &SharedPageConn,
    sid: &str,
    object_id: &str,
    declaration: &str,
    args: Vec<Value>,
) -> Result<Value, AppError> {
    let resp = cdp_send(
        conn,
        "Runtime.callFunctionOn",
        json!({
            "objectId": object_id,
            "functionDeclaration": declaration,
            "arguments": args.into_iter().map(|v| json!({ "value": v })).collect::<Vec<_>>(),
            "returnByValue": true,
        }),
        Some(sid),
    )
    .await?;
    if let Some(d) = resp.get("exceptionDetails") {
        let text = d
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| d.pointer("/exception/description").and_then(Value::as_str))
            .unwrap_or("unknown error");
        return Err(AppError::business(
            500,
            "PAGE_EVAL_FAILED",
            format!("fill: page script failed: {text}"),
        ));
    }
    Ok(resp
        .pointer("/result/value")
        .cloned()
        .unwrap_or(Value::Null))
}

// ============================================================================
// 6b. 工具编排续：eval / html / clickxy / type / loadall
// （复用既有 eval 基件与 click 派发链）
// ============================================================================

/// eval 的结果序列化：对象/数组 → pretty JSON 字符串（
/// JSON.stringify(v, null, 2) 的 2 空格缩进）、字符串原样、
/// undefined/null → "undefined"/"null"、数字/布尔 → JS String
/// 形态（undefined 取字面量而非空串——空串与「无返回」
/// 不可区分，字面量更诚实）。
fn eval_str_from_remote(result: &Value) -> String {
    match result.get("value") {
        // returnByValue 下 undefined 无 value 键、null 的 value 为 JSON null，
        // 二者在 Value 上坍缩，用 result.type 区分
        None | Some(Value::Null) => match result.get("type").and_then(Value::as_str) {
            Some("object") => "null".to_string(),
            _ => "undefined".to_string(),
        },
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => js_num(n.as_f64().unwrap_or(0.0)),
        Some(Value::Bool(b)) => b.to_string(),
        Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".into()),
    }
}

pub(crate) async fn eval_tool(
    watched: &Watched,
    sid: &str,
    expression: &str,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let result = eval_remote_object(&conn, sid, expression).await?;
    Ok(json!({ "result": eval_str_from_remote(&result) }))
}

/// raw <method> [json]：任意 CDP 命令透传（逃生舱，权限等价 eval）。
/// browser=true 发 browser 级（Target.getTargets 等），
/// 否则发到 tab 的 page session（Runtime/DOM/Input/Emulation 全域可达）。
/// 响应 = CDP 原始 result 对象，原样透传不裁剪。
pub(crate) async fn raw_tool(
    watched: &Watched,
    sid: &str,
    method: &str,
    params: Value,
    browser: bool,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let result: Value = cdp_send(
        &conn,
        method,
        params,
        if browser { None } else { Some(sid) },
    )
    .await
    .map_err(|e| AppError::from(PageControlError::dispatch_failed(&e.to_string(), false)))?;
    Ok(json!({ "result": result }))
}

/// html：selector 缺省 = 整文档 outerHTML；命中元素 → outerHTML。
/// 未命中报 404 PAGE_ELEMENT_NOT_FOUND——与 click/locate
/// 的未命中语义一致，调用方按码分支而非解析文案。
pub(crate) async fn html_tool(
    watched: &Watched,
    sid: &str,
    selector: Option<&str>,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let expr = match selector {
        None => "document.documentElement.outerHTML".to_string(),
        Some(sel) => format!(
            "(function() {{ const el = document.querySelector({sel}); return el ? el.outerHTML : null; }})()",
            sel = crate::domain::page_control::json_escape(sel)
        ),
    };
    let result = eval_remote_object(&conn, sid, &expr).await?;
    match result.get("value") {
        Some(Value::String(html)) => Ok(json!({ "html": html })),
        // selector 路径的 null = 页内脚本显式回的未命中（区别于 eval 异常）
        _ if selector.is_some() => {
            Err(PageControlError::element_not_found(selector.unwrap_or("")).into())
        }
        // 整文档路径不会返回 null；防御空串（不装作成功也不 panic）
        _ => Ok(json!({ "html": "" })),
    }
}
