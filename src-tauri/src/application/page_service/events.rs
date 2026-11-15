use super::cdp_send;
use crate::application::browser_kit::recording;
use crate::application::page_service::dialogs::on_dialog_opening;
use crate::application::page_service::downloads::{on_download_begin, on_download_progress};
use crate::application::page_service::intercept::on_request_paused;
use crate::application::page_service::snap::reset_sheet_registry;
use crate::application::page_service::state::Watched;
use crate::domain::page_control::{DialogBuffer, DialogInfo};
use crate::infrastructure::cdp::page_cdp::SharedPageConn;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc::UnboundedReceiver;
// ============================================================================
// 2. watch_loop：事件消费（对接 P2-A 模型）
// ============================================================================

/// watcher 主循环：消费事件流直到通道关闭（= reader 结束 = 连接死亡，与
/// diagnostics 同信号）。退出时清理：连接槽置 None（仅当槽里仍是自己的
/// 连接）+ 从登记表移除（仅当仍是当前代且登记表仍指向本 Watched——双守卫
/// 防旧代误删新代状态：同 Watched 重建靠 generation，整体替换靠 ptr_eq）。
pub(crate) async fn watch_loop(
    ins_id: String,
    registry: Arc<Mutex<HashMap<String, Arc<Watched>>>>,
    watched: Arc<Watched>,
    my_conn: SharedPageConn,
    mut rx: UnboundedReceiver<Value>,
    gen: u64,
) {
    while let Some(event) = rx.recv().await {
        if watched.generation.load(Ordering::Relaxed) != gen {
            // 新一代已接管（断连后命令重建了连接）：本代即刻退场，
            // 状态与登记表归新一代所有，什么都不动
            return;
        }
        handle_event(&watched, &event).await;
        // 事件总线转发（browser_kit 订阅面）：在本代自己的事件处理后广播。
        // 无订阅者时 send 是廉价 no-op；Lagged 由订阅者自行消化
        if let Some(ev) = crate::application::browser_kit::cdp_event_from_frame(&event) {
            let _ = watched.event_bus.send(ev);
        }
    }
    let mut slot = watched.conn.lock().unwrap();
    if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &my_conn)) {
        *slot = None;
    }
    drop(slot);
    if watched.generation.load(Ordering::Relaxed) == gen {
        let mut map = registry.lock().unwrap();
        if map
            .get(&ins_id)
            .is_some_and(|cur| Arc::ptr_eq(cur, &watched))
        {
            map.remove(&ins_id);
        }
    }
}

