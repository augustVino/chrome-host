//! Extension 服务：一级扩展资源的注册/启停/移除 + 运行时解析。
//!
//! 分层职责：
//! - 注册校验（manifest 可读）在此；仓储只做 CRUD
//! - `sync_system_extensions`：内置扩展目录自动发现（resources/extensions 即真相源，
//!   新增扩展无需改代码）+ prune 已移除目录的注册行
//! - `resolve_for_runtime` 是实例启动加载扩展的唯一入口，加载方式由目录内
//!   chrome-host.json 声明：static（缺省）直引源目录；materialize 统一哑管道
//!   （模板原样拷贝 + 写 config.js，配置组装见 assemble_config）；
//!   User 扩展恒为 Direct Mode（用户改代码，新实例即加载最新）
//! - 安全铁律：实例加载的扩展路径只能来自注册表，REST 不提供按任意路径加载

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::domain::environment::Environment;
use crate::domain::extension::{Extension, ExtensionStatus, ExtensionType};
use crate::domain::instance::Instance;
use crate::domain::net_rules;
use crate::error::AppError;
use crate::infrastructure::db::repositories::environment_repository::EnvironmentRepository;
use crate::infrastructure::db::repositories::extension_config_repository::{
    ExtensionConfigRepository, GLOBAL_ENV_ID,
};
use crate::infrastructure::db::repositories::extension_repository::ExtensionRepository;
use crate::infrastructure::extension;

use super::settings_service::SettingsService;

/// 扩展配置体大小上限（防滥用；net-rules 实际场景几 KB 足够）
const CONFIG_MAX_BYTES: usize = 64 * 1024;

pub struct ExtensionService {
    repo: Arc<ExtensionRepository>,
    /// config 写入时的 envId 存在性校验（防拼写错误静默落库永不生效）
    envs: Arc<EnvironmentRepository>,
    config_repo: Arc<ExtensionConfigRepository>,
    settings: Arc<SettingsService>,
    /// app_data 根（generated-extensions 输出位置）
    data_root: PathBuf,
    /// resources 根（内置扩展目录所在）
    resource_root: PathBuf,
}

impl ExtensionService {
    pub fn new(
        repo: Arc<ExtensionRepository>,
        envs: Arc<EnvironmentRepository>,
        config_repo: Arc<ExtensionConfigRepository>,
        settings: Arc<SettingsService>,
        data_root: PathBuf,
        resource_root: PathBuf,
    ) -> Self {
        ExtensionService { repo, envs, config_repo, settings, data_root, resource_root }
    }

    /// 启动同步（自动发现 + prune，幂等）：扫描 `resources/extensions/*/manifest.json`
    /// 全部注册为 system 扩展；DB 中 source_path 已不在扫描结果内的 system 行删除
    /// （版本升级移除了内置目录）。生产只认编译期源清单 [`extension::BUILTIN_EXTENSION_DIRS`]，
    /// resources 拷贝产物里清单外的陈旧目录（幽灵）被直接无视。
    /// - 同源目录已注册（无论 system/user）→ 跳过，防重复插入/双重加载
    /// - 个别目录 manifest 损坏 → 告警跳过注册（行保留，读取时惰性标 invalid），不阻断启动
    /// - 资源根目录缺失 → Err（打包破损，快速失败）
    pub fn sync_system_extensions(&self) -> Result<(), AppError> {
        self.sync_system_extensions_with(extension::BUILTIN_EXTENSION_DIRS)
    }

