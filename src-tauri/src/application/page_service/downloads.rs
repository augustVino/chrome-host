use super::{download_failed, MAX_DOWNLOAD_HISTORY};
use crate::application::page_service::state::Watched;
use crate::error::AppError;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::oneshot;
// ============================================================================
// 3c. 下载运行态（W-file）：browser 级事件登记 + 终态结算 + 命令式等待
// ============================================================================

/// 实例级下载运行态（Browser 域事件无 session 维度，故非 per-tab）。
/// 生命周期 = page 控制连接：establish 整体重置（重新 setDownloadBehavior
/// 与新目录），等待者随登记一起被 drop（download_wait 的 rx 收 Err 诚实报
/// 失效，而非挂死到自己的超时）。
#[derive(Default)]
pub(crate) struct DownloadState {
    /// 落盘目录（establish 时解析 = <profile>/downloads）
    pub(crate) dir: std::path::PathBuf,
    /// guid → WillBegin 登记信息（Progress 终态时摘除）
    pub(crate) in_flight: HashMap<String, InflightDownload>,
    /// 终态记录环（含被等待者消费的；容量 MAX_DOWNLOAD_HISTORY）
    pub(crate) history: std::collections::VecDeque<Value>,
    /// download-wait 等待者（先注册后触发；超时方 drop rx，残留 sender
    /// 在下次结算时 send 失败被丢弃——与 load_waiters 同款懒清理）
    pub(crate) waiters: std::collections::VecDeque<oneshot::Sender<Result<Value, AppError>>>,
}

/// WillBegin 登记信息（downloadProgress 只带 guid，需回查这些字段）。
pub(crate) struct InflightDownload {
    /// 净化后的展示名（unsafe 时可能为空串）
    file_name: String,
    /// 原始 url（记录时脱敏，与 intercept journal 同策略）
    url: String,
    /// frameId 归属的 tab targetId（无法归属 = None）
    tab: Option<String>,
    started_ms: i64,
    /// suggestedFilename 含路径分隔符/../ 等（拒绝拼路径，防逃逸）
    unsafe_name: bool,
}

impl DownloadState {
    /// establish 时整体重建（新连接/新目录；等待者 drop → rx Err 分支）
    pub(crate) fn reset(&mut self, dir: std::path::PathBuf) {
        self.dir = dir;
        self.in_flight.clear();
        self.history.clear();
        self.waiters.clear();
    }
}
/// 下载文件名净化（W-file 安全边界）：拒路径分隔符与 ../ 逃逸。
/// 返回 (展示名, 是否拒绝拼路径)。判定规则：
/// - 含 '/' 或 '\\'、或 file_name() 取出的末段 ≠ 原文（含 ../ 类形态）、
///   或末段为空/. /.. → unsafe；
/// - unsafe 时展示名仍保末段（可读），但 filePath 不拼（拒绝逃逸）。
fn sanitize_download_filename(raw: &str) -> (String, bool) {
    let trimmed = raw.trim();
    let has_sep = trimmed.contains('/') || trimmed.contains('\\');
    let base = std::path::Path::new(trimmed)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let degenerate = base.is_empty() || base == "." || base == "..";
    let unsafe_name = has_sep || degenerate || base != trimmed;
    (base.to_string(), unsafe_name)
}

/// WillBegin 的 frameId → 归属 tab（root 或任一已登记子帧；含 OOPIF——
/// frameAttached 对同源/跨域子帧都发）。归属不了（无 frames 表/未知帧）
/// = None：记录仍生成，只是无 tab 标注（诚实披露，不猜）。
pub(crate) fn download_tab_of_frame(watched: &Watched, frame_id: &str) -> Option<String> {
    if frame_id.is_empty() {
        return None;
    }
    let tables = watched.frames.lock().unwrap();
    for (tid, table) in tables.iter() {
        if table.root.as_deref() == Some(frame_id) || table.frames.contains_key(frame_id) {
            return Some(tid.clone());
        }
    }
    None
}

