use crate::application::page_service::state::Watched;
use std::collections::HashMap;

/// FrameTable → kit 的 iframe_sessions 视图（frameId → sessionId，仅含已
/// attach 的 OOPIF 帧，不含 root——root 就是 page session 本身）。kit 的
/// 快照管线与元素解析链以此路由帧内命令：跨域帧用自有会话，同源帧不在
/// 表中（无独立 target），kit 回落主会话 + frameId 参数。
pub(crate) fn iframe_sessions_view(watched: &Watched, tab_tid: &str) -> HashMap<String, String> {
    watched
        .frames
        .lock()
        .unwrap()
        .get(tab_tid)
        .map(|t| {
            t.frames
                .iter()
                .filter_map(|(fid, info)| {
                    info.session_id
                        .as_ref()
                        .map(|sid| (fid.clone(), sid.clone()))
                })
                .collect()
        })
        .unwrap_or_default()
}
