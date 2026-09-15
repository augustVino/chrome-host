use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tauri::AppHandle;
use uuid::Uuid;

use crate::domain::environment::Environment;
use crate::domain::instance::{Instance, InstanceStatus, RuntimeUpdate};
use crate::error::AppError;
use crate::infrastructure::cdp;
use crate::infrastructure::db::repositories::environment_repository::EnvironmentRepository;
use crate::infrastructure::db::repositories::instance_repository::InstanceRepository;
use crate::infrastructure::hosts;
use crate::infrastructure::kernel::KernelManager;
use crate::infrastructure::paths::AppPaths;
use crate::infrastructure::platform;
use crate::infrastructure::process::port_allocator;
use crate::infrastructure::process::{LaunchSpec, ProcessManager};

use super::activity::Activity;
use super::extension_service::ExtensionService;
use super::keep_alive::KeepAliveWatcher;
use super::profile_service::ProfileService;
use super::reconciler::Reconciler;
use super::settings_service::SettingsService;

const CDP_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// CDP endpoint 视图
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CdpEndpoint {
    pub instance_id: String,
    pub host: String,
    pub port: u16,
    pub http_url: String,
    pub web_socket_url: Option<String>,
}

/// 全量实例视图：Instance 平铺 + environmentName（PRD cli.md §7.2 契约）。
/// 视图结构放服务层（同 CdpEndpoint 先例），api 层只做 Json 包装。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceView {
    #[serde(flatten)]
    pub instance: Instance,
    pub environment_name: String,
}

pub struct InstanceService {
    envs: Arc<EnvironmentRepository>,
    instances: Arc<InstanceRepository>,
    kernel: Arc<KernelManager>,
    process: Arc<dyn ProcessManager>,
    reconciler: Arc<Reconciler>,
    profiles: Arc<ProfileService>,
    activity: Arc<Activity>,
    keep_alive: Arc<KeepAliveWatcher>,
    extensions: Arc<ExtensionService>,
    settings: Arc<SettingsService>,
    paths: AppPaths,
    app: AppHandle,
}

