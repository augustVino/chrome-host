use crate::application::page_service::refs::{content_quads, largest_quad};
use crate::application::page_service::snap::resolve_frame_owner;
use crate::application::page_service::state::Watched;
use crate::domain::page_control::PageControlError;
use crate::error::AppError;
use crate::infrastructure::cdp::page_cdp::SharedPageConn;
/// ref → backendNodeId 查表（miss 按 P2-A ref_miss 语义选 stale/not_found）。
/// 帧的当前会话（优先活登记表，回落 snap 时记忆的会话 id）
pub(crate) fn live_frame_session(
    watched: &Watched,
    tab_tid: &str,
    frame_tid: &str,
    fallback: Option<&str>,
) -> Option<String> {
    let live = {
        let tables = watched.frames.lock().unwrap();
        tables
            .get(tab_tid)
            .and_then(|t| t.frames.get(frame_tid))
            .and_then(|i| i.session_id.clone())
    };
    live.or_else(|| fallback.map(str::to_string))
}

/// 投影链单步：帧 → (父 frameId[None/根=主文档], 父文档 session, owner 元素
/// backendNodeId)。父帧已 attach（OOPIF）用其会话，否则主会话（同源中间帧
/// 的 DOM 本就属于主文档进程，getFrameOwner 在主会话可解析任意后代）。
async fn frame_chain_step(
    watched: &Watched,
    conn: &SharedPageConn,
    tab_tid: &str,
    main_sid: &str,
    frame_tid: &str,
) -> Option<(Option<String>, String, i64)> {
    let (parent, parent_session) = {
        let tables = watched.frames.lock().unwrap();
        let table = tables.get(tab_tid)?;
        let info = table.frames.get(frame_tid)?;
        let root = table.root.clone();
        let parent = info.parent_frame_id.clone();
        let parent_session = match parent.as_deref() {
            Some(p) if !p.is_empty() && Some(p.to_string()) != root => {
                table.frames.get(p).and_then(|i| i.session_id.clone())
            }
            _ => None,
        };
        (parent, parent_session)
    };
    let parent_sid = parent_session.unwrap_or_else(|| main_sid.to_string());
    let owner =
        resolve_frame_owner(watched, conn, tab_tid, frame_tid, &parent, &parent_sid).await?;
    Some((parent, parent_sid, owner))
}

/// 帧内点 → 根视口坐标（逐级投影：owner 元素 quad 平移+夹取；嵌套逐级上投）。
/// 上限 8 级防御环/脏数据。失败按 target_moved（几何链断 = 目标已不可用）。
pub(crate) async fn project_frame_point(
    watched: &Watched,
    conn: &SharedPageConn,
    tab_tid: &str,
    main_sid: &str,
    frame_tid: &str,
    p: crate::domain::page_control::Point,
) -> Result<crate::domain::page_control::Point, AppError> {
    use crate::domain::page_control::project_point;
    let root = {
        let tables = watched.frames.lock().unwrap();
        tables.get(tab_tid).and_then(|t| t.root.clone())
    };
    let mut cur = frame_tid.to_string();
    let mut p = p;
    for _ in 0..8 {
        let (parent, parent_sid, owner_bni) =
            frame_chain_step(watched, conn, tab_tid, main_sid, &cur)
                .await
                .ok_or_else(|| {
                    PageControlError::target_moved(
                        "owning iframe chain is not resolvable (frame detached or navigating?)",
                    )
                })?;
        let quads = content_quads(conn, &parent_sid, owner_bni)
            .await
            .map_err(|e| {
                PageControlError::target_moved(&format!("iframe owner geometry failed: {e}"))
            })?;
        let quad = largest_quad(&quads).ok_or_else(|| {
            PageControlError::target_moved("iframe owner has no usable quad (zero area?)")
        })?;
        p = project_point(&quad, p)
            .ok_or_else(|| PageControlError::target_moved("iframe owner quad is degenerate"))?;
        // 父级即根（或未知 → 视为主文档直属）：已到根视口。兜底：parent 未
        // 登记在 frames 表（主帧从不入表，root 未知/丢失时同此收敛——
        // frameAttached 对一切子帧都发，活着的中间帧必在表中）
        let at_root = match parent.as_deref() {
            None | Some("") => true,
            Some(pf) => {
                let tables = watched.frames.lock().unwrap();
                Some(pf.to_string()) == root
                    || !tables
                        .get(tab_tid)
                        .is_some_and(|t| t.frames.contains_key(pf))
            }
        };
        if at_root {
            return Ok(p);
        }
        cur = parent.expect("非空已判");
    }
    Err(AppError::internal(
        "iframe 投影链超限（嵌套 >8 或登记表成环）",
    ))
}
