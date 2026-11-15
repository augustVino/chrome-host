use super::{cdp_send, kit_client, require_conn, FULL_SHOT_MAX_DEVICE_SIDE};
use crate::application::browser_kit::element::RefMap;
use crate::application::browser_kit::screenshot::{take_screenshot, ScreenshotOptions};
use crate::application::page_service::eval::eval_raw;
use crate::application::page_service::frames::iframe_sessions_view;
use crate::application::page_service::refs::{is_ref_target, map_kit_error, require_ref};
use crate::application::page_service::state::Watched;
use crate::error::AppError;
use crate::infrastructure::cdp::page_cdp::SharedPageConn;
use serde_json::{json, Value};
// ============================================================================
// 6b. 截图 / PDF / 仿真（screenshot 内核 = kit screenshot.rs）
// ============================================================================

/// screenshot 工具（内核 = kit `screenshot::take_screenshot`）。三形态
/// → kit 三模式：视口（无 clip、fromSurface）/ 元素（selector 入参，kit
/// 对象解析 + 视口 rect → 文档坐标换算 → clip + captureBeyondViewport）/
/// 全页（Page.getLayoutMetrics cssContentSize → clip + captureBeyondViewport）。
/// 我方保留层（kit 无此保护）：全页 DPR 16384 设备像素上限与 truncated
/// 披露、互斥校验（service 入口）、响应形状契约。
///
/// 行为要点：
/// - 元素路径不做 scrollIntoView（kit 文档坐标换算不依赖滚动，截图不
///   改变页面滚动位置）；clip 为 kit 浮点
///   rect 直传（DPR≠1 下边缘可能存在亚像素差异）；
/// - 元素几何退化（零宽/高）不前置拦为 409 PAGE_TARGET_MOVED——kit 无
///   该校验，0×0 clip 由 CDP 拒绝 → 500 PAGE_ACTION_FAILED 带原文；
/// - 全页尺寸源 = getLayoutMetrics
///   cssContentSize（与 kit 内部 clip 同源，避免两套读数的边缘错位）；
/// - 超限（cssContentSize 任一边 > 16384/DPR）时 kit 无法表达截断 clip
///   （ScreenshotOptions 无自定义 clip 入口，且 kit 禁改），走我方 CDP
///   直发截断序列——对外契约（截断 + truncated:
///   true + 原始读数）不变。
pub(crate) async fn screenshot_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: Option<&str>,
    full: bool,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let client = kit_client(watched)?;
    // 锁窗口内取 clone（不跨 await；视口/全页模式不消费 ref_map，元素模式 kit 自取）
    let ref_map: RefMap = watched
        .refs
        .lock()
        .unwrap()
        .get(tid)
        .cloned()
        .unwrap_or_else(RefMap::new);
    let iframe_sessions = iframe_sessions_view(watched, tid);
    match (full, target) {
        // 全页：我方保留层先行（DPR + cssContentSize 预读 → 超限判定），
        // 未超限才交给 kit（超限回退见函数注释：我方截断直发，契约优先）
        (true, None) => {
            let (doc_w, doc_h, dpr, have_metrics) = full_doc_metrics(&conn, sid).await?;
            let max_side = (FULL_SHOT_MAX_DEVICE_SIDE / dpr).floor();
            let over_limit = have_metrics && (doc_w > max_side || doc_h > max_side);
            if over_limit {
                // 截断路径：clip = min(读数, 上限)，最小 1×1（空文档
                // cssContentSize 可能为 0，空 clip 会报错）
                let (w, h) = (
                    (doc_w.min(max_side).floor().max(1.0)) as i64,
                    (doc_h.min(max_side).floor().max(1.0)) as i64,
                );
                let resp = cdp_send(
                    &conn,
                    "Page.captureScreenshot",
                    json!({
                        "format": "png",
                        "captureBeyondViewport": true,
                        "clip": { "x": 0, "y": 0, "width": w, "height": h, "scale": 1 }
                    }),
                    Some(sid),
                )
                .await?;
                let data = resp
                    .get("data")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AppError::internal("截图响应缺少 data 字段"))?
                    .to_string();
                return Ok(json!({
                    "format": "png",
                    "data": data,
                    "full": true,
                    "width": w,
                    "height": h,
                    "documentWidth": doc_w.floor() as i64,
                    "documentHeight": doc_h.floor() as i64,
                    "truncated": true,
                }));
            }
            // 未超限（或无 cssContentSize 读数——极老 Chrome，交给 kit 的
            // contentSize fallback）：kit full_page；响应尺寸证据 = 我方预读值
            //（与 kit 内部 clip 同源 cssContentSize；无读数时不装作知道）
            let opts = kit_options(None, true);
            let result = take_screenshot(&client, sid, &ref_map, &opts, &iframe_sessions)
                .await
                .map_err(|e| map_kit_error(&e, "screenshot"))?;
            let mut resp = json!({
                "format": "png",
                "data": result.base64,
                "full": true,
                "truncated": false,
            });
            if have_metrics {
                let (w, h) = (doc_w.floor() as i64, doc_h.floor() as i64);
                resp["width"] = json!(w);
                resp["height"] = json!(h);
                resp["documentWidth"] = json!(w);
                resp["documentHeight"] = json!(h);
            }
            Ok(resp)
        }
        // 互斥校验在 service 入口（screenshot），工具层不重复；真到达这里 = 内部 bug
        (true, Some(_)) => Err(AppError::internal("full 与 target 互斥校验未在入口拦截")),
        // 视口：kit 无 clip 模式（fromSurface=true，与 CDP 缺省同义）
        (false, None) => {
            let opts = kit_options(None, false);
            let result = take_screenshot(&client, sid, &ref_map, &opts, &iframe_sessions)
                .await
                .map_err(|e| map_kit_error(&e, "screenshot"))?;
            Ok(json!({ "format": "png", "data": result.base64 }))
        }
        // 元素：selector 入参统一走 kit 对象解析（@eN 先查表快速失败，语义
        // 错误码不依赖 kit 文案映射；selector 未命中 → kit "Element not
        // found" → map_kit_error → 404 PAGE_ELEMENT_NOT_FOUND）
        (false, Some(t)) => {
            if is_ref_target(t) {
                require_ref(watched, tid, t)?;
            }
            let opts = kit_options(Some(t), false);
            let result = take_screenshot(&client, sid, &ref_map, &opts, &iframe_sessions)
                .await
                .map_err(|e| map_kit_error(&e, "screenshot"))?;
            Ok(json!({ "format": "png", "data": result.base64 }))
        }
    }
}