/// 单事件分派。flatten 模式：session 维度事件在顶层带 sessionId。
pub(crate) async fn handle_event(watched: &Watched, event: &Value) {
    let Some(method) = event.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = event.get("params").cloned().unwrap_or(Value::Null);
    match method {
        // §3.6 失效源接线（daemon.mjs bindRefInvalidation）+ attach 映射 +
        // OOPIF 帧登记（W-oopif）
        "Target.attachedToTarget" => on_attached(watched, &params).await,
        "Target.detachedFromTarget" => on_detached(watched, &params),
        "Target.targetDestroyed" => {
            if let Some(tid) = params.get("targetId").and_then(Value::as_str) {
                drop_target(watched, tid);
            }
        }
        "Page.javascriptDialogOpening" => {
            if let Some(sid) = event.get("sessionId").and_then(Value::as_str) {
                on_dialog_opening(watched, sid, &params).await;
            }
        }
        // 导航命令族（navigate/reload/back/forward）的加载信号：等待者在命令
        // 发送前登记（见 register_load_waiter），load 事件到达即放行该 tab 全部
        // 等待者。Page.enable 在 attach 时已发（on_attached），事件必然可达。
        "Page.loadEventFired" => {
            if let Some(tid) = tab_of_session(watched, event) {
                if let Some(waiters) = watched.load_waiters.lock().unwrap().remove(&tid) {
                    for tx in waiters {
                        let _ = tx.send(());
                    }
                }
            }
        }
        // hover 扫描的样式表登记：CSS.enable（on_attached）后 Chrome 回放/增量
        // 推送 styleSheetAdded；Removed 按表撤销。读取侧才过滤 origin/disabled。
        // 扩展注入表（sourceURL 以 chrome-extension:// 开头）登记即排除：对
        // 页面 hover 语义无贡献，且取文本不稳定（真机复测：混入 regular 扫描
        // 会恒触发 truncated，狼来了效应压死 skipped）。
        "CSS.styleSheetAdded" => {
            if let Some(tid) = tab_of_session(watched, event) {
                let Some(header) = params.get("header") else {
                    return;
                };
                let (Some(id), Some(origin)) = (
                    header.get("styleSheetId").and_then(Value::as_str),
                    header.get("origin").and_then(Value::as_str),
                ) else {
                    return;
                };
                let source_url = header
                    .get("sourceURL")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if source_url.starts_with("chrome-extension://") {
                    tracing::debug!(
                        "[page] tab {tid} 跳过扩展样式表 {id}（sourceURL={source_url}，origin={origin}）"
                    );
                    return;
                }
                let disabled = header
                    .get("disabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                watched
                    .sheets
                    .lock()
                    .unwrap()
                    .entry(tid)
                    .or_default()
                    .insert(id.to_string(), (origin.to_string(), disabled));
            }
        }
        "CSS.styleSheetRemoved" => {
            if let Some(tid) = tab_of_session(watched, event) {
                if let Some(id) = params.get("styleSheetId").and_then(Value::as_str) {
                    if let Some(m) = watched.sheets.lock().unwrap().get_mut(&tid) {
                        m.remove(id);
                    }
                }
            }
        }
        // Fetch 拦截决策（任务红线 ①）：requestPaused 在**事件流内**决策并回——
        // on_request_paused 不碰 per-instance 命令串行锁（watch_loop 天然不持
        // 锁），拦截响应命令走 conn 并发 send，与 in-flight 工具命令互不阻塞；
        // 否则规则拦截会堵死全部 page 命令。
        "Fetch.requestPaused" => {
            if let Some(sid) = event.get("sessionId").and_then(Value::as_str) {
                on_request_paused(watched, sid, &params).await;
            }
        }
        // W-file：browser 级下载事件（无 session 维度，establish 时
        // setDownloadBehavior{eventsEnabled} 开启）：WillBegin 登记、
        // Progress 终态结算 + 等待者放行（同步登记，不发命令，不碰串行锁）
        "Browser.downloadWillBegin" => on_download_begin(watched, &params),
        "Browser.downloadProgress" => on_download_progress(watched, &params),
        // 仅根 frame（parentId 为空）才换主文档。I1b 起 refs 不再主动失效
        //（durable refs：stale 由解析时 is_detached / AX 唯一匹配恢复判定产生）；
        // 帧表维护保留：根导航 = 新文档的旧子帧全部死亡（frameDetached 不保证
        // 补发，整表重建），子帧导航无事可做。
        "Page.frameNavigated" => {
            if let Some(tid) = tab_of_session(watched, event) {
                let fid = params
                    .pointer("/frame/id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let child_frame = params
                    .pointer("/frame/parentId")
                    .and_then(Value::as_str)
                    .is_some_and(|p| !p.is_empty());
                if !child_frame {
                    // 换文档：Chrome 在文档销毁时不保证逐张发
                    // styleSheetRemoved（真机实测：导航后登记表残留死表，
                    // getStyleSheetText 报 No style sheet with given id found →
                    // hoverScan 恒 truncated）。新文档的表由 styleSheetAdded
                    // 重新登记（CSS 域 session 级持续有效）。
                    reset_sheet_registry(watched, &tid);
                    // 帧登记整体重建：新文档的 frameAttached/attach 会重登记。
                    // root 必须在清表**之后**写入——先写后清会让 root 随整表
                    // 删除丢失（root 恒 None：主帧发起的下载无 tab 归属）。
                    let mut tables = watched.frames.lock().unwrap();
                    tables.remove(&tid);
                    if !fid.is_empty() {
                        tables.entry(tid.clone()).or_default().root = Some(fid);
                    }
                }
            }
        }
        // OOPIF 帧树维护（主 frame session 事件）：attach 登记（含父链），
        // detach → 摘除登记（I1b 起不标记 ref 失效，durable refs 自判）
        "Page.frameAttached" => {
            if let Some(tid) = tab_of_session(watched, event) {
                let (Some(fid), Some(parent)) = (
                    params.get("frameId").and_then(Value::as_str),
                    params.get("parentFrameId").and_then(Value::as_str),
                ) else {
                    return;
                };
                if fid.is_empty() {
                    return;
                }
                let orphan = watched.orphan_frame_sessions.lock().unwrap().remove(fid);
                let mut tables = watched.frames.lock().unwrap();
                let table = tables.entry(tid).or_default();
                let info = table.frames.entry(fid.to_string()).or_default();
                info.parent_frame_id = Some(parent.to_string());
                if orphan.is_some() {
                    info.session_id = orphan;
                }
            }
        }
        "Page.frameDetached" => {
            if let Some(tid) = tab_of_session(watched, event) {
                if let Some(fid) = params.get("frameId").and_then(Value::as_str) {
                    if !fid.is_empty() {
                        drop_frame(watched, &tid, fid);
                    }
                }
            }
        }
        "Page.documentOpened" => {
            if let Some(tid) = tab_of_session(watched, event) {
                // document.open() 同样换文档：旧样式表全部销毁且
                // styleSheetRemoved 不保证补发（与根导航同一死表形态）——
                // 清登记 + 重开结算窗口，新表由 styleSheetAdded 重放。
                // refs 不主动失效（I1b durable 语义）。
                reset_sheet_registry(watched, &tid);
            }
        }
        _ => {}
    }
}

/// 录制 capture session 判定（I4）：该 (targetId, sessionId) 是否属于当前
/// 录制的专属 capture session。两层：豁免表（attach 响应后登记，含 stop
/// 后 slot 已清的窗口——detach 事件消费式摘除）；kit owns_attachment
///（attach in-flight：Chrome 先发 attachedToTarget 再回 attach 响应，
/// 该窗口 slot 里只有 target_id 无 session_id，按 target 匹配）。
fn is_capture_session(watched: &Watched, tid: &str, sid: &str) -> bool {
    if watched.capture_sessions.lock().unwrap().contains_key(sid) {
        return true;
    }
    watched
        .recording
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|slot| recording::owns_attachment(&slot.state.capture_session, tid, sid))
}

/// attachedToTarget：type=page 建 targetId→sessionId 映射并补域 enable；
/// type=iframe 登记 OOPIF 帧会话（W-oopif，enable 复用同一套——fill/press/
/// eval 的 DOM/Runtime 命令将路由到 frame session）。best-effort：enable/
/// 探测失败只缺对应能力，不影响命令路径。
async fn on_attached(watched: &Watched, params: &Value) {
    let ty = params
        .pointer("/targetInfo/type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(tid) = params
        .pointer("/targetInfo/targetId")
        .and_then(Value::as_str)
    else {
        return;
    };
    let Some(sid) = params.get("sessionId").and_then(Value::as_str) else {
        return;
    };
    // I4 attach 豁免（关键）：kit 录制对锚点 page target 建第二个 flattened
    // capture session（screencast 专属），其 attach 事件 type=page——不豁免会
    // 被 tabs.attach 原位覆写锚点 tab 的 session（后续命令全部路由到 capture
    // session，路由全乱）。两层判定见 is_capture_session。
    if is_capture_session(watched, tid, sid) {
        return;
    }
    let Some(conn) = watched.conn.lock().unwrap().clone() else {
        return;
    };
    match ty {
        "page" => {
            watched.tabs.lock().unwrap().attach(tid, sid);
            enable_session_domains(watched, &conn, tid, sid, true).await;
        }
        // OOPIF（W-oopif）：targetId == frameId（Chrome 不变式）。归属 tab 未知
        // 时先存孤儿会话（frameAttached 到达时认领）；enable 照发——frame
        // session 的 Page/DOM 域是 fill/press 跨 frame 的命令通道
        "iframe" => {
            let owner_tab = {
                let mut tables = watched.frames.lock().unwrap();
                let mut owner = None;
                for (tab_tid, table) in tables.iter_mut() {
                    if let Some(info) = table.frames.get_mut(tid) {
                        info.session_id = Some(sid.to_string());
                        owner = Some(tab_tid.clone());
                        break;
                    }
                }
                owner
            };
            let owner_tab = match owner_tab {
                Some(tab) => tab,
                None => {
                    watched
                        .orphan_frame_sessions
                        .lock()
                        .unwrap()
                        .insert(tid.to_string(), sid.to_string());
                    String::new()
                }
            };
            enable_session_domains(watched, &conn, &owner_tab, sid, false).await;
        }
        _ => {}
    }
}

/// per-session 域启用（page 与 iframe attach 共用，W-oopif 抽出）：
/// 对话框盲探测先行（L3，悬挂对话框时唯一不被阻塞的命令）→ Page/DOM/CSS
/// enable。`is_page`：仅 page target 记 css_since（hover 结算窗口起点）——
/// frame target 不记（hover 扫描不进 frame，其样式表事件无 tab 可归属）。
pub(crate) async fn enable_session_domains(
    watched: &Watched,
    conn: &SharedPageConn,
    tid: &str,
    sid: &str,
    is_page: bool,
) {
    // L3：盲探测必须**先于** enable 发出——对话框悬挂时唯一不被 renderer
    // 阻塞的命令就是 handleJavaScriptDialog（Page/Runtime.enable 等都会挂起
    // 至超时）；若 alert 挂着，先探测 accept 再 enable 才能全部成功。探测
    // 与常规事件驱动路径互不干扰（无对话框时 -32602 静默）。frame 会话
    // 归属未知（孤儿）时跳过：无 tid 可归档，frame 内 alert 走事件驱动。
    if !tid.is_empty() {
        probe_reconnected_dialog(watched, conn, tid, sid).await;
    }
    // CSS.enable 与 Page/DOM 同点位：hover 扫描的 styleSheetAdded 回放从
    // attach 起积累（snap 时按需 enable 无法同步拿到回放——事件异步，注册
    // 表会是空）。成功时刻入 css_since：hover 扫描的结算窗口以此为起点。
    let mut css_enabled = false;
    for method in ["Page.enable", "DOM.enable", "CSS.enable"] {
        let ok = match cdp_send(conn, method, json!({}), Some(sid)).await {
            Ok(_) => true,
            Err(e) => {
                tracing::debug!("[page] session {sid} {method} 失败: {e}");
                false
            }
        };
        if method == "CSS.enable" {
            css_enabled = ok;
        }
    }
    // session 级 auto-attach（W-oopif）：browser 级 setAutoAttach 只覆盖顶层
    // target，OOPIF（type=iframe）的 attachedToTarget 由 page/帧 session 自己
    // 的 autoAttach 产生（flatten：事件发到父 session，params.sessionId =
    // 新帧会话——on_attached 按 params 处理，天然兼容）。帧会话同样下发，
    // 支持嵌套 iframe。best-effort：失败只缺帧能力，不影响主路径。
    if let Err(e) = cdp_send(
        conn,
        "Target.setAutoAttach",
        json!({ "autoAttach": true, "flatten": true, "waitForDebuggerOnStart": false }),
        Some(sid),
    )
    .await
    {
        tracing::debug!("[page] session {sid} 级 setAutoAttach 失败: {e}");
    }
    if css_enabled && is_page {
        watched
            .css_since
            .lock()
            .unwrap()
            .insert(tid.to_string(), Instant::now());
    }
}

/// L3 重连对话框盲探测：CDP 断连重连后**不重播** javascriptDialogOpening——
/// 重连前已挂着的 JS 对话框对 PageService 失明（无事件可驱动
/// [`on_dialog_opening`]，该 tab 的全部 CDP 命令被阻塞至超时、永久卡死）。
/// 故每个新 page session 在 Page.enable 后盲发一次 handleJavaScriptDialog
///（accept）充当探测：
/// - 成功 = 真有挂着的历史对话框，已被本探测接管——无事件参数可得，用保守
///   默认构造 DialogInfo（type=alert、message 固定文案）入缓冲，sequence 随
///   [`DialogBuffer::record`] 顺延；
/// - 失败（无对话框时 Chrome 回 -32602 No dialog is showing，或其他错误）→
///   debug 日志静默忽略（首连/无对话框的常态路径，不 Warning 刷屏）。
async fn probe_reconnected_dialog(watched: &Watched, conn: &SharedPageConn, tid: &str, sid: &str) {
    match cdp_send(
        conn,
        "Page.handleJavaScriptDialog",
        json!({ "accept": true }),
        Some(sid),
    )
    .await
    {
        Ok(_) => {
            tracing::debug!("[page] session {sid} 探测接管了重连前挂着的历史对话框（tab {tid}）");
            watched
                .dialogs
                .lock()
                .unwrap()
                .entry(tid.to_string())
                .or_insert_with(|| DialogBuffer::new(tid.to_string()))
                .record(DialogInfo {
                    r#type: "alert".to_string(),
                    message: "reconnected dialog (auto-accepted)".to_string(),
                    url: None,
                    default_prompt: None,
                    has_browser_handler: None,
                });
        }
        Err(e) => {
            tracing::debug!("[page] session {sid} 无挂着的历史对话框（探测静默）: {e}");
        }
    }
}

/// detachedFromTarget / targetDestroyed 的共同清理：page target → 整 tab
/// 摘除（映射 + RefMap / DialogBuffer / load 等待者一起删——会话已死，
/// 状态随之作废；等待中的导航命令各自走到自己的超时分支）；
/// frame target → 仅摘除该帧登记（I1b 起不标记 ref 失效：durable refs 由
/// 解析链自行判定，帧会话死亡会让帧内 ref 的恢复解析自然失败）。
pub(crate) fn drop_target(watched: &Watched, tid: &str) {
    if watched.tabs.lock().unwrap().contains_target(tid) {
        drop_tab(watched, tid);
        return;
    }
    {
        let mut tables = watched.frames.lock().unwrap();
        for (_tab_tid, table) in tables.iter_mut() {
            if table.frames.remove(tid).is_some() {
                break;
            }
        }
    }
    watched.orphan_frame_sessions.lock().unwrap().remove(tid);
}

/// 摘除单个 OOPIF 帧登记（frameDetached / drop_target 共用）
/// 摘除单个 OOPIF 帧登记（frameDetached / drop_target 共用）
fn drop_frame(watched: &Watched, tab_tid: &str, fid: &str) {
    watched
        .frames
        .lock()
        .unwrap()
        .get_mut(tab_tid)
        .map(|t| t.frames.remove(fid));
}

fn on_detached(watched: &Watched, params: &Value) {
    let tid = params.get("targetId").and_then(Value::as_str);
    let sid = params.get("sessionId").and_then(Value::as_str);
    // I4 detach 豁免：capture session 的 detachedFromTarget 带**锚点 targetId**
    //（capture session 挂在同一 page target 上）——不豁免会被 drop_target
    // 当作锚点 tab 关闭误删整 tab。豁免表按 sessionId 消费式摘除（条目
    // 生命周期终点；stop 后在途事件依豁免此层，见 state.rs 注释）；kit
    // owns_attachment 兼容豁免表登记前的窗口（attach 事件先于响应）。
    if let Some(s) = sid {
        if watched.capture_sessions.lock().unwrap().remove(s).is_some() {
            return;
        }
    }
    if let (Some(t), Some(s)) = (tid, sid) {
        if watched
            .recording
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|slot| recording::owns_attachment(&slot.state.capture_session, t, s))
        {
            return;
        }
    }
    match (tid, sid) {
        (Some(t), _) => drop_target(watched, t),
        (None, Some(s)) => {
            let target = watched
                .tabs
                .lock()
                .unwrap()
                .target_of_session(s)
                .map(str::to_string);
            if let Some(t) = target {
                drop_tab(watched, &t);
                return;
            }
            // frame session（W-oopif）：按会话反查帧
            if let Some((tab_tid, fid)) = frame_session_lookup(watched, s) {
                drop_frame(watched, &tab_tid, &fid);
            }
        }
        (None, None) => {}
    }
}

/// detachedFromTarget（page target）的整 tab 清理（原 drop_tab，语义不变）
pub(crate) fn drop_tab(watched: &Watched, tid: &str) {
    watched.tabs.lock().unwrap().detach(tid);
    watched.refs.lock().unwrap().remove(tid);
    watched.snap_gen.lock().unwrap().remove(tid);
    watched.dialogs.lock().unwrap().remove(tid);
    watched.load_waiters.lock().unwrap().remove(tid);
    watched.sheets.lock().unwrap().remove(tid);
    watched.css_since.lock().unwrap().remove(tid);
    watched.frames.lock().unwrap().remove(tid);
    // I3：diff 滚动基线随 tab 摘除；profiler 锚点匹配时废弃（session 死后
    // 事件流无处可收，trace 不可回收——stop 将报 NOT_RUNNING 并在 message
    // 披露废弃原因，不装作可继续）
    watched.last_snap.lock().unwrap().remove(tid);
    watched.profiler.lock().unwrap().take_if(|p| p.tid == tid);
    // I4：录制随锚点 tab 关闭废弃（同 profiler 语义——capture session 死于
    // 同一 target，帧流不可回收，保留只会让 stop 报误导性错误）。锁内同步
    // best-effort：发 cancel 让常驻任务自行收尾（stopScreencast/detach 由
    // 任务内部 best-effort，client clone 在任务闭包内）；JoinHandle 随 state
    // drop = detached。豁免表指向本锚点的条目一并摘（capture session 同死）。
    if let Some(mut slot) = watched.recording.lock().unwrap().take_if(|r| r.tid == tid) {
        if let Some(tx) = slot.state.cancel_tx.take() {
            let _ = tx.send(());
        }
    }
    watched
        .capture_sessions
        .lock()
        .unwrap()
        .retain(|_, anchor| anchor.as_str() != tid);
}

/// frame session → (tab tid, frame tid)；未登记返回 None
pub(crate) fn frame_session_lookup(watched: &Watched, sid: &str) -> Option<(String, String)> {
    let tables = watched.frames.lock().unwrap();
    for (tab_tid, table) in tables.iter() {
        for (fid, info) in table.frames.iter() {
            if info.session_id.as_deref() == Some(sid) {
                return Some((tab_tid.clone(), fid.clone()));
            }
        }
    }
    None
}

/// 顶层 sessionId → targetId（session 维度事件定位 tab 用）。
fn tab_of_session(watched: &Watched, event: &Value) -> Option<String> {
    let sid = event.get("sessionId").and_then(Value::as_str)?;
    watched
        .tabs
        .lock()
        .unwrap()
        .target_of_session(sid)
        .map(str::to_string)
}
