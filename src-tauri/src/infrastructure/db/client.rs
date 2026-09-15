use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use std::path::Path;

use crate::error::AppError;

pub type DbPool = Pool<SqliteConnectionManager>;

/// 初始化连接池：WAL + 外键约束，多连接读写安全。
pub fn init_pool(db_file: &Path) -> Result<DbPool, AppError> {
    if let Some(parent) = db_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let manager = SqliteConnectionManager::file(db_file).with_init(|conn| {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
    });
    let pool = Pool::new(manager).map_err(|e| AppError::Pool(e.to_string()))?;
    Ok(pool)
}