/// kit ScreenshotOptions 构造（我方固定面：png、无 annotate——简洁优先，
/// annotate 模式不引入；path/output_dir 落盘职责在 CLI --out 层）
fn kit_options(selector: Option<&str>, full_page: bool) -> ScreenshotOptions {
    ScreenshotOptions {
        selector: selector.map(str::to_string),
        path: None,
        full_page,
        format: "png".to_string(),
        quality: None,
        annotate: false,
        output_dir: None,
    }
}

/// 全页尺寸预读：devicePixelRatio（eval）+ cssContentSize
/// （Page.getLayoutMetrics）。返回 (w, h, dpr, have_metrics)：have_metrics
/// = false 表示响应无 cssContentSize（极老 Chrome，截断判定交给 kit 的
/// contentSize fallback，响应不带尺寸证据字段）。
async fn full_doc_metrics(
    conn: &SharedPageConn,
    sid: &str,
) -> Result<(f64, f64, f64, bool), AppError> {
    let dpr_val = eval_raw(conn, sid, "(() => { return window.devicePixelRatio; })()").await?;
    let dpr = match dpr_val.as_f64() {
        Some(d) if d.is_finite() && d > 0.0 => d,
        _ => {
            return Err(AppError::internal(
                "full 截图读取 devicePixelRatio 失败（非有限正数）",
            ))
        }
    };
    let metrics = cdp_send(conn, "Page.getLayoutMetrics", json!({}), Some(sid)).await?;
    let size = metrics.get("cssContentSize");
    let (w, h) = (
        size.and_then(|s| s.get("width")).and_then(Value::as_f64),
        size.and_then(|s| s.get("height")).and_then(Value::as_f64),
    );
    match (w, h) {
        (Some(w), Some(h)) if w.is_finite() && h.is_finite() => Ok((w, h, dpr, true)),
        _ => Ok((0.0, 0.0, dpr, false)),
    }
}

