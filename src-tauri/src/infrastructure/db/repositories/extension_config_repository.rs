//! Extension 运行时配置仓储：extension_configs 表 CRUD。
//! 仅数据访问，不做业务校验（可配置性判断在服务层）；单语句 SQL、无显式事务，
//! 与 extension_repository 同风格。`env_id = ''` 表示全局默认行（被 per-env 行覆盖）。

use std::collections::HashMap;

use rusqlite::params;

use crate::error::AppError;

use crate::infrastructure::db::client::DbPool;

/// 全局默认行的 env_id 哨兵值（空串排序在任何真实 env id 之前，
/// 依赖 resolve_map_for_env 的 ORDER BY 覆盖语义）
pub const GLOBAL_ENV_ID: &str = "";

pub struct ExtensionConfigRepository {
    pool: DbPool,
}

impl ExtensionConfigRepository {
    pub fn new(pool: DbPool) -> Self {
        ExtensionConfigRepository { pool }
    }

    pub fn get(&self, env_id: &str, extension_id: &str) -> Result<Option<String>, AppError> {
        let conn = self.pool.get()?;
        let result = conn.query_row(
            "SELECT config_json FROM extension_configs WHERE env_id = ?1 AND extension_id = ?2",
            params![env_id, extension_id],
            |row| row.get(0),
        );
        match result {
            Ok(json) => Ok(Some(json)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// UPSERT：同 (env_id, extension_id) 整体覆盖（配置无增量语义）
    pub fn put(&self, env_id: &str, extension_id: &str, config_json: &str) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO extension_configs (env_id, extension_id, config_json, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(env_id, extension_id) DO UPDATE SET
                config_json = excluded.config_json, updated_at = excluded.updated_at",
            params![env_id, extension_id, config_json, chrono::Utc::now().timestamp_millis()],
        )?;
        Ok(())
    }

    /// 环境删除编排的级联清理（全局行 env_id='' 不受影响）
    pub fn delete_for_env(&self, env_id: &str) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM extension_configs WHERE env_id = ?1", params![env_id])?;
        Ok(())
    }

    /// 实例启动装配视图：extension_id → 生效配置 JSON。
    /// 一条 SQL 取全局 + per-env 两层，ORDER BY env_id ASC 保证空串全局行先进 map、
    /// 真实 env 行后进覆盖（per-env 覆盖全局默认）。
    pub fn resolve_map_for_env(&self, env_id: &str) -> Result<HashMap<String, String>, AppError> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT extension_id, config_json FROM extension_configs
             WHERE env_id = ?1 OR env_id = ?2 ORDER BY env_id ASC",
        )?;
        let rows = stmt.query_map(params![GLOBAL_ENV_ID, env_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (extension_id, config_json) = row?;
            map.insert(extension_id, config_json);
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::db::client::init_pool;
    use crate::infrastructure::db::schema::init_schema;

    fn setup() -> ExtensionConfigRepository {
        let dir = std::env::temp_dir().join(format!("cem-extcfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = init_pool(&dir.join("test.db")).unwrap();
        init_schema(&pool).unwrap();
        ExtensionConfigRepository::new(pool)
    }

    /// 覆盖语义是 resolve 的核心价值：per-env 行覆盖全局行，未配置的环境回落全局
    #[test]
    fn per_env_overrides_global_in_resolve_map() {
        let repo = setup();
        repo.put(GLOBAL_ENV_ID, "ext_1", r#"{"block":["a.com"]}"#).unwrap();
        repo.put("env_a", "ext_1", r#"{"block":["b.com"]}"#);
        repo.put("env_a", "ext_2", r#"{"block":["c.com"]}"#).unwrap();

        let env_a = repo.resolve_map_for_env("env_a").unwrap();
        assert_eq!(env_a.get("ext_1").unwrap(), r#"{"block":["b.com"]}"#, "per-env 覆盖全局");
        assert_eq!(env_a.get("ext_2").unwrap(), r#"{"block":["c.com"]}"#);

        let env_b = repo.resolve_map_for_env("env_b").unwrap();
        assert_eq!(env_b.get("ext_1").unwrap(), r#"{"block":["a.com"]}"#, "未配置环境回落全局");
        assert!(!env_b.contains_key("ext_2"), "其他 env 的行不泄漏");
    }

    /// get / put(upsert 覆盖) / delete_for_env 基础面
    #[test]
    fn get_put_upsert_and_env_cleanup() {
        let repo = setup();
        assert_eq!(repo.get("env_a", "ext_1").unwrap(), None, "未写入 → None");

        repo.put("env_a", "ext_1", r#"{"v":1}"#).unwrap();
        assert_eq!(repo.get("env_a", "ext_1").unwrap(), Some(r#"{"v":1}"#.into()));
        repo.put("env_a", "ext_1", r#"{"v":2}"#).unwrap();
        assert_eq!(repo.get("env_a", "ext_1").unwrap(), Some(r#"{"v":2}"#.into()), "同键 UPSERT 覆盖");

        repo.put("env_b", "ext_1", r#"{"v":3}"#).unwrap();
        repo.put(GLOBAL_ENV_ID, "ext_1", r#"{"v":0}"#).unwrap();
        repo.delete_for_env("env_a").unwrap();
        assert_eq!(repo.get("env_a", "ext_1").unwrap(), None, "环境删除清理该 env 行");
        assert!(repo.get("env_b", "ext_1").unwrap().is_some(), "其他 env 行保留");
        assert!(repo.get(GLOBAL_ENV_ID, "ext_1").unwrap().is_some(), "全局行保留");
    }
}