    /// 清单可注入版（测试用自定义目录集；生产恒为编译期 BUILTIN_EXTENSION_DIRS）
    pub fn sync_system_extensions_with(&self, allowed: &[&str]) -> Result<(), AppError> {
        let bundled_root = self.resource_root.join(extension::BUNDLED_EXTENSIONS_DIR);
        let entries = std::fs::read_dir(&bundled_root).map_err(|e| {
            AppError::internal(format!(
                "内置扩展资源目录不可读（打包缺失？）: {} - {e}",
                bundled_root.display()
            ))
        })?;

        // 有效内置目录 = extensions/ 下含 manifest.json 的一级子目录（排序保证注册顺序稳定）；
        // 再按清单过滤——清单外目录即使残留（拷贝只增不删）也不注册不保留
        let mut dirs: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let dir_name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if !path.is_dir()
                || !path.join("manifest.json").is_file()
                || !allowed.contains(&dir_name.as_str())
            {
                continue;
            }
            dirs.push(path);
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
                        configurable: false,
                        config_docs_url: None,
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
            configurable: false,
            config_docs_url: None,
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
    /// 调用方计算——服务间不互相依赖仓储）；`configs`：extension_id → 生效配置 JSON
    /// （调用方查 extension_configs 表装配，读失败按空 map 不阻断启动）。
    /// 加载方式按目录元数据分派：static（缺省）直引；materialize 统一走
    /// config.js 哑管道（物化器无 per-consumer 分支，配置组装的少量分派见
    /// [`Self::assemble_config`]）。返回目录路径列表，调用方以逗号拼接为
    /// `--load-extension`；个别扩展失败不阻断启动（仅告警跳过）。
    pub fn resolve_for_runtime(
        &self,
        env: &Environment,
        ins: &Instance,
        chrome_index: usize,
        configs: &HashMap<String, String>,
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
            let meta = if row.extension_type == ExtensionType::System {
                match extension::read_bundle_meta(&dir) {
                    Ok(meta) => meta,
                    Err(e) => {
                        tracing::warn!("扩展 {} 元数据不可读（跳过）: {e}", row.name);
                        continue;
                    }
                }
            } else {
                Default::default()
            };
            match meta.mode {
                extension::ExtensionLoadMode::Static => match extension::read_manifest(&dir) {
                    Ok(_) => paths.push(dir),
                    Err(e) => tracing::warn!("扩展 {} 不可加载（跳过）: {e}", row.name),
                },
                extension::ExtensionLoadMode::Materialize => {
                    let leaf = dir
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let out =
                        crate::infrastructure::paths::generated_extensions_dir(&self.data_root, &ins.id)
                            .join(&leaf);
                    let config_file = meta.config_file.as_deref().unwrap_or(extension::DEFAULT_CONFIG_FILE);
                    let result = self
                        .assemble_config(&leaf, &row.id, env, chrome_index, configs)
                        .and_then(|config| {
                            extension::materialize_with_config(&dir, &out, config_file, &config)
                        });
                    match result {
                        Ok(p) => paths.push(p),
                        Err(e) => tracing::warn!("扩展 {} 物化失败（跳过）: {e}", row.name),
                    }
                }
            }
        }
        paths
    }

    /// 物化管道唯一的 per-consumer 分派（配置组装；物化器本身是哑管道）：
    /// - environment-label：文本 = `<环境名> · Chrome #N`，位置/颜色来自全局设置
    ///   （未声明 configFile → 不读配置表，也不可经 config API 写入）
    /// - net-rules：配置表原始配置编译为 DNR 规则（background.js 哑应用 cfg.rules）
    /// - 其他声明了 configFile 的物化扩展：表内原始配置原样透传
    fn assemble_config(
        &self,
        leaf: &str,
        extension_id: &str,
        env: &Environment,
        chrome_index: usize,
        configs: &HashMap<String, String>,
    ) -> Result<serde_json::Value, AppError> {
        match leaf {
            "environment-label" => {
                let settings = self.settings.get()?;
                Ok(serde_json::json!({
                    "text": format!("{} · Chrome #{}", env.name, chrome_index),
                    "position": settings.env_label_position,
                    "color": settings.env_label_color,
                }))
            }
            "net-rules" => {
                let config = configs
                    .get(extension_id)
                    .map(|s| serde_json::from_str::<serde_json::Value>(s))
                    .transpose()
                    .map_err(|e| {
                        AppError::business(
                            400,
                            "EXTENSION_CONFIG_INVALID",
                            format!("net-rules 配置非法 JSON: {e}"),
                        )
                    })?
                    .unwrap_or_else(|| serde_json::json!({}));
                let rules = net_rules::compile(&config)?;
                Ok(serde_json::json!({ "rules": rules }))
            }
            other => {
                let config = configs
                    .get(extension_id)
                    .map(|s| serde_json::from_str::<serde_json::Value>(s))
                    .transpose()
                    .map_err(|e| {
                        AppError::business(
                            400,
                            "EXTENSION_CONFIG_INVALID",
                            format!("扩展 {other} 配置非法 JSON: {e}"),
                        )
                    })?
                    .unwrap_or_else(|| serde_json::json!({}));
                Ok(config)
            }
        }
    }

