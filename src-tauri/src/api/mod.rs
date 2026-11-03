pub mod auth;
pub mod cdp_proxy;
pub mod environments;
pub mod events;
pub mod extensions;
pub mod error;
pub mod health;
pub mod instances;
pub mod kernel;
pub mod login_profiles;
pub mod mcp;
pub mod pages;
pub mod remote_access;
pub mod settings;
pub mod server;
pub mod session;

use crate::error::AppError;

/// 可选 JSON body 的宽容解析：空 / 纯空白 → `None`；非法 JSON → 400 `INVALID_REQUEST`。
///
/// 不能用 `Option<Json<T>>`：axum 0.8 的 `OptionalFromRequest` 仅在**缺失**
/// `Content-Type: application/json` 头时给 `None`——GUI 的 `request()` 封装对无 body
/// 的 POST 也恒带该头，空 body 会被 Json 提取器按语法错误拒绝（400 EOF），请求
/// 根本到不了 handler（0.6.0 实证回归：GUI 创建实例必败、CLI 恒发 body 不受影响）。
/// AGENT-API.md 契约承诺「无 body / 空 / 字段 null 均合法」，故手工解析原始 bytes。
pub fn optional_json_body<T: serde::de::DeserializeOwned>(
    body: &[u8],
) -> Result<Option<T>, AppError> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(None);
    }
    serde_json::from_slice(body)
        .map(Some)
        .map_err(|e| AppError::invalid_request(format!("请求 body 不是合法 JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::optional_json_body;

    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct Body {
        name: Option<String>,
    }

    /// 回归：GUI 无 body POST 恒带 content-type: application/json——
    /// 空 body 必须解析为 None 而不是拒绝（0.6.0 曾因此 GUI 创建实例必败）
    #[test]
    fn empty_or_whitespace_body_yields_none() {
        assert_eq!(optional_json_body::<Body>(b"").unwrap(), None);
        assert_eq!(optional_json_body::<Body>(b" \n\t").unwrap(), None);
    }

    #[test]
    fn valid_bodies_parse() {
        assert_eq!(
            optional_json_body::<Body>(b"{\"name\":null}").unwrap(),
            Some(Body { name: None })
        );
        assert_eq!(
            optional_json_body::<Body>("{\"name\":\"qa任务A\"}".as_bytes()).unwrap(),
            Some(Body { name: Some("qa任务A".into()) })
        );
    }

    #[test]
    fn invalid_json_is_structured_400() {
        let err = optional_json_body::<Body>(b"{oops").unwrap_err();
        assert_eq!(err.status(), 400);
        assert_eq!(err.code(), "INVALID_REQUEST");
    }
}
