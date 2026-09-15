use rusqlite::params;
use std::collections::BTreeSet;

use crate::domain::instance::{Instance, InstanceStatus, RuntimeUpdate};
use crate::error::AppError;

use crate::infrastructure::db::client::DbPool;

pub struct InstanceRepository {
    pool: DbPool,
}

impl InstanceRepository {
    pub fn new(pool: DbPool) -> Self {
        InstanceRepository { pool }
    }

    /// 只读连接池访问（keepAlive 扫描等 JOIN 查询使用）
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    pub fn insert(&self, ins: &Instance) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO instances (id, environment_id, login_profile_id, profile_dir, pid, cdp_port, status, host_rules, browser_version, started_at, stopped_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                ins.id,
                ins.environment_id,
                ins.login_profile_id,
                ins.profile_dir,
                ins.pid,
                ins.cdp_port,
                ins.status.as_str(),
                ins.host_rules,
                ins.browser_version,
                ins.started_at,
                ins.stopped_at,
                ins.created_at,
                ins.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Instance, AppError> {
        let conn = self.pool.get()?;
        let result = conn.query_row(
            &format!("{} WHERE id = ?1", Self::SELECT),
            params![id],
            Self::map_row,
        );
        result.map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                AppError::not_found("INSTANCE_NOT_FOUND", format!("实例不存在: {id}"))
            }
            other => other.into(),
        })
    }

    pub fn list_by_env(&self, env_id: &str) -> Result<Vec<Instance>, AppError> {
        let conn = self.pool.get()?;
        let mut stmt =
            conn.prepare(&format!("{} WHERE environment_id = ?1 ORDER BY created_at ASC", Self::SELECT))?;
        let rows = stmt.query_map(params![env_id], Self::map_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    pub fn list_all(&self) -> Result<Vec<Instance>, AppError> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!("{} ORDER BY created_at ASC", Self::SELECT))?;
        let rows = stmt.query_map([], Self::map_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// 动态更新运行时字段（仅写入提供的字段），同时刷新 updated_at。
    /// 全部使用匿名占位符，避免与 sqlite 参数索引机制混淆。
    pub fn update_runtime(&self, id: &str, patch: RuntimeUpdate) -> Result<(), AppError> {
        let mut sets: Vec<&'static str> = vec!["updated_at = ?"];
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(chrono::Utc::now().timestamp_millis())];

        if let Some(pid) = patch.pid {
            sets.push("pid = ?");
            values.push(Box::new(pid as i64));
        }
        if let Some(status) = patch.status {
            sets.push("status = ?");
            values.push(Box::new(status.as_str().to_string()));
        }
        if let Some(ts) = patch.started_at {
            sets.push("started_at = ?");
            values.push(Box::new(ts));
        }
        if let Some(ts) = patch.stopped_at {
            sets.push("stopped_at = ?");
            values.push(Box::new(ts));
        }
        if let Some(v) = patch.browser_version {
            sets.push("browser_version = ?");
            values.push(Box::new(v));
        }
        if let Some(v) = patch.host_rules {
            sets.push("host_rules = ?");
            values.push(Box::new(v));
        }

        let sql = format!("UPDATE instances SET {} WHERE id = ?", sets.join(", "));
        values.push(Box::new(id.to_string()));

        let conn = self.pool.get()?;
        let params_ref: Vec<&dyn rusqlite::types::ToSql> =
            values.iter().map(|v| v.as_ref()).collect();
        conn.execute(&sql, params_ref.as_slice())?;
        Ok(())
    }

    /// 标记启动中并显式清空上一轮 pid。
    /// 专用方法的原因：update_runtime 的 RuntimeUpdate.Option 语义是"None = 不更新"，
    /// 无法表达"pid 置空"；而 start 必须清掉旧 pid，否则对账会看到
    /// "starting + 旧 pid 已死"误判为启动失败（状态闪"异常"的根因）。
    pub fn mark_starting(&self, id: &str) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE instances SET pid = NULL, status = 'starting', updated_at = ?1 WHERE id = ?2",
            params![chrono::Utc::now().timestamp_millis(), id],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM instances WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn delete_by_env(&self, env_id: &str) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM instances WHERE environment_id = ?1", params![env_id])?;
        Ok(())
    }

    /// 所有已登记实例占用的 CDP 端口（含已停止实例——端口保留）
    pub fn used_ports(&self) -> Result<BTreeSet<u16>, AppError> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT cdp_port FROM instances WHERE cdp_port IS NOT NULL")?;
        let rows = stmt.query_map([], |row: &rusqlite::Row| row.get::<_, i64>(0))?;
        let mut ports = BTreeSet::new();
        for row in rows {
            ports.insert(row? as u16);
        }
        Ok(ports)
    }

    pub fn count_running_by_env(&self, env_id: &str) -> Result<i64, AppError> {
        let conn = self.pool.get()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM instances WHERE environment_id = ?1 AND status IN ('running', 'starting')",
            params![env_id],
            |row: &rusqlite::Row| row.get(0),
        )?;
        Ok(count)
    }

    const SELECT: &'static str =
        "SELECT id, environment_id, login_profile_id, profile_dir, pid, cdp_port, status, host_rules, browser_version, started_at, stopped_at, created_at, updated_at
         FROM instances";

    fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Instance> {
        let pid: Option<i64> = row.get(4)?;
        let cdp_port: Option<i64> = row.get(5)?;
        Ok(Instance {
            id: row.get(0)?,
            environment_id: row.get(1)?,
            login_profile_id: row.get(2)?,
            profile_dir: row.get(3)?,
            pid: pid.map(|p| p as u32),
            cdp_port: cdp_port.map(|p| p as u16),
            status: InstanceStatus::from_db(&row.get::<_, String>(6)?),
            host_rules: row.get(7)?,
            browser_version: row.get(8)?,
            started_at: row.get(9)?,
            stopped_at: row.get(10)?,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::environment::Environment;
    use crate::infrastructure::db::client;
    use crate::infrastructure::db::schema;
    use crate::infrastructure::db::repositories::environment_repository::EnvironmentRepository;

    fn setup() -> (DbPool, EnvironmentRepository, InstanceRepository) {
        let dir = std::env::temp_dir().join(format!("cem-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = client::init_pool(&dir.join("test.db")).unwrap();
        schema::init_schema(&pool).unwrap();
        let envs = EnvironmentRepository::new(pool.clone());
        let instances = InstanceRepository::new(pool.clone());
        (pool, envs, instances)
    }

    fn sample_instance(env_id: &str, id: &str, port: u16) -> Instance {
        Instance {
            id: id.to_string(),
            environment_id: env_id.to_string(),
            login_profile_id: None,
            profile_dir: format!("/tmp/instances/{id}"),
            pid: None,
            cdp_port: Some(port),
            status: InstanceStatus::Starting,
            host_rules: None,
            browser_version: None,
            started_at: None,
            stopped_at: None,
            created_at: 100,
            updated_at: 100,
        }
    }

    #[test]
    fn insert_update_runtime_roundtrip() {
        let (_pool, envs, instances) = setup();
        envs.insert(&Environment {
            id: "env_a".into(),
            name: "A".into(),
            hosts_source_url: None,
            icon: None,
            startup_args: None,
            keep_alive: false,
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
        instances.insert(&sample_instance("env_a", "ins_1", 9222)).unwrap();

        // 模拟启动成功
        instances
            .update_runtime(
                "ins_1",
                RuntimeUpdate {
                    pid: Some(4321),
                    status: Some(InstanceStatus::Running),
                    started_at: Some(200),
                    browser_version: Some("131.0.6778.204".into()),
                    ..Default::default()
                },
            )
            .unwrap();

        let got = instances.get("ins_1").unwrap();
        assert_eq!(got.status, InstanceStatus::Running);
        assert_eq!(got.pid, Some(4321));
        assert_eq!(got.browser_version.as_deref(), Some("131.0.6778.204"));
    }

    #[test]
    fn used_ports_includes_stopped_and_counts_running() {
        let (_pool, envs, instances) = setup();
        envs.insert(&Environment {
            id: "env_a".into(),
            name: "A".into(),
            hosts_source_url: None,
            icon: None,
            startup_args: None,
            keep_alive: false,
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();

        instances.insert(&sample_instance("env_a", "ins_1", 9222)).unwrap();
        let mut stopped = sample_instance("env_a", "ins_2", 9223);
        stopped.status = InstanceStatus::Stopped;
        instances.insert(&stopped).unwrap();

        let ports = instances.used_ports().unwrap();
        assert!(ports.contains(&9222) && ports.contains(&9223), "已停止实例端口仍保留");

        instances
            .update_runtime("ins_1", RuntimeUpdate { status: Some(InstanceStatus::Running), ..Default::default() })
            .unwrap();
        assert_eq!(instances.count_running_by_env("env_a").unwrap(), 1);
    }
}
