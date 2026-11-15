use super::{cdp_send, kit_client, require_conn, FULL_SHOT_MAX_DEVICE_SIDE};
use crate::application::browser_kit::element::{resolve_element_object_id, RefMap};
use crate::application::page_service::eval::eval_raw;
use crate::application::page_service::frames::iframe_sessions_view;
use crate::application::page_service::refs::{is_ref_target, map_kit_error, require_ref};
use crate::application::page_service::state::Watched;
use crate::domain::page_control::{quads_bbox, rounded_clip, PageControlError, Rect};
use crate::error::AppError;
use crate::infrastructure::cdp::page_cdp::SharedPageConn;
use serde_json::{json, Value};
// ============================================================================
// 6b. 截图 / PDF / 仿真（W-cap）
// ============================================================================

/// DOM 节点句柄（selector 路径：getDocument → querySelector 得 nodeId）——
/// scrollIntoViewIfNeeded / getContentQuads / resolveNode 同时接受 nodeId。
/// （ref 路径自 I1b 走 kit 对象解析链，不再产出 backendNodeId 句柄。）
#[derive(Clone, Copy)]
struct NodeHandle {
    node_id: i64,
}

/// 元素在视口中的可截图矩形：scroll best-effort → quads 联合 bbox →
/// rect 回退，退化几何统一报 PAGE_TARGET_MOVED（与点击链同语义，不另造码）。
/// selector 路径专用（nodeId 句柄）；ref 路径自 I1b 走 kit 对象解析 +
/// getBoundingClientRect（见 [`element_clip_rect`]，与旧 quads 链的联合
/// bbox 语义等价——getBoundingClientRect 本就是多碎片 border-box 的并集）。
async fn resolve_clip_rect(
    conn: &SharedPageConn,
    sid: &str,
    handle: NodeHandle,
) -> Result<Rect, AppError> {
    let _ = cdp_send(
        conn,
        "DOM.scrollIntoViewIfNeeded",
        json!({ "nodeId": handle.node_id }),
        Some(sid),
    )
    .await;
    if let Ok(resp) = cdp_send(
        conn,
        "DOM.getContentQuads",
        json!({ "nodeId": handle.node_id }),
        Some(sid),
    )
    .await
    {
        let quads: Vec<Vec<f64>> = resp
            .get("quads")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .flat_map(|q| {
                        q.as_array()
                            .map(|nums| nums.iter().filter_map(Value::as_f64).collect::<Vec<f64>>())
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(r) = quads_bbox(&quads) {
            return Ok(r);
        }
    }
    // 回退：resolveNode → getBoundingClientRect（与 resolve_ref_point 同源）
    let resp = cdp_send(
        conn,
        "DOM.resolveNode",
        json!({ "nodeId": handle.node_id }),
        Some(sid),
    )
    .await
    .map_err(|e| PageControlError::target_moved(&e.to_string()))?;
    let object_id = resp
        .pointer("/object/objectId")
        .and_then(Value::as_str)
        .ok_or_else(|| PageControlError::target_moved("no objectId"))?;
    rect_from_object(conn, sid, object_id).await
}

/// rect 读取页内脚本（原 refs.rs RECT_CENTER_FN 迁入，唯一消费者）：
/// getBoundingClientRect 的 {x,y,w,h}（多碎片 inline 元素返回并集盒，
/// 与 quads 联合 bbox 同义）
const RECT_READ_FN: &str = "function(){ const r = this.getBoundingClientRect(); return { x: r.x, y: r.y, w: r.width, h: r.height }; }";

/// 对象句柄 → 可截图矩形（ref 路径：kit 已解析出 objectId + effective
/// session，此处滚动 + 读 rect；退化 → PAGE_TARGET_MOVED）
async fn rect_from_object(
    conn: &SharedPageConn,
    sid: &str,
    object_id: &str,
) -> Result<Rect, AppError> {
    let _ = cdp_send(
        conn,
        "DOM.scrollIntoViewIfNeeded",
        json!({ "objectId": object_id }),
        Some(sid),
    )
    .await;
    let resp = cdp_send(
        conn,
        "Runtime.callFunctionOn",
        json!({ "objectId": object_id, "functionDeclaration": RECT_READ_FN, "returnByValue": true }),
        Some(sid),
    )
    .await
    .map_err(|e| PageControlError::target_moved(&e.to_string()))?;
    let rect = resp
        .pointer("/result/value")
        .cloned()
        .unwrap_or(Value::Null);
    let (x, y, w, h) = (
        rect.get("x").and_then(Value::as_f64),
        rect.get("y").and_then(Value::as_f64),
        rect.get("w").and_then(Value::as_f64),
        rect.get("h").and_then(Value::as_f64),
    );
    match (x, y, w, h) {
        (Some(x), Some(y), Some(w), Some(h)) if w > 0.0 && h > 0.0 => Ok(Rect {
            x,
            y,
            width: w,
            height: h,
        }),
        _ => Err(PageControlError::target_moved("empty rect").into()),
    }
}

/// screenshot 工具：无 target = 整视口；有 target = ref / selector 定位链 →
/// clip（整数化、scale 1）；full = 全文档（W-shot）。响应与诊断面 screenshot
/// 同形状（format/data），full 另带尺寸证据字段。
pub(crate) async fn screenshot_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: Option<&str>,
    full: bool,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let params = match (full, target) {
        // 全页：先读发起时刻的文档尺寸与设备像素比，再 clip 全文档 +
        // captureBeyondViewport。设计决策（真机 spike 验证，详见任务报告）：
        // - clip 用 CSS 像素整数 + scale 显式 1：DPR≠1（Retina）下换算由 CDP 内部
        //   完成，亚像素 clip 在缩放链路会产生边缘半截像素（同元素路径 rounded_clip
        //   的结论）；png 实际尺寸 = clip × DPR。
        // - 截断上限按设备像素 16384 换算 CSS（DPR=2 → 8192 CSS px）：真机实测
        //   clip 超过 16384 device px 的边直接被 CDP 拒绝（-32000），不是静默
        //   截断——上限必须在设备像素层定，否则 Retina 长页拿到裸 500。
        // - captureBeyondViewport 不滚动页面（截图后原滚动位置不变，真机验证）；
        //   fixed 元素只出现一次，位置随当前滚动偏移（scroll=0 → 图顶部，
        //   scroll=800 → 图中 800..840 处；真机验证，文档如实描述）。
        // - 诚实边界：截的是发起时刻的 scrollHeight，虚拟列表/懒加载在截图后才
        //   加载的内容不在图中；不做自动滚动加载（那是 loadall 的职责）。响应带
        //   documentWidth/documentHeight 原始读数，调用方可比对判断。
        (true, None) => {
            let dims = eval_raw(
                &conn,
                sid,
                "(() => { const de = document.documentElement; return [de.scrollWidth, de.scrollHeight, window.devicePixelRatio]; })()",
            )
            .await?;
            let nums: Vec<f64> = dims
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_f64).collect())
                .unwrap_or_default();
            let (doc_w, doc_h, dpr) = match nums.as_slice() {
                [w, h, d] if w.is_finite() && h.is_finite() && *d > 0.0 && d.is_finite() => {
                    (*w, *h, *d)
                }
                _ => {
                    return Err(AppError::internal(
                        "full 截图读取文档尺寸失败（scrollWidth/scrollHeight/devicePixelRatio 非有限数）",
                    ))
                }
            };
            // CSS 单边上限 = 16384 device px / DPR（向下取整，不越设备硬顶）。
            // DPR 异常小时理论上限反而变大，但设备硬顶仍在——按换算值走。
            let max_side = (FULL_SHOT_MAX_DEVICE_SIDE / dpr).floor();
            // 截断保护：任一边超限 → 取上限，响应标 truncated + 原始读数（诚实证据）。
            // 最小 1×1：空文档（about:blank）scrollWidth/Height 为 0，空 clip 会报错。
            let (w, h) = (
                (doc_w.min(max_side).floor().max(1.0)) as i64,
                (doc_h.min(max_side).floor().max(1.0)) as i64,
            );
            let truncated = doc_w > max_side || doc_h > max_side;
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
            let data = capture_data(&resp)?;
            return Ok(json!({
                "format": "png",
                "data": data,
                "full": true,
                // 实际截取尺寸（截断后，CSS 像素）
                "width": w,
                "height": h,
                // 发起时刻的文档原始尺寸（未截断读数，供调用方判断增长/截断）
                "documentWidth": doc_w.floor() as i64,
                "documentHeight": doc_h.floor() as i64,
                "truncated": truncated,
            }));
        }
        // 互斥校验在 service 入口（screenshot），工具层不重复；真到达这里 = 内部 bug
        (true, Some(_)) => {
            return Err(AppError::internal("full 与 target 互斥校验未在入口拦截"));
        }
        (false, None) => json!({ "format": "png" }),
        (false, Some(t)) => {
            if is_ref_target(t) {
                // I1b：kit 对象解析链（帧路由 + stale 恢复在 kit 内）→ 滚动 +
                // rect 读取 → rounded_clip（行为不变：外扩取整；退化 →
                // PAGE_TARGET_MOVED）。captureScreenshot 发 effective session
                //（同源帧即主会话；OOPIF 帧会话按其自身视口截取）
                require_ref(watched, tid, t)?;
                let client = kit_client(watched)?;
                let iframe_sessions = iframe_sessions_view(watched, tid);
                let ref_map = watched
                    .refs
                    .lock()
                    .unwrap()
                    .get(tid)
                    .cloned()
                    .unwrap_or_else(RefMap::new);
                let (object_id, esid) =
                    resolve_element_object_id(&client, sid, &ref_map, t, &iframe_sessions)
                        .await
                        .map_err(|e| map_kit_error(&e, "screenshot"))?;
                let rect = rect_from_object(&conn, &esid, &object_id).await?;
                let _ = cdp_send(
                    &conn,
                    "Runtime.releaseObject",
                    json!({ "objectId": object_id }),
                    Some(&esid),
                )
                .await;
                let clip = rounded_clip(&rect);
                json!({
                    "format": "png",
                    "clip": { "x": clip.x, "y": clip.y, "width": clip.width, "height": clip.height, "scale": 1 }
                })
            } else {
                // selector 路径：与 fill 同链（getDocument → querySelector），
                // nodeId 0 = 未命中（同码 PAGE_ELEMENT_NOT_FOUND）
                let doc = cdp_send(&conn, "DOM.getDocument", json!({}), Some(sid)).await?;
                let root = doc
                    .pointer("/root/nodeId")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let q = cdp_send(
                    &conn,
                    "DOM.querySelector",
                    json!({ "nodeId": root, "selector": t }),
                    Some(sid),
                )
                .await?;
                let node_id = q.get("nodeId").and_then(Value::as_i64).unwrap_or(0);
                if node_id == 0 {
                    return Err(PageControlError::element_not_found(t).into());
                }
                let handle = NodeHandle { node_id };
                let rect = resolve_clip_rect(&conn, sid, handle).await?;
                let clip = rounded_clip(&rect);
                json!({
                    "format": "png",
                    "clip": { "x": clip.x, "y": clip.y, "width": clip.width, "height": clip.height, "scale": 1 }
                })
            }
        }
    };
    let resp = cdp_send(&conn, "Page.captureScreenshot", params, Some(sid)).await?;
    let data = capture_data(&resp)?;
    Ok(json!({ "format": "png", "data": data }))
}

/// captureScreenshot 响应的 data 提取：缺 data = 显式报错，不静默降级
/// （viewport/元素/全页三路径共用）。
fn capture_data(resp: &Value) -> Result<String, AppError> {
    resp.get("data")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| AppError::internal("截图响应缺少 data 字段"))
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

/// 存在性探针（TS loadAllStr 的 `!!document.querySelector(sel)`）：eval
/// 失败（导航中）原样上抛，循环不吞异常（TS 同为抛错退出）。
pub(crate) async fn selector_exists(
    conn: &SharedPageConn,
    sid: &str,
    exists_expr: &str,
) -> Result<bool, AppError> {
    Ok(eval_raw(conn, sid, exists_expr).await? == Value::Bool(true))
}
