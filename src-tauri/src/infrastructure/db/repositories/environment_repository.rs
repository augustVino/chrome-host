use rusqlite::params;

use crate::domain::environment::Environment;
use crate::error::AppError;

use crate::infrastructure::db::client::DbPool;

pub struct EnvironmentRepository {
    pool: DbPool,
}

impl EnvironmentRepository {
    pub fn new(pool: DbPool) -> Self {
        EnvironmentRepository { pool }
    }

    pub fn insert(&self, env: &Environment) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO environments (id, name, hosts_source_url, icon, startup_args, keep_alive, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                env.id,
                env.name,
                env.hosts_source_url,
                env.icon,
                env.startup_args,
                env.keep_alive as i64,
                env.created_at,
                env.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Environment, AppError> {
        let conn = self.pool.get()?;
        let result = conn.query_row(
            "SELECT id, name, hosts_source_url, icon, startup_args, keep_alive, created_at, updated_at
             FROM environments WHERE id = ?1",
            params![id],
            Self::map_row,
        );
        result.map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                AppError::not_found("ENVIRONMENT_NOT_FOUND", format!("环境不存在: {id}"))
            }
            other => other.into(),
        })
    }

    pub fn list(&self) -> Result<Vec<Environment>, AppError> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, hosts_source_url, icon, startup_args, keep_alive, created_at, updated_at
             FROM environments ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], Self::map_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn delete(&self, id: &str) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM environments WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// 全行更新（PATCH 后由服务层组装完整实体）
    pub fn update(&self, env: &Environment) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE environments SET name = ?1, hosts_source_url = ?2, icon = ?3, startup_args = ?4, keep_alive = ?5, updated_at = ?6 WHERE id = ?7",
            params![
                env.name,
                env.hosts_source_url,
                env.icon,
                env.startup_args,
                env.keep_alive as i64,
                env.updated_at,
                env.id,
            ],
        )?;
        Ok(())
    }

    fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Environment> {
        Ok(Environment {
            id: row.get(0)?,
            name: row.get(1)?,
            hosts_source_url: row.get(2)?,
            icon: row.get(3)?,
            startup_args: row.get(4)?,
            keep_alive: row.get::<_, i64>(5)? != 0,
            created_at: row.get(6)?,
            updated_at: row.get(7)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::db::client;

    fn test_pool() -> DbPool {
        let dir = std::env::temp_dir().join(format!("cem-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = client::init_pool(&dir.join("test.db")).unwrap();
        crate::infrastructure::db::schema::init_schema(&pool).unwrap();
        pool
    }

    fn sample_env(id: &str) -> Environment {
        Environment {
            id: id.to_string(),
            name: "Staging".to_string(),
            hosts_source_url: None,
            icon: None,
            startup_args: None,
            keep_alive: false,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn insert_get_delete_roundtrip() {
        let pool = test_pool();
        let repo = EnvironmentRepository::new(pool);
        repo.insert(&sample_env("env_a")).unwrap();

        let fetched = repo.get("env_a").unwrap();
        assert_eq!(fetched.name, "Staging");

        repo.delete("env_a").unwrap();
        assert!(repo.get("env_a").is_err_and(|e| e.code() == "ENVIRONMENT_NOT_FOUND"));
    }

    #[test]
    fn get_missing_returns_not_found_code() {
        let pool = test_pool();
        let repo = EnvironmentRepository::new(pool);
        let err = repo.get("nope").unwrap_err();
        assert_eq!(err.code(), "ENVIRONMENT_NOT_FOUND");
    }
}
