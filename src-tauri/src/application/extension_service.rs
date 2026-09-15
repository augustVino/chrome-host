//! Extension 服务：一级扩展资源的注册/启停/移除 + 运行时解析。
//!
//! 分层职责：
//! - 注册校验（manifest 可读）在此；仓储只做 CRUD
//! - `sync_system_extensions`：内置扩展目录自动发现（resources/extensions 即真相源，
//!   新增扩展无需改代码）+ prune 已移除目录的注册行
//! - `resolve_for_runtime` 是实例启动加载扩展的唯一入口，加载方式由目录内
//!   chrome-host.json 声明：static（缺省）直引源目录；materialize 物化烘焙
//!   （MV3 content script 无文件系统访问，占位符替换为唯一可行方案）；
//!   User 扩展恒为 Direct Mode（用户改代码，新实例即加载最新）
//! - 安全铁律：实例加载的扩展路径只能来自注册表，REST 不提供按任意路径加载

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::domain::environment::Environment;
use crate::domain::extension::{Extension, ExtensionStatus, ExtensionType};
use crate::domain::instance::Instance;
use crate::error::AppError;
use crate::infrastructure::db::repositories::extension_repository::ExtensionRepository;
use crate::infrastructure::extension;

use super::settings_service::SettingsService;

pub struct ExtensionService {
    repo: Arc<ExtensionRepository>,
    settings: Arc<SettingsService>,
    /// app_data 根（generated-extensions 输出位置）
    data_root: PathBuf,
    /// resources 根（内置扩展目录所在）
    resource_root: PathBuf,
}

impl ExtensionService {
    pub fn new(
        repo: Arc<ExtensionRepository>,
        settings: Arc<SettingsService>,
        data_root: PathBuf,
        resource_root: PathBuf,
    ) -> Self {
        ExtensionService { repo, settings, data_root, resource_root }
    }

    /// 启动同步（自动发现 + prune，幂等）：扫描 `resources/extensions/*/manifest.json`
    /// 全部注册为 system 扩展；DB 中 source_path 已不在扫描结果内的 system 行删除
    /// （版本升级移除了内置目录）。
    /// - 同源目录已注册（无论 system/user）→ 跳过，防重复插入/双重加载
    /// - 个别目录 manifest 损坏 → 告警跳过注册（行保留，读取时惰性标 invalid），不阻断启动
    /// - 资源根目录缺失 → Err（打包破损，快速失败）
    pub fn sync_system_extensions(&self) -> Result<(), AppError> {
        let bundled_root = self.resource_root.join(extension::BUNDLED_EXTENSIONS_DIR);
        let entries = std::fs::read_dir(&bundled_root).map_err(|e| {
            AppError::internal(format!(
                "内置扩展资源目录不可读（打包缺失？）: {} - {e}",
                bundled_root.display()
            ))
        })?;

        // 有效内置目录 = extensions/ 下含 manifest.json 的一级子目录（排序保证注册顺序稳定）
        let mut dirs: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join("manifest.json").is_file() {
                dirs.push(path);
            }
        }
        dirs.sort();
        let valid_sources: HashSet<String> =
            dirs.iter().map(|d| d.to_string_lossy().to_string()).collect();

        let existing: HashSet<String> =
            self.repo.list()?.into_iter().map(|e| e.source_path).collect();
        for dir in &dirs {
            let source = dir.to_string_lossy().to_string();
            if existing.contains(&source) {
                continue;
            }
            match extension::read_manifest(dir) {
                Ok(manifest) => {
                    let now = chrono::Utc::now().timestamp_millis();
                    self.repo.insert(&Extension {
                        id: format!("ext_{}", uuid::Uuid::new_v4()),
                        name: manifest.name.clone(),
                        description: manifest.description,
                        version: manifest.version,
                        manifest_version: manifest.manifest_version,
                        extension_type: ExtensionType::System,
                        source_path: source,
                        enabled: true,
                        status: ExtensionStatus::Ready,
                        created_at: now,
                        updated_at: now,
                    })?;
                    tracing::info!("内置扩展已注册: {} ({})", manifest.name, dir.display());
                }
                Err(e) => {
                    tracing::warn!("内置扩展 {} manifest 不可读，跳过注册: {e}", dir.display());
                }
            }
        }

