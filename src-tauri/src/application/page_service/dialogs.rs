use super::cdp_send;
use crate::application::page_service::events::frame_session_lookup;
use crate::application::page_service::state::Watched;
use crate::domain::page_control::{dialog_accept_params, parse_dialog_opening, DialogBuffer};
use serde_json::Value;
/// 对话框接管：无条件 accept（prompt 带 promptText = defaultPrompt ?? ""）；
/// **成功后**才入缓冲——失败仅日志，
/// 对话框保持打开、CDP 命令继续阻塞直至各自超时（与上游一致）。
///
/// handle 走主连接：send 已 &self 并发化，与 in-flight 工具
/// 命令天然并发——sessionId 是**连接级作用域**，换连接发必 -32001
/// Session not found（旁路短连接方案真机已证伪并回滚）。
pub(crate) async fn on_dialog_opening(watched: &Watched, session_id: &str, params: &Value) {
    let tid = {
        let tabs = watched.tabs.lock().unwrap();
        let mut tid = tabs.target_of_session(session_id).map(str::to_string);
        drop(tabs);
        if tid.is_none() {
            // frame 会话的对话框：按帧归属到 tab
            tid = frame_session_lookup(watched, session_id).map(|(tab, _)| tab);
        }
        tid
    };
    let Some(tid) = tid else {
        return; // 未映射 session（attach 竞态/非 page target）：无法归属 tab
    };
    let Some(conn) = watched.conn.lock().unwrap().clone() else {
        return;
    };
    let info = parse_dialog_opening(params);
    let accept = dialog_accept_params(&info);
    match cdp_send(
        &conn,
        "Page.handleJavaScriptDialog",
        accept,
        Some(session_id),
    )
    .await
    {
        Ok(_) => {
            watched
                .dialogs
                .lock()
                .unwrap()
                .entry(tid.clone())
                .or_insert_with(|| DialogBuffer::new(tid.clone()))
                .record(info);
        }
        Err(e) => {
            tracing::warn!(
                "[page] Page.handleJavaScriptDialog 失败（不入缓冲，对话框保持打开）: {e}"
            )
        }
    }
}
