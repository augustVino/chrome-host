//! PageCdp：CdpConnection 的 dyn 化适配。
//!
//! application/page_service.rs 需要把「发一条 CDP 命令」抽象成 trait 对象，
//! 以便测试注入 fake（不依赖真 Chrome）。ws.rs 的 `CdpConnection::send` 是
//! 泛型方法（`T: DeserializeOwned`），无法直接 dyn 化；本文件把它收敛为
//! 「参数 Value、返回 Value」的最小形态并给出实现——生产路径零行为差异，
//! ws.rs 本体不动。
//!
//! 并发模型：`CdpConnection::send` 为 &self，共享形态就是裸
//! `Arc<dyn PageCdp>`（无锁包裹）——多条命令并发 in-flight，对话框 handle
//! 不会被 in-flight 工具命令阻塞。
//!
//! 事件流不经本 trait：连接建立时经 [`EventSink`] 接出（与常驻诊断
//! 同一机制），命令与事件两条路径在 CdpConnection 内部天然分流。
//!
//! 约束：不引入 async-trait crate，手写 boxed future（`Box::pin`）。

use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::Value;

use super::ws::{CdpConnection, EventSink};
use crate::error::AppError;

/// 单命令 future（Send：跨线程 runtime 调度）。
pub type PageCdpFuture<'a> = BoxFuture<'a, Result<Value, AppError>>;

/// 发 CDP 命令的最小抽象：Value 进、Value出（&self：可共享并发发，
/// Sync 是跨线程共享 &self 的前提）。
///
/// `session_id` 供 flatten 模式（Target.setAutoAttach）的 per-target 命令；
/// browser 级命令传 None。错误语义与 `CdpConnection::send` 一致：Chrome 侧
/// 返回 error、命令超时、连接中断统一归为 `CDP_COMMAND_FAILED`。
pub trait PageCdp: Send + Sync {
    /// 三个借用参数共享生命周期：future 存续期间 method/session_id 均存活
    /// （调用侧均为活过 await 的局部 &str，无额外约束）。
    fn send_value<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        session_id: Option<&'a str>,
    ) -> PageCdpFuture<'a>;

    /// raw 响应帧文本通道（browser_kit typed_raw 用：MB 级响应单次反序列
    /// 化，防 Value 中间树双份内存）。默认实现报不支持——测试 fake 无需
    /// 覆写；CdpConnection 原生实现。
    fn send_raw<'a>(
        &'a self,
        _method: &'a str,
        _params: Value,
        _session_id: Option<&'a str>,
    ) -> PageCdpRawFuture<'a> {
        Box::pin(async {
            Err(AppError::business(
                500,
                "CDP_COMMAND_FAILED",
                "raw 通道不可用（该连接形态不支持）",
            ))
        })
    }
}

/// raw 通道的 future 形态（与 PageCdpFuture 同构，输出为帧文本）。
pub type PageCdpRawFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, AppError>> + Send + 'a>>;

/// 共享连接形态：裸 `Arc<dyn PageCdp>`（生产 = CdpConnection，测试 = fake）。
/// send 为 &self，无需锁包裹（若用 `Arc<tokio::sync::Mutex<...>>` 包裹，
/// 命令串行会导致对话框 handle 被 in-flight 命令阻塞）。
pub type SharedPageConn = Arc<dyn PageCdp>;

impl PageCdp for CdpConnection {
    fn send_value<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        session_id: Option<&'a str>,
    ) -> PageCdpFuture<'a> {
        // 显式走固有方法（inherent 优先于 trait），仅收敛泛型为 Value
        Box::pin(CdpConnection::send::<Value>(
            self, method, params, session_id,
        ))
    }

    fn send_raw<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        session_id: Option<&'a str>,
    ) -> PageCdpRawFuture<'a> {
        Box::pin(CdpConnection::send_raw(
            self, method, params, session_id,
        ))
    }
}

/// 生产连接工厂：browser 级 eventing WS + 事件透传（page_service 注入用）。
pub async fn connect_page(port: u16, events: EventSink) -> Result<SharedPageConn, AppError> {
    let conn = CdpConnection::connect_eventing(port, events).await?;
    Ok(Arc::new(conn))
}