/// pdf 工具：参数透传（默认值已在 service 入口归一）。缺 data = 显式报错，
/// 不静默降级（如被 page preview 拦截等异常路径）。
pub(crate) async fn pdf_tool(
    watched: &Watched,
    sid: &str,
    landscape: bool,
    print_background: bool,
    paper_width: f64,
    paper_height: f64,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let resp = cdp_send(
        &conn,
        "Page.printToPDF",
        json!({
            "landscape": landscape,
            "printBackground": print_background,
            "paperWidth": paper_width,
            "paperHeight": paper_height,
        }),
        Some(sid),
    )
    .await?;
    let data = resp
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::internal("PDF 响应缺少 data 字段"))?
        .to_string();
    Ok(json!({ "format": "pdf", "data": data }))
}

/// emulate 工具：无条件发 set/clear（CDP 幂等），零状态实现覆盖语义——
/// 应用时 UA 未给则恢复默认（空串 setUserAgentOverride：CfT 131 无
/// clearUserAgentOverride，真机实测）、touch 未给则关闭（未给 = 恢复默认，
/// 非保持旧值）；off 清全部三域（metrics clear + UA 空串 + touch 关）。
/// 仿真只作用于本 tab 的 page session（CDP session 级），与实例级无关。
pub(crate) async fn emulate_tool(
    watched: &Watched,
    sid: &str,
    action: crate::domain::emulate::EmulateAction,
) -> Result<Value, AppError> {
    use crate::domain::emulate::{EmulateAction, EmulateOrigin};
    let conn = require_conn(watched)?;
    match action {
        EmulateAction::Apply(spec) => {
            cdp_send(
                &conn,
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": spec.width,
                    "height": spec.height,
                    "deviceScaleFactor": 0,
                    "mobile": spec.mobile,
                }),
                Some(sid),
            )
            .await?;
            match &spec.user_agent {
                Some(ua) => {
                    cdp_send(
                        &conn,
                        "Emulation.setUserAgentOverride",
                        json!({ "userAgent": ua }),
                        Some(sid),
                    )
                    .await?;
                }
                None => {
                    // 覆盖语义：本次不带 UA = 不仿真 UA，恢复默认。CfT 131
                    // 无 Emulation.clearUserAgentOverride（-32601，真机实测），
                    // 改发空串 UA——真机验证可恢复默认 UA
                    cdp_send(
                        &conn,
                        "Emulation.setUserAgentOverride",
                        json!({ "userAgent": "" }),
                        Some(sid),
                    )
                    .await?;
                }
            }
            cdp_send(
                &conn,
                "Emulation.setTouchEmulationEnabled",
                json!({ "enabled": spec.touch }),
                Some(sid),
            )
            .await?;
            let origin = match spec.origin {
                EmulateOrigin::Preset(name) => format!("preset {name}"),
                EmulateOrigin::Custom => "custom viewport".to_string(),
            };
            let ua_note = if spec.user_agent.is_some() {
                ", UA overridden"
            } else {
                ""
            };
            Ok(json!({
                "result": format!(
                    "Emulated {} ({}×{}, mobile={}, touch={}{ua_note})",
                    origin, spec.width, spec.height, spec.mobile, spec.touch
                )
            }))
        }
        EmulateAction::Off => {
            // clearDeviceMetricsOverride 存在且有效（真机验证）；UA 恢复用
            // 空串 setUserAgentOverride（CfT 131 无 clearUserAgentOverride，
            // 真机实测 -32601；空串已验证恢复默认 UA）
            cdp_send(
                &conn,
                "Emulation.clearDeviceMetricsOverride",
                json!({}),
                Some(sid),
            )
            .await?;
            cdp_send(
                &conn,
                "Emulation.setUserAgentOverride",
                json!({ "userAgent": "" }),
                Some(sid),
            )
            .await?;
            cdp_send(
                &conn,
                "Emulation.setTouchEmulationEnabled",
                json!({ "enabled": false }),
                Some(sid),
            )
            .await?;
            Ok(json!({ "result": "Cleared device emulation (viewport / UA / touch restored)" }))
        }
    }
}

/// 存在性探针（!!document.querySelector(sel)）：eval
/// 失败（导航中）原样上抛，循环不吞异常。
pub(crate) async fn selector_exists(
    conn: &SharedPageConn,
    sid: &str,
    exists_expr: &str,
) -> Result<bool, AppError> {
    Ok(eval_raw(conn, sid, exists_expr).await? == Value::Bool(true))
}
