//! input 族（I2）：click/fill/type/press 换 browser_kit interaction 内核，
//! 新增 hover/scroll/drag/select/check/tap。分工：
//!
//! - **click**：kit `interaction::click`（resolve 链 + ClickPhase 派发 +
//!   settle + actionability 全开：可见/启用/遮挡 + 对话框竞速）。ref 与
//!   selector 两路径统一走 kit（我方 locate/hit-test JS 与三段派发删除）；
//!   dialog 竞速的 pending_release 补偿在 kit 外闭环（watch_loop 的
//!   on_dialog_opening 先 accept 对话框、事件才转发到 bus，此处补发的
//!   release 落在已解锁的渲染进程上——顺序天然正确）。双击/右键经
//!   ClickSpec 透传。
//! - **fill**：kit `interaction::fill` + 我方契约保全层——可填前置检查
//!   （422 PAGE_TARGET_NOT_FILLABLE，kit 无此检查）+ contenteditable 预清空
//!   （kit 清空只覆盖表单控件）+ 回读校验追加（PAGE_FILL_VALUE_MISMATCH /
//!   gone / PAGE_FILL_FOCUS_LOST——focus_lost 从清空阶段移到回读阶段，
//!   触发点变化、码与状态不变）。
//! - **type**：kit `type_text_into_active_context`（逐字符：可打印字符
//!   insertText，\n/\t 走 keyDown/keyUp——对焦点元素无定位无校验语义不变，
//!   但从单次 insertText 变为逐字符派发）。
//! - **press**：`parse_key` 校验（400 PAGE_KEY_UNKNOWN 契约）后走 kit
//!   `press_key(_with_modifiers)`（keyDown+keyUp 两段——原 rawKeyDown/char/
//!   keyUp 三段序列随之退役，文本抑制语义保留：Ctrl/Meta 组合不发 text）。
//!   `--target` 聚焦链两端统一走 kit resolve（selector 路径原页内 eval
//!   focus 脚本删除；不再先滚动——DOM.focus 不依赖可见性）。
//! - **wait**：**保留我方 eval 轮询实现**（kit 无 wait_for_selector——该
//!   原语在未 vendor 的上游 actions.rs 层；owner 已批：wait 为我方永久
//!   自有原语）。四态映射与 PAGE_WAIT_TIMEOUT（附最后观察态 + 轮询数）
//!   契约不变。
//! - **clickxy**：保留我方三段派发（上游无坐标点击原语；`dispatch_click`
//!   仅此一处调用方）。loadall 循环层换 kit click（selector 路径）。

use super::{
    cdp_send, fill_focus_lost, focus_failed, kit_client, require_conn, CLICK_RELEASE_DELAY,
    TOOL_TIMEOUT, WAIT_POLL_INTERVAL,
};
use crate::application::browser_kit::element::{
    resolve_element_center, resolve_element_object_id, RefMap,
};
use crate::application::browser_kit::interaction::{self, ClickResult};
use crate::application::browser_kit::pointer::{ClickSpec, Modifiers, MouseButton};
use crate::application::browser_kit::CdpClient;
use crate::application::page_service::capture::selector_exists;
use crate::application::page_service::eval::{call_fn, eval_json, js_num, truncate40};
use crate::application::page_service::frames::iframe_sessions_view;
use crate::application::page_service::refs::{is_ref_target, map_kit_error, require_ref};
use crate::application::page_service::state::Watched;
use crate::application::page_service::PageService;
use crate::domain::page_control::{
    fill_fillable_check_expr, normalize_ref, observed_matches_state, parse_key, wait_observe_expr,
    ObservedState, PageControlError, WaitState,
};
use crate::error::AppError;
use crate::infrastructure::cdp::page_cdp::SharedPageConn;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

