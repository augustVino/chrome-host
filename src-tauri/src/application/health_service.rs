//! 健康检查服务：聚合数据库、内核、目录、扩展四类检查，产出单份快照。
//!
//! 设计要点：
//! - 健康检查语义属于应用服务（聚合多仓储 + 路径探测），不放 api 薄壳层
//! - 服务层不依赖 Tauri：app_version 由 main.rs 从 package_info 传入；
//!   counts 所需仓储以 Arc 直传构造器，不反向依赖其他服务
//! - snapshot 各项检查独立执行、不短路：doctor 场景要一次给出全部问题
//! - 目录可写用探测法（创建+删除临时文件），不假设具体权限模型

use std::sync::Arc;

use serde::Serialize;

use crate::domain::extension::ExtensionStatus;
use crate::infrastructure::db::client::DbPool;
use crate::infrastructure::db::repositories::environment_repository::EnvironmentRepository;
use crate::infrastructure::db::repositories::extension_repository::ExtensionRepository;
use crate::infrastructure::db::repositories::instance_repository::InstanceRepository;
use crate::infrastructure::kernel::KernelManager;
use crate::infrastructure::paths::AppPaths;

/// 单项检查结果
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub suggestion: Option<String>,
}

/// 资源计数（CLI status 摘要行用）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub environments: u64,
    pub instances: u64,
    pub running: u64,
    pub extensions: u64,
}

/// 健康报告（GET /api/v1/health 响应体，CLI status/doctor 共用数据源）
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthReport {
    /// chrome-host 应用版本
    pub version: String,
    /// 全部检查通过
    pub ok: bool,
    pub checks: Vec<HealthCheck>,
    pub counts: Counts,
}

pub struct HealthService {
    pool: DbPool,
    kernel: Arc<KernelManager>,
    paths: AppPaths,
    env_repo: Arc<EnvironmentRepository>,
    ins_repo: Arc<InstanceRepository>,
    extensions: Arc<ExtensionRepository>,
    app_version: String,
}

impl HealthService {
    pub fn new(
        pool: DbPool,
        kernel: Arc<KernelManager>,
        paths: AppPaths,
        env_repo: Arc<EnvironmentRepository>,
        ins_repo: Arc<InstanceRepository>,
        extensions: Arc<ExtensionRepository>,
        app_version: String,
    ) -> Self {
        HealthService { pool, kernel, paths, env_repo, ins_repo, extensions, app_version }
    }

    /// 产出健康快照。各项检查独立执行、不短路（doctor 要一次给出全部问题）
    pub fn snapshot(&self) -> HealthReport {
        let checks = vec![
            self.check_database(),
            self.check_kernel(),
            self.check_directories(),
            self.check_extensions(),
        ];
        let ok = checks.iter().all(|c| c.ok);
        HealthReport { version: self.app_version.clone(), ok, checks, counts: self.counts() }
    }

