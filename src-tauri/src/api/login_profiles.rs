use axum::extract::{Path, State};
use axum::Json;
use serde_json::json;

use crate::error::AppError;

use super::server::ApiState;

/// GET /api/v1/login-profiles（Login Profiles 页全量列表）
pub async fn list(State(state): State<ApiState>) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!(state.profile_service.list().await?)))
}

/// GET /api/v1/environments/{id}/login-profile
pub async fn get(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!(state.profile_service.view(&id).await?)))
}

/// POST /api/v1/environments/{id}/login-profile/launch（用母本目录启动浏览器供登录）
pub async fn launch(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!(state.profile_service.launch(&id).await?)))
}

/// POST /api/v1/environments/{id}/login-profile/capture（停机捕获快照，运行中 → 409）
pub async fn capture(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!(state.profile_service.capture(&id).await?)))
}

/// POST /api/v1/environments/{id}/login-profile/reset（清空母本，回到 Not Configured）
pub async fn reset(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!(state.profile_service.reset(&id).await?)))
}
