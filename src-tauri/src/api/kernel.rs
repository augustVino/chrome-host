//! 内核管理端点：状态、下载、取消

use axum::extract::State;
use axum::Json;
use serde_json::json;

use super::server::ApiState;

/// GET /api/v1/kernel/status
pub async fn status(State(state): State<ApiState>) -> Json<serde_json::Value> {
    Json(json!(state.kernel.status()))
}

/// POST /api/v1/kernel/download：触发后台下载；已在下载中返回 409 语义（ok:false）
pub async fn download(State(state): State<ApiState>) -> Json<serde_json::Value> {
    let started = state.kernel.start_download(state.app.clone());
    Json(json!({ "ok": started, "downloading": true }))
}

/// POST /api/v1/kernel/cancel
pub async fn cancel(State(state): State<ApiState>) -> Json<serde_json::Value> {
    let cancelled = state.kernel.cancel_download().await;
    Json(json!({ "ok": cancelled }))
}
