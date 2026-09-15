use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;

use super::server::ApiState;

/// GET /api/v1/settings
pub async fn get(State(state): State<ApiState>) -> Result<Json<serde_json::Value>, AppError> {
    let view = state.settings_service.get()?;
    Ok(Json(json!(view)))
}

#[derive(Deserialize)]
pub struct UpdateSettingsBody {
    #[serde(rename = "developerMode")]
    pub developer_mode: Option<bool>,
    #[serde(rename = "envLabelPosition")]
    pub env_label_position: Option<String>,
    #[serde(rename = "envLabelColor")]
    pub env_label_color: Option<String>,
    #[serde(rename = "defaultStartUrl")]
    pub default_start_url: Option<String>,
}

/// PUT /api/v1/settings（部分更新：只更新传入字段）
pub async fn put(
    State(state): State<ApiState>,
    Json(body): Json<UpdateSettingsBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let view = state
        .settings_service
        .update(
            body.developer_mode,
            body.env_label_position,
            body.env_label_color,
            body.default_start_url,
        )?;
    Ok(Json(json!(view)))
}

