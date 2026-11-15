//! page record start/stop（I4）：kit recording.rs 的录屏编排。
//!
//! 常驻任务特例（同 profiler 模式，不走 command() 骨架）：
//! - **不占实例串行锁**（ensure 后 drop(serial)）：录制期间其他 page 命令
//!   必须能继续执行——录制的意义就是记录这些操作（click/drag 的
//!   [`record_cursor`] 钩子在无录制时零开销：一次 Mutex lock 读 Option）；
//! - **无 30s 命令预算**：stop 的编码收尾（cancel → screencast teardown →
//!   ffmpeg flush/退出）可能数秒，外层预算会在收尾中途误杀（产物半开且
//!   状态悬置）。超时由 kit 内部各阶段兜底（teardown 命令各 2s；encoder
//!   flush 写超时 2s/帧）——join 本身无外层超时，ffmpeg 进程级 hang 的
//!   极端形态由调用方 kill 兜底（披露项）。
//!
//! 生命周期：establish 整清（连接死 = capture session 死，帧流不可回收）；
//! 锚点 tab 关闭（drop_tab）废弃（best-effort cancel，同 profiler 语义——
//! 产物不可回收）。attach/detach 事件豁免见 events.rs 的
//! [`is_capture_session`](super::events) 与 state.rs 的 capture_sessions 表。
//!
//! 错误映射：kit String 错误 → [`map_record_error`]（专属码优先，兜底
//! PAGE_ACTION_FAILED——不复用 refs::map_kit_error：其 "not found" →
//! PAGE_ELEMENT_NOT_FOUND 规则会把 kit 的 "ffmpeg not found..." 误映射成
//! 404 元素错误，同 profiler 自建 mapper 的先例理由）。

use super::state::{RecordingSlot, Watched};
use super::{kit_client, PageService};
use crate::application::browser_kit::recording::{
    attach_capture_session, capture_initial_image, check_ffmpeg_available, detach_capture_session,
    recording_start, recording_stop, spawn_recording_task, stop_recording_task, RecordingOptions,
    RecordingState, DEFAULT_CONTACT_SHEET_THRESHOLD,
};
use crate::domain::page_control::PageControlError;
use crate::error::AppError;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use tokio::sync::oneshot;

impl PageService {
    /// page record start <tab> <file> [--fps N] [--cursor]：ffmpeg 预检 →
    /// kit 校验链（扩展名/fps）→ attach 专属 capture session（豁免登记）→
    /// 初始帧 → spawn 常驻采集/编码任务。FILE = 主机路径，扩展名定容器
    ///（.webm/.mp4/.mov…；无扩展名 → 400）。
    pub async fn record_start(
        &self,
        ins_id: &str,
        tab: &str,
        file: &str,
        fps: Option<u32>,
        cursor: bool,
    ) -> Result<Value, AppError> {
        let (watched, serial) = self.ensure(ins_id).await?;
        let (tid, sid) = super::resolve_target(&watched.tabs.lock().unwrap(), tab)?;
        // 常驻任务不占实例串行锁（同 profiler/download_wait 特例）
        drop(serial);
        let mut resp = record_start_tool(&watched, &tid, &sid, file, fps, cursor).await?;
        resp["instanceId"] = json!(ins_id);
        resp["targetId"] = json!(tid);
        Ok(resp)
    }

    /// page record stop [--status]：take-out → kit stop_recording_task
    ///（cancel → join 常驻任务收尾：screencast teardown + ffmpeg flush）→
    /// recording_stop 组装产物信息。status=true 附 kit 完整终态（capture
    /// 模式/设备缩放等披露字段）。
    pub async fn record_stop(&self, ins_id: &str, status: bool) -> Result<Value, AppError> {
        let (watched, serial) = self.ensure(ins_id).await?;
        drop(serial);
        let mut resp = record_stop_tool(&watched, status).await?;
        resp["instanceId"] = json!(ins_id);
        Ok(resp)
    }
}