impl InstanceService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        envs: Arc<EnvironmentRepository>,
        instances: Arc<InstanceRepository>,
        kernel: Arc<KernelManager>,
        process: Arc<dyn ProcessManager>,
        reconciler: Arc<Reconciler>,
        profiles: Arc<ProfileService>,
        activity: Arc<Activity>,
        keep_alive: Arc<KeepAliveWatcher>,
        extensions: Arc<ExtensionService>,
        settings: Arc<SettingsService>,
        paths: AppPaths,
        app: AppHandle,
    ) -> Self {
        InstanceService { envs, instances, kernel, process, reconciler, profiles, activity, keep_alive, extensions, settings, paths, app }
    }

    /// 创建并启动 Instance。创建非幂等：每调用一次新增一个 Instance。
    pub async fn create(&self, env_id: &str) -> Result<Instance, AppError> {
        let env = self.envs.get(env_id)?;

        // 1. 分配 ID 与 CDP 端口
        let id = format!("ins_{}", Uuid::new_v4());
        let used = self.instances.used_ports()?;
        let port = port_allocator::allocate(port_allocator::START_PORT, &used).await?;

        // 2. 实例独立 profile 目录+ 登录态克隆（端口分配后、hosts 解析前）。
        //    未配置/未就绪/降级 → None，实例照常创建。
        let login_profile_id = self.profiles.clone_for_instance(env_id, &id)?;
        let profile_dir = self.paths.instance_profile(env_id, &id);
        std::fs::create_dir_all(&profile_dir)?;

        // 3. 先落 starting 审计记录，后续步骤失败可见
        let now = chrono::Utc::now().timestamp_millis();
        let draft = Instance {
            id: id.clone(),
            environment_id: env.id.clone(),
            login_profile_id,
            profile_dir: profile_dir.to_string_lossy().to_string(),
            pid: None,
            cdp_port: Some(port),
            status: InstanceStatus::Starting,
            host_rules: None,
            browser_version: None,
            started_at: None,
            stopped_at: None,
            created_at: now,
            updated_at: now,
        };
        self.instances.insert(&draft)?;
        self.emit("instance-created", &id);

        // 4. 公共启动流程（内核 → hosts → spawn → CDP）
        let running = self.launch_and_wait(&env, &draft).await?;
        if env.keep_alive {
            self.keep_alive.register(&running.id);
        }
        Ok(running)
    }

    /// start 前的端口预检：被占时先尝试清理本实例残留进程并等待释放（Chrome 退出异步），
    /// 仍被外部进程占用才报 409。
    async fn ensure_port_ready(&self, ins: &Instance) -> Result<(), AppError> {
        let port = ins.cdp_port.ok_or_else(|| AppError::internal("实例缺少 CDP 端口"))?;
        if port_allocator::is_free(port).await {
            return Ok(());
        }
        // 可能是本实例旧进程未退透：终止并等待端口释放（≤3s）
        if let Some(pid) = ins.pid {
            if self.process.is_alive(pid, std::path::Path::new(&ins.profile_dir))? {
                tracing::info!("[start] 端口 {port} 被本实例旧进程占用，先终止: pid={pid}");
                self.process.terminate(pid)?;
            }
        }
        for _ in 0..30 {
            if port_allocator::is_free(port).await {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        Err(AppError::conflict(
            "CDP_PORT_UNAVAILABLE",
            format!("端口 {port} 已被占用，无法启动实例 {}", ins.id),
        ))
    }

    /// 启动已停止/失败实例：按原配置重新拉起，hosts 现拉最新。
    pub async fn start(&self, id: &str) -> Result<Instance, AppError> {
        let ins = self.reconciler.reconciled(id).await?;
        match ins.status {
            InstanceStatus::Stopped | InstanceStatus::Error | InstanceStatus::Created => {}
            _ => {
                // 进程退出回调可能有毫秒级延迟：进程确实已死则视同已停，继续 start
                let alive = ins
                    .pid
                    .map(|p| {
                        self.process
                            .is_alive(p, std::path::Path::new(&ins.profile_dir))
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if alive {
                    return Err(AppError::conflict(
                        "INSTANCE_ALREADY_RUNNING",
                        format!("实例 {id} 已在运行"),
                    ));
                }
            }
        }

        self.ensure_port_ready(&ins).await?;

        let env = self.envs.get(&ins.environment_id)?;
        // 清残留 pid + 标 starting（pid 必须显式置 NULL，见 mark_starting 注释）
        self.instances.mark_starting(id)?;

        self.launch_and_wait(&env, &ins).await
    }

    /// 公共启动流程：内核就绪 → hosts 解析注入 → spawn → 等 CDP。
    /// 调用方保证实例记录已存在（starting 状态），失败路径由本方法统一置 error。
    async fn launch_and_wait(&self, env: &Environment, ins: &Instance) -> Result<Instance, AppError> {
        // 1. hosts 解析（在 spawn 之前；失败即失败，不起"缺环境"的实例）
        let rules = match hosts::resolver::resolve(env.hosts_source_url.as_deref()).await {
            Ok(r) => r,
            Err(e) => {
                self.mark_error(&ins.id);
                return Err(e);
            }
        };
        if let Some(rules) = &rules {
            self.instances.update_runtime(
                &ins.id,
                RuntimeUpdate { host_rules: Some(rules.to_snapshot_json()), ..Default::default() },
            )?;
            self.emit_with("hosts-resolved", &ins.id, &format!("{} 条映射", rules.len()));
        }

        // 2. 内核就绪（未装则下载，进度经 kernel-download 事件推送）
        let executable = match self.kernel.ensure_ready(&self.app).await {
            Ok(path) => path,
            Err(e) => {
                self.mark_error(&ins.id);
                return Err(AppError::business(500, "KERNEL_NOT_READY", e));
            }
        };

        // 3. 启动参数：DNS 注入与 no-proxy 成对出现
        let mut extra_args: Vec<String> = Vec::new();
        if let Some(rules) = &rules {
            extra_args.push(format!("--host-resolver-rules={}", rules.to_resolver_rules()));
            extra_args.push("--no-proxy-server".to_string());
        }
        if let Some(args) = &env.startup_args {
            extra_args.extend(args.split_whitespace().map(str::to_string));
        }

        // 扩展加载：System（环境标识物化）+ enabled 用户扩展，统一由 ExtensionService 解析；
        // 全部不可用时不含 --load-extension，不阻断启动
        let chrome_index = self
            .instances
            .list_by_env(&env.id)?
            .iter()
            .position(|i| i.id == ins.id)
            .map(|p| p + 1)
            .unwrap_or(0);
        let ext_paths = self.extensions.resolve_for_runtime(env, ins, chrome_index);
        if !ext_paths.is_empty() {
            let joined = ext_paths
                .iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>()
                .join(",");
            extra_args.push(format!("--load-extension={}", joined));
        }

        // 启动页：全局默认起始页（Settings 页配置）→ 未配置则打开 about:blank
        let start_url = self.settings.resolve_start_url()?;
        extra_args.push(start_url);

        let spec = LaunchSpec {
            executable,
            user_data_dir: PathBuf::from(&ins.profile_dir),
            cdp_port: ins.cdp_port.unwrap_or_default(),
            extra_args,
        };

        // 4. spawn（detached，pid 落库）+ 挂退出监听（事件驱动的退出检测）
        let pid = match self.process.launch(&spec) {
            Ok(pid) => pid,
            Err(e) => {
                self.mark_error(&ins.id);
                return Err(e);
            }
        };
        self.attach_exit_watcher(&ins.id, pid);
        self.instances
            .update_runtime(&ins.id, RuntimeUpdate { pid: Some(pid), ..Default::default() })?;

        // 5. 等 CDP 就绪（不能仅凭 spawn 成功判断 Runtime 成功）
        match cdp::client::wait_until_ready(ins.cdp_port.unwrap_or_default(), CDP_READY_TIMEOUT)
            .await
        {
            Ok(v) => {
                self.instances.update_runtime(
                    &ins.id,
                    RuntimeUpdate {
                        status: Some(InstanceStatus::Running),
                        started_at: Some(chrono::Utc::now().timestamp_millis()),
                        browser_version: Some(v.browser),
                        ..Default::default()
                    },
                )?;
                self.emit("instance-started", &ins.id);
                tracing::info!("实例启动成功: {} pid={pid} cdp={}", ins.id, ins.cdp_port.unwrap_or_default());
            }
            Err(_) => {
                // 起了但 CDP 不可达：杀进程 + error
                let _ = self.process.terminate(pid);
                self.mark_error(&ins.id);
                return Err(AppError::business(
                    500,
                    "INSTANCE_START_FAILED",
                    format!("实例 {} 的 CDP 端口未在超时内就绪，已回收进程", ins.id),
                ));
            }
        }

        Ok(self.instances.get(&ins.id)?)
    }

    /// 读路径统一走对账
    pub async fn get(&self, id: &str) -> Result<Instance, AppError> {
        self.reconciler.reconciled(id).await
    }

    pub async fn list_by_env(&self, env_id: &str) -> Result<Vec<Instance>, AppError> {
        self.envs.get(env_id)?;
        let all = self.instances.list_by_env(env_id)?;
        let mut result = Vec::with_capacity(all.len());
        for ins in &all {
            result.push(self.reconciler.reconcile(ins).await);
        }
        Ok(result)
    }

    /// 全量实例列表（GET /api/v1/instances）：对账循环与 list_by_env 同构；
    /// 环境名孤儿回退见 [`Self::resolve_environment_name`]。
    pub async fn list(&self) -> Result<Vec<InstanceView>, AppError> {
        let all = self.instances.list_all()?;
        let mut result = Vec::with_capacity(all.len());
        for ins in &all {
            let environment_name = Self::resolve_environment_name(&self.envs, &ins.environment_id);
            result.push(InstanceView {
                instance: self.reconciler.reconcile(ins).await,
                environment_name,
            });
        }
        Ok(result)
    }

    /// 环境名解析：环境已删（孤儿行）→ 回退 environment_id 字符串，不阻断全量列表。
    fn resolve_environment_name(envs: &EnvironmentRepository, environment_id: &str) -> String {
        envs.get(environment_id)
            .map(|e| e.name)
            .unwrap_or_else(|_| environment_id.to_string())
    }

    pub async fn stop(&self, id: &str) -> Result<Instance, AppError> {
        let ins = self.reconciler.reconciled(id).await?;
        match ins.status {
            InstanceStatus::Running | InstanceStatus::Starting => {}
            _ => {
                return Err(AppError::conflict(
                    "INSTANCE_NOT_RUNNING",
                    format!("实例 {id} 未在运行"),
                ))
            }
        }

        if let Some(pid) = ins.pid {
            self.process.terminate(pid)?;
        }
        self.instances.update_runtime(
            id,
            RuntimeUpdate {
                status: Some(InstanceStatus::Stopped),
                stopped_at: Some(chrono::Utc::now().timestamp_millis()),
                ..Default::default()
            },
        )?;
        self.emit("instance-stopped", id);
        tracing::info!("实例已停止: {id}");
        Ok(self.instances.get(id)?)
    }

    /// stop-all：停止该环境所有运行中实例。
    /// 幂等停止单元：对账/退出回调竞态导致的状态抖动跳过而非 409。
    pub async fn stop_all_for_env(&self, env_id: &str) -> Result<Vec<String>, AppError> {
        let list = self.list_by_env(env_id).await?;
        let mut stopped = Vec::new();
        for ins in list {
            if !ins.status.is_alive_status() {
                continue;
            }
            self.keep_alive.cancel(&ins.id);
            if let Some(pid) = ins.pid {
                self.process.terminate(pid)?;
            }
            self.instances.update_runtime(
                &ins.id,
                RuntimeUpdate {
                    status: Some(InstanceStatus::Stopped),
                    stopped_at: Some(chrono::Utc::now().timestamp_millis()),
                    ..Default::default()
                },
            )?;
            self.emit("instance-stopped", &ins.id);
            stopped.push(ins.id);
        }
        Ok(stopped)
    }

    /// restart = stop + start
    pub async fn restart(&self, id: &str) -> Result<Instance, AppError> {
        self.stop(id).await?;
        self.start(id).await
    }

    /// CDP endpoint：运行中才可获取；webSocketUrl 透传给 Agent
    pub async fn get_cdp(&self, id: &str) -> Result<CdpEndpoint, AppError> {
        let ins = self.reconciler.reconciled(id).await?;
        if ins.status != InstanceStatus::Running {
            return Err(AppError::conflict(
                "INSTANCE_NOT_RUNNING",
                format!("实例 {id} 未在运行"),
            ));
        }
        let port = ins.cdp_port.ok_or_else(|| AppError::internal("实例缺少 CDP 端口"))?;
        let version = cdp::client::version(port).await.map_err(|_| {
            AppError::business(500, "CDP_CONNECTION_FAILED", format!("CDP 端口 {port} 不可达"))
        })?;
        Ok(CdpEndpoint {
            instance_id: ins.id,
            host: "127.0.0.1".to_string(),
            port,
            http_url: format!("http://127.0.0.1:{port}"),
            web_socket_url: version.web_socket_url,
        })
    }

    /// focus：运行中实例唤出窗口。CDP 激活优先（精准定位实例），失败回退平台 API。
    pub async fn focus(&self, id: &str) -> Result<Instance, AppError> {
        let ins = self.reconciler.reconciled(id).await?;
        if ins.status != InstanceStatus::Running {
            return Err(AppError::conflict(
                "INSTANCE_NOT_RUNNING",
                format!("实例 {id} 未在运行，无法唤出"),
            ));
        }
        let port = ins.cdp_port.ok_or_else(|| AppError::internal("实例缺少 CDP 端口"))?;

        let activated = (|| async {
            let targets = cdp::client::list_targets(port).await.ok()?;
            let page = targets.iter().find(|t| t.target_type == "page")?;
            cdp::client::activate_target(port, &page.id).await.ok()
        })()
        .await
        .unwrap_or(false);

        if !activated {
            if let Some(pid) = ins.pid {
                platform::bring_to_front(pid)?;
            }
        }
        Ok(ins)
    }

    // ---------- Tab / Navigate API----------

    /// 运行中实例的前置校验：返回 (实例, cdp 端口)
    async fn require_running(&self, id: &str) -> Result<(Instance, u16), AppError> {
        let ins = self.reconciler.reconciled(id).await?;
        if ins.status != InstanceStatus::Running {
            return Err(AppError::conflict(
                "INSTANCE_NOT_RUNNING",
                format!("实例 {id} 未在运行"),
            ));
        }
        let port = ins.cdp_port.ok_or_else(|| AppError::internal("实例缺少 CDP 端口"))?;
        Ok((ins, port))
    }

    /// GET tabs：仅返回 type == page 的目标
    pub async fn list_tabs(&self, id: &str) -> Result<Vec<cdp::client::Target>, AppError> {
        let (_, port) = self.require_running(id).await?;
        let targets = cdp::client::list_targets(port).await?;
        Ok(targets.into_iter().filter(|t| t.target_type == "page").collect())
    }

    fn validate_http_url(url: &str) -> Result<(), AppError> {
        let parsed = url::Url::parse(url)
            .map_err(|_| AppError::invalid_request(format!("URL 不合法: {url}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(AppError::invalid_request("URL 必须 http/https 开头"));
        }
        Ok(())
    }

    /// POST tabs：新建标签页
    pub async fn open_tab(&self, id: &str, url: &str) -> Result<cdp::client::Target, AppError> {
        Self::validate_http_url(url)?;
        let (_, port) = self.require_running(id).await?;
        cdp::client::new_target(port, url).await
    }

    /// POST navigate：MVP 用 new_target + close_target 组合近似（会丢失旧页历史，
    /// API 文档已明示语义差异，Phase 2 WS Page.navigate 兕底）
    pub async fn navigate(
        &self,
        id: &str,
        tab_id: &str,
        url: &str,
    ) -> Result<cdp::client::Target, AppError> {
        Self::validate_http_url(url)?;
        let (_, port) = self.require_running(id).await?;
        let new = cdp::client::new_target(port, url).await?;
        // 旧页关闭失败（已被用户关掉等）不影响新页已打开的事实
        let _ = cdp::client::close_target(port, tab_id).await;
        Ok(new)
    }

    /// 删除：运行中 → 409 必须先 stop；清理 profile 目录 + 记录。
    pub async fn delete(&self, id: &str) -> Result<(), AppError> {
        let ins = self.reconciler.reconciled(id).await?;
        if ins.status.is_alive_status() {
            return Err(AppError::conflict(
                "INSTANCE_ALREADY_RUNNING",
                format!("实例 {id} 运行中，必须先停止再删除"),
            ));
        }
        self.keep_alive.cancel(id);

        let dir = std::path::Path::new(&ins.profile_dir);
        if let Err(e) = std::fs::remove_dir_all(dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("清理实例 profile 失败（继续删除记录）: {} - {e}", ins.profile_dir);
            }
        }
        self.instances.delete(id)?;
        self.emit("instance-deleted", id);
        tracing::info!("实例已删除: {id}");
        Ok(())
    }

    /// 进程退出监听：reaper 线程回收子进程（防僵尸）并事件化退出。
    /// 用户手动关闭 Chrome 窗口 → 此回调立即触发 → DB stopped + 事件 → UI 即时回正。
    /// pid 匹配防止旧实例的迟到回调污染 stop/restart 后的状态。
    fn attach_exit_watcher(&self, ins_id: &str, pid: u32) {
        let instances = self.instances.clone();
        let app = self.app.clone();
        let id = ins_id.to_string();
        let result = self.process.attach_exit_watcher(
            pid,
            Box::new(move |_code| {
                let Ok(row) = instances.get(&id) else { return };
                if row.pid == Some(pid) && row.status == InstanceStatus::Running {
                    let _ = instances.update_runtime(
                        &id,
                        RuntimeUpdate {
                            status: Some(InstanceStatus::Stopped),
                            stopped_at: Some(chrono::Utc::now().timestamp_millis()),
                            ..Default::default()
                        },
                    );
                    if let Err(e) = tauri::Emitter::emit(
                        &app,
                        "instance-stopped",
                        serde_json::json!({ "id": id }),
                    ) {
                        tracing::error!("发送事件 instance-stopped 失败: {e}");
                    }
                    tracing::info!("实例进程退出（事件驱动检测）: {id} pid={pid}");
                }
            }),
        );
        if let Err(e) = result {
            tracing::error!("挂载退出监听失败（退化为轮询对账）: {e}");
        }
    }

    fn mark_error(&self, id: &str) {
        let _ = self.instances.update_runtime(
            id,
            RuntimeUpdate { status: Some(InstanceStatus::Error), ..Default::default() },
        );
        self.emit("instance-error", id);
    }

    /// emit 与 Activity 落库同点：服务层禁止直接调 app.emit
    fn emit(&self, event: &str, id: &str) {
        self.emit_with(event, id, "");
    }

    fn emit_with(&self, event: &str, id: &str, message: &str) {
        self.activity.record("info", event, None, Some(id), message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::db::client::{init_pool, DbPool};
    use crate::infrastructure::db::schema::init_schema;

    /// 最小装配：仅仓储层（临时 DB），不构造 InstanceService。
    /// 取舍说明：InstanceService → Reconciler → Activity 依赖 tauri AppHandle（进程
    /// 事件出口），单元测试拿不到；因此收敛为
    /// 「list_all + 孤儿环境名回退」的等价验证。resolve_environment_name 即
    /// list() 内联使用的生产代码，非测试副本；对账循环与 list_by_env 同构、无新增分支。
    struct Ctx {
        pool: DbPool,
        envs: EnvironmentRepository,
        instances: InstanceRepository,
    }

    fn setup() -> Ctx {
        let dir = std::env::temp_dir().join(format!("cem-ins-svc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = init_pool(&dir.join("test.db")).unwrap();
        init_schema(&pool).unwrap();
        Ctx {
            pool: pool.clone(),
            envs: EnvironmentRepository::new(pool.clone()),
            instances: InstanceRepository::new(pool),
        }
    }

    fn sample_ins(id: &str, env_id: &str, created_at: i64) -> Instance {
        Instance {
            id: id.into(),
            environment_id: env_id.into(),
            login_profile_id: None,
            profile_dir: "/tmp/x".into(),
            pid: None,
            cdp_port: Some(9333),
            status: InstanceStatus::Stopped,
            host_rules: None,
            browser_version: None,
            started_at: None,
            stopped_at: None,
            created_at,
            updated_at: created_at,
        }
    }

    fn sample_env(id: &str, name: &str) -> Environment {
        Environment {
            id: id.into(),
            name: name.into(),
            hosts_source_url: None,
            icon: None,
            startup_args: None,
            keep_alive: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    /// 孤儿行回退：一个实例指向已删环境 → 全量 2 条，孤儿行 environment_name == env_id。
    /// 注意：DB 开了外键约束，且生产删环境走应用层级联（environment_service::delete
    /// 先清实例行），孤儿行只可能来自异常路径/历史数据——故测试在持连接上临时关 FK
    /// 直接制造孤儿行（用后恢复），验证的是 list() 的防御性回退而非常规删除路径。
    #[test]
    fn list_resolves_names_with_orphan_fallback() {
        let ctx = setup();
        ctx.envs.insert(&sample_env("env_alive", "qa-a")).unwrap();
        ctx.envs.insert(&sample_env("env_gone", "qa-b")).unwrap();
        ctx.instances.insert(&sample_ins("ins_a", "env_alive", 1)).unwrap();
        ctx.instances.insert(&sample_ins("ins_b", "env_gone", 2)).unwrap();
        // 绕过外键直接删环境行，复现「实例行的 environment_id 悬挂」的孤儿场景
        let guard = ctx.pool.get().unwrap();
        guard.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        guard.execute("DELETE FROM environments WHERE id = 'env_gone'", []).unwrap();
        guard.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        drop(guard);

        let all = ctx.instances.list_all().unwrap();
        assert_eq!(all.len(), 2, "环境删除不清实例行，全量列表应返回 2 条");
        let names: Vec<(String, String)> = all
            .iter()
            .map(|ins| {
                (
                    ins.id.clone(),
                    InstanceService::resolve_environment_name(&ctx.envs, &ins.environment_id),
                )
            })
            .collect();
        assert_eq!(names[0], ("ins_a".into(), "qa-a".into()), "正常行取环境名");
        assert_eq!(
            names[1],
            ("ins_b".into(), "env_gone".into()),
            "孤儿行回退 env_id"
        );
    }

    /// InstanceView 序列化契约：flatten 平铺 + environmentName camelCase（PRD cli.md §7.2）
    #[test]
    fn instance_view_serializes_flat_camel_case() {
        let view = InstanceView {
            instance: sample_ins("ins_a", "env_alive", 1),
            environment_name: "qa-a".into(),
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["environmentName"], "qa-a");
        assert_eq!(
            json["environmentId"], "env_alive",
            "Instance 字段平铺且 camelCase"
        );
        assert!(json.get("instance").is_none(), "不得出现嵌套 instance 键");
    }
}