    /// 数据库：连接池可用 + 完整性校验（经 r2d2 连接执行 PRAGMA integrity_check）
    fn check_database(&self) -> HealthCheck {
        let failed = |detail: String| HealthCheck {
            name: "database".into(),
            ok: false,
            detail,
            suggestion: Some("数据库文件可能损坏，请备份应用数据目录后重装".into()),
        };
        match self.pool.get() {
            Ok(conn) => {
                match conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)) {
                    Ok(result) if result == "ok" => HealthCheck {
                        name: "database".into(),
                        ok: true,
                        detail: "SQLite 完整性校验通过（integrity_check: ok）".into(),
                        suggestion: None,
                    },
                    Ok(result) => failed(format!("SQLite 完整性校验异常: {result}")),
                    Err(e) => failed(format!("PRAGMA integrity_check 执行失败: {e}")),
                }
            }
            Err(e) => failed(format!("连接池获取连接失败: {e}")),
        }
    }

    /// 内核：pinned 版本已装且二进制存在 → ok；未装但下载中 → ok=true（正在自动恢复）；
    /// 其余 → ok=false 并给出安装指引
    fn check_kernel(&self) -> HealthCheck {
        let status = self.kernel.status();
        if status.installed {
            HealthCheck {
                name: "kernel".into(),
                ok: true,
                detail: format!(
                    "Chrome for Testing {} 就绪: {}",
                    status.pinned_version,
                    status.binary_path.unwrap_or_default()
                ),
                suggestion: None,
            }
        } else if status.downloading {
            HealthCheck {
                name: "kernel".into(),
                ok: true,
                detail: format!("Chrome for Testing {} 未安装，正在下载中", status.pinned_version),
                suggestion: None,
            }
        } else {
            HealthCheck {
                name: "kernel".into(),
                ok: false,
                detail: format!("Chrome for Testing {} 未安装", status.pinned_version),
                suggestion: Some(
                    "运行 chrome-host runtime install 或在 GUI 设置页下载 Chrome for Testing".into(),
                ),
            }
        }
    }

    /// 目录：root / kernel_root / environments_root 存在且可写。
    /// 可写用探测法（创建+删除临时文件），不假设权限模型
    fn check_directories(&self) -> HealthCheck {
        let targets = [
            ("root", self.paths.root.clone()),
            ("kernel", self.paths.kernel_root()),
            ("environments", self.paths.environments_root()),
        ];
        let mut ok = true;
        let mut problems: Vec<String> = Vec::new();
        for (name, path) in &targets {
            if !path.is_dir() {
                ok = false;
                problems.push(format!("{name} 目录不存在: {}", path.display()));
                continue;
            }
            let probe = path.join(format!(".health-probe-{}", std::process::id()));
            match std::fs::write(&probe, b"") {
                Ok(()) => {
                    let _ = std::fs::remove_file(&probe);
                }
                Err(e) => {
                    ok = false;
                    problems.push(format!("{name} 目录不可写: {} ({e})", path.display()));
                }
            }
        }
        if ok {
            HealthCheck {
                name: "directories".into(),
                ok: true,
                detail: format!("{} 个数据目录均存在且可写", targets.len()),
                suggestion: None,
            }
        } else {
            HealthCheck {
                name: "directories".into(),
                ok: false,
                detail: problems.join("; "),
                suggestion: Some("首次启动会自动创建所需目录；若持续出现请检查磁盘权限".into()),
            }
        }
    }

    /// 扩展：注册行计数 + 非 ready 状态行计数。
    /// status 为注册表存储值（惰性刷新属 ExtensionService 职责），此处只读注册表
    fn check_extensions(&self) -> HealthCheck {
        match self.extensions.list() {
            Ok(rows) => {
                let not_ready =
                    rows.iter().filter(|e| e.status != ExtensionStatus::Ready).count();
                HealthCheck {
                    name: "extensions".into(),
                    ok: not_ready == 0,
                    detail: format!("共 {} 个扩展注册，{} 个状态非 ready", rows.len(), not_ready),
                    suggestion: (not_ready > 0)
                        .then(|| "运行 chrome-host extension list 查看状态异常的扩展".to_string()),
                }
            }
            Err(e) => HealthCheck {
                name: "extensions".into(),
                ok: false,
                detail: format!("扩展注册表读取失败: {e}"),
                suggestion: Some("运行 chrome-host extension list 检查注册表".into()),
            },
        }
    }

    /// 资源计数：仓储读取失败按 0 处理（计数是附加信息，不应让整份报告失败）
    fn counts(&self) -> Counts {
        let environments = self.env_repo.list().map(|v| v.len()).unwrap_or(0) as u64;
        let instances = self.ins_repo.list_all().unwrap_or_default();
        // running 口径与全应用一致：is_alive_status（running/starting 均算存活）
        let running = instances.iter().filter(|i| i.status.is_alive_status()).count() as u64;
        let extensions = self.extensions.list().map(|v| v.len()).unwrap_or(0) as u64;
        Counts {
            environments,
            instances: instances.len() as u64,
            running,
            extensions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::environment::Environment;
    use crate::domain::extension::{Extension, ExtensionType};
    use crate::domain::instance::{Instance, InstanceStatus};
    use crate::infrastructure::db::client::init_pool;
    use crate::infrastructure::db::schema::init_schema;
    use crate::infrastructure::kernel::constants::{CFT_VERSION, cft_binary_rel_path, cft_platform};

    fn sample_env(id: &str) -> Environment {
        Environment {
            id: id.to_string(),
            name: "health-test".to_string(),
            hosts_source_url: None,
            icon: None,
            startup_args: None,
            keep_alive: false,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn sample_instance(env_id: &str, id: &str, port: u16, status: InstanceStatus) -> Instance {
        Instance {
            id: id.to_string(),
            environment_id: env_id.to_string(),
            login_profile_id: None,
            profile_dir: format!("/tmp/health/{id}"),
            pid: None,
            cdp_port: Some(port),
            status,
            host_rules: None,
            browser_version: None,
            started_at: None,
            stopped_at: None,
            created_at: 100,
            updated_at: 100,
        }
    }

    fn sample_extension(source_path: &str) -> Extension {
        Extension {
            id: format!("ext_{}", uuid::Uuid::new_v4()),
            name: "Test Ext".into(),
            description: None,
            version: Some("1.0.0".into()),
            manifest_version: Some(3),
            extension_type: ExtensionType::User,
            source_path: source_path.to_string(),
            enabled: true,
            status: ExtensionStatus::Ready,
            created_at: 1,
            updated_at: 1,
        }
    }

    /// 构造"内核已装"假象：meta.json（版本/平台匹配）+ 空二进制文件。
    /// installed_binary_path 只校验 meta + 文件存在（不执行），空文件即可通过
    fn fake_installed_kernel(root: &std::path::Path) {
        let version_dir = root.join(CFT_VERSION);
        // binary rel path 含多级目录（.app/Contents/MacOS），先建父目录
        let binary = version_dir.join(cft_binary_rel_path());
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, b"").unwrap();
        std::fs::write(
            root.join("meta.json"),
            format!(
                r#"{{"version":"{CFT_VERSION}","platform":"{}","downloaded_at":"2025-01-01T00:00:00Z"}}"#,
                cft_platform(),
            ),
        )
        .unwrap();
    }

    /// 公共装配：临时 DB + 临时目录（AppPaths 三目录就绪）+ 指定 kernel_root。
    /// 返回 pool 供测试用例插入数据（DbPool 是 Arc 包装，clone 廉价）
    fn setup(kernel_root: std::path::PathBuf) -> (HealthService, DbPool) {
        let dir = std::env::temp_dir().join(format!("cem-health-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = AppPaths::new(dir.clone());
        // 三个被检目录就绪（生产中分别由首次启动 init_pool / 内核下载 / 创建环境时创建）
        std::fs::create_dir_all(paths.kernel_root()).unwrap();
        std::fs::create_dir_all(paths.environments_root()).unwrap();

        let pool = init_pool(&paths.db_file()).unwrap();
        init_schema(&pool).unwrap();
        let svc = HealthService::new(
            pool.clone(),
            Arc::new(KernelManager::new(kernel_root)),
            paths,
            Arc::new(EnvironmentRepository::new(pool.clone())),
            Arc::new(InstanceRepository::new(pool.clone())),
            Arc::new(ExtensionRepository::new(pool.clone())),
            "0.0.0-test".into(),
        );
        (svc, pool)
    }

    /// 临时 DB + 临时目录 + 内核已装 → 全部检查通过，counts 与数据一致
    #[test]
    fn all_healthy_reports_full_counts() {
        let kernel_root =
            std::env::temp_dir().join(format!("cem-health-kernel-{}", uuid::Uuid::new_v4()));
        fake_installed_kernel(&kernel_root);
        let (svc, pool) = setup(kernel_root);

        // 1 环境 + 2 实例（1 running / 1 stopped）+ 1 扩展
        let envs = EnvironmentRepository::new(pool.clone());
        let instances = InstanceRepository::new(pool.clone());
        let extensions = ExtensionRepository::new(pool.clone());
        envs.insert(&sample_env("env_1")).unwrap();
        instances
            .insert(&sample_instance("env_1", "ins_running", 9333, InstanceStatus::Running))
            .unwrap();
        instances
            .insert(&sample_instance("env_1", "ins_stopped", 9334, InstanceStatus::Stopped))
            .unwrap();
        extensions.insert(&sample_extension("/tmp/ext-a")).unwrap();

        let report = svc.snapshot();
        assert!(report.ok, "全健康场景 report.ok 应为 true: {:?}", report.checks);
        assert!(report.checks.iter().all(|c| c.ok), "四项检查均通过: {:?}", report.checks);
        assert_eq!(report.version, "0.0.0-test");
        assert_eq!(report.counts.environments, 1);
        assert_eq!(report.counts.instances, 2);
        assert_eq!(
            report.counts.running, 1,
            "running 按 is_alive_status 口径（running/starting），stopped 不计入"
        );
        assert_eq!(report.counts.extensions, 1);
    }

    /// kernel_root 指向不存在路径 → 仅 kernel 项 ok=false；
    /// 其余检查独立执行不受影响（不短路契约）
    #[test]
    fn missing_kernel_fails_isolated_check() {
        // 不构造 meta/二进制：root 不存在 → 未安装
        let kernel_root =
            std::env::temp_dir().join(format!("cem-health-missing-{}", uuid::Uuid::new_v4()));
        let (svc, _pool) = setup(kernel_root);

        let report = svc.snapshot();
        assert!(!report.ok, "任一检查失败 → report.ok == false");
        let kernel = report.checks.iter().find(|c| c.name == "kernel").unwrap();
        assert!(!kernel.ok);
        assert!(
            kernel.suggestion.as_deref().unwrap().contains("chrome-host runtime install"),
            "未安装时给出安装指引: {kernel:?}"
        );

        for name in ["database", "directories", "extensions"] {
            let c = report.checks.iter().find(|c| c.name == name).unwrap();
            assert!(c.ok, "{name} 不应受 kernel 失败影响（不短路）: {c:?}");
        }
    }
}
