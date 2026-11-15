use super::{
    cdp_send, fill_focus_lost, focus_failed, require_conn, CLICK_RELEASE_DELAY, WAIT_POLL_INTERVAL,
};
use crate::application::page_service::capture::selector_exists;
use crate::application::page_service::eval::{call_fn, eval_json, js_num, truncate40};
use crate::application::page_service::frames::{live_frame_session, project_frame_point};
use crate::application::page_service::refs::{is_ref_target, resolve_ref_entry, resolve_ref_point};
use crate::application::page_service::state::Watched;
use crate::domain::page_control::{
    fill_fillable_check_expr, hit_test_expr, locate_expr, normalize_ref, observed_matches_state,
    parse_key, wait_observe_expr, ObservedState, PageControlError, WaitState,
};
use crate::error::AppError;
use crate::infrastructure::cdp::page_cdp::SharedPageConn;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
// fill 的 clear/verify 页内脚本（TS fill.mjs 逐字）。check 脚本已由 P2-A 的
/// fill_fillable_check_expr 提供；clear/verify 需要把同一个 fillEditable 内联
/// 进各自模板（页面无模块作用域），故保留一份源码文本（TS 的 toString 技巧）。
const FILL_EDITABLE: &str = r#"function fillEditable(el) {
  if (!el || !el.isConnected) return { editable: false, why: 'element is not in the document' };
  const disabled = el.matches(':disabled'); // covers ancestor <fieldset disabled> too
  if (el.tagName === 'TEXTAREA') {
    if (disabled) return { editable: false, why: 'disabled' };
    if (el.readOnly) return { editable: false, why: 'readonly' };
    return { editable: true, kind: 'native' };
  }
  if (el.tagName === 'INPUT') {
    const type = (el.getAttribute('type') || 'text').toLowerCase();
    const TEXTUAL = ['text', 'search', 'tel', 'url', 'email', 'password', 'number'];
    if (!TEXTUAL.includes(type)) {
      return { editable: false, why: `input type "${type}" is not fillable (use a text-like input)` };
    }
    if (disabled) return { editable: false, why: 'disabled' };
    if (el.readOnly) return { editable: false, why: 'readonly' };
    return { editable: true, kind: 'native' };
  }
  if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) {
    return { editable: true, kind: 'editable' };
  }
  return {
    editable: false,
    why: el.isContentEditable
      ? 'not the contenteditable editor root (fill the editing host itself instead)'
      : 'not a text input, textarea, or contenteditable editor root',
  };
}"#;

fn fill_clear_expr() -> String {
    r#"function() {
  const check = (__FILL_EDITABLE__)(this);
  if (!check.editable) return { not_fillable: check.why };
  // DOM focus (getRootNode semantics work in background tabs, unlike
  // document.hasFocus()).
  if (this.getRootNode().activeElement !== this) return { focus_lost: true };
  if (check.kind === 'native') {
    const proto = this.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(this, '');
  } else {
    this.textContent = '';
  }
  this.dispatchEvent(new Event('input', { bubbles: true }));
  return { cleared: true };
}"#
    .replace("__FILL_EDITABLE__", FILL_EDITABLE)
}

fn fill_verify_expr() -> String {
    r#"function(multiLine, singleLine) {
  if (!this.isConnected) return { gone: true };
  const check = (__FILL_EDITABLE__)(this);
  const actual = check.kind === 'native' ? this.value : this.textContent;
  const expected = check.kind === 'native' && this.tagName === 'INPUT' ? singleLine : multiLine;
  return { matches: actual === expected, actual: String(actual === undefined ? '' : actual), expected };
}"#
    .replace("__FILL_EDITABLE__", FILL_EDITABLE)
}

/// press --target 的 selector 路径 focus 脚本（TS press.mjs focusPressTarget）。
fn press_focus_expr(selector: &str) -> String {
    r#"(function() {
  const el = document.querySelector(__SEL__);
  if (!el) return JSON.stringify({ error: 'not_found' });
  el.scrollIntoView({ block: 'center' });
  el.focus();
  if (el.getRootNode().activeElement !== el) return JSON.stringify({ error: 'focus_failed' });
  const label = el.getAttribute('aria-label') || el.getAttribute('name')
    || el.getAttribute('placeholder') || el.id || '';
  return JSON.stringify({ tag: el.tagName, label });
})()"#
        .replace(
            "__SEL__",
            &crate::domain::page_control::json_escape(selector),
        )
}

