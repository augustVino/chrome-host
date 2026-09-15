//! CLI 侧统一错误模型。
//!
//! 四个变体对应四类失败面，与 PRD cli.md §18 exit code 契约挂钩：
//! 映射逻辑集中在 exit.rs（唯一映射点），本文件只描述错误、不做任何映射。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CliError {
    /// Agent API 不可达（连接拒绝 / 超时）→ exit 8。
    /// 携带 reqwest 错误描述；文案必须给出用户可执行的动作指引（桌面应用需运行）。
    #[error("无法连接 chrome-host Agent API（{0}）。请确认 chrome-host 桌面应用已运行。")]
    Unreachable(String),

    /// 服务端业务错误 → 按 (status, code) 映射 3/4/5/6/7/1。
    /// code 取值与服务端 src-tauri/src/error.rs 的错误码同源，CLI 只透传不造码。
    #[error("[{code}] {message}（HTTP {status}）")]
    Api {
        /// HTTP 状态码（400/403/404/409/5xx…）
        status: u16,
        /// 服务端机器错误码，如 INSTANCE_NOT_FOUND
        code: String,
        /// 服务端消息，CLI 不二次加工
        message: String,
    },

    /// 协议破坏：响应不是合法 JSON，或形状不符契约 → exit 1。
    /// 正常情况下不应出现；一旦出现即服务端 / 中间代理异常。
    #[error("Agent API 响应格式异常（{0}）")]
    Protocol(String),

    /// 本地失败：请求未发出（参数本地校验不通过、HTTP 客户端构造失败等）→ exit 2。
    #[error("{0}")]
    Local(String),
}