/// Browser.downloadWillBegin：登记 in_flight（同步登记，不发命令——
/// 事件流内不碰串行锁，与 on_request_paused 同纪律）。
pub(crate) fn on_download_begin(watched: &Watched, params: &Value) {
    let (Some(guid), Some(name)) = (
        params.get("guid").and_then(Value::as_str),
        params.get("suggestedFilename").and_then(Value::as_str),
    ) else {
        return;
    };
    if guid.is_empty() {
        return;
    }
    let (file_name, unsafe_name) = sanitize_download_filename(name);
    if unsafe_name {
        tracing::warn!(
            "[page] 下载 {guid} suggestedFilename 含路径形态（{:?}），路径拼接已拒绝",
            name
        );
    }
    let url = params.get("url").and_then(Value::as_str).unwrap_or("");
    let frame_id = params.get("frameId").and_then(Value::as_str).unwrap_or("");
    let tab = download_tab_of_frame(watched, frame_id);
    watched.downloads.lock().unwrap().in_flight.insert(
        guid.to_string(),
        InflightDownload {
            file_name,
            url: url.to_string(),
            tab,
            started_ms: chrono::Utc::now().timestamp_millis(),
            unsafe_name,
        },
    );
}

/// Browser.downloadProgress 终态结算：completed/canceled/interrupted
/// → 摘除 in_flight、生成记录入环、结算最早一个等待者（completed 且安全
/// → Ok(record)；失败/取消/名字不安全 → Err，如实报错）。inProgress 与
/// 未知状态（前向兼容）忽略。
pub(crate) fn on_download_progress(watched: &Watched, params: &Value) {
    let (Some(guid), Some(state)) = (
        params.get("guid").and_then(Value::as_str),
        params.get("state").and_then(Value::as_str),
    ) else {
        return;
    };
    if !matches!(state, "completed" | "canceled" | "interrupted") {
        return;
    }
    let mut st = watched.downloads.lock().unwrap();
    let Some(info) = st.in_flight.remove(guid) else {
        return; // 无 WillBegin 登记（跨连接残留/事件乱序）：无从结算，丢弃
    };
    let received = params
        .get("receivedBytes")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completed_ms = chrono::Utc::now().timestamp_millis();
    // 落盘核证（仅 completed 且名字安全）：文件在 → bytes 用真实大小；
    // 不在（Chrome 可能因重名改为 "name (1)"）→ bytes 用事件 receivedBytes
    // 且 verified=false（诚实披露：路径是预期值，未实测）
    let expected_path = if info.unsafe_name || info.file_name.is_empty() {
        None
    } else {
        Some(st.dir.join(&info.file_name))
    };
    let (bytes, verified) = match (state, &expected_path) {
        ("completed", Some(p)) => match std::fs::metadata(p) {
            Ok(m) => (m.len(), true),
            Err(_) => (received, false),
        },
        _ => (received, false),
    };
    let mut record = json!({
        "guid": guid,
        "state": state,
        "fileName": if info.file_name.is_empty() { Value::Null } else { json!(info.file_name) },
        "filePath": expected_path.as_ref().map(|p| json!(p.display().to_string())).unwrap_or(Value::Null),
        "bytes": bytes,
        "mimeType": null, // CDP 下载事件不携带 mimeType（诚实置 null，不猜扩展名）
        "url": crate::domain::redact::redact_url(&info.url),
        "tab": info.tab,
        "startedAt": info.started_ms,
        "completedAt": completed_ms,
        "verified": verified,
    });
    if info.unsafe_name {
        record["note"] = json!("suggestedFilename 含路径形态，路径拼接已拒绝");
    }
    // 等待者结算：弹栈直到 send 成功（超时残留的死 sender 在此清理）；
    // 先到先得 = 多下载并行时等待者拿最早结算的（任务口径：wait 返回
    // 最先完成的；其余留在环里供 GET /page/downloads 查询）
    let mut matched = false;
    while let Some(tx) = st.waiters.pop_front() {
        // 发送给等待者的副本自带 matched=true（正在被它消费）；历史环的
        // 记录随后统一打标（未被消费 = false）
        let mut payload = record.clone();
        payload["matched"] = json!(true);
        let outcome = match state {
            "completed" if !info.unsafe_name => Ok(payload),
            "canceled" | "interrupted" => Err(download_failed(state, &info.file_name)),
            _ => Err(download_failed("rejected", &info.file_name)),
        };
        if tx.send(outcome).is_ok() {
            matched = true;
            break;
        }
    }
    record["matched"] = json!(matched);
    st.history.push_back(record);
    while st.history.len() > MAX_DOWNLOAD_HISTORY {
        st.history.pop_front();
    }
}