/// locate 结果分类（TS classifyLocate）→ (x, y, tag, text)。
pub(crate) fn classify_locate(
    r: &Value,
    selector: &str,
) -> Result<(f64, f64, String, String), AppError> {
    match r.get("error").and_then(Value::as_str) {
        Some("not_found") => return Err(PageControlError::element_not_found(selector).into()),
        Some("not_visible") => {
            let why = r.get("why").and_then(Value::as_str).unwrap_or("");
            return Err(PageControlError::element_not_visible(selector, why).into());
        }
        _ => {}
    }
    // 页面必须回有限坐标，否则 hit-test 表达式可能被注入内容构造
    match (
        r.get("x").and_then(Value::as_f64),
        r.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Ok((
            x,
            y,
            r.get("tag")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            r.get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        )),
        _ => Err(AppError::internal(format!(
            "click: unexpected locate result for {selector}"
        ))),
    }
}

/// hit-test 结果分类（TS classifyHit）：命中静默通过，其余按原因报错。
pub(crate) fn classify_hit(r: &Value, selector: &str) -> Result<(), AppError> {
    if r.get("hit").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    match r.get("reason").and_then(Value::as_str) {
        Some("target-gone") => Err(PageControlError::element_not_found(selector).into()),
        Some("covered") => Err(PageControlError::element_blocked_covered(
            selector,
            r.get("by").and_then(Value::as_str).unwrap_or(""),
        )
        .into()),
        Some("nothing-at-point") => Err(PageControlError::element_blocked_nothing(selector).into()),
        _ => Err(AppError::internal(format!(
            "click: unexpected hit-test result for {selector}"
        ))),
    }
}

/// 三段鼠标派发（§2.4）：mouseMoved → mousePressed → 50ms → mouseReleased。
/// press 后失败补一发 release（按钮绝不卡死按下态），effect_state 按位次
/// 标注（§2.7：pre-press = none / post-press = unknown）。
async fn dispatch_click(conn: &SharedPageConn, sid: &str, x: f64, y: f64) -> Result<(), AppError> {
    let base = |ty: &str| json!({ "type": ty, "x": x, "y": y, "button": "left", "clickCount": 1, "modifiers": 0 });
    cdp_send(
        conn,
        "Input.dispatchMouseEvent",
        base("mouseMoved"),
        Some(sid),
    )
    .await
    .map_err(|e| AppError::from(PageControlError::dispatch_failed(&e.to_string(), false)))?;
    cdp_send(
        conn,
        "Input.dispatchMouseEvent",
        base("mousePressed"),
        Some(sid),
    )
    .await
    .map_err(|e| AppError::from(PageControlError::dispatch_failed(&e.to_string(), false)))?;
    tokio::time::sleep(CLICK_RELEASE_DELAY).await;
    if let Err(e) = cdp_send(
        conn,
        "Input.dispatchMouseEvent",
        base("mouseReleased"),
        Some(sid),
    )
    .await
    {
        let _ = cdp_send(
            conn,
            "Input.dispatchMouseEvent",
            base("mouseReleased"),
            Some(sid),
        )
        .await;
        return Err(PageControlError::dispatch_failed(&e.to_string(), true).into());
    }
    Ok(())
}

/// selector 路径点击链（click 与 loadall 共用，TS clickSelectorStr 原生分支）：
/// locate → hit-test → 三段派发，返回人性化结果串。
async fn click_selector(
    conn: &SharedPageConn,
    sid: &str,
    selector: &str,
) -> Result<String, AppError> {
    let located = eval_json(conn, sid, &locate_expr(selector), "click").await?;
    let (x, y, tag, text_val) = classify_locate(&located, selector)?;
    let hit = eval_json(conn, sid, &hit_test_expr(selector, x, y), "click").await?;
    classify_hit(&hit, selector)?;
    dispatch_click(conn, sid, x, y).await?;
    Ok(format!(
        "Clicked <{tag}> \"{text_val}\" (native input @{},{})",
        js_num(x),
        js_num(y)
    ))
}