        // prune：只删 system 行（user 行绝不触碰）；目录仍在（哪怕 manifest 暂时损坏）的行保留
        for row in self.repo.list()? {
            if row.extension_type == ExtensionType::System
                && !valid_sources.contains(&row.source_path)
            {
                tracing::info!(
                    "[extensions] 内置扩展目录已移除，清理注册行: {} ({})",
                    row.name,
                    row.source_path
                );
                self.repo.delete(&row.id)?;
            }
        }
        Ok(())
    }

    /// 注册用户扩展：读取 manifest 提取元数据（Direct Mode，只存路径引用）。
    /// 校验失败 → 400，不产生半注册状态。
    pub fn register(&self, path: &str) -> Result<Extension, AppError> {
        let dir = PathBuf::from(path);
        let manifest = extension::read_manifest(&dir)?;
        let now = chrono::Utc::now().timestamp_millis();
        let ext = Extension {
            id: format!("ext_{}", uuid::Uuid::new_v4()),
            name: manifest.name,
            description: manifest.description,
            version: manifest.version,
            manifest_version: manifest.manifest_version,
            extension_type: ExtensionType::User,
            source_path: path.to_string(),
            enabled: true,
            status: ExtensionStatus::Ready,
            created_at: now,
            updated_at: now,
        };
        self.repo.insert(&ext)?;
        tracing::info!("扩展已注册: {} → {}", ext.name, path);
        Ok(self.with_fresh_status(ext))
    }

    pub fn list(&self) -> Result<Vec<Extension>, AppError> {
        Ok(self
            .repo
            .list()?
            .into_iter()
            .map(|ext| self.with_fresh_status(ext))
            .collect())
    }

    pub fn get(&self, id: &str) -> Result<Extension, AppError> {
        Ok(self.with_fresh_status(self.repo.get(id)?))
    }

    /// 启用/禁用。System 扩展锁定：不可启停。
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<Extension, AppError> {
        let ext = self.repo.get(id)?;
        if ext.extension_type == ExtensionType::System {
            return Err(AppError::business(403, "EXTENSION_SYSTEM_LOCKED", "系统扩展不可禁用"));
        }
        self.repo.set_enabled(id, enabled)?;
        Ok(self.with_fresh_status(self.repo.get(id)?))
    }

    /// 移除注册项。System 锁定；绝不删除用户源文件（Direct Mode 只删引用）。
    pub fn delete(&self, id: &str) -> Result<(), AppError> {
        let ext = self.repo.get(id)?;
        if ext.extension_type == ExtensionType::System {
            return Err(AppError::business(403, "EXTENSION_SYSTEM_LOCKED", "系统扩展不可移除"));
        }
        self.repo.delete(id)?;
        tracing::info!("扩展注册项已移除（源文件保留）: {}", ext.source_path);
        Ok(())
    }

    /// 实例启动时解析加载列表。
    /// `chrome_index`：实例在环境内的序号（1 起，与 UI 命名一致，由持有 instances 仓储的
    /// 调用方计算——服务间不互相依赖仓储）。
    /// 加载方式按目录元数据分派：static（缺省）直引；materialize 按目录名分派物化器。
    /// 返回目录路径列表，调用方以逗号拼接为 `--load-extension`；
    /// 个别扩展失败不阻断启动（仅告警跳过）。
    pub fn resolve_for_runtime(
        &self,
        env: &Environment,
        ins: &Instance,
        chrome_index: usize,
    ) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let rows = match self.repo.list() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("扩展列表读取失败（跳过全部扩展加载）: {e}");
                return paths;
            }
        };

        for row in rows {
            if !row.enabled {
                continue;
            }
            let dir = PathBuf::from(&row.source_path);
            // 元数据仅对内置（system）扩展生效；user 扩展恒为 Direct Mode
            let mode = if row.extension_type == ExtensionType::System {
                match extension::read_bundle_meta(&dir) {
                    Ok(meta) => meta.mode,
                    Err(e) => {
                        tracing::warn!("扩展 {} 元数据不可读（跳过）: {e}", row.name);
                        continue;
                    }
                }
            } else {
                extension::ExtensionLoadMode::Static
            };
            match mode {
                extension::ExtensionLoadMode::Static => match extension::read_manifest(&dir) {
                    Ok(_) => paths.push(dir),
                    Err(e) => tracing::warn!("扩展 {} 不可加载（跳过）: {e}", row.name),
                },
                extension::ExtensionLoadMode::Materialize => {
                    let dir_name = dir
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    // 新增物化型内置扩展：目录放 chrome-host.json + 在此加一个分派臂 + 物化函数
                    let result = match dir_name.as_str() {
                        "environment-label" => {
                            self.materialize_env_label(&dir, env, ins, chrome_index)
                        }
                        other => Err(AppError::internal(format!(
                            "物化型扩展「{other}」未实现物化器"
                        ))),
                    };
                    match result {
                        Ok(p) => paths.push(p),
                        Err(e) => tracing::warn!("扩展 {} 物化失败（跳过）: {e}", row.name),
                    }
                }
            }
        }
        paths
    }

    /// 物化环境标识（当前唯一的物化器）：标签文本 = `<环境名> · Chrome #N`，
    /// 位置/颜色来自设置页（持久化于 app_settings，启动时物化进实例副本）
    fn materialize_env_label(
        &self,
        source_dir: &Path,
        env: &Environment,
        ins: &Instance,
        chrome_index: usize,
    ) -> Result<PathBuf, AppError> {
        let leaf = source_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "environment-label".to_string());
        let out = self
            .data_root
            .join("generated-extensions")
            .join(&ins.id)
            .join(leaf);
        let settings = self.settings.get()?;
        extension::materialize_environment_label(
            source_dir,
            &out,
            &format!("{} · Chrome #{}", env.name, chrome_index),
            &settings.env_label_position,
            &settings.env_label_color,
        )
    }

    /// 状态惰性刷新：目录没了 → missing；manifest 坏了 → invalid；否则保持 ready
    fn with_fresh_status(&self, ext: Extension) -> Extension {
        let mut ext = ext;
        let dir = PathBuf::from(&ext.source_path);
        ext.status = if !dir.is_dir() {
            ExtensionStatus::Missing
        } else if extension::read_manifest(&dir).is_err() {
            ExtensionStatus::Invalid
        } else {
            ExtensionStatus::Ready
        };
        ext
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::instance::InstanceStatus;
    use crate::infrastructure::db::client::init_pool;
    use crate::infrastructure::db::schema::init_schema;
    use std::path::Path;

    struct Ctx {
        svc: ExtensionService,
        settings: Arc<SettingsService>,
        templates: PathBuf,
    }

    fn setup() -> Ctx {
        let dir = std::env::temp_dir().join(format!("cem-ext-svc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = init_pool(&dir.join("test.db")).unwrap();
        init_schema(&pool).unwrap();
        let settings = Arc::new(SettingsService::new(pool.clone()));
        // resource_root 下按 ENV_LABEL_TEMPLATE 相对路径（extensions/environment-label）存放模板
        let templates = dir.join("templates").join("extensions").join("environment-label");
        std::fs::create_dir_all(&templates).unwrap();
        std::fs::write(
            templates.join("manifest.json"),
            r#"{"name":"Environment Label","manifest_version":3}"#,
        )
        .unwrap();
        std::fs::write(
            templates.join("content.js"),
            "const n = 'ENVIRONMENT_NAME_PLACEHOLDER';",
        )
        .unwrap();
        // env-label 声明为物化型（与真实 bundle 一致）
        std::fs::write(templates.join("chrome-host.json"), r#"{"mode":"materialize"}"#).unwrap();
        let static_ext = dir.join("templates").join("extensions").join("sample-static");
        std::fs::create_dir_all(&static_ext).unwrap();
        std::fs::write(
            static_ext.join("manifest.json"),
            r#"{"name":"Sample Static","version":"1.0","manifest_version":3}"#,
        )
        .unwrap();
        std::fs::write(static_ext.join("content.js"), "// static").unwrap();
        let svc = ExtensionService::new(
            Arc::new(ExtensionRepository::new(pool)),
            settings.clone(),
            dir.clone(),
            dir.join("templates"),
        );
        Ctx { svc, settings, templates }
    }

    fn sample_ins(env_id: &str) -> Instance {
        Instance {
            id: "ins_test".into(),
            environment_id: env_id.into(),
            login_profile_id: None,
            profile_dir: "/tmp/x".into(),
            pid: None,
            cdp_port: Some(9333),
            status: InstanceStatus::Starting,
            host_rules: None,
            browser_version: None,
            started_at: None,
            stopped_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn sample_env() -> Environment {
        let now = chrono::Utc::now().timestamp_millis();
        Environment {
            id: "env_test".into(),
            name: "qapub".into(),
            hosts_source_url: None,
            icon: None,
            startup_args: None,
            keep_alive: false,
            created_at: now,
            updated_at: now,
        }
    }

    fn write_user_ext(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            format!(r#"{{"name":"{name}","version":"1.0.0","manifest_version":3}}"#),
        )
        .unwrap();
    }

    /// 目录自动发现：sync 幂等；system 锁定；目录移除 → prune 清理注册行
    #[test]
    fn sync_is_idempotent_prunes_and_locks_system() {
        let ctx = setup();
        ctx.svc.sync_system_extensions().unwrap();
        ctx.svc.sync_system_extensions().unwrap(); // 二次幂等
        let list = ctx.svc.list().unwrap();
        assert_eq!(list.len(), 2, "目录自动发现两个内置 system 扩展");
        assert!(list.iter().all(|e| e.extension_type == ExtensionType::System));
        assert!(list.iter().all(|e| e.status == ExtensionStatus::Ready));

        for e in &list {
            let err = ctx.svc.delete(&e.id).unwrap_err();
            assert_eq!(err.status(), 403, "System 不可删");
            let err = ctx.svc.set_enabled(&e.id, false).unwrap_err();
            assert_eq!(err.status(), 403, "System 不可禁用");
        }

        // prune：目录被移除（模拟升级删除内置扩展）→ 行清理；其余保留
        std::fs::remove_dir_all(ctx.templates.parent().unwrap().join("sample-static")).unwrap();
        ctx.svc.sync_system_extensions().unwrap();
        let names: Vec<String> =
            ctx.svc.list().unwrap().into_iter().map(|e| e.name).collect();
        assert!(names.contains(&"Environment Label".to_string()));
        assert!(
            !names.contains(&"Sample Static".to_string()),
            "目录移除 → 注册行被 prune: {names:?}"
        );
    }

    #[test]
    fn register_validates_manifest_and_rejects_bad_path() {
        let ctx = setup();
        let err = ctx.svc.register("/nonexistent/dir").unwrap_err();
        assert_eq!(err.status(), 400, "目录不存在 → 400");

        let bad = ctx.templates.parent().unwrap().join("bad-ext");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("manifest.json"), "not-json").unwrap();
        let err = ctx.svc.register(bad.to_str().unwrap()).unwrap_err();
        assert_eq!(err.status(), 400, "manifest 非法 → 400");

        let good = ctx.templates.parent().unwrap().join("good-ext");
        write_user_ext(&good, "My Ext");
        let ext = ctx.svc.register(good.to_str().unwrap()).unwrap();
        assert_eq!(ext.name, "My Ext");
        assert_eq!(ext.extension_type, ExtensionType::User);
        assert!(ext.enabled, "注册默认启用");
        assert_eq!(ext.status, ExtensionStatus::Ready);
    }

    #[test]
    fn status_refreshes_lazily_and_delete_keeps_source() {
        let ctx = setup();
        let good = ctx.templates.parent().unwrap().join("good-ext");
        write_user_ext(&good, "My Ext");
        let ext = ctx.svc.register(good.to_str().unwrap()).unwrap();

        // 禁用 → 启用 roundtrip
        assert!(!ctx.svc.set_enabled(&ext.id, false).unwrap().enabled);
        assert!(ctx.svc.set_enabled(&ext.id, true).unwrap().enabled);

        // 源目录被移走 → missing；恢复后 manifest 坏 → invalid
        std::fs::remove_dir_all(&good).unwrap();
        assert_eq!(ctx.svc.get(&ext.id).unwrap().status, ExtensionStatus::Missing);
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join("manifest.json"), "{").unwrap();
        assert_eq!(ctx.svc.get(&ext.id).unwrap().status, ExtensionStatus::Invalid);

        // 删除注册项不删源文件
        std::fs::write(good.join("manifest.json"), r#"{"name":"X"}"#).unwrap();
        ctx.svc.delete(&ext.id).unwrap();
        assert!(good.is_dir(), "源目录保留");
        assert!(ctx.svc.list().unwrap().iter().all(|e| e.id != ext.id));
    }

    /// 加载方式按目录元数据分派：static 直引 / materialize 物化；
    /// user 扩展恒为直引（误配元数据不生效）；禁用/manifest 损坏/未知物化器均跳过不阻断
    #[test]
    fn resolve_dispatches_by_metadata() {
        let ctx = setup();
        ctx.svc.sync_system_extensions().unwrap();

        let on = ctx.templates.parent().unwrap().join("ext-on");
        let off = ctx.templates.parent().unwrap().join("ext-off");
        let broken = ctx.templates.parent().unwrap().join("ext-broken");
        write_user_ext(&on, "On");
        write_user_ext(&off, "Off");
        write_user_ext(&broken, "Broken");

        ctx.svc.register(on.to_str().unwrap()).unwrap();
        let e_off = ctx.svc.register(off.to_str().unwrap()).unwrap();
        let _ = ctx.svc.register(broken.to_str().unwrap()).unwrap();
        ctx.svc.set_enabled(&e_off.id, false).unwrap();
        // user 扩展即使误配 materialize 元数据也走直引（元数据仅对 system 生效）
        std::fs::write(on.join("chrome-host.json"), r#"{"mode":"materialize"}"#).unwrap();
        // 注册后源目录损坏（manifest 被删）：resolve 时应跳过而非阻断启动
        std::fs::remove_file(broken.join("manifest.json")).unwrap();

        let env = sample_env();
        let ins = sample_ins(&env.id);
        let paths = ctx.svc.resolve_for_runtime(&env, &ins, 1);
        let strs: Vec<String> =
            paths.iter().map(|p| p.to_string_lossy().to_string()).collect();

        assert!(
            strs.iter().any(|p| p.contains("generated-extensions/ins_test")),
            "env-label 按元数据物化: {strs:?}"
        );
        assert!(
            strs.iter().any(|p| p.ends_with("extensions/sample-static")),
            "静态 system 直引: {strs:?}"
        );
        assert!(
            strs.iter().any(|p| p.ends_with("ext-on")),
            "user 直引（meta 对 user 不生效）: {strs:?}"
        );
        assert_eq!(paths.len(), 3, "off（禁用）/broken（损坏）排除: {strs:?}");

        // 物化内容含标签文本与序号
        let label = paths
            .iter()
            .find(|p| p.to_string_lossy().contains("generated-extensions"))
            .unwrap();
        let js = std::fs::read_to_string(label.join("content.js")).unwrap();
        assert!(js.contains("qapub · Chrome #1"), "标签文本烘焙: {js}");

        // 未知物化器：system 目录声明 materialize 但无分派臂 → 告警跳过，不阻断
        std::fs::write(
            ctx.templates
                .parent()
                .unwrap()
                .join("sample-static")
                .join("chrome-host.json"),
            r#"{"mode":"materialize"}"#,
        )
        .unwrap();
        let paths = ctx.svc.resolve_for_runtime(&env, &ins, 1);
        assert!(
            !paths.iter().any(|p| p.ends_with("extensions/sample-static")),
            "未知物化器应跳过"
        );
    }
}