// fill 的回读校验页内脚本（TS fill.mjs 的 verify 演化）。I2 起 fill 主链
// 走 kit（focus/select 清空/insertText），本脚本是回读契约的载体：
// gone（元素被移除）/ focus_lost（值不一致且焦点不在目标上——页面抢焦点
// 导致 insertText 落到别处）/ matches（含期望/实际摘要）。值一致时不再
// 检查焦点（成功路径行为同旧链：页面 fill 后主动 blur 不算失败）。
// FILL_EDITABLE 内联理由：页面无模块作用域，每个模板各自携带源码文本。
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

fn fill_verify_expr() -> String {
    r#"function(multiLine, singleLine) {
  if (!this.isConnected) return { gone: true };
  const check = (__FILL_EDITABLE__)(this);
  const actual = check.kind === 'native' ? this.value : this.textContent;
  const expected = check.kind === 'native' && this.tagName === 'INPUT' ? singleLine : multiLine;
  if (actual !== expected) {
    if (this.getRootNode().activeElement !== this) return { focus_lost: true };
    return { matches: false, actual: String(actual === undefined ? '' : actual), expected };
  }
  return { matches: true };
}"#
    .replace("__FILL_EDITABLE__", FILL_EDITABLE)
}

/// contenteditable 预清空（I2）：kit fill 的清空是 `select() + value = ''`，
/// 只对表单控件生效；编辑根需要 textContent 清空。focus 由随后的 kit fill
/// 建立（本脚本只做 DOM 清理 + input 事件，无焦点前提）。
fn fill_preclear_editable_expr() -> String {
    r#"function() {
  if (this.isContentEditable) this.textContent = '';
  this.dispatchEvent(new Event('input', { bubbles: true }));
  return { precleared: true };
}"#
    .to_string()
}

/// 工具链公共前置：ref 表 + iframe 会话视图（锁内快照，禁止跨 await 持锁）。
fn ref_maps(watched: &Watched, tid: &str) -> (RefMap, HashMap<String, String>) {
    let ref_map = watched
        .refs
        .lock()
        .unwrap()
        .get(tid)
        .cloned()
        .unwrap_or_else(RefMap::new);
    (ref_map, iframe_sessions_view(watched, tid))
}

/// 结果文案的目标主语：ref → `@eN` / `@eN "name"`（查表 404 契约）；
/// selector → `"#sel"`（引号包裹，与坐标文案区分）。
fn target_subject(watched: &Watched, tid: &str, target: &str) -> Result<String, AppError> {
    if is_ref_target(target) {
        let entry = require_ref(watched, tid, target)?;
        Ok(if entry.name.is_empty() {
            format!("@{}", normalize_ref(target))
        } else {
            format!("@{} \"{}\"", normalize_ref(target), entry.name)
        })
    } else {
        Ok(format!("\"{target}\""))
    }
}

/// kit click + 对话框竞速补偿：press→release 间对话框打开时 kit 返回
/// pending_release（按钮滞留按下态），此处补发 release 闭合。失败按保守
/// 语义报错（可能已有输入生效，禁止盲目重试）。
async fn kit_click(
    client: &CdpClient,
    sid: &str,
    ref_map: &RefMap,
    target: &str,
    spec: ClickSpec,
    iframe_sessions: &HashMap<String, String>,
    tool: &str,
) -> Result<ClickResult, AppError> {
    let result = interaction::click(client, sid, ref_map, target, spec, iframe_sessions)
        .await
        .map_err(|e| map_kit_error(&e, tool))?;
    if let Some(release) = &result.pending_release {
        interaction::dispatch_pending_release(client, release)
            .await
            .map_err(|e| pending_release_failed(tool, &e))?;
    }
    Ok(result)
}

/// 补偿 release 失败：PAGE_DISPATCH_FAILED（500），effect 语义为 unknown
/// 的保守变体——kit 不外露失败相位，但 press 极可能已发出，绝不标 none
///（盲目重试 = 二次点击）。
fn pending_release_failed(tool: &str, raw: &str) -> AppError {
    AppError::business(
        500,
        "PAGE_DISPATCH_FAILED",
        format!(
            "{tool}: pending mouseReleased compensation failed: {raw} \
             (effect_state: unknown — the mousePressed may have been dispatched; observe the page before retrying)"
        ),
    )
}

