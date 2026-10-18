use rusqlite::params;
use std::collections::BTreeSet;

use crate::domain::login_profile::{LoginProfile, LoginProfileStatus};
use crate::error::AppError;

use crate::infrastructure::db::client::DbPool;

/// login_profiles 仓储。
/// 状态迁移一律专用方法而非"动态 patch"：与 InstanceRepository.mark_starting 同理，
/// 显式表达每一步迁移写的字段集合，杜绝 Option 缺省语义歧义。
pub struct LoginProfileRepository {
    pool: DbPool,
}

impl LoginProfileRepository {
    pub fn new(pool: DbPool) -> Self {
        LoginProfileRepository { pool }
    }

    pub fn find_by_env(&self, env_id: &str) -> Result<Option<LoginProfile>, AppError> {
        let conn = self.pool.get()?;
        let result = conn.query_row(
            &format!("{} WHERE environment_id = ?1", Self::SELECT),
            params![env_id],
            Self::map_row,
        );
        match result {
            Ok(p) => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// 惰性创建：首次访问 login-profile API/UI 时建行。
    /// INSERT OR IGNORE + SELECT：并发首次访问幂等（environment_id 唯一索引兜底）。
    pub fn get_or_create(
        &self,
        env_id: &str,
        name: &str,
        profile_dir: &str,
    ) -> Result<LoginProfile, AppError> {
        {
            let conn = self.pool.get()?;
            let now = chrono::Utc::now().timestamp_millis();
            conn.execute(
                "INSERT OR IGNORE INTO login_profiles (id, environment_id, name, profile_dir, snapshot_version, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 0, 'not_configured', ?5, ?5)",
                params![format!("lp_{}", uuid::Uuid::new_v4()), env_id, name, profile_dir, now],
            )?;
        }
        // 忽略并发下他人插入的行，统一回读
        Ok(self.find_by_env(env_id)?.expect("get_or_create 后行必存在"))
    }

    pub fn list_all(&self) -> Result<Vec<LoginProfile>, AppError> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(&format!("{} ORDER BY created_at ASC", Self::SELECT))?;
        let rows = stmt.query_map([], Self::map_row)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }



    pub fn mark_capturing(&self, id: &str) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE login_profiles SET status = 'capturing', updated_at = ?1 WHERE id = ?2",
            params![chrono::Utc::now().timestamp_millis(), id],
        )?;
        Ok(())
    }

    /// 固化完成：status=ready + version 前进 + 记录捕获时间。
    pub fn mark_ready(&self, id: &str, snapshot_version: i64) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE login_profiles SET status = 'ready', snapshot_version = ?1, last_captured_at = ?2, updated_at = ?2 WHERE id = ?3",
            params![snapshot_version, chrono::Utc::now().timestamp_millis(), id],
        )?;
        Ok(())
    }

    /// 重置：回到 not_configured，快照版本归零、清捕获时间（语义级"清空"，专用 SQL）。
    pub fn mark_reset(&self, id: &str) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE login_profiles SET status = 'not_configured', snapshot_version = 0, last_captured_at = NULL, updated_at = ?1 WHERE id = ?2",
            params![chrono::Utc::now().timestamp_millis(), id],
        )?;
        Ok(())
    }

    pub fn mark_error(&self, id: &str) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE login_profiles SET status = 'error', updated_at = ?1 WHERE id = ?2",
            params![chrono::Utc::now().timestamp_millis(), id],
        )?;
        Ok(())
    }

    /// 环境级联删除用。调用方保证登录浏览器已处置。
    pub fn delete_by_env(&self, env_id: &str) -> Result<(), AppError> {
        let conn = self.pool.get()?;
        conn.execute("DELETE FROM login_profiles WHERE environment_id = ?1", params![env_id])?;
        Ok(())
    }

    /// 使用该快照的实例数。
    /// 注意是"克隆自该快照"的累计口径，与实例是否运行无关。
    pub fn count_instances_using(&self, id: &str) -> Result<i64, AppError> {
        let conn = self.pool.get()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM instances WHERE login_profile_id = ?1",
            params![id],
            |row: &rusqlite::Row| row.get(0),
        )?;
        Ok(count)
    }


    const SELECT: &'static str =
        "SELECT id, environment_id, name, profile_dir, snapshot_version, status, last_captured_at, created_at, updated_at
         FROM login_profiles";

    fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LoginProfile> {
        Ok(LoginProfile {
            id: row.get(0)?,
            environment_id: row.get(1)?,
            name: row.get(2)?,
            profile_dir: row.get(3)?,
            snapshot_version: row.get(4)?,
            status: LoginProfileStatus::from_db(&row.get::<_, String>(5)?),
            last_captured_at: row.get(6)?,
            created_at: row.get(7)?,
            updated_at: row.get(8)?,
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

    fn setup() -> (DbPool, EnvironmentRepository, LoginProfileRepository) {
        let dir = std::env::temp_dir().join(format!("cem-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = client::init_pool(&dir.join("test.db")).unwrap();
        schema::init_schema(&pool).unwrap();
        (
            pool.clone(),
            EnvironmentRepository::new(pool.clone()),
            LoginProfileRepository::new(pool),
        )
    }

    fn insert_env(envs: &EnvironmentRepository, id: &str) {
        envs.insert(&Environment {
            id: id.into(),
            name: "A".into(),
            hosts_source_url: None,
            icon: None,
            startup_args: None,
            keep_alive: false,
            session_capture: Default::default(),
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
    }

    /// 核心语义：惰性创建幂等 + 默认 not_configured/v0（Ready 前创建实例 → empty）
    #[test]
    fn get_or_create_is_idempotent_with_defaults() {
        let (_pool, envs, profiles) = setup();
        insert_env(&envs, "env_a");

        let first = profiles.get_or_create("env_a", "A Login", "/tmp/lp").unwrap();
        let second = profiles.get_or_create("env_a", "A Login", "/tmp/lp").unwrap();

        assert_eq!(first.id, second.id, "重复 get_or_create 不得新建行");
        assert_eq!(first.status, LoginProfileStatus::NotConfigured);
        assert_eq!(first.snapshot_version, 0);
        assert_eq!(first.last_captured_at, None);
    }

    /// 状态机全流转：not_configured → ready → capturing → ready；reset 归零
    #[test]
    fn status_transitions_full_cycle() {
        let (_pool, envs, profiles) = setup();
        insert_env(&envs, "env_a");
        let lp = profiles.get_or_create("env_a", "A Login", "/tmp/lp").unwrap();

        profiles.mark_capturing(&lp.id).unwrap();
        assert_eq!(
            profiles.find_by_env("env_a").unwrap().unwrap().status,
            LoginProfileStatus::Capturing
        );

        profiles.mark_ready(&lp.id, 1).unwrap();
        let ready = profiles.find_by_env("env_a").unwrap().unwrap();
        assert_eq!(ready.status, LoginProfileStatus::Ready);
        assert_eq!(ready.snapshot_version, 1);
        assert!(ready.last_captured_at.is_some());

        // reset：版本归零 + 捕获时间清空 + 回 not_configured
        profiles.mark_reset(&lp.id).unwrap();
        let reset = profiles.find_by_env("env_a").unwrap().unwrap();
        assert_eq!(reset.status, LoginProfileStatus::NotConfigured);
        assert_eq!(reset.snapshot_version, 0);
        assert_eq!(reset.last_captured_at, None);

        // error 态
        profiles.mark_error(&lp.id).unwrap();
        assert_eq!(
            profiles.find_by_env("env_a").unwrap().unwrap().status,
            LoginProfileStatus::Error
        );
    }

    /// instancesUsing 计数：计数口径 = 克隆自该快照的实例累计数
    /// （0.3.0 起登录浏览器端口登记随 launch 子系统移除）
    #[test]
    fn instances_using_counts_clones() {
        let (pool, envs, profiles) = setup();
        insert_env(&envs, "env_a");
        let lp = profiles.get_or_create("env_a", "A Login", "/tmp/lp").unwrap();

        assert_eq!(profiles.count_instances_using(&lp.id).unwrap(), 0);

        let instances =
            crate::infrastructure::db::repositories::instance_repository::InstanceRepository::new(pool);
        instances
            .insert(&crate::domain::instance::Instance {
                id: "ins_1".into(),
                environment_id: "env_a".into(),
                login_profile_id: Some(lp.id.clone()),
                profile_dir: "/tmp/ins_1".into(),
                pid: None,
                cdp_port: Some(9400),
                status: crate::domain::instance::InstanceStatus::Stopped,
                host_rules: None,
                browser_version: None,
                started_at: None,
                stopped_at: None,
                created_at: 1,
                updated_at: 1,
            })
            .unwrap();
        assert_eq!(profiles.count_instances_using(&lp.id).unwrap(), 1);
    }
}
