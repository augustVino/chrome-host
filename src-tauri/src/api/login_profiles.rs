use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
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

/// POST /api/v1/environments/{id}/login-profile/capture
/// 可选 body {"fromInstanceId"}：从已停止实例捕获（实例即登录源；推荐路径）。
/// 无 body：传统母本捕获（兼容保留，后续版本废弃）。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CaptureBody {
    from_instance_id: Option<String>,
}

pub async fn capture(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    body: Option<Json<CaptureBody>>,
) -> Result<Json<serde_json::Value>, AppError> {
    match body.and_then(|Json(b)| b.from_instance_id) {
        Some(ins_id) => Ok(Json(
            json!(state.session_service.capture_from_instance(&id, &ins_id).await?),
        )),
        None => Ok(Json(json!(state.profile_service.capture(&id).await?))),
    }
}

/// POST /api/v1/environments/{id}/login-profile/reset
/// 清空身份层两层（Cookie 保险库 + 文件快照母本）；登录浏览器运行中 → 409
pub async fn reset(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(json!(state.session_service.reset(&id).await?)))
}
