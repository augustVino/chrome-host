use super::cdp_send;
use crate::application::page_service::state::Watched;
use crate::domain::page_control::{largest_quad_centroid, PageControlError, Point};
use crate::error::AppError;
use crate::infrastructure::cdp::page_cdp::SharedPageConn;
use serde_json::{json, Value};
pub(crate) fn invalidate_refs(watched: &Watched, tid: &str, reason: &str) {
    if let Some(store) = watched.refs.lock().unwrap().get_mut(tid) {
        store.invalidate(reason);
    }
}

/// 单帧 ref 失效（W-oopif）：仅标记该帧条目 stale，主 frame 条目不动
pub(crate) fn invalidate_frame_refs(watched: &Watched, tid: &str, frame_tid: &str, reason: &str) {
    if let Some(store) = watched.refs.lock().unwrap().get_mut(tid) {
        store.invalidate_frame(frame_tid, reason);
    }
}

/// TS interact.mjs isRefTarget：'@…' 宽松（CSS 选择器不可能以 @ 开头）、
/// 裸 'e<digits>' 严格；'E3'/'@E3' 走 ref 路径后大小写敏感失配（§3.7），
/// 而不是被误读成选择器。
pub(crate) fn is_ref_target(target: &str) -> bool {
    if target.starts_with('@') {
        return true;
    }
    let rest = target.strip_prefix('e').unwrap_or("");
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// 几何回退用的 rect 读取函数（TS resolveRefPoint 尾段逐字）。
pub(crate) const RECT_CENTER_FN: &str = "function(){ const r = this.getBoundingClientRect(); return { x: r.x, y: r.y, w: r.width, h: r.height }; }";

/// getContentQuads 的原始四边形集合（数值防御：非有限数剔除）
pub(crate) async fn content_quads(
    conn: &SharedPageConn,
    sid: &str,
    backend_node_id: i64,
) -> Result<Vec<Vec<f64>>, String> {
    let resp = cdp_send(
        conn,
        "DOM.getContentQuads",
        json!({ "backendNodeId": backend_node_id }),
        Some(sid),
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(resp
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
        .unwrap_or_default())
}

/// 面积最大的四边形（投影用本体；质心场景另用 largest_quad_centroid）
pub(crate) fn largest_quad(quads: &[Vec<f64>]) -> Option<Vec<f64>> {
    let mut best: Option<&Vec<f64>> = None;
    let mut best_area = 0.0f64;
    for q in quads {
        if q.len() != 8 || q.iter().any(|n| !n.is_finite()) {
            continue;
        }
        let mut area2 = 0.0;
        for i in 0..4 {
            let j = ((i + 1) % 4) * 2;
            area2 += q[i * 2] * q[j + 1] - q[j] * q[i * 2 + 1];
        }
        let area = area2.abs() / 2.0;
        if area > best_area {
            best_area = area;
            best = Some(q);
        }
    }
    best.cloned()
}

pub(crate) fn resolve_ref_entry(
    watched: &Watched,
    tid: &str,
    raw_ref: &str,
) -> Result<crate::domain::page_control::RefEntry, AppError> {
    let tabs = watched.refs.lock().unwrap();
    match tabs.get(tid) {
        // 无表 = 从未 snap：等价 TS 的 throwaway store → ref_not_found
        None => Err(PageControlError::ref_not_found(raw_ref).into()),
        Some(store) => match store.resolve(raw_ref) {
            // 帧级失效（W-oopif）：条目仍在表但所在 iframe 已导航/移除——
            // 报 stale（重 snap）而非 not_found（语义精确：曾经存在，已失效）
            Some(e) if e.stale_reason.is_some() => {
                Err(PageControlError::ref_stale(e.stale_reason.as_deref()).into())
            }
            Some(entry) => Ok(entry.clone()),
            None => Err(PageControlError::ref_miss(store, raw_ref).into()),
        },
    }
}

/// ref 几何链（TS resolveRefPoint）：scrollIntoView best-effort →
/// getContentQuads 最大四边形质心（P2-A）→ 回退 resolveNode + rect 中心。
pub(crate) async fn resolve_ref_point(
    conn: &SharedPageConn,
    sid: &str,
    backend_node_id: i64,
) -> Result<Point, PageControlError> {
    let _ = cdp_send(
        conn,
        "DOM.scrollIntoViewIfNeeded",
        json!({ "backendNodeId": backend_node_id }),
        Some(sid),
    )
    .await;
    if let Ok(resp) = cdp_send(
        conn,
        "DOM.getContentQuads",
        json!({ "backendNodeId": backend_node_id }),
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
        if let Some(p) = largest_quad_centroid(&quads) {
            return Ok(p);
        }
    }
    let fallback: Result<Point, String> = async {
        let resp = cdp_send(conn, "DOM.resolveNode", json!({ "backendNodeId": backend_node_id }), Some(sid))
            .await
            .map_err(|e| e.to_string())?;
        let object_id = resp
            .pointer("/object/objectId")
            .and_then(Value::as_str)
            .ok_or("no objectId")?;
        let resp = cdp_send(
            conn,
            "Runtime.callFunctionOn",
            json!({ "objectId": object_id, "functionDeclaration": RECT_CENTER_FN, "returnByValue": true }),
            Some(sid),
        )
        .await
        .map_err(|e| e.to_string())?;
        let rect = resp.pointer("/result/value").ok_or("empty rect")?;
        let (x, y, w, h) = (
            rect.get("x").and_then(Value::as_f64),
            rect.get("y").and_then(Value::as_f64),
            rect.get("w").and_then(Value::as_f64),
            rect.get("h").and_then(Value::as_f64),
        );
        match (x, y, w, h) {
            (Some(x), Some(y), Some(w), Some(h)) if w > 0.0 && h > 0.0 => {
                Ok(Point { x: x + w / 2.0, y: y + h / 2.0 })
            }
            _ => Err("empty rect".to_string()),
        }
    }
    .await;
    fallback.map_err(|detail| PageControlError::target_moved(&detail))
}
