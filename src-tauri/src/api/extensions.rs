use axum::extract::{Path, Query, State};
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

#[derive(Deserialize)]
pub struct ConfigQuery {
    /// 省略 = 全局默认配置（被 per-env 配置覆盖）
    #[serde(rename = "environmentId")]
    pub environment_id: Option<String>,
}

/// GET /api/v1/extensions/:id/config?environmentId=
/// 返回 `{environmentId, config}`；未配置 → config 为 null
pub async fn get_config(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(q): Query<ConfigQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let view = state.extension_service.get_config(&id, q.environment_id.as_deref())?;
    Ok(Json(view))
}

/// PUT /api/v1/extensions/:id/config?environmentId=：body 即配置 JSON 对象（整体覆盖）。
/// 仅声明 configFile 的 system 物化扩展可配置（否则 400 EXTENSION_CONFIG_INVALID）；
/// 体 ≤64KB；net-rules 写入时同步做 DNR 可编译校验。对新启动实例生效。
pub async fn put_config(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(q): Query<ConfigQuery>,
    body: String,
) -> Result<Json<serde_json::Value>, AppError> {
    state.extension_service.set_config(&id, q.environment_id.as_deref(), &body)?;
    Ok(Json(json!({ "ok": true })))
}