    /// 读取扩展运行时配置（environmentId 缺省 = 全局默认行；未配置 → null）
    pub fn get_config(
        &self,
        extension_id: &str,
        environment_id: Option<&str>,
    ) -> Result<serde_json::Value, AppError> {
        let env_key = self.config_scope(extension_id, environment_id)?;
        let config = self
            .config_repo
            .get(&env_key, extension_id)?
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
        Ok(serde_json::json!({
            "environmentId": environment_id,
            "config": config,
        }))
    }

    /// 写入扩展运行时配置（整体覆盖；environmentId 缺省 = 全局默认行）。
    /// 校验：仅声明了 configFile 的 system 物化扩展可配置；体 ≤64KB；合法 JSON 对象；
    /// net-rules 额外做编译校验（写入即反馈，不留到启动时才告警）。对新启动实例生效。
    pub fn set_config(
        &self,
        extension_id: &str,
        environment_id: Option<&str>,
        body: &str,
    ) -> Result<(), AppError> {
        let env_key = self.config_scope(extension_id, environment_id)?;
        if let Some(env) = environment_id {
            self.envs.get(env)?; // 拼写错误立即 404，不静默落库永不生效
        }
        if body.len() > CONFIG_MAX_BYTES {
            return Err(AppError::business(
                400,
                "EXTENSION_CONFIG_INVALID",
                format!("配置体超过 {}KB 上限", CONFIG_MAX_BYTES / 1024),
            ));
        }
        let config: serde_json::Value = serde_json::from_str(body).map_err(|e| {
            AppError::business(400, "EXTENSION_CONFIG_INVALID", format!("配置非合法 JSON: {e}"))
        })?;
        if !config.is_object() {
            return Err(AppError::business(
                400,
                "EXTENSION_CONFIG_INVALID",
                "配置必须是 JSON 对象",
            ));
        }
        let dir = PathBuf::from(self.repo.get(extension_id)?.source_path);
        if dir.file_name().map(|n| n == "net-rules").unwrap_or(false) {
            net_rules::compile(&config)?; // 写入即校验可编译
        }
        self.config_repo.put(&env_key, extension_id, &config.to_string())?;
        tracing::info!(
            "扩展配置已更新: {extension_id} scope={}",
            environment_id.unwrap_or("<global>")
        );
        Ok(())
    }

    /// config 读写前置：扩展存在 + 可配置（system + materialize + 声明 configFile）。
    /// 返回落库的 env key（全局行哨兵或真实 env id）
    fn config_scope(
        &self,
        extension_id: &str,
        environment_id: Option<&str>,
    ) -> Result<String, AppError> {
        let ext = self.repo.get(extension_id)?;
        if !self.is_configurable(&ext) {
            return Err(AppError::business(
                400,
                "EXTENSION_CONFIG_INVALID",
                format!("扩展 {} 不支持运行时配置（仅声明 configFile 的内置物化扩展可配置）", ext.name),
            ));
        }
        Ok(environment_id.unwrap_or(GLOBAL_ENV_ID).to_string())
    }

