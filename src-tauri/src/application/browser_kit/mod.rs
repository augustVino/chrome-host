//! browser_kit：浏览器操控能力库（vendor 自 vercel-labs/agent-browser，Apache-2.0）。
//!
//! 分层纪律：
//! - 本目录内的文件是「纯 CDP 工具层」——只经由 [`client::CdpClient`] 的
//!   send_command*/subscribe* 原语访问连接，不 import page_service 的任何
//!   类型（Watched/RefStore 等），反之 page_service 可以自由调用本层；
//! - 每个文件的文件头登记 upstream commit 与本项目适配记录；
//! - 事件形态 [`types::CdpEvent`] 由 Watched 的 event_bus 广播（watch_loop
//!   逐事件转发），本层订阅消费；
//! - 集成层负责把本层的 String 错误映射回 chrome-host 错误码体系。
//!
//! M0 状态：全部文件为机械 vendor（仅 import 路径改写），尚未接入
//! page_service——适配与集成按 V/I 波任务逐文件推进。

#[allow(dead_code)]
pub mod client;
#[allow(dead_code)]
pub mod cookies;
#[allow(dead_code)]
pub mod diff;
#[allow(dead_code)]
pub mod downloads;
#[allow(dead_code)]
pub mod element;
#[allow(dead_code)]
pub mod interaction;
#[allow(dead_code)]
pub mod pointer;
#[allow(dead_code)]
pub mod recording;
#[allow(dead_code)]
pub mod screenshot;
#[allow(dead_code)]
pub mod snapshot;
#[allow(dead_code)]
pub mod storage;
#[allow(dead_code)]
pub mod tracing;
#[allow(dead_code)]
pub mod types;

pub use client::{cdp_event_from_frame, CdpClient};
pub use types::CdpEvent;
