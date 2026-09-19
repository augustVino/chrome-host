//! 远程接入端点：隧道状态与令牌轮换（方案 §8）。

use axum::extract::State;
use axum::Json;
use serde_json::json;

use crate::error::AppError;

use super::server::ApiState;

/// GET /api/v1/remote-access/status —— 隧道实时状态（UI 状态行轮询、诊断共用）
pub async fn status(State(state): State<ApiState>) -> Json<serde_json::Value> {
    Json(json!(state.tunnel.status()))
}

/// POST /api/v1/remote-access/token —— 轮换访问令牌（旧令牌立即失效）。
/// 17890（本地）直接可调；17891（隧道）需持旧令牌——中间件统一拦截，此处不区分。
/// 轮换同时清空全部活跃 CDP 会话（方案 §6：旧凭证体系整体作废）。
pub async fn rotate_token(
    State(state): State<ApiState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let token = state.settings_service.rotate_remote_token()?;
    let cleared = state.sessions.clear_all();
    state.activity.record(
        "warn",
        "remote_access_token_rotated",
        None,
        None,
        &format!("远程访问令牌已轮换（旧令牌立即失效，清空 {cleared} 个活跃 CDP 会话）"),
    );
    Ok(Json(json!({ "token": token })))
}