pub(crate) async fn click_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: &str,
) -> Result<Value, AppError> {
    if target.is_empty() {
        return Err(AppError::invalid_request("CSS selector required"));
    }
    let conn = require_conn(watched)?;
    let text = if is_ref_target(target) {
        // ref 路径：查表 → 几何 → 原生坐标点击（refs 永远走原生输入）。
        // W-oopif：帧内 ref 的 quad 是帧本地坐标——逐级投影到根视口后，
        // Input.dispatchMouseEvent 发根 target（浏览器按命中测试路由进 OOPIF）
        let entry = resolve_ref_entry(watched, tid, target)?;
        let p = match &entry.frame_target_id {
            None => resolve_ref_point(&conn, sid, entry.backend_node_id).await?,
            Some(fid) => {
                let Some(fsid) =
                    live_frame_session(watched, tid, fid, entry.frame_session_id.as_deref())
                else {
                    return Err(PageControlError::target_moved(
                        "owning iframe session is gone (frame detached?) — re-snap",
                    )
                    .into());
                };
                let local = resolve_ref_point(&conn, &fsid, entry.backend_node_id).await?;
                project_frame_point(watched, &conn, tid, sid, fid, local).await?
            }
        };
        dispatch_click(&conn, sid, p.x, p.y).await?;
        let label = entry.name.map(|n| format!(" \"{n}\"")).unwrap_or_default();
        format!(
            "Clicked @{}{label} at CSS ({}, {})",
            normalize_ref(target),
            js_num(p.x),
            js_num(p.y)
        )
    } else {
        // selector 路径：locate → hit-test → 三段派发（TS clickSelectorStr）
        click_selector(&conn, sid, target).await?
    };
    Ok(json!({ "result": text }))
}

/// fill 目标解析 → (稳定 objectId, ref 名, 描述, **执行会话**)（SPEC
/// DOM-script 原则：两次 eval 之间选择器可能命中不同元素，对象句柄跨调用
/// 稳定）。ref 路径失败 = target_moved；selector 路径 nodeId 0 =
/// element_not_found（与 click 同码）。W-oopif：帧内 ref 的 DOM/Runtime
/// 命令全部路由到 frame session（backendNodeId 属帧自己的 DOM agent）。
#[allow(clippy::type_complexity)]
async fn resolve_fill_target(
    watched: &Watched,
    conn: &SharedPageConn,
    tid: &str,
    sid: &str,
    target: &str,
) -> Result<(String, Option<String>, String, String), AppError> {
    if is_ref_target(target) {
        let entry = resolve_ref_entry(watched, tid, target)?;
        let esid = match &entry.frame_target_id {
            None => sid.to_string(),
            Some(fid) => live_frame_session(watched, tid, fid, entry.frame_session_id.as_deref())
                .ok_or_else(|| {
                PageControlError::target_moved(
                    "owning iframe session is gone (frame detached?) — re-snap",
                )
            })?,
        };
        let resp = match cdp_send(
            conn,
            "DOM.resolveNode",
            json!({ "backendNodeId": entry.backend_node_id }),
            Some(&esid),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => return Err(PageControlError::target_moved(&e.to_string()).into()),
        };
        let Some(object_id) = resp.pointer("/object/objectId").and_then(Value::as_str) else {
            return Err(PageControlError::target_moved("no objectId").into());
        };
        Ok((
            object_id.to_string(),
            entry.name,
            format!("@{}", normalize_ref(target)),
            esid,
        ))
    } else {
        if target.is_empty() {
            return Err(AppError::invalid_request(
                "CSS selector or @eN ref required",
            ));
        }
        let doc = cdp_send(conn, "DOM.getDocument", json!({}), Some(sid)).await?;
        let root = doc
            .pointer("/root/nodeId")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let q = cdp_send(
            conn,
            "DOM.querySelector",
            json!({ "nodeId": root, "selector": target }),
            Some(sid),
        )
        .await?;
        let node_id = q.get("nodeId").and_then(Value::as_i64).unwrap_or(0);
        if node_id == 0 {
            return Err(PageControlError::element_not_found(target).into());
        }
        let resp = match cdp_send(
            conn,
            "DOM.resolveNode",
            json!({ "nodeId": node_id }),
            Some(sid),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(AppError::internal(format!(
                    "fill: could not resolve {target} to a node ({e})"
                )))
            }
        };
        let Some(object_id) = resp.pointer("/object/objectId").and_then(Value::as_str) else {
            return Err(AppError::internal(format!(
                "fill: could not resolve {target} to a node (no objectId)"
            )));
        };
        Ok((
            object_id.to_string(),
            None,
            target.to_string(),
            sid.to_string(),
        ))
    }
}