pub(crate) async fn click_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: &str,
    double: bool,
    right: bool,
) -> Result<Value, AppError> {
    if target.is_empty() {
        return Err(AppError::invalid_request("CSS selector required"));
    }
    let subject = target_subject(watched, tid, target)?;
    let spec = ClickSpec {
        button: if right {
            MouseButton::Right
        } else {
            MouseButton::Left
        },
        count: u8::from(double) + 1,
        modifiers: Modifiers::default(),
    };
    let client = kit_client(watched)?;
    let (ref_map, iframe_sessions) = ref_maps(watched, tid);
    let result = kit_click(
        &client,
        sid,
        &ref_map,
        target,
        spec,
        &iframe_sessions,
        "click",
    )
    .await?;
    // 坐标 = kit 投影后的顶层视口坐标（帧内元素含帧偏移；旧 ref 路径打印
    // 帧本地坐标——文案坐标基准变化，提交信息披露）
    let verb = match (double, right) {
        (true, true) => "Double-right-clicked",
        (true, false) => "Double-clicked",
        (false, true) => "Right-clicked",
        (false, false) => "Clicked",
    };
    let mut text = format!(
        "{verb} {subject} at CSS ({}, {})",
        js_num(result.x),
        js_num(result.y)
    );
    if result.dialog_opened {
        text.push_str(" — a dialog opened and was auto-accepted (see dialogs_seen)");
    }
    Ok(json!({ "result": text }))
}

