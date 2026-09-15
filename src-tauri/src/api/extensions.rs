use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;

use super::server::ApiState;

/// GET /api/v1/extensions
pub async fn list(State(state): State<ApiState>) -> Result<Json<serde_json::Value>, AppError> {
    let list = state.extension_service.list()?;
    Ok(Json(json!(list)))
}

/// GET /api/v1/extensions/:id
pub async fn get(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let ext = state.extension_service.get(&id)?;
    Ok(Json(json!(ext)))
}

#[derive(Deserialize)]
pub struct RegisterBody {
    pub path: String,
}

/// POST /api/v1/extensions：注册用户扩展（服务端读 manifest 提取元数据）。
/// 安全边界：注册是用户主动行为；实例加载只引用注册表，无按路径加载。
pub async fn register(
    State(state): State<ApiState>,
    Json(body): Json<RegisterBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if body.path.trim().is_empty() {
        return Err(AppError::invalid_request("path 不能为空"));
    }
    let ext = state.extension_service.register(body.path.trim())?;
    Ok(Json(json!(ext)))
}

#[derive(Deserialize)]
pub struct PatchBody {
    pub enabled: bool,
}

/// PATCH /api/v1/extensions/:id：启用/禁用（System → 403）
pub async fn patch(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<PatchBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let ext = state.extension_service.set_enabled(&id, body.enabled)?;
    Ok(Json(json!(ext)))
}

/// DELETE /api/v1/extensions/:id：移除注册项，不删源文件（System → 403）
pub async fn delete(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.extension_service.delete(&id)?;
    Ok(Json(json!({ "ok": true })))
}