fn require_fillable(r: &Value, desc: &str) -> Result<(), AppError> {
    if let Some(why) = r.get("not_fillable").and_then(Value::as_str) {
        return Err(PageControlError::target_not_fillable(desc, why).into());
    }
    Ok(())
}

pub(crate) async fn fill_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: &str,
    value: &str,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let (object_id, ref_name, desc, esid) =
        resolve_fill_target(watched, &conn, tid, sid, target).await?;
    let outcome = fill_pipeline(&conn, &esid, &object_id, &desc, ref_name, value).await;
    // finally 语义（TS）：无论成败都归还对象句柄（best-effort）——归还发到
    // 解析它的同一会话（objectId 会话作用域）
    let _ = cdp_send(
        &conn,
        "Runtime.releaseObject",
        json!({ "objectId": object_id }),
        Some(&esid),
    )
    .await;
    Ok(json!({ "result": outcome? }))
}

async fn fill_pipeline(
    conn: &SharedPageConn,
    sid: &str,
    object_id: &str,
    desc: &str,
    ref_name: Option<String>,
    value: &str,
) -> Result<String, AppError> {
    // focus 前滚动 best-effort
    let _ = cdp_send(
        conn,
        "DOM.scrollIntoViewIfNeeded",
        json!({ "objectId": object_id }),
        Some(sid),
    )
    .await;
    let check = call_fn(conn, sid, object_id, &fill_fillable_check_expr(), vec![]).await?;
    require_fillable(&check, desc)?;
    if let Err(e) = cdp_send(
        conn,
        "DOM.focus",
        json!({ "objectId": object_id }),
        Some(sid),
    )
    .await
    {
        // Chrome 对 disabled 控件拒绝 DOM.focus：复查后按不可填报，而非泛化 focus 失败
        let recheck = call_fn(conn, sid, object_id, &fill_fillable_check_expr(), vec![]).await?;
        require_fillable(&recheck, desc)?;
        return Err(fill_focus_lost(format!(
            "fill target {desc} could not be focused ({e})"
        )));
    }
    let clear = call_fn(conn, sid, object_id, &fill_clear_expr(), vec![]).await?;
    require_fillable(&clear, desc)?; // 页面可能在检查后禁用了它
    if clear.get("focus_lost").and_then(Value::as_bool) == Some(true) {
        return Err(fill_focus_lost(format!(
            "fill target {desc} does not have focus (page moved it after focus)"
        )));
    }
    // insertText 丢弃 CR 载体；单行 input 把换行折成空格——两种期望值都预计算
    let multi_line = value.replace("\r\n", "\n").replace('\r', "\n");
    if !multi_line.is_empty() {
        cdp_send(
            conn,
            "Input.insertText",
            json!({ "text": multi_line }),
            Some(sid),
        )
        .await?;
    }
    let single_line = multi_line.replace('\n', " ");
    let verify = call_fn(
        conn,
        sid,
        object_id,
        &fill_verify_expr(),
        vec![
            Value::String(multi_line.clone()),
            Value::String(single_line),
        ],
    )
    .await?;
    if verify.get("gone").and_then(Value::as_bool) == Some(true) {
        return Err(PageControlError::fill_value_mismatch(format!(
            "fill target {desc} was removed or replaced during fill — observe the page before deciding whether to retry"
        ))
        .into());
    }
    if verify.get("matches").and_then(Value::as_bool).is_none() {
        return Err(PageControlError::fill_value_mismatch(format!(
            "fill could not verify the value of {desc} (unexpected verify result)"
        ))
        .into());
    }
    if verify.get("matches").and_then(Value::as_bool) != Some(true) {
        let expected = verify
            .get("expected")
            .and_then(Value::as_str)
            .unwrap_or(&multi_line);
        let actual = verify.get("actual").and_then(Value::as_str).unwrap_or("");
        return Err(PageControlError::fill_value_mismatch(format!(
            "fill could not verify the expected value (the write may have partially applied — observe before retrying): expected \"{}\", got \"{}\"",
            truncate40(expected),
            truncate40(actual)
        ))
        .into());
    }
    let tag = check
        .get("tag")
        .and_then(Value::as_str)
        .unwrap_or("ELEMENT");
    let label = ref_name
        .or_else(|| {
            check
                .get("label")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|l| !l.is_empty());
    let shown = truncate40(value);
    let n = value.chars().count();
    let unit = if n == 1 { "char" } else { "chars" };
    Ok(format!(
        "Filled <{tag}>{} with \"{shown}\" ({n} {unit})",
        label.map(|l| format!(" \"{l}\"")).unwrap_or_default()
    ))
}

/// press 的 --target focus（TS focusPressTarget）：ref 走 DOM.focus
/// （backendNodeId），selector 走页内 eval（scrollIntoView + focus + 复查）。
#[allow(clippy::type_complexity)]
async fn focus_press_target(
    watched: &Watched,
    conn: &SharedPageConn,
    tid: &str,
    sid: &str,
    target: &str,
) -> Result<(String, String), AppError> {
    if is_ref_target(target) {
        let entry = resolve_ref_entry(watched, tid, target)?;
        // W-oopif：帧内 ref 的滚动/聚焦在 frame session 上做
        let esid = match &entry.frame_target_id {
            None => sid.to_string(),
            Some(fid) => live_frame_session(watched, tid, fid, entry.frame_session_id.as_deref())
                .ok_or_else(|| {
                PageControlError::target_moved(
                    "owning iframe session is gone (frame detached?) — re-snap",
                )
            })?,
        };
        let _ = cdp_send(
            conn,
            "DOM.scrollIntoViewIfNeeded",
            json!({ "backendNodeId": entry.backend_node_id }),
            Some(&esid),
        )
        .await;
        if let Err(e) = cdp_send(
            conn,
            "DOM.focus",
            json!({ "backendNodeId": entry.backend_node_id }),
            Some(&esid),
        )
        .await
        {
            return Err(focus_failed(format!(
                "press --target {target} could not be focused ({e})"
            )));
        }
        let key = normalize_ref(target);
        Ok((
            entry
                .name
                .map(|n| format!("@{key} \"{n}\""))
                .unwrap_or(format!("@{key}")),
            esid,
        ))
    } else {
        let r = eval_json(conn, sid, &press_focus_expr(target), "press").await?;
        match r.get("error").and_then(Value::as_str) {
            Some("not_found") => Err(PageControlError::element_not_found(target).into()),
            Some("focus_failed") => Err(focus_failed(format!(
                "press --target {target} could not be focused"
            ))),
            _ => {
                let Some(tag) = r.get("tag").and_then(Value::as_str) else {
                    return Err(AppError::internal(format!(
                        "press: unexpected focus result for {target}"
                    )));
                };
                let label = r
                    .get("label")
                    .and_then(Value::as_str)
                    .filter(|l| !l.is_empty());
                Ok((
                    label
                        .map(|l| format!("<{tag}> \"{l}\""))
                        .unwrap_or(format!("<{tag}>")),
                    sid.to_string(),
                ))
            }
        }
    }
}

pub(crate) async fn press_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    key: &str,
    target: Option<&str>,
) -> Result<Value, AppError> {
    if key.is_empty() {
        return Err(AppError::invalid_request(
            "key required (e.g. Enter, a, Control+A)",
        ));
    }
    let kp = parse_key(key)?; // PAGE_KEY_UNKNOWN 400（P2-A）
    let conn = require_conn(watched)?;
    let (where_on, key_sid) = match target.filter(|t| !t.is_empty()) {
        None => (String::new(), sid.to_string()),
        Some(t) => {
            let (label, esid) = focus_press_target(watched, &conn, tid, sid, t).await?;
            (format!(" on {label}"), esid)
        }
    };
    // rawKeyDown (+char) → keyUp（P2-A 序列生成，绝不发裸 keyDown）。
    // char/keyUp 失败补一发 keyUp（按键绝不卡死按下态）；rawKeyDown 失败
    // 原样上抛（尚无输入生效，TS 同）
    let seq = kp.dispatch_sequence();
    let last = serde_json::to_value(seq.last().expect("序列至少 rawKeyDown+keyUp")).unwrap();
    for (i, ev) in seq.iter().enumerate() {
        if let Err(e) = cdp_send(
            &conn,
            "Input.dispatchKeyEvent",
            serde_json::to_value(ev).unwrap(),
            Some(&key_sid),
        )
        .await
        {
            if i > 0 {
                let _ = cdp_send(
                    &conn,
                    "Input.dispatchKeyEvent",
                    last.clone(),
                    Some(&key_sid),
                )
                .await;
            }
            return Err(e);
        }
    }
    Ok(json!({ "result": format!("Pressed {}{where_on}", kp.display()) }))
}

