//! 17891 隧道落地监听器的 token 校验中间件（方案 §6）。
//!
//! 凭证模型两级：Bearer token 守控制面（`/api/v1/*`、`/mcp`）；
//! `/cdp/` 前缀豁免 Bearer——URL 内会话 key 即该层凭证（WS 升级请求无法携带
//! 自定义 header），会话校验由代理路由承担（PR-2 落地，规则先行防规格漂移）。
//!
//! 401 响应必须走统一错误形状（ErrorBody）：旧 CLI 的「401 → exit 7 +
//! UNAUTHORIZED 错误码」降级语义依赖该形状，缺失会落入 Protocol（exit 1）。

use axum::extract::Request;
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::extract::State;

use crate::error::AppError;

use super::server::ApiState;

/// 会话凭证路径前缀（豁免 Bearer）。
const SESSION_PATH_PREFIX: &str = "/cdp/";

/// 17891 全路由 Bearer 校验（`/cdp/` 豁免）。
pub async fn require_token(
    State(state): State<ApiState>,
    req: Request,
    next: Next,
) -> Response {
    if req.uri().path().starts_with(SESSION_PATH_PREFIX) {
        return next.run(req).await;
    }
    let expected = state.settings_service.remote_token().unwrap_or_default();
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if expected.is_empty() || !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        return AppError::business(
            401,
            "UNAUTHORIZED",
            "远程访问缺少令牌或令牌不正确（Authorization: Bearer <token>）",
        )
        .into_response();
    }
    next.run(req).await
}

/// 常数时间字节比较：长度不等提前返回（长度本身不敏感），等长逐字节积累差异。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
