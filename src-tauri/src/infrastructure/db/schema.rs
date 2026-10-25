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
    updated_at       INTEGER NOT NULL,
    session_capture  TEXT NOT NULL DEFAULT 'first-login'
);

CREATE TABLE IF NOT EXISTS login_profiles (
    id                TEXT PRIMARY KEY,
    environment_id    TEXT NOT NULL REFERENCES environments(id),
    name              TEXT NOT NULL,
    profile_dir       TEXT NOT NULL,
    snapshot_version  INTEGER NOT NULL DEFAULT 0,
    status            TEXT NOT NULL DEFAULT 'not_configured',
    last_captured_at  INTEGER,
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
    updated_at       INTEGER NOT NULL,
    name             TEXT
);

-- 注意：instances 的同环境 name 唯一索引（partial，NULL 不参与）不在本 batch 内
-- 创建——旧库执行到此处时 name 列尚不存在（列由 ensure_instances_name_column
-- ALTER 追加），索引统一在该函数内建，新旧库路径收敛。

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

-- Extension 运行时配置（per-env 覆盖全局默认；env_id='' 为全局行。
-- 不建 FK：全局行 env_id 非真实环境；环境删除由 EnvironmentService 编排清理）
CREATE TABLE IF NOT EXISTS extension_configs (
    env_id       TEXT NOT NULL,
    extension_id TEXT NOT NULL,
    config_json  TEXT NOT NULL,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (env_id, extension_id)
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
    drop_login_profile_cdp_port(&conn)?;
    ensure_session_capture_column(&conn)?;
    ensure_instances_name(&conn)?;
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

/// 迁移：删除 login_profiles.cdp_port（0.3.0 移除登录浏览器 launch 子系统后，
/// 端口凭证不再存在）。与 drop_legacy_columns 同款 pragma 检查 + 幂等。
fn drop_login_profile_cdp_port(conn: &PooledConnection<SqliteConnectionManager>) -> Result<(), AppError> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('login_profiles') WHERE name = 'cdp_port'",
        [],
        |row| row.get(0),
    )?;
    if exists > 0 {
        conn.execute("ALTER TABLE login_profiles DROP COLUMN cdp_port", [])?;
        tracing::info!("[schema] 已删除 login_profiles.cdp_port 列（launch 子系统移除迁移）");
    }
    Ok(())
}

/// 迁移：为旧库的 instances 补 name 列（实例可选名，同环境唯一）。
/// 与 ensure_session_capture_column 同款 pragma 检查 + 幂等；列固定加在末尾，
/// 保证新库（CREATE 内联）与旧库（ALTER 追加）的列序一致。
/// 唯一索引随列远迁在本函数内统一创建（不在 SCHEMA batch 内，原因见
/// instances 表旁注释）：partial unique index，NULL name 不参与约束。
fn ensure_instances_name(conn: &PooledConnection<SqliteConnectionManager>) -> Result<(), AppError> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('instances') WHERE name = 'name'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        conn.execute("ALTER TABLE instances ADD COLUMN name TEXT", [])?;
        tracing::info!("[schema] 已为 instances 增加 name 列（可空，同环境唯一）");
    }
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_instances_env_name \
         ON instances(environment_id, name) WHERE name IS NOT NULL",
    )?;
    Ok(())
}

/// 迁移：为旧库的 environments 补 session_capture 列（身份层回写策略）。
/// 与 drop_legacy_columns 同款 pragma 检查 + 幂等；列固定加在末尾，
/// 保证新库（CREATE 内联）与旧库（ALTER 追加）的 SELECT * 列序一致。
fn ensure_session_capture_column(conn: &PooledConnection<SqliteConnectionManager>) -> Result<(), AppError> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('environments') WHERE name = 'session_capture'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        conn.execute(
            "ALTER TABLE environments ADD COLUMN session_capture TEXT NOT NULL DEFAULT 'first-login'",
            [],
        )?;
        tracing::info!("[schema] 已为 environments 增加 session_capture 列（默认 first-login）");
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

    /// 旧构建的 instances 表结构（无 name 列），用于 name 迁移测试
    const LEGACY_INSTANCES_SCHEMA: &str = r#"
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
            created_at        INTEGER NOT NULL,
            updated_at        INTEGER NOT NULL
        );
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
        assert!(cols.contains(&"session_capture".to_string()), "旧库自动补 session_capture 列: {cols:?}");
        let (name, keep_alive): (String, bool) = conn
            .query_row(
                "SELECT name, keep_alive FROM environments WHERE id = 'env_old'",
                [],
                |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0)),
            )
            .unwrap();
        assert_eq!(name, "老环境");
        assert!(keep_alive, "其余列数据完整");
        let sc: String = conn
            .query_row(
                "SELECT session_capture FROM environments WHERE id = 'env_old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sc, "first-login", "存量行自动取默认档");
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

    /// 迁移：旧库 instances（无 name 列 + 数据）→ init_schema 后 name 列补上、
    /// 存量行 name 为 NULL、数据完整，且同环境唯一索引立即生效。
    /// 表达迁移核心价值：升级不丢实例，唯一约束在旧库上立即兜底。
    #[test]
    fn legacy_instances_get_name_column_and_unique_index() {
        let dir = std::env::temp_dir().join(format!("cem-mig3-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::infrastructure::db::client::init_pool(&dir.join("test.db")).unwrap();
        let conn = pool.get().unwrap();
        conn.execute_batch(LEGACY_INSTANCES_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO environments (id, name, created_at, updated_at) VALUES ('env_a', 'A', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO instances (id, environment_id, profile_dir, status, created_at, updated_at)
             VALUES ('ins_old', 'env_a', '/tmp/ins_old', 'stopped', 1, 1)",
            [],
        )
        .unwrap();
        drop(conn);

        init_schema(&pool).unwrap();

        let conn = pool.get().unwrap();
        let cols = column_names(&conn, "instances");
        assert!(cols.contains(&"name".to_string()), "旧库自动补 name 列: {cols:?}");
        let (status, name): (String, Option<String>) = conn
            .query_row(
                "SELECT status, name FROM instances WHERE id = 'ins_old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "stopped", "存量行数据完整");
        assert_eq!(name, None, "存量行 name 为 NULL（不参与唯一约束）");

        // 唯一索引语义：同环境同名拒绝、同环境多个 NULL 允许、跨环境同名允许
        let ins = |id: &str, env: &str, name: Option<&str>| {
            format!(
                "INSERT INTO instances (id, environment_id, profile_dir, status, created_at, updated_at, name)
                 VALUES ('{id}', '{env}', '/tmp/{id}', 'created', 1, 1, {})",
                name.map(|n| format!("'{n}'")).unwrap_or_else(|| "NULL".into())
            )
        };
        assert!(conn.execute(&ins("ins_n1", "env_a", Some("task-1")), []).is_ok());
        assert!(
            conn.execute(&ins("ins_n2", "env_a", Some("task-1")), []).is_err(),
            "同环境同名 → 唯一索引拒绝"
        );
        assert!(
            conn.execute(&ins("ins_n3", "env_a", None), []).is_ok(),
            "同环境多个 NULL name 允许"
        );
        // 跨环境同名：需第二个环境行（FK）
        conn.execute(
            "INSERT INTO environments (id, name, created_at, updated_at) VALUES ('env_b', 'B', 1, 1)",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(&ins("ins_n4", "env_b", Some("task-1")), []).is_ok(),
            "跨环境同名允许（唯一性是环境级的）"
        );
    }
}
