use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::json;

use crate::domain::environment::{CreateEnvironmentInput, UpdateEnvironmentInput};
use crate::error::AppError;

use super::server::ApiState;

/// POST /api/v1/environments
pub async fn create(
    State(state): State<ApiState>,
    Json(input): Json<CreateEnvironmentInput>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let env = state.env_service.create(input)?;
    Ok((StatusCode::CREATED, Json(json!(env))))
}

/// GET /api/v1/environments（含对账后的运行时摘要）
pub async fn list(State(state): State<ApiState>) -> Result<Json<serde_json::Value>, AppError> {
    let envs = state.env_service.list_summary().await?;
    Ok(Json(json!(envs)))
}

/// GET /api/v1/environments/{id}
pub async fn get(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!(state.env_service.get(&id)?)))
}

/// PATCH /api/v1/environments/{id}
pub async fn patch(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(input): Json<UpdateEnvironmentInput>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!(state.env_service.update(&id, input)?)))
}

/// GET /api/v1/environments/{id}/status
pub async fn status(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let instances = state.instance_service.list_by_env(&id).await?;
    let (mut total, mut running, mut starting, mut stopped, mut error) =
        (0i64, 0i64, 0i64, 0i64, 0i64);
    for ins in instances {
        total += 1;
        match ins.status {
            crate::domain::instance::InstanceStatus::Running => running += 1,
            crate::domain::instance::InstanceStatus::Starting => starting += 1,
            crate::domain::instance::InstanceStatus::Stopped => stopped += 1,
            crate::domain::instance::InstanceStatus::Error
            | crate::domain::instance::InstanceStatus::Crashed => error += 1,
            _ => {}
        }
    }
    Ok(Json(json!({
        "environmentId": id,
        "instances": { "total": total, "running": running, "starting": starting, "stopped": stopped, "error": error }
    })))
}

/// DELETE /api/v1/environments/{id}
pub async fn delete(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.env_service.delete(&id)?;
    Ok(Json(json!({ "ok": true })))
}

/// GET /api/v1/environments/{id}/activity?limit=50
pub async fn activity(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, i64>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let limit = params.get("limit").copied().unwrap_or(50);
    let events = state.activity.list_by_env(&id, limit)?;
    Ok(Json(json!(events)))
}
