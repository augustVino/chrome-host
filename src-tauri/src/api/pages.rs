//! page 控制端点（P2-C）：`/api/v1/instances/{id}/page/*`。
//!
//! handler 只做参数解包与透传——tab 路由（targetId / 唯一前缀）、错误分类、
//! 对话框游标、超时预算全部在 [`crate::application::page_service::PageService`]
//! （P2-B）。响应体为 service 返回原样（与 console/network/screenshot 等
//! 既有端点一致，无额外包裹层）。
//!
//! `tab` 缺省语义（service 侧 `resolve_target("")`，P2-F F1）：取最近
//! 打开（attach）的页面标签（活跃 tab 近似——CDP 无 browser 级 active tab
//! 直查 API）；显式前缀仍是无匹配 404 / 多匹配 409。

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

use crate::error::AppError;

use super::server::ApiState;

/// POST /api/v1/instances/{id}/page/snap —— AX 树快照 + @eN ref 整表替换。
#[derive(Deserialize)]
pub struct SnapBody {
    /// 目标 tab（targetId 或唯一前缀；缺省 = 最近打开的页面标签）
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/click —— 原生输入点击。
#[derive(Deserialize)]
pub struct ClickBody {
    /// 点击目标：@eN ref（上次 snap 产生）或 CSS 选择器
    pub target: String,
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/fill —— 聚焦 + 清空 + 输入 + 回读校验。
#[derive(Deserialize)]
pub struct FillBody {
    /// 填写目标：@eN ref 或 CSS 选择器
    pub target: String,
    /// 填入文本；空串 = 仅清空
    pub value: String,
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/press —— 键盘三段序列（rawKeyDown/char/keyUp）。
#[derive(Deserialize)]
pub struct PressBody {
    /// 键名：Enter/Tab/单字符/组合键（Control+A 等）；未知 → 400 PAGE_KEY_UNKNOWN
    pub key: String,
    /// 可选聚焦目标：@eN ref 或 CSS 选择器（先 focus 再按键）
    pub target: Option<String>,
    pub tab: Option<String>,
}

/// POST /api/v1/instances/{id}/page/wait —— 轮询等待元素状态。
#[derive(Deserialize)]
pub struct WaitBody {
    pub selector: String,
    /// visible（缺省）/ hidden / attached / detached
    pub state: Option<String>,
    /// 时长字符串："10s" / "250ms" / "500"（裸数字 = ms）；缺省 10s
    pub timeout: Option<String>,
    pub tab: Option<String>,
}

/// GET /api/v1/instances/{id}/page/dialogs —— 对话框缓冲查询（已被自动 accept）。
#[derive(Deserialize)]
pub struct DialogsQuery {
    pub tab: Option<String>,
    /// 游标：只返回 sequence 严格大于 N 的条目；缺省 0 = 全部
    pub since: Option<u64>,
}

pub async fn snap(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<SnapBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .snap(&id, body.tab.as_deref().unwrap_or(""))
        .await?;
    Ok(Json(resp))
}

pub async fn click(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<ClickBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .click(&id, body.tab.as_deref().unwrap_or(""), &body.target)
        .await?;
    Ok(Json(resp))
}

pub async fn fill(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<FillBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .fill(
            &id,
            body.tab.as_deref().unwrap_or(""),
            &body.target,
            &body.value,
        )
        .await?;
    Ok(Json(resp))
}

pub async fn press(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<PressBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .press(
            &id,
            body.tab.as_deref().unwrap_or(""),
            &body.key,
            body.target.as_deref(),
        )
        .await?;
    Ok(Json(resp))
}

pub async fn wait(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<WaitBody>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .wait(
            &id,
            body.tab.as_deref().unwrap_or(""),
            &body.selector,
            body.state.as_deref(),
            body.timeout.as_deref(),
        )
        .await?;
    Ok(Json(resp))
}

pub async fn dialogs(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(q): Query<DialogsQuery>,
) -> Result<Json<Value>, AppError> {
    let resp = state
        .page
        .dialogs(&id, q.tab.as_deref().unwrap_or(""), q.since)
        .await?;
    Ok(Json(resp))
}