    fn is_configurable(&self, ext: &Extension) -> bool {
        ext.extension_type == ExtensionType::System
            && extension::read_bundle_meta(Path::new(&ext.source_path))
                .map(|m| {
                    m.mode == extension::ExtensionLoadMode::Materialize && m.config_file.is_some()
                })
                .unwrap_or(false)
    }

    /// 状态惰性刷新：目录没了 → missing；manifest 坏了 → invalid；否则保持 ready；
    /// configurable/configDocsUrl 同步按目录元数据重算（仅 system 扩展有资格）
    fn with_fresh_status(&self, mut ext: Extension) -> Extension {
        let dir = PathBuf::from(&ext.source_path);
        ext.status = if !dir.is_dir() {
            ExtensionStatus::Missing
        } else if extension::read_manifest(&dir).is_err() {
            ExtensionStatus::Invalid
        } else {
            ExtensionStatus::Ready
        };
        let meta_ok = ext.extension_type == ExtensionType::System
            && extension::read_bundle_meta(&dir)
                .map(|m| m.mode == extension::ExtensionLoadMode::Materialize && m.config_file.is_some())
                .unwrap_or(false);
        ext.configurable = meta_ok;
        ext.config_docs_url = if meta_ok {
            extension::read_bundle_meta(&dir).ok().and_then(|m| m.config_docs_url)
        } else {
            None
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

    /// 测试清单：setup 造的三个模板目录（sample-static 仅存在于测试资源树，
    /// 不在编译期 BUILTIN_EXTENSION_DIRS——由 _with 注入才可见）
    const TPL_LIST: &[&str] = &["environment-label", "net-rules", "sample-static"];

    struct Ctx {
        svc: ExtensionService,
        settings: Arc<SettingsService>,
        templates: PathBuf,
        /// net-rules 模板目录（声明 configFile，可配置）
        net_rules_dir: PathBuf,
        config_repo: Arc<ExtensionConfigRepository>,
    }

    fn setup() -> Ctx {
        let dir = std::env::temp_dir().join(format!("cem-ext-svc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = init_pool(&dir.join("test.db")).unwrap();
        init_schema(&pool).unwrap();
        let settings = Arc::new(SettingsService::new(pool.clone()));
        // resource_root 下存放模板（extensions/<name>），与真实 bundle 布局一致
        let templates = dir.join("templates").join("extensions").join("environment-label");
        std::fs::create_dir_all(&templates).unwrap();
        std::fs::write(
            templates.join("manifest.json"),
            r#"{"name":"Environment Label","manifest_version":3}"#,
        )
        .unwrap();
        std::fs::write(templates.join("content.js"), "// reads self.__CHROME_HOST_CONFIG__").unwrap();
        // env-label 声明为物化型且不声明 configFile（配置由全局设置组装，不可经 API 写入）
        std::fs::write(templates.join("chrome-host.json"), r#"{"mode":"materialize"}"#).unwrap();
        let net_rules_dir = dir.join("templates").join("extensions").join("net-rules");
        std::fs::create_dir_all(&net_rules_dir).unwrap();
        std::fs::write(
            net_rules_dir.join("manifest.json"),
            r#"{"name":"Net Rules","manifest_version":3}"#,
        )
        .unwrap();
        std::fs::write(net_rules_dir.join("background.js"), "// applies cfg.rules").unwrap();
        std::fs::write(
            net_rules_dir.join("chrome-host.json"),
            r#"{"mode":"materialize","configFile":"config.js","configDocsUrl":["https://dev.example/dnr"]}"#,
        )
        .unwrap();
        let static_ext = dir.join("templates").join("extensions").join("sample-static");
        std::fs::create_dir_all(&static_ext).unwrap();
        std::fs::write(
            static_ext.join("manifest.json"),
            r#"{"name":"Sample Static","version":"1.0","manifest_version":3}"#,
        )
        .unwrap();
        std::fs::write(static_ext.join("content.js"), "// static").unwrap();
        // set_config 的 envId 校验需要一条真实环境
        crate::infrastructure::db::repositories::environment_repository::EnvironmentRepository::new(
            pool.clone(),
        )
        .insert(&sample_env())
        .unwrap();
        let config_repo = Arc::new(ExtensionConfigRepository::new(pool.clone()));
        let svc = ExtensionService::new(
            Arc::new(ExtensionRepository::new(pool.clone())),
            Arc::new(
                crate::infrastructure::db::repositories::environment_repository::EnvironmentRepository::new(pool.clone()),
            ),
            config_repo.clone(),
            settings.clone(),
            dir.clone(),
            dir.join("templates"),
        );
        Ctx { svc, settings, templates, net_rules_dir, config_repo }
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
            session_capture: Default::default(),
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
        ctx.svc.sync_system_extensions_with(TPL_LIST).unwrap();
        ctx.svc.sync_system_extensions_with(TPL_LIST).unwrap(); // 二次幂等
        let list = ctx.svc.list().unwrap();
        assert_eq!(list.len(), 3, "目录自动发现三个内置 system 扩展");
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
        ctx.svc.sync_system_extensions_with(TPL_LIST).unwrap();
        let names: Vec<String> =
            ctx.svc.list().unwrap().into_iter().map(|e| e.name).collect();
        assert!(names.contains(&"Environment Label".to_string()));
        assert!(
            !names.contains(&"Sample Static".to_string()),
            "目录移除 → 注册行被 prune: {names:?}"
        );
    }

    /// 清单外目录（resources 拷贝残留的幽灵）被无视：不注册；已有 DB 行被 prune
    /// ——修复 devtools-unlock 幽灵的根因语义
    #[test]
    fn dirs_outside_builtin_list_are_ignored_and_pruned() {
        let ctx = setup();
        // 幽灵目录：完整可解析的 manifest，但不在注入清单内
        let ghost = ctx.templates.parent().unwrap().join("devtools-unlock");
        std::fs::create_dir_all(&ghost).unwrap();
        std::fs::write(
            ghost.join("manifest.json"),
            r#"{"name":"Devtools Unlock","manifest_version":3}"#,
        )
        .unwrap();

        ctx.svc.sync_system_extensions_with(TPL_LIST).unwrap();
        let names: Vec<String> = ctx.svc.list().unwrap().into_iter().map(|e| e.name).collect();
        assert!(!names.contains(&"Devtools Unlock".to_string()), "清单外目录不注册: {names:?}");

        // 残留行自愈：模拟旧版本已注册（绕过 sync 直接插行），prune 按「有效扫描集」删除
        let now = chrono::Utc::now().timestamp_millis();
        (ctx.svc.repo).insert(&Extension {
            id: "ext_ghost".into(),
            name: "Devtools Unlock".into(),
            description: None,
            version: None,
            manifest_version: Some(3),
            extension_type: ExtensionType::System,
            source_path: ghost.to_string_lossy().to_string(),
            enabled: true,
            status: ExtensionStatus::Ready,
            configurable: false,
            config_docs_url: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
        ctx.svc.sync_system_extensions_with(TPL_LIST).unwrap();
        let names: Vec<String> = ctx.svc.list().unwrap().into_iter().map(|e| e.name).collect();
        assert!(!names.contains(&"Devtools Unlock".to_string()), "幽灵行被 prune: {names:?}");
        assert_eq!(names.len(), 3, "清单内三个不受影响: {names:?}");
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

    /// 加载方式按目录元数据分派：static 直引 / materialize 统一哑管道；
    /// user 扩展恒为直引（误配元数据不生效）；禁用/manifest 损坏均跳过不阻断。
    /// 物化配置按 consumer 组装：env-label = 设置公式；net-rules = 表配置编译 DNR
    /// （per-env 覆盖全局）；其他声明 configFile 的目录 = 表内原始配置透传
    #[test]
    fn resolve_dispatches_by_metadata() {
        let ctx = setup();
        ctx.svc.sync_system_extensions_with(TPL_LIST).unwrap();
        let net_rules_id = ctx
            .svc
            .list()
            .unwrap()
            .into_iter()
            .find(|e| e.name == "Net Rules")
            .unwrap()
            .id;

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
        std::fs::write(on.join("chrome-host.json"), r#"{"mode":"materialize","configFile":"config.js"}"#).unwrap();
        // 注册后源目录损坏（manifest 被删）：resolve 时应跳过而非阻断启动
        std::fs::remove_file(broken.join("manifest.json")).unwrap();

        // net-rules 配置：全局默认 + per-env 覆盖（写入路径同时被编译校验）
        ctx.svc.set_config(&net_rules_id, None, r#"{"block":["*.google-analytics.com"]}"#).unwrap();
        ctx.svc.set_config(&net_rules_id, Some("env_test"), r#"{"block":["example.com"]}"#).unwrap();

        let env = sample_env();
        let ins = sample_ins(&env.id);
        let configs = ctx.config_repo.resolve_map_for_env(&env.id).unwrap();
        let paths = ctx.svc.resolve_for_runtime(&env, &ins, 1, &configs);
        let strs: Vec<String> =
            paths.iter().map(|p| p.to_string_lossy().to_string()).collect();

        let label = paths
            .iter()
            .find(|p| p.to_string_lossy().contains("environment-label"))
            .expect("env-label 按元数据物化");
        let net = paths
            .iter()
            .find(|p| p.to_string_lossy().contains("net-rules"))
            .expect("net-rules 按元数据物化");
        assert!(
            strs.iter().any(|p| p.ends_with("extensions/sample-static")),
            "静态 system 直引: {strs:?}"
        );
        assert!(
            strs.iter().any(|p| p.ends_with("ext-on")),
            "user 直引（meta 对 user 不生效）: {strs:?}"
        );
        assert_eq!(paths.len(), 4, "off（禁用）/broken（损坏）排除: {strs:?}");

        // env-label 配置 = 设置公式烘入 config.js
        let js = std::fs::read_to_string(label.join("config.js")).unwrap();
        assert!(js.contains("qapub · Chrome #1"), "标签文本烘焙: {js}");
        assert!(js.contains("bottom-left"), "位置取全局设置默认: {js}");

        // net-rules 配置 = 表内原始配置编译为 DNR 规则（per-env 覆盖全局）
        let js = std::fs::read_to_string(net.join("config.js")).unwrap();
        assert!(js.contains("||example.com"), "per-env 覆盖生效: {js}");
        assert!(!js.contains("google-analytics"), "全局默认被覆盖: {js}");
        assert!(js.contains("\"block\""), "DNR 规则已编译: {js}");
        assert!(net.join("background.js").exists(), "模板文件随目录拷贝");

        // 统一管道：无专门组装逻辑的物化目录 → 表内原始配置透传（不再有未知物化器跳过）
        let static_dir = ctx.templates.parent().unwrap().join("sample-static");
        std::fs::write(
            static_dir.join("chrome-host.json"),
            r#"{"mode":"materialize","configFile":"config.js"}"#,
        )
        .unwrap();
        let static_id = ctx
            .svc
            .list()
            .unwrap()
            .into_iter()
            .find(|e| e.name == "Sample Static")
            .unwrap()
            .id;
        ctx.svc.set_config(&static_id, Some("env_test"), r#"{"k":"v"}"#).unwrap();
        let configs = ctx.config_repo.resolve_map_for_env(&env.id).unwrap();
        let paths = ctx.svc.resolve_for_runtime(&env, &ins, 1, &configs);
        let copy = paths
            .iter()
            .find(|p| p.to_string_lossy().contains("sample-static"))
            .expect("物化声明即物化（哑管道无未知分支）");
        let js = std::fs::read_to_string(copy.join("config.js")).unwrap();
        assert!(js.contains("\"k\":\"v\""), "原始配置透传: {js}");
    }

    /// config API 面：get/set 往返、可配置标记、校验拒绝矩阵
    #[test]
    fn config_crud_and_validation() {
        let ctx = setup();
        ctx.svc.sync_system_extensions_with(TPL_LIST).unwrap();
        let list = ctx.svc.list().unwrap();
        let by_name = |n: &str| list.iter().find(|e| e.name == n).unwrap().clone();
        let net_rules_id = by_name("Net Rules").id;
        let env_label_id = by_name("Environment Label").id;
        let static_id = by_name("Sample Static").id;

        // configurable 惰性标记：仅声明 configFile 的物化 system 扩展为 true
        assert!(by_name("Net Rules").configurable);
        assert_eq!(
            by_name("Net Rules").config_docs_url.as_deref(),
            Some(["https://dev.example/dnr".to_string()].as_slice()),
            "configDocsUrl 惰性透出（仅 configurable 扩展）"
        );
        assert!(!by_name("Environment Label").configurable, "未声明 configFile 不可配置");
        assert!(!by_name("Sample Static").configurable);
        assert_eq!(by_name("Sample Static").config_docs_url, None, "非 configurable 无文档链接");

        // 未配置 → null；set/get 往返（全局 + per-env）
        assert_eq!(
            ctx.svc.get_config(&net_rules_id, None).unwrap()["config"],
            serde_json::json!(null)
        );
        ctx.svc.set_config(&net_rules_id, None, r#"{"block":["a.com"]}"#).unwrap();
        assert_eq!(
            ctx.svc.get_config(&net_rules_id, None).unwrap()["config"],
            serde_json::json!({"block":["a.com"]})
        );
        ctx.svc.set_config(&net_rules_id, Some("env_test"), r#"{"block":["b.com"]}"#).unwrap();
        assert_eq!(
            ctx.svc.get_config(&net_rules_id, Some("env_test")).unwrap()["config"],
            serde_json::json!({"block":["b.com"]})
        );
        assert_eq!(
            ctx.svc.get_config(&net_rules_id, None).unwrap()["config"],
            serde_json::json!({"block":["a.com"]}),
            "全局行不被 per-env 写入影响"
        );

        // 拒绝矩阵：均 400 EXTENSION_CONFIG_INVALID
        let rejects: [(&String, Option<&str>, &str, u16); 6] = [
            (&net_rules_id, None, "[1,2]", 400),
            (&net_rules_id, None, "{bad", 400),
            (&net_rules_id, None, &"x".repeat(64 * 1024 + 1), 400),
            (&net_rules_id, None, r#"{"block":[42]}"#, 400),
            (&env_label_id, None, "{}", 400),
            (&static_id, None, "{}", 400),
        ];
        for (id, env, body, status) in rejects {
            let err = ctx.svc.set_config(id, env, body).unwrap_err();
            assert_eq!(err.status(), status, "拒绝 body={:.20}...", body);
            assert_eq!(err.code(), "EXTENSION_CONFIG_INVALID");
        }
        // user 扩展不可配置
        let good = ctx.templates.parent().unwrap().join("good-ext");
        write_user_ext(&good, "My Ext");
        let user_ext = ctx.svc.register(good.to_str().unwrap()).unwrap();
        let err = ctx.svc.set_config(&user_ext.id, None, "{}").unwrap_err();
        assert_eq!(err.status(), 400);
        // 未知环境：防拼写错误静默落库
        let err = ctx.svc.set_config(&net_rules_id, Some("env_missing"), "{}").unwrap_err();
        assert_eq!(err.status(), 404);
        assert_eq!(err.code(), "ENVIRONMENT_NOT_FOUND");
        // 拒绝后表内无脏数据
        assert_eq!(ctx.config_repo.get("", &env_label_id).unwrap(), None);
    }
}