pub(crate) async fn wait_tool(
    watched: &Watched,
    _tid: &str,
    sid: &str,
    selector: &str,
    state: WaitState,
    timeout: Duration,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let expr = wait_observe_expr(selector);
    let start = Instant::now();
    let mut polls: usize = 0;
    let mut last;
    loop {
        polls += 1;
        // eval 失败（导航中）= detached 继续轮询；返回垃圾 = unknown（永不
        // 满足除 attached 外的任何状态——P2-A 语义原样保留）
        last = match eval_json(&conn, sid, &expr, "wait").await {
            Ok(v) => v
                .get("observed")
                .and_then(Value::as_str)
                .map(ObservedState::parse)
                .unwrap_or(ObservedState::Unknown),
            Err(_) => ObservedState::Detached,
        };
        if observed_matches_state(last, state) {
            return Ok(json!({
                "result": format!(
                    "Waited: {selector} is {} (after {}ms, {polls} polls)",
                    state.as_str(),
                    start.elapsed().as_millis()
                )
            }));
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return Err(PageControlError::wait_timeout(
                selector,
                state.as_str(),
                elapsed.as_millis() as i64,
                last.as_str(),
                polls,
            )
            .into());
        }
        // 绝不睡过截止线（TS 同）
        tokio::time::sleep(WAIT_POLL_INTERVAL.min(timeout.saturating_sub(elapsed))).await;
    }
}

