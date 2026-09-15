//! Extension 仓储：extensions 表 CRUD。
//! 仅数据访问，不做业务校验（服务层职责）；status 惰性计算，仓储只存注册时初始值。

use rusqlite::params;

use crate::domain::extension::{Extension, ExtensionStatus, ExtensionType};
use crate::error::AppError;

use crate::infrastructure::db::client::DbPool;

pub struct ExtensionRepository {
    pool: DbPool,
}

impl ExtensionRepository {
    pub fn new(pool: DbPool) -> Self {
        ExtensionRepository { pool }
    }

    pub fn insert(&self, ext: &Extension) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO extensions (id, name, description, version, manifest_version, type, source_path, enabled, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                ext.id,
                ext.name,
                ext.description,
                ext.version,
                ext.manifest_version,
                ext.extension_type.as_str(),
                ext.source_path,
                ext.enabled as i64,
                ext.status_as_str(),
                ext.created_at,
                ext.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Extension, AppError> {
        let conn = self.pool.get()?;
        let result = conn.query_row(
            "SELECT id, name, description, version, manifest_version, type, source_path, enabled, status, created_at, updated_at
             FROM extensions WHERE id = ?1",
            params![id],
            Self::map_row,
        );
        result.map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                AppError::not_found("EXTENSION_NOT_FOUND", format!("扩展不存在: {id}"))
            }
            other => other.into(),
        })
    }

    pub fn list(&self) -> Result<Vec<Extension>, AppError> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, description, version, manifest_version, type, source_path, enabled, status, created_at, updated_at
             FROM extensions ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], Self::map_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// 启用/禁用（仅 User 扩展会走到这里；System 的拒绝在服务层）
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "UPDATE extensions SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, enabled as i64, chrono::Utc::now().timestamp_millis()],
        )?;
        if n == 0 {
            return Err(AppError::not_found("EXTENSION_NOT_FOUND", format!("扩展不存在: {id}")));
        }
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        let n = conn.execute("DELETE FROM extensions WHERE id = ?1", params![id])?;
        if n == 0 {
            return Err(AppError::not_found("EXTENSION_NOT_FOUND", format!("扩展不存在: {id}")));
        }
        Ok(())
    }

    fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Extension> {
        let type_str: String = row.get(5)?;
        let status_str: String = row.get(8)?;
        Ok(Extension {
            id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            version: row.get(3)?,
            manifest_version: row.get(4)?,
            extension_type: match type_str.as_str() {
                "system" => ExtensionType::System,
                _ => ExtensionType::User,
            },
            source_path: row.get(6)?,
            enabled: row.get::<_, i64>(7)? != 0,
            status: ExtensionStatus::from_str_or_ready(&status_str),
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
        })
    }
}
