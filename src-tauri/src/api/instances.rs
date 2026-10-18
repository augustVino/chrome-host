use axum::extract::{Path, Query, State};
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

/// GET /api/v1/instances/{id}/diagnostics：页面诊断快照透传
/// （实例未运行/无 http 页 → 409 DIAGNOSTICS_UNAVAILABLE）
pub async fn diagnostics(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let view = state.instance_service.diagnostics(&id).await?;
    Ok(Json(view))
}

#[derive(Deserialize)]
pub struct ConsoleQuery {
    /// error / warning / info / debug / log（缺省全量）
    pub level: Option<String>,
    /// 缺省 100，上限 1000
    pub limit: Option<usize>,
}

/// GET /api/v1/instances/{id}/console：常驻订阅的 console 环（500 条，进程内态）
pub async fn console(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(q): Query<ConsoleQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let view = state.diagnostics.console(&id, q.level.as_deref(), q.limit)?;
    Ok(Json(view))
}

#[derive(Deserialize)]
pub struct NetworkQuery {
    /// "true" = 只看失败请求或 HTTP ≥400
    #[serde(rename = "failedOnly", default)]
    pub failed_only: bool,
    pub limit: Option<usize>,
}

/// GET /api/v1/instances/{id}/network：常驻订阅的请求环（1000 条，完成/失败时入环）
pub async fn network(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(q): Query<NetworkQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let view = state.diagnostics.network(&id, q.failed_only, q.limit)?;
    Ok(Json(view))
}

/// GET /api/v1/instances/{id}/screenshot：当前页面 png 截图（base64，
/// 复用常驻诊断连接；实例未运行/连接未就绪/无 http 页 → 409）
pub async fn screenshot(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let view = state.diagnostics.screenshot(&id).await?;
    Ok(Json(view))
}

/// GET /api/v1/instances/{id}/har：从请求环生成 HAR 1.2 文档
pub async fn har(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let view = state.diagnostics.har(&id)?;
    Ok(Json(view))
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

/// POST /api/v1/instances/{id}/cdp/sessions —— 发放 CDP 会话（方案 §7.1）。
/// 17891 上需 Bearer token；会话 key 是 token 的下级凭证（单实例作用域 + 30 分钟），
/// 用于 /cdp/{ins}/{sid}/* 代理路径（含 WS，无法携带 header）。
pub async fn create_cdp_session(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    // 发放前置：实例须运行中（404/409 语义与 get_cdp 一致）
    state.instance_service.require_cdp_port(&id).await?;
    let view = state.sessions.create(&id);
    Ok((StatusCode::CREATED, Json(json!(view))))
}
