//! 健康检查端点：CLI status/doctor 共用数据源

use axum::extract::State;
use axum::Json;
use serde_json::json;

use super::server::ApiState;

/// GET /api/v1/health
pub async fn get(State(state): State<ApiState>) -> Json<serde_json::Value> {
    Json(json!(state.health.snapshot()))
}
