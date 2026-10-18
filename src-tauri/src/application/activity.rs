//! Activity 流：应用内事件日志。
//!
//! 铁律：emit 与落库走同一入口（防双写遗漏）——服务层禁止直接调 app.emit，
//! 统一经 `Activity::record`。target 为实例时 environment_id 自动回填（查 instances 表）。
//! M7-B 起 record 同点旁挂 SSE broadcast（[`SSE_CHANNEL_CAP`]）：REST `/api/v1/events`
//! 订阅者收到落库行同款富事件；无订阅者时 send 为 no-op，满 256 滞后仅丢该订阅者
//!（SSE 语义可接受，订阅侧以 lagged 注释行感知）。

use serde::Serialize;
use std::sync::Arc;
use tokio::sync::broadcast::{self, Receiver, Sender};

use crate::error::AppError;
use crate::infrastructure::db::client::DbPool;
use tauri::{AppHandle, Emitter};

/// 超过该条数时启动清理旧行
const MAX_EVENTS: i64 = 5000;
/// SSE 旁挂通道容量：覆盖订阅者短暂停顿（如终端渲染）的突发；
/// 满则该订阅者滞后丢事件（不阻断 record 主路径）
const SSE_CHANNEL_CAP: usize = 256;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppEvent {
    pub ts: i64,
    pub level: String,
    pub event: String,
    pub environment_id: Option<String>,
    pub target_id: Option<String>,
    pub message: String,
}

pub struct Activity {
    app: AppHandle,
    pool: DbPool,
    sse: Arc<Sender<AppEvent>>,
}

impl Activity {
    pub fn new(app: AppHandle, pool: DbPool) -> Self {
        let (sse, _) = broadcast::channel(SSE_CHANNEL_CAP);
        Activity { app, pool, sse: Arc::new(sse) }
    }

    /// SSE 订阅入口：每次调用返回独立 Receiver（/api/v1/events 每连接一个；
    /// 连接断开随 stream drop 自动退订，无泄漏）
    pub fn subscribe(&self) -> Receiver<AppEvent> {
        self.sse.subscribe()
    }

    /// 统一入口：落库 + emit + SSE broadcast（三者同点，防双写遗漏）。
    /// environment_id 传 None 且 target 是实例时，自动回填其所属环境。
    pub fn record(
        &self,
        level: &str,
        event: &str,
        environment_id: Option<&str>,
        target_id: Option<&str>,
        message: &str,
    ) {
        let ts = chrono::Utc::now().timestamp_millis();
        let environment_id = match environment_id {
            Some(e) => Some(e.to_string()),
            None => target_id.and_then(|t| self.resolve_environment_id(t)),
        };
        let evt = AppEvent {
            ts,
            level: level.to_string(),
            event: event.to_string(),
            environment_id: environment_id.clone(),
            target_id: target_id.map(str::to_string),
            message: message.to_string(),
        };
        if let Ok(conn) = self.pool.get() {
            let _ = conn.execute(
                "INSERT INTO app_events (ts, level, event, environment_id, target_id, message) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![evt.ts, evt.level, evt.event, evt.environment_id, evt.target_id, evt.message],
            );
        } else {
            tracing::warn!("activity 落库失败（连接池不可用）: event={}", evt.event);
        }
        // 无订阅者 = no-op；某订阅者滞后仅丢具（其 stream 会收到 lagged 标记）
        let _ = self.sse.send(evt.clone());
        if let Err(e) = self.app.emit(
            &evt.event,
            serde_json::json!({ "id": evt.target_id, "environmentId": evt.environment_id, "message": evt.message }),
        ) {
            tracing::error!("发送事件 {} 失败: {e}", evt.event);
        }
    }

    fn resolve_environment_id(&self, target_id: &str) -> Option<String> {
        let conn = self.pool.get().ok()?;
        conn.query_row(
            "SELECT environment_id FROM instances WHERE id = ?1",
            rusqlite::params![target_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
    }

    /// 启动清理：仅保留最近 5000 条
    pub fn cleanup(&self) {
        if let Ok(conn) = self.pool.get() {
            let _ = conn.execute(
                "DELETE FROM app_events WHERE id NOT IN (SELECT id FROM app_events ORDER BY ts DESC LIMIT ?1)",
                rusqlite::params![MAX_EVENTS],
            );
        }
    }

    pub fn list_by_env(&self, environment_id: &str, limit: i64) -> Result<Vec<AppEvent>, AppError> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT ts, level, event, environment_id, target_id, message
             FROM app_events WHERE environment_id = ?1
             ORDER BY ts DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![environment_id, limit.clamp(1, 200)],
            |row| {
                Ok(AppEvent {
                    ts: row.get(0)?,
                    level: row.get(1)?,
                    event: row.get(2)?,
                    environment_id: row.get(3)?,
                    target_id: row.get(4)?,
                    message: row.get(5)?,
                })
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}
