//! Schema 初始化：直接创建最终形态的表结构（全新产品，无增量迁移历史）。
//! CREATE IF NOT EXISTS 幂等；已有库（表结构一致）不受影响。
//! 旧构建遗留列（environments.host / auto_start）由 drop_legacy_columns 在启动时幂等清理。

use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;

use crate::error::AppError;

use super::client::DbPool;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS environments (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    hosts_source_url TEXT,
    icon             TEXT,
    startup_args     TEXT,
    keep_alive       INTEGER NOT NULL DEFAULT 0,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS login_profiles (
    id                TEXT PRIMARY KEY,
    environment_id    TEXT NOT NULL REFERENCES environments(id),
    name              TEXT NOT NULL,
    profile_dir       TEXT NOT NULL,
    snapshot_version  INTEGER NOT NULL DEFAULT 0,
    status            TEXT NOT NULL DEFAULT 'not_configured',
    last_captured_at  INTEGER,
    cdp_port          INTEGER,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_login_profiles_env ON login_profiles(environment_id);

CREATE TABLE IF NOT EXISTS instances (
    id               TEXT PRIMARY KEY,
    environment_id   TEXT NOT NULL REFERENCES environments(id),
    login_profile_id TEXT REFERENCES login_profiles(id),
    profile_dir      TEXT NOT NULL,
    pid              INTEGER,
    cdp_port         INTEGER,
    status           TEXT NOT NULL DEFAULT 'created',
    host_rules       TEXT,
    browser_version  TEXT,
    started_at       INTEGER,
    stopped_at       INTEGER,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_instances_env ON instances(environment_id);
CREATE INDEX IF NOT EXISTS idx_instances_status ON instances(status);

CREATE TABLE IF NOT EXISTS app_settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Extension 一级资源（Direct Mode：source_path 只存引用，不复制源码）
CREATE TABLE IF NOT EXISTS extensions (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    description      TEXT,
    version          TEXT,
    manifest_version INTEGER,
    type             TEXT NOT NULL,
    source_path      TEXT NOT NULL,
    enabled          INTEGER NOT NULL DEFAULT 1,
    status           TEXT NOT NULL,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS app_events (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    ts             INTEGER NOT NULL,
    level          TEXT NOT NULL,
    event          TEXT NOT NULL,
    environment_id TEXT,
    target_id      TEXT,
    message        TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_app_events_env ON app_events(environment_id, ts DESC);
"#;

/// 建库（幂等）。已有库表结构一致时为 no-op；历史遗留列随后清理。
pub fn init_schema(pool: &DbPool) -> Result<(), AppError> {
    let conn: PooledConnection<_> = pool.get()?;
    conn.execute_batch(SCHEMA)?;
    drop_legacy_columns(&conn)?;
    Ok(())
}

/// 迁移：删除 environments 表的历史遗留列（起始页已上收至全局设置 default_start_url）。
/// - `host`：环境专属起始页（v1 表单字段），功能由 Settings.defaultStartUrl 取代
/// - `auto_start`：旧构建遗留死列，现行代码零引用
/// 幂等：pragma_table_info 检查存在才 DROP（SQLite ≥3.35，rusqlite bundled 满足）；
/// 注意不支持降级——迁移后的库不能被旧版本二进制打开（INSERT 引用已删列会报错）。
fn drop_legacy_columns(conn: &PooledConnection<SqliteConnectionManager>) -> Result<(), AppError> {
    for column in ["host", "auto_start"] {
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('environments') WHERE name = ?1",
            params![column],
            |row| row.get(0),
        )?;
        if exists > 0 {
            conn.execute(&format!("ALTER TABLE environments DROP COLUMN {column}"), [])?;
            tracing::info!("[schema] 已删除 environments.{column} 列（历史遗留迁移）");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧构建的表结构（含 host / auto_start），用于迁移测试
    const LEGACY_SCHEMA: &str = r#"
        CREATE TABLE IF NOT EXISTS environments (
            id               TEXT PRIMARY KEY,
            name             TEXT NOT NULL,
            host             TEXT NOT NULL,
            hosts_source_url TEXT,
            icon             TEXT,
            startup_args     TEXT,
            auto_start       INTEGER NOT NULL DEFAULT 0,
            keep_alive       INTEGER NOT NULL DEFAULT 0,
            created_at       INTEGER NOT NULL,
            updated_at       INTEGER NOT NULL
        );
    "#;

    fn column_names(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    /// 迁移：旧库（含遗留列 + 数据）→ init_schema 后列被删除、其余数据完整保留。
    /// 表达迁移的核心价值：升级不丢环境。
    #[test]
    fn legacy_columns_dropped_and_data_preserved() {
        let dir = std::env::temp_dir().join(format!("cem-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::infrastructure::db::client::init_pool(&dir.join("test.db")).unwrap();
        let conn = pool.get().unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO environments (id, name, host, keep_alive, created_at, updated_at)
             VALUES ('env_old', '老环境', 'https://legacy.example.com', 1, 1, 1)",
            [],
        )
        .unwrap();
        drop(conn);

        init_schema(&pool).unwrap();

        let conn = pool.get().unwrap();
        let cols = column_names(&conn, "environments");
        assert!(!cols.contains(&"host".to_string()), "host 列已删除: {cols:?}");
        assert!(!cols.contains(&"auto_start".to_string()), "auto_start 列已删除: {cols:?}");
        let (name, keep_alive): (String, bool) = conn
            .query_row(
                "SELECT name, keep_alive FROM environments WHERE id = 'env_old'",
                [],
                |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0)),
            )
            .unwrap();
        assert_eq!(name, "老环境");
        assert!(keep_alive, "其余列数据完整");
    }

    /// 幂等：新库重复 init_schema 为 no-op（迁移对新库/已迁移库不动作）
    #[test]
    fn migration_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("cem-mig2-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::infrastructure::db::client::init_pool(&dir.join("test.db")).unwrap();
        init_schema(&pool).unwrap();
        init_schema(&pool).unwrap();
    }
}