/// start 工具：互斥检查（锁内同步，无 await）→ kit 校验链（本地新 state）
/// → ffmpeg 预检（422）→ 写回 active 壳（此后 attach 豁免的 kit 判定层
/// 生效）→ attach capture session → 豁免表登记 → 初始帧 → spawn 常驻
/// 任务 → 句柄写回。任一步失败 = 干净失败（槽/豁免表/best-effort detach
/// 全部撤净，状态区不残留）。
#[allow(clippy::too_many_lines)]
pub(crate) async fn record_start_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    path: &str,
    fps: Option<u32>,
    cursor: bool,
) -> Result<Value, AppError> {
    // 互斥（Chrome 每实例一路 screencast capture + 一个编码器）：Some 即占用
    if watched.recording.lock().unwrap().is_some() {
        return Err(PageControlError::record_already_active(
            "该实例已有运行中的录制（先 record stop 落盘再重新 start）",
        )
        .into());
    }
    // kit 校验链（本地 fresh state：无扩展名/fps 越界 → 400，原文透传；
    // contact sheet 族固定关闭——命令面未暴露，kit 阈值取缺省）
    let mut state = RecordingState::new();
    let started = recording_start(
        &mut state,
        path,
        RecordingOptions {
            fps,
            cursor,
            contact_sheet: false,
            contact_sheet_threshold: DEFAULT_CONTACT_SHEET_THRESHOLD,
        },
    )
    .map_err(|e| map_record_error(&e, "start"))?;
    // Arc 句柄先取出（state 随后移入槽位）
    let shared_capture = state.capture_session.clone();
    let shared_cursor = state.shared_cursor.clone();
    let cursor_overlays = state.cursor_overlays.clone();
    let effective_fps = state.fps;
    // 写回 active 壳：此后事件流的 kit owns_attachment 豁免层生效——
    // attach_capture_session 会先把 (target_id, None) 写入 shared slot，
    // Chrome 在 attach 响应**之前**发出 attachedToTarget，该窗口按
    // target_id 匹配豁免（见 kit CaptureSession::owns_attachment 注释）
    *watched.recording.lock().unwrap() = Some(RecordingSlot {
        state,
        tid: tid.to_string(),
    });
    let discard = |w: &Watched| {
        w.recording.lock().unwrap().take();
    };
    // ffmpeg 预检（422，先于浏览器 attach——缺二进制不动浏览器侧状态）
    if let Err(e) = check_ffmpeg_available().await {
        discard(watched);
        return Err(PageControlError::record_no_ffmpeg(e).into());
    }
    let client = match kit_client(watched) {
        Ok(c) => c,
        Err(e) => {
            discard(watched);
            return Err(e);
        }
    };
    // attach 专属 capture session（同 target 第二个 flattened session，
    // screencast 专属——豁免两层：shared slot（in-flight）+ 豁免表（稳定期））
    let capture_sid = match attach_capture_session(&client, sid, &shared_capture).await {
        Ok(s) => s,
        Err(e) => {
            // kit 内部已把 shared slot 清回 None（见 attach_capture_session）
            discard(watched);
            return Err(map_record_error(&e, "start"));
        }
    };
    watched
        .capture_sessions
        .lock()
        .unwrap()
        .insert(capture_sid.clone(), tid.to_string());
    // 初始帧（Chrome 不保证为静态页发首帧，kit 以显式截图播种）+ spawn
    match capture_initial_image(&client, sid).await {
        Ok(initial) => {
            // 该 tab 的 OOPIF frame session（--cursor 的页内覆盖层要铺到帧）
            let iframe_sessions: Vec<String> = {
                let tables = watched.frames.lock().unwrap();
                tables
                    .get(tid)
                    .map(|t| {
                        t.frames
                            .values()
                            .filter_map(|i| i.session_id.clone())
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let (cancel_tx, cancel_rx) = oneshot::channel();
            let shared_count = Arc::new(AtomicU64::new(0));
            let shared_captured = Arc::new(AtomicU64::new(0));
            let shared_contact = Arc::new(AtomicU64::new(0));
            let shared_screenshot = Arc::new(AtomicBool::new(false));
            let task = spawn_recording_task(
                Arc::new(client),
                capture_sid,
                sid.to_string(),
                initial,
                path.to_string(),
                effective_fps,
                Arc::clone(&shared_count),
                Arc::clone(&shared_captured),
                cursor,
                shared_cursor,
                cursor_overlays,
                iframe_sessions,
                None,
                DEFAULT_CONTACT_SHEET_THRESHOLD,
                Arc::clone(&shared_contact),
                Arc::clone(&shared_screenshot),
                cancel_rx,
            );
            if let Some(slot) = watched.recording.lock().unwrap().as_mut() {
                slot.state.capture_task = Some(task);
                slot.state.cancel_tx = Some(cancel_tx);
                slot.state.shared_frame_count = Some(shared_count);
                slot.state.shared_captured_count = Some(shared_captured);
                slot.state.shared_contact_sheet_count = Some(shared_contact);
                slot.state.shared_screenshot_capture = Some(shared_screenshot);
            }
            let mut resp = started;
            resp["result"] = json!("recording started");
            Ok(resp)
        }
        Err(e) => {
            // 干净失败：豁免条目 + best-effort detach + 槽位全撤
            watched
                .capture_sessions
                .lock()
                .unwrap()
                .remove(&capture_sid);
            detach_capture_session(&client, &capture_sid).await;
            discard(watched);
            Err(map_record_error(&e, "start"))
        }
    }
}

/// stop 工具：take-out 模式（锁内不 await）→ kit stop_recording_task
///（cancel → 常驻任务收尾：stopScreencast/detach 各 2s 超时 + encoder
/// flush/ffmpeg 退出，join 完成后清 shared slot）→ recording_stop 组装
/// 产物。失败不回填：capture 任务已终结（cancel 已发），回填只会让重试
/// stop 走空 join 后再撞 "No frames captured"，诚实报错即终态。
pub(crate) async fn record_stop_tool(watched: &Watched, status: bool) -> Result<Value, AppError> {
    let taken = watched.recording.lock().unwrap().take();
    let Some(slot) = taken else {
        return Err(PageControlError::record_not_running(
            "从未 start / 已 stop / 连接重建或录制所在 tab 已关闭致运行态废弃",
        )
        .into());
    };
    let mut state = slot.state;
    stop_recording_task(&mut state)
        .await
        .map_err(|e| map_record_error(&e, "stop"))?;
    let v = recording_stop(&mut state).map_err(|e| map_record_error(&e, "stop"))?;
    let mut resp = json!({
        "result": "recording stopped",
        "path": v.get("path").cloned().unwrap_or(Value::Null),
        "frames": v.get("frames").cloned().unwrap_or(Value::Null),
        "capturedFrames": v.get("capturedFrames").cloned().unwrap_or(Value::Null),
        "fps": v.get("fps").cloned().unwrap_or(Value::Null),
    });
    if status {
        // kit 完整终态（capture: "screenshots" 模式 / deviceScaleFactor /
        // contactSheet* 披露字段原样并入）
        resp["status"] = v;
    }
    Ok(resp)
}

/// --cursor 录制的输入采样钩子（input.rs 的 click/drag/clickxy 成功派发后
/// 追加调用）：锁内读 recording 槽，Some 才进 kit RecordingCursorHistory
///（其 record 自带 enabled 判定——非 cursor 录制不积累样本）。同步无
/// await；无录制时开销 = 一次 Mutex lock 读 Option。坐标 = 顶层视口
/// CSS 坐标（与 Input.dispatchMouseEvent 同基准）。
pub(crate) fn record_cursor(watched: &Watched, x: f64, y: f64, buttons: i32) {
    let guard = watched.recording.lock().unwrap();
    if let Some(slot) = guard.as_ref() {
        if let Ok(mut cursor) = slot.state.shared_cursor.lock() {
            cursor.record(x, y, buttons);
        }
    }
}

/// kit recording 的 String 错误 → chrome-host 错误码（专属码优先；兜底
/// PAGE_ACTION_FAILED 500，语义对齐 refs::map_kit_error 的兜底分支）。
/// 文案特征取自 kit recording.rs 源码（recording_start/stop、
/// check_ffmpeg_available、attach_capture_session）。
fn map_record_error(raw: &str, op: &str) -> AppError {
    let m = raw.to_lowercase();
    if m.contains("already active") {
        PageControlError::record_already_active(raw.to_string()).into()
    } else if m.contains("no recording in progress") {
        PageControlError::record_not_running(raw).into()
    } else if m.contains("invalid output path")
        || m.contains("invalid fps")
        || m.contains("invalid contact sheet threshold")
    {
        AppError::invalid_request(raw.to_string())
    } else if m.contains("ffmpeg not found") {
        PageControlError::record_no_ffmpeg(raw.to_string()).into()
    } else {
        PageControlError::action_failed(format!("record {op}: {raw}")).into()
    }
}