/// fill 目标解析 → (稳定 objectId, ref 名, 描述, 执行会话)。ref 与 selector
/// 两路径统一走 kit `resolve_element_object_id`（帧路由 + stale 恢复在 kit
/// 内；selector 未命中 → 404 PAGE_ELEMENT_NOT_FOUND，与 click 同码）。
/// 执行会话 = kit 返回的 effective session（帧内 ref 的 DOM/Runtime 命令
/// 路由到 frame session）。
#[allow(clippy::type_complexity)]
async fn resolve_fill_target(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: &str,
) -> Result<(String, Option<String>, String, String), AppError> {
    if target.is_empty() {
        return Err(AppError::invalid_request(
            "CSS selector or @eN ref required",
        ));
    }
    let (ref_name, desc) = if is_ref_target(target) {
        let entry = require_ref(watched, tid, target)?;
        let key = format!("@{}", normalize_ref(target));
        ((!entry.name.is_empty()).then(|| entry.name.clone()), key)
    } else {
        (None, target.to_string())
    };
    let client = kit_client(watched)?;
    let (ref_map, iframe_sessions) = ref_maps(watched, tid);
    let (object_id, esid) =
        resolve_element_object_id(&client, sid, &ref_map, target, &iframe_sessions)
            .await
            .map_err(|e| map_kit_error(&e, "fill"))?;
    Ok((object_id, ref_name, desc, esid))
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
    let (object_id, ref_name, desc, esid) = resolve_fill_target(watched, tid, sid, target).await?;
    let outcome = fill_pipeline(
        watched, &conn, tid, sid, &esid, &object_id, target, &desc, ref_name, value,
    )
    .await;
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

#[allow(clippy::too_many_arguments)]
async fn fill_pipeline(
    watched: &Watched,
    conn: &SharedPageConn,
    tid: &str,
    sid: &str,
    esid: &str,
    object_id: &str,
    target: &str,
    desc: &str,
    ref_name: Option<String>,
    value: &str,
) -> Result<String, AppError> {
    // ① 可填前置检查（契约保全：kit fill 无此检查；tag/label 供结果文案）
    let check = call_fn(conn, esid, object_id, &fill_fillable_check_expr(), vec![]).await?;
    require_fillable(&check, desc)?;
    // ② contenteditable 预清空（kit 清空只覆盖表单控件）
    if check.get("kind").and_then(Value::as_str) == Some("editable") {
        call_fn(
            conn,
            esid,
            object_id,
            &fill_preclear_editable_expr(),
            vec![],
        )
        .await?;
    }
    // ③ kit fill：focus + select/value 清空（表单控件）+ Input.insertText
    //（page 级会话——渲染进程按焦点路由，OOPIF 同理）。CR 归一后传入
    //（回读期望与旧链同口径）。
    let multi_line = value.replace("\r\n", "\n").replace('\r', "\n");
    let client = kit_client(watched)?;
    let (ref_map, iframe_sessions) = ref_maps(watched, tid);
    interaction::fill(
        &client,
        sid,
        &ref_map,
        target,
        &multi_line,
        &iframe_sessions,
    )
    .await
    .map_err(|e| map_kit_error(&e, "fill"))?;
    // ④ 回读校验追加（契约：fill_value_mismatch / gone / fill_focus_lost）
    let single_line = multi_line.replace('\n', " ");
    let verify = call_fn(
        conn,
        esid,
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
    if verify.get("focus_lost").and_then(Value::as_bool) == Some(true) {
        return Err(fill_focus_lost(format!(
            "fill target {desc} does not have focus (page moved it — the text may have landed elsewhere; observe before retrying)"
        )));
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

/// press 的 --target focus：两端统一 kit resolve（帧路由 + stale 恢复在
/// kit 内）→ DOM.focus(objectId)（焦点链与 fill 同构）。focus 失败 → 409
/// PAGE_FOCUS_FAILED；resolve 未命中 → 404（map_kit_error）。
async fn focus_press_target(
    watched: &Watched,
    conn: &SharedPageConn,
    tid: &str,
    sid: &str,
    target: &str,
) -> Result<(String, String), AppError> {
    let (display, name) = if is_ref_target(target) {
        let entry = require_ref(watched, tid, target)?;
        let key = format!("@{}", normalize_ref(target));
        (key, (!entry.name.is_empty()).then(|| entry.name.clone()))
    } else {
        (target.to_string(), None)
    };
    let client = kit_client(watched)?;
    let (ref_map, iframe_sessions) = ref_maps(watched, tid);
    let (object_id, esid) =
        resolve_element_object_id(&client, sid, &ref_map, target, &iframe_sessions)
            .await
            .map_err(|e| map_kit_error(&e, "press"))?;
    if let Err(e) = cdp_send(
        conn,
        "DOM.focus",
        json!({ "objectId": object_id }),
        Some(&esid),
    )
    .await
    {
        return Err(focus_failed(format!(
            "press --target {target} could not be focused ({e})"
        )));
    }
    // 对象句柄归还（fill 同构的 finally 语义，best-effort）
    let _ = cdp_send(
        conn,
        "Runtime.releaseObject",
        json!({ "objectId": object_id }),
        Some(&esid),
    )
    .await;
    Ok((
        match name {
            Some(n) => format!("{display} \"{n}\""),
            None => display,
        },
        esid,
    ))
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
    let kp = parse_key(key)?; // PAGE_KEY_UNKNOWN 400（kit named_key_info 无校验，故校验留在我方）
    let conn = require_conn(watched)?;
    let (where_on, key_sid) = match target.filter(|t| !t.is_empty()) {
        None => (String::new(), sid.to_string()),
        Some(t) => {
            let (label, esid) = focus_press_target(watched, &conn, tid, sid, t).await?;
            (format!(" on {label}"), esid)
        }
    };
    // kit press_key（keyDown+keyUp 两段；Ctrl/Meta 组合抑制 text——与旧链
    // 的 char 抑制语义一致）。修饰键位域 = CDP 位掩码（Alt1/Ctrl2/Meta4/
    // Shift8），domain 的 u8 位序与之相同，直接透传。
    let client = kit_client(watched)?;
    if kp.modifiers != 0 {
        interaction::press_key_with_modifiers(
            &client,
            &key_sid,
            &kp.key,
            Some(i32::from(kp.modifiers)),
        )
        .await
    } else {
        interaction::press_key(&client, &key_sid, &kp.key).await
    }
    .map_err(|e| map_kit_error(&e, "press"))?;
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

/// TS clickXyStr：复用我方三段派发链（kit 无坐标点击原语），无 locate/
/// hit-test（坐标即意图）。
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

/// TS typeStr 语义（对当前焦点元素输入、无定位无校验）换 kit
/// `type_text_into_active_context`：逐字符派发（可打印字符 insertText、
/// \n/\t 走 keyDown/keyUp——Enter 现在是真按键事件，可触发提交）。
/// 字符计数用 Unicode 标量（fill 同口径）。
pub(crate) async fn type_tool(watched: &Watched, sid: &str, text: &str) -> Result<Value, AppError> {
    let client = kit_client(watched)?;
    interaction::type_text_into_active_context(&client, sid, text, None)
        .await
        .map_err(|e| map_kit_error(&e, "type"))?;
    Ok(json!({
        "result": format!("Typed {} characters", text.chars().count())
    }))
}

/// TS loadAllStr 的循环骨架。点击层换 kit click（selector 路径：定位 +
/// actionability + 三段派发一体）。两处任务口径偏差（I1 前既有，保留）：
/// - 点击用原生链替换 TS 的页内 el.click() 合成点击（项目纪律是原生输入）；
/// - 上限从 5min 硬顶改为 max-clicks，达上限后补一次存在性复查定文案
///   （诚实区分「消失了」与「仍在页面上」）。
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
    let client = kit_client(watched)?;
    // loadall 是纯 selector 命令：ref 表/帧会话不参与（selector 只查主 frame）
    let ref_map = RefMap::new();
    let iframe_sessions = HashMap::new();
    let mut clicks: u32 = 0;
    let mut disappeared = false;
    while clicks < max_clicks {
        if !selector_exists(&conn, sid, &exists_expr).await? {
            disappeared = true;
            break;
        }
        kit_click(
            &client,
            sid,
            &ref_map,
            selector,
            ClickSpec::default(),
            &iframe_sessions,
            "loadall",
        )
        .await?;
        clicks += 1;
        tokio::time::sleep(interval).await;
    }
    if !disappeared {
        // 达上限退出（或 max-clicks=0）：末次观察仍在页面——复查一次定文案
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

// ============================================================================
// I2 新命令：hover / scroll / drag / select / check / tap
// ============================================================================

/// `page hover <ID> <sel|@eN>`：kit interaction::hover（resolve + 单发
/// mouseMoved；坐标 = 顶层视口投影）。
pub(crate) async fn hover_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: &str,
) -> Result<Value, AppError> {
    if target.is_empty() {
        return Err(AppError::invalid_request(
            "CSS selector or @eN ref required",
        ));
    }
    let subject = target_subject(watched, tid, target)?;
    let client = kit_client(watched)?;
    let (ref_map, iframe_sessions) = ref_maps(watched, tid);
    let (x, y) = interaction::hover(&client, sid, &ref_map, target, &iframe_sessions)
        .await
        .map_err(|e| map_kit_error(&e, "hover"))?;
    Ok(json!({
        "result": format!("Hovered {subject} at CSS ({}, {})", js_num(x), js_num(y))
    }))
}

/// `page scroll <ID> [sel|@eN] <dx> <dy>`：有 target 滚元素（kit
/// resolve + this.scrollBy）、无 target 滚页面（window.scrollBy）。delta
/// 单位 = CSS 像素，可负。
pub(crate) async fn scroll_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: Option<&str>,
    dx: f64,
    dy: f64,
) -> Result<Value, AppError> {
    let target = target.filter(|t| !t.is_empty());
    let client = kit_client(watched)?;
    let (ref_map, iframe_sessions) = ref_maps(watched, tid);
    interaction::scroll(&client, sid, &ref_map, target, dx, dy, &iframe_sessions)
        .await
        .map_err(|e| map_kit_error(&e, "scroll"))?;
    let subject = match target {
        Some(t) => format!("\"{t}\""),
        None => "page".to_string(),
    };
    Ok(json!({
        "result": format!("Scrolled {subject} by ({}, {})", js_num(dx), js_num(dy))
    }))
}

/// drag 轨迹合成（任务书 seed 的等价重写，本仓库代码）：步数
/// `clamp(dist/12, 1, 60)`、LCG 弧线（二次贝塞尔控制点垂直偏移）、
/// smoothstep 缓动。LCG 种子由端点坐标位混合派生——确定性（同端点同
/// 轨迹，测试可复现），无时钟依赖。
pub(crate) fn drag_trajectory(from: (f64, f64), to: (f64, f64)) -> Vec<(f64, f64)> {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let dist = dx.hypot(dy);
    let steps = (((dist / 12.0).clamp(1.0, 60.0).round() as usize).max(1)).max(1);
    let mut state = (from.0.to_bits() ^ from.1.to_bits().rotate_left(17))
        .wrapping_add(to.0.to_bits().rotate_left(31) ^ to.1.to_bits().rotate_left(47))
        | 1; // 奇数种子（LCG 周期下界）
    let mut lcg = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    // 弧线弯度：距离的 15% 内、上限 24px（人类拖拽的可感知弧度量级）
    let bow = (lcg() - 0.5) * 2.0 * (dist * 0.15).min(24.0);
    let len = dist.max(1.0);
    let (px, py) = (-dy / len, dx / len); // 垂直方向
    let cx = from.0 + dx / 2.0 + px * bow;
    let cy = from.1 + dy / 2.0 + py * bow;
    let smoothstep = |t: f64| t * t * (3.0 - 2.0 * t);
    (0..=steps)
        .map(|i| {
            let t = smoothstep(i as f64 / steps as f64);
            let u = 1.0 - t;
            (
                u * u * from.0 + 2.0 * u * t * cx + t * t * to.0,
                u * u * from.1 + 2.0 * u * t * cy + t * t * to.1,
            )
        })
        .collect()
}

/// drag 的单发鼠标事件（我方直发 Input.dispatchMouseEvent——kit 未暴露
/// move/press/release 组合原语；无对话框竞速，已知边界见提交信息披露）。
/// buttons 位图：移动中持键 = 1（左键按下态），press = 1、release = 0；
/// clickCount 仅 press/release 携带。
async fn drag_mouse(
    client: &CdpClient,
    sid: &str,
    event_type: &str,
    x: f64,
    y: f64,
    buttons: i32,
    click_count: Option<i32>,
) -> Result<(), AppError> {
    let mut params = json!({ "type": event_type, "x": x, "y": y, "buttons": buttons });
    if buttons != 0 || event_type != "mouseMoved" {
        params["button"] = json!("left");
    }
    if let Some(n) = click_count {
        params["clickCount"] = json!(n);
    }
    client
        .send_command("Input.dispatchMouseEvent", Some(params), Some(sid))
        .await
        .map_err(|e| dispatch_failed_conservative("drag", &e))?;
    Ok(())
}

/// 派发失败的保守语义（与 pending_release_failed 同理）：kit/直发链路
/// 无法可靠区分相位，effect_state 按 unknown 处理（绝不标 none）。
fn dispatch_failed_conservative(tool: &str, raw: &str) -> AppError {
    AppError::business(
        500,
        "PAGE_DISPATCH_FAILED",
        format!(
            "{tool}: input dispatch failed mid-sequence: {raw} \
             (effect_state: unknown — an input event may have been dispatched; observe the page before retrying)"
        ),
    )
}

/// `page drag <ID> <from> <to>`：两端点 kit resolve（actionability 全开：
/// 可见/启用/遮挡——拖影此时尚未生成，drop 目标的遮挡判定不被拖影误伤）
/// → move → press → 插值移动（LCG 弧线 + smoothstep）→ release。跨帧拖拽
/// 不支持（两端点解析到不同会话 → 422）。
pub(crate) async fn drag_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    from: &str,
    to: &str,
) -> Result<Value, AppError> {
    if from.is_empty() || to.is_empty() {
        return Err(AppError::invalid_request(
            "from and to targets required (@eN or CSS selector)",
        ));
    }
    let from_subject = target_subject(watched, tid, from)?;
    let to_subject = target_subject(watched, tid, to)?;
    let client = kit_client(watched)?;
    let (ref_map, iframe_sessions) = ref_maps(watched, tid);
    let (fx, fy, fsid) =
        resolve_element_center(&client, sid, &ref_map, from, &iframe_sessions, true)
            .await
            .map_err(|e| map_kit_error(&e, "drag"))?;
    let (tx, ty, tsid) = resolve_element_center(&client, sid, &ref_map, to, &iframe_sessions, true)
        .await
        .map_err(|e| map_kit_error(&e, "drag"))?;
    if fsid != tsid {
        return Err(AppError::business(
            422,
            "PAGE_DRAG_CROSS_FRAME",
            "drag endpoints live in different frames — drag across frame boundaries is not supported (both endpoints must resolve into the same frame)",
        ));
    }
    let pts = drag_trajectory((fx, fy), (tx, ty));
    let (first, rest) = (pts.first().copied().unwrap_or((fx, fy)), &pts[1..]);
    drag_mouse(&client, &fsid, "mouseMoved", first.0, first.1, 0, None).await?;
    drag_mouse(&client, &fsid, "mousePressed", first.0, first.1, 1, Some(1)).await?;
    for &(x, y) in rest {
        drag_mouse(&client, &fsid, "mouseMoved", x, y, 1, None).await?;
    }
    let last = pts.last().copied().unwrap_or((tx, ty));
    drag_mouse(&client, &fsid, "mouseReleased", last.0, last.1, 0, Some(1)).await?;
    Ok(json!({
        "result": format!(
            "Dragged {from_subject} to {to_subject} ({} steps)",
            pts.len() - 1
        )
    }))
}

/// `page select <ID> <sel|@eN> <value>`：kit select_option。匹配语义 =
/// kit JS：原生 select 按 value / label / textContent 匹配；role=combobox/
/// listbox 控件按可访问名或 data-value/value 匹配（--label 未设——kit
/// 已同时按值与可见文本匹配，label-only 无行为差异，owner 已批）。
pub(crate) async fn select_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: &str,
    value: &str,
) -> Result<Value, AppError> {
    if target.is_empty() {
        return Err(AppError::invalid_request(
            "CSS selector or @eN ref required",
        ));
    }
    let subject = target_subject(watched, tid, target)?;
    let client = kit_client(watched)?;
    let (ref_map, iframe_sessions) = ref_maps(watched, tid);
    interaction::select_option(
        &client,
        sid,
        &ref_map,
        target,
        &[value.to_string()],
        &iframe_sessions,
    )
    .await
    .map_err(|e| map_kit_error(&e, "select"))?;
    Ok(json!({
        "result": format!("Selected \"{}\" on {subject}", truncate40(value))
    }))
}

/// `page check <ID> <sel|@eN> [--uncheck]`：kit check/uncheck（已处于目标
/// 态 = no-op 成功，position None 时文案明示 already）。已知边界：kit
/// check/uncheck 内部的 click 若被对话框打断，其 pending_release 不外露
///（kit 返回值丢弃了 ClickResult），无法补发补偿——对话框仍会被自动
/// accept 且 dialogs_seen 可见，按钮逻辑态可能滞留至下一次点击。
pub(crate) async fn check_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: &str,
    uncheck: bool,
) -> Result<Value, AppError> {
    if target.is_empty() {
        return Err(AppError::invalid_request(
            "CSS selector or @eN ref required",
        ));
    }
    let subject = target_subject(watched, tid, target)?;
    let client = kit_client(watched)?;
    let (ref_map, iframe_sessions) = ref_maps(watched, tid);
    let position = if uncheck {
        interaction::uncheck(&client, sid, &ref_map, target, &iframe_sessions).await
    } else {
        interaction::check(&client, sid, &ref_map, target, &iframe_sessions).await
    }
    .map_err(|e| map_kit_error(&e, "check"))?;
    let verb = if uncheck { "Unchecked" } else { "Checked" };
    let note = if position.is_some() {
        String::new()
    } else {
        format!(
            " (already {})",
            if uncheck { "unchecked" } else { "checked" }
        )
    };
    Ok(json!({ "result": format!("{verb} {subject}{note}") }))
}

/// `page tap <ID> <sel|@eN>`：kit tap_touch（resolve + touchStart/touchEnd
/// 触摸序列——移动端仿真页面用 tap 而非 click）。
pub(crate) async fn tap_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: &str,
) -> Result<Value, AppError> {
    if target.is_empty() {
        return Err(AppError::invalid_request(
            "CSS selector or @eN ref required",
        ));
    }
    let subject = target_subject(watched, tid, target)?;
    let client = kit_client(watched)?;
    let (ref_map, iframe_sessions) = ref_maps(watched, tid);
    interaction::tap_touch(&client, sid, &ref_map, target, &iframe_sessions)
        .await
        .map_err(|e| map_kit_error(&e, "tap"))?;
    Ok(json!({ "result": format!("Tapped {subject}") }))
}

