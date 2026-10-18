//! 会话身份层端点：状态查询与回滚（实例即登录源模型的观测面）。

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;

use super::server::ApiState;

/// GET /api/v1/environments/{id}/session
pub async fn status(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!(state.session_service.status(&id)?)))
}

/// POST /api/v1/environments/{id}/session/restore
/// body: { "generation": <n> }
/// 边界：仅回滚 Cookie 保险库；文件层快照保留当前版本（见 SessionService::restore）
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreBody {
    pub generation: i64,
}

pub async fn restore(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<RestoreBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!(state.session_service.restore(&id, body.generation).await?)))
}