/// TS clickXyStr：复用 click 的三段派发链，无 locate/hit-test（坐标即意图）。
pub(crate) async fn clickxy_tool(
    watched: &Watched,
    sid: &str,
    x: f64,
    y: f64,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    dispatch_click(&conn, sid, x, y).await?;
    Ok(json!({
        "result": format!("Clicked at CSS ({}, {})", js_num(x), js_num(y))
    }))
}

/// TS typeStr：对当前焦点元素直接 Input.insertText（无定位、无校验）。
/// 字符计数用 Unicode 标量（fill 同口径；TS 的 .length 是 UTF-16 单元，
/// 仅增补平面字符有差）。
pub(crate) async fn type_tool(watched: &Watched, sid: &str, text: &str) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    cdp_send(
        &conn,
        "Input.insertText",
        json!({ "text": text }),
        Some(sid),
    )
    .await?;
    Ok(json!({
        "result": format!("Typed {} characters", text.chars().count())
    }))
}

/// TS loadAllStr 的循环骨架，两处任务口径偏差：
/// - 点击用原生 selector 链（locate/hit-test/三段派发）替换 TS 的页内
///   el.click() 合成点击——项目纪律是原生输入（synthetic 仅 --unsafe-synth
///   场景才对，此处不引入）；
/// - 上限从 5min 硬顶改为 max-clicks，达上限后补一次存在性复查定文案
///   （诚实区分「消失了」与「仍在页面上」，调用方据此决定是否继续）。
pub(crate) async fn loadall_tool(
    watched: &Watched,
    sid: &str,
    selector: &str,
    interval: Duration,
    max_clicks: u32,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let exists_expr = format!(
        "!!document.querySelector({})",
        crate::domain::page_control::json_escape(selector)
    );
    let mut clicks: u32 = 0;
    let mut disappeared = false;
    while clicks < max_clicks {
        if !selector_exists(&conn, sid, &exists_expr).await? {
            disappeared = true;
            break;
        }
        click_selector(&conn, sid, selector).await?;
        clicks += 1;
        tokio::time::sleep(interval).await;
    }
    if !disappeared {
        // 达上限退出（或 max-clicks=0）：末次观察仍在页面——复查一次定文案
        // （selector_exists 返回「存在」，disappeared 取反）
        disappeared = !selector_exists(&conn, sid, &exists_expr).await?;
    }
    let result = if disappeared {
        format!("Clicked \"{selector}\" {clicks} time(s) until it disappeared")
    } else {
        format!(
            "Clicked \"{selector}\" {clicks} time(s) but it is still present (--max-clicks {max_clicks} reached)"
        )
    };
    Ok(json!({ "result": result }))
}
