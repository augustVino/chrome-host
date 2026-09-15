use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;

use super::server::ApiState;

#[derive(Deserialize)]
pub struct OpenTabBody {
    pub url: String,
}

#[derive(Deserialize)]
pub struct NavigateBody {
    #[serde(rename = "tabId")]
    pub tab_id: String,
    pub url: String,
}

/// POST /api/v1/environments/{environmentId}/instances
pub async fn create(
    State(state): State<ApiState>,
    Path(environment_id): Path<String>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let ins = state.instance_service.create(&environment_id).await?;
    Ok((StatusCode::CREATED, Json(json!(ins))))
}

/// POST /api/v1/environments/{environmentId}/open
pub async fn open(
    State(state): State<ApiState>,
    Path(environment_id): Path<String>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let ins = state.instance_service.create(&environment_id).await?;
    Ok((StatusCode::CREATED, Json(json!(ins))))
}

/// POST /api/v1/environments/{environmentId}/stop-all
pub async fn stop_all(
    State(state): State<ApiState>,
    Path(environment_id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let stopped = state.instance_service.stop_all_for_env(&environment_id).await?;
    Ok(Json(json!({ "environmentId": environment_id, "stopped": stopped })))
}

/// GET /api/v1/environments/{environmentId}/instances
pub async fn list_by_env(
    State(state): State<ApiState>,
    Path(environment_id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let list = state.instance_service.list_by_env(&environment_id).await?;
    Ok(Json(json!(list)))
}

/// GET /api/v1/instances：全量实例列表，含 environmentName
pub async fn list_all(State(state): State<ApiState>) -> Result<Json<serde_json::Value>, AppError> {
    let list = state.instance_service.list().await?;
    Ok(Json(json!(list)))
}

/// GET /api/v1/instances/{id}
pub async fn get(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!(state.instance_service.get(&id).await?)))
}

/// GET /api/v1/instances/{id}/status
pub async fn status(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let ins = state.instance_service.get(&id).await?;
    Ok(Json(json!({
        "id": ins.id,
        "status": ins.status,
        "pid": ins.pid,
        "cdpPort": ins.cdp_port,
        "browserVersion": ins.browser_version,
    })))
}

/// POST /api/v1/instances/{id}/start
pub async fn start(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!(state.instance_service.start(&id).await?)))
}

/// POST /api/v1/instances/{id}/stop
pub async fn stop(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let ins = state.instance_service.stop(&id).await?;
    Ok(Json(json!(ins)))
}

/// POST /api/v1/instances/{id}/restart
pub async fn restart(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!(state.instance_service.restart(&id).await?)))
}

/// POST /api/v1/instances/{id}/focus
pub async fn focus(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let ins = state.instance_service.focus(&id).await?;
    Ok(Json(json!(ins)))
}

/// GET /api/v1/instances/{id}/cdp
pub async fn cdp(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let endpoint = state.instance_service.get_cdp(&id).await?;
    Ok(Json(json!(endpoint)))
}

/// GET /api/v1/instances/{id}/tabs
pub async fn tabs(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let tabs = state.instance_service.list_tabs(&id).await?;
    Ok(Json(json!(tabs)))
}

/// POST /api/v1/instances/{id}/tabs
pub async fn open_tab(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<OpenTabBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let target = state.instance_service.open_tab(&id, &body.url).await?;
    Ok((StatusCode::CREATED, Json(json!(target))))
}

/// POST /api/v1/instances/{id}/navigate
pub async fn navigate(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<NavigateBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let target = state
        .instance_service
        .navigate(&id, &body.tab_id, &body.url)
        .await?;
    Ok(Json(json!(target)))
}

/// DELETE /api/v1/instances/{id}
pub async fn delete(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.instance_service.delete(&id).await?;
    Ok(Json(json!({ "ok": true })))
}
