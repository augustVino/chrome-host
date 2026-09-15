use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::error::AppError;

/// 统一错误响应体：Agent 依赖 code 而非 message
#[derive(serde::Serialize)]
struct ErrorBody {
    error: ErrorInner,
}

#[derive(serde::Serialize)]
struct ErrorInner {
    code: &'static str,
    message: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = ErrorBody {
            error: ErrorInner { code: self.code(), message: self.to_string() },
        };
        (status, axum::Json(json!(body))).into_response()
    }
}
