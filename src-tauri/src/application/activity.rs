//! Activity 流：应用内事件日志。
//!
//! 铁律：emit 与落库走同一入口（防双写遗漏）——服务层禁止直接调 app.emit，
//! 统一经 `Activity::record`。target 为实例时 environment_id 自动回填（查 instances 表）。

use serde::Serialize;

use crate::error::AppError;
use crate::infrastructure::db::client::DbPool;
use tauri::{AppHandle, Emitter};

/// 超过该条数时启动清理旧行
const MAX_EVENTS: i64 = 5000;

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
}

impl Activity {
    pub fn new(app: AppHandle, pool: DbPool) -> Self {
        Activity { app, pool }
    }

    /// 统一入口：落库 + emit（emit 与落库同点）。
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
        if let Ok(conn) = self.pool.get() {
            let _ = conn.execute(
                "INSERT INTO app_events (ts, level, event, environment_id, target_id, message) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![ts, level, event, environment_id, target_id, message],
            );
        } else {
            tracing::warn!("activity 落库失败（连接池不可用）: event={event}");
        }
        if let Err(e) = self.app.emit(
            event,
            serde_json::json!({ "id": target_id, "environmentId": environment_id, "message": message }),
        ) {
            tracing::error!("发送事件 {event} 失败: {e}");
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
