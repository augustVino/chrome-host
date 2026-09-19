//! CDP 会话代理路由（方案 §7.2）：`/cdp/{ins}/{sid}/{*rest}`。
//!
//! 凭证语义：`/cdp/` 前缀已在 auth 中间件豁免 Bearer（WS 升级无法携带 header），
//! **会话 key 即该层凭证**——每个请求（含 WS 升级前握手）都过 validate。

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;

/// `Option<WebSocketUpgrade>` 的替代（axum 0.8 移除了 `Option<T>` 万能提取器，
/// WebSocketUpgrade 未实现 OptionalFromRequestParts）：升级请求 → Some，否则 None。
pub(crate) struct MaybeUpgrade(Option<WebSocketUpgrade>);

impl<S: Send + Sync> FromRequestParts<S> for MaybeUpgrade {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        // 复用 axum 的升级请求识别（connection: upgrade + upgrade: websocket），
        // 非升级请求的 rejection 一律折算为 None
        let ws = WebSocketUpgrade::from_request_parts(parts, state).await.ok();
        Ok(MaybeUpgrade(ws))
    }
}

use crate::error::AppError;
use crate::infrastructure::cdp::proxy::{
    bridge_ws, classify_proxy_path, proxy_json, ProxyKind,
};

use super::server::ApiState;

/// GET（含 WS 升级）/cdp/{ins}/{sid}/{*rest}
///
/// 同一路由承载两类流量（按 path 前缀分类）：
/// - `json` 家族（白名单三个端点）→ HTTP 代理 + webSocketDebuggerUrl 改写；
/// - `devtools/*` → WS 升级 + 帧级桥到实例真实端口；
/// - 其余（/json/new 等变更端点）→ 400，远程变更统一走 REST。
pub async fn proxy(
    State(state): State<ApiState>,
    Path((instance_id, session_id, rest)): Path<(String, String, String)>,
    MaybeUpgrade(ws): MaybeUpgrade,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    // 会话校验（未知/过期/实例不匹配一律同错误——防枚举）
    if !state.sessions.validate(&instance_id, &session_id) {
        return Err(AppError::not_found(
            "SESSION_NOT_FOUND",
            "CDP 会话不存在或已过期（30 分钟有效）；请重新发放：POST /api/v1/instances/{id}/cdp/sessions",
        ));
    }
    // 实例须运行中（对账 + 404/409 语义与 get_cdp 一致）
    let port = state.instance_service.require_cdp_port(&instance_id).await?;

    // Host 头改写：客户端拿到的 WS URL 自动指向自己可达的地址
    // （经隧道时即 yun 侧的 127.0.0.1:17890）
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1:17890")
        .to_string();
    let session_prefix = format!("/cdp/{instance_id}/{session_id}");

    match classify_proxy_path(&rest) {
        ProxyKind::Json => json_response(proxy_json(port, "/json", &host, &session_prefix).await),
        ProxyKind::JsonList => {
            json_response(proxy_json(port, "/json/list", &host, &session_prefix).await)
        }
        ProxyKind::JsonVersion => {
            json_response(proxy_json(port, "/json/version", &host, &session_prefix).await)
        }
        ProxyKind::DevToolsWs(devtools_path) => {
            let Some(upgrade) = ws else {
                return Err(AppError::invalid_request(
                    "devtools 路径仅支持 WebSocket 连接（HTTP 调试端点仅 /json 家族）",
                ));
            };
            Ok(upgrade
                .on_upgrade(move |socket| bridge_ws(socket, port, devtools_path))
                .into_response())
        }
        ProxyKind::Rejected => Err(AppError::invalid_request(
            "非代理端点（仅 GET /json、/json/list、/json/version 与 WS /devtools/*）；\
             开新页请走 REST open_tab，激活请走 focus",
        )),
    }
}

fn json_response(value: Result<serde_json::Value, AppError>) -> Result<Response, AppError> {
    Ok(Json(value?).into_response())
}