// ============================================================================
// 我方三段派发（clickxy 专属；click/loadall 已换 kit）
// ============================================================================

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

// ============================================================================
// I2 新命令的 pub 入口（独立 impl 块——避免与其他并行任务在同一块内冲突）
// ============================================================================

impl PageService {
    /// hover <tab> <sel|@eN>（I2 新命令）：kit interaction::hover。
    pub async fn hover(&self, ins_id: &str, tab: &str, target: &str) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "hover",
            ins_id,
            tab,
            |w, tid, sid| async move { hover_tool(&w, &tid, &sid, target).await },
        )
        .await
    }

    /// scroll <tab> [sel|@eN] <dx> <dy>（I2 新命令）：有 target 滚元素、
    /// 无 target 滚页面。非有限 delta 在建连前快速失败（400）。
    pub async fn scroll(
        &self,
        ins_id: &str,
        tab: &str,
        target: Option<&str>,
        dx: f64,
        dy: f64,
    ) -> Result<Value, AppError> {
        if !dx.is_finite() || !dy.is_finite() {
            return Err(AppError::invalid_request(
                "dx and dy must be finite numbers (CSS pixels)",
            ));
        }
        self.command(
            TOOL_TIMEOUT,
            "scroll",
            ins_id,
            tab,
            |w, tid, sid| async move { scroll_tool(&w, &tid, &sid, target, dx, dy).await },
        )
        .await
    }

    /// drag <tab> <from> <to>（I2 新命令）：kit resolve 两端点 + 我方轨迹
    /// 插值链（LCG 弧线 + smoothstep）。
    pub async fn drag(
        &self,
        ins_id: &str,
        tab: &str,
        from: &str,
        to: &str,
    ) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "drag",
            ins_id,
            tab,
            |w, tid, sid| async move { drag_tool(&w, &tid, &sid, from, to).await },
        )
        .await
    }

    /// select <tab> <sel|@eN> <value>（I2 新命令）：kit select_option
    ///（按 value 或可见文本匹配；空串 value 合法 = 选中空值选项）。
    pub async fn select(
        &self,
        ins_id: &str,
        tab: &str,
        target: &str,
        value: &str,
    ) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "select",
            ins_id,
            tab,
            |w, tid, sid| async move { select_tool(&w, &tid, &sid, target, value).await },
        )
        .await
    }

    /// check <tab> <sel|@eN> [--uncheck]（I2 新命令）：kit check/uncheck。
    pub async fn check(
        &self,
        ins_id: &str,
        tab: &str,
        target: &str,
        uncheck: bool,
    ) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "check",
            ins_id,
            tab,
            |w, tid, sid| async move { check_tool(&w, &tid, &sid, target, uncheck).await },
        )
        .await
    }

    /// tap <tab> <sel|@eN>（I2 新命令）：kit tap_touch（触摸序列）。
    pub async fn tap(&self, ins_id: &str, tab: &str, target: &str) -> Result<Value, AppError> {
        self.command(TOOL_TIMEOUT, "tap", ins_id, tab, |w, tid, sid| async move {
            tap_tool(&w, &tid, &sid, target).await
        })
        .await
    }
}
