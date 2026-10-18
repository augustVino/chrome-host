//! GET /api/v1/events：业务事件 SSE 流（text/event-stream）。
//!
//! 数据源是 `Activity::record` 同点旁挂的 broadcast（落库行同款富事件：
//! event/level/ts/environmentId/targetId/message——比 Tauri 事件 payload 多
//! level/ts/targetId）。agent 从轮询 activity 转订阅。
//!
//! 语义：
//! - `event:` 为既有事件名（instance-started 等，与 Tauri 事件同源同名）；
//! - 满缓冲滞后 → 该订阅者收到 `: lagged n` 注释行（丢的是它自己的追赶窗口，
//!   不影响其他订阅者与主路径）；
//! - 心跳：15s `: ping` 注释行（SSH 隧道/代理空闲掐连兜底，axum KeepAlive 原生）；
//! - 客户端断开 → stream drop → broadcast 自动退订，无连接泄漏；
//! - 鉴权：17890 免鉴权不变；17891 全路由 Bearer 中间件天然覆盖（非 /cdp/ 豁免路径）。

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use super::server::ApiState;

/// SSE 空闲心跳间隔（计划锚点：隧道/代理常见空闲阈 30-60s，15s 留足裕量）
const HEARTBEAT: Duration = Duration::from_secs(15);

pub async fn events(
    State(state): State<ApiState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.activity.subscribe();
    let stream = BroadcastStream::new(rx).map(|item| {
        Ok(match item {
            Ok(evt) => Event::default()
                .event(&evt.event)
                .json_data(evt)
                .expect("AppEvent 序列化不会失败（纯数据 DTO）"),
            Err(lagged) => {
                tracing::warn!("[events] SSE 订阅者滞后，丢弃 {lagged} 条");
                Event::default().comment(format!("lagged {lagged}"))
            }
        })
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(HEARTBEAT).text("ping"))
}
