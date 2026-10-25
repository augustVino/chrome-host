use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tauri::AppHandle;
use uuid::Uuid;

use crate::domain::environment::Environment;
use crate::domain::instance::{self as instance_domain, Instance, InstanceStatus, RuntimeUpdate};
use crate::error::AppError;
use crate::infrastructure::cdp;
use crate::infrastructure::cdp::ws::CdpConnection;
use crate::infrastructure::db::repositories::environment_repository::EnvironmentRepository;
use crate::infrastructure::db::repositories::extension_config_repository::ExtensionConfigRepository;
use crate::infrastructure::db::repositories::instance_repository::InstanceRepository;
use crate::infrastructure::hosts;
use crate::infrastructure::kernel::KernelManager;
use crate::infrastructure::paths::AppPaths;
use crate::infrastructure::platform;
use crate::infrastructure::process::port_allocator;
use crate::infrastructure::process::{LaunchSpec, ProcessManager};
use crate::infrastructure::session_vault;

use super::activity::Activity;
use super::diagnostics_service::DiagnosticsService;
use super::extension_service::ExtensionService;
use super::keep_alive::KeepAliveWatcher;
use super::profile_service::ProfileService;
use super::reconciler::Reconciler;
use super::session_service::SessionService;
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
    sessions: Arc<SessionService>,
    activity: Arc<Activity>,
    keep_alive: Arc<KeepAliveWatcher>,
    extensions: Arc<ExtensionService>,
    /// 常驻诊断（M6）：置 running 后 spawn watcher，不阻塞启动路径
    diagnostics: Arc<DiagnosticsService>,
    /// 扩展运行时配置装配（resolve_for_runtime 入参；读失败按空 map 降级）
    extension_configs: Arc<ExtensionConfigRepository>,
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
        sessions: Arc<SessionService>,
        activity: Arc<Activity>,
        keep_alive: Arc<KeepAliveWatcher>,
        extensions: Arc<ExtensionService>,
        diagnostics: Arc<DiagnosticsService>,
        extension_configs: Arc<ExtensionConfigRepository>,
        settings: Arc<SettingsService>,
        paths: AppPaths,
        app: AppHandle,
    ) -> Self {
        InstanceService {
            envs,
            instances,
            kernel,
            process,
            reconciler,
            profiles,
            sessions,
            activity,
            keep_alive,
            extensions,
            diagnostics,
            extension_configs,
            settings,
            paths,
            app,
        }
    }

    /// 创建并启动 Instance。创建非幂等：每调用一次新增一个 Instance。
    /// name 可选（校验见 domain::normalize_name）；同环境内唯一，
    /// 先查后插预检 + DB 唯一索引兑底双路径。
    pub async fn create(&self, env_id: &str, name: Option<&str>) -> Result<Instance, AppError> {
        let env = self.envs.get(env_id)?;
        let name = instance_domain::normalize_name(name)?;
        if let Some(n) = &name {
            if self.instances.name_exists(env_id, n)? {
                return Err(AppError::conflict(
                    "INSTANCE_NAME_CONFLICT",
                    format!("实例名 {n} 在环境 {} 内已存在", env.name),
                ));
            }
        }

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
            name,
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
        if let Err(e) = self.instances.insert(&draft) {
            // 兑底：并发创建同名实例漏过预检窗口时由 DB 唯一索引拦下，
            // 统一映射 409（实例表仅 PK 与 env+name 两个唯一约束，PK 为 uuid
            // 生成不可能碰撞，可安全归类）
            return Err(map_insert_unique_violation(e, &draft.name));
        }
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
        let port = ins
            .cdp_port
            .ok_or_else(|| AppError::internal("实例缺少 CDP 端口"))?;
        if port_allocator::is_free(port).await {
            return Ok(());
        }
        // 可能是本实例旧进程未退透：终止并等待端口释放（≤3s）
        if let Some(pid) = ins.pid {
            if self
                .process
                .is_alive(pid, std::path::Path::new(&ins.profile_dir))?
            {
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
    async fn launch_and_wait(
        &self,
        env: &Environment,
        ins: &Instance,
    ) -> Result<Instance, AppError> {
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
                RuntimeUpdate {
                    host_rules: Some(rules.to_snapshot_json()),
                    ..Default::default()
                },
            )?;
            self.emit_with(
                "hosts-resolved",
                &ins.id,
                &format!("{} 条映射", rules.len()),
            );
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
            extra_args.push(format!(
                "--host-resolver-rules={}",
                rules.to_resolver_rules()
            ));
            extra_args.push("--no-proxy-server".to_string());
        }
        if let Some(args) = &env.startup_args {
            extra_args.extend(args.split_whitespace().map(str::to_string));
        }

        // 扩展加载：System（物化管道注入运行时配置）+ enabled 用户扩展，统一由
        // ExtensionService 解析；全部不可用时不含 --load-extension，不阻断启动。
        // 配置查表失败 → 空 map 降级（扩展照常加载，仅无 per-env 配置）
        let chrome_index = self
            .instances
            .list_by_env(&env.id)?
            .iter()
            .position(|i| i.id == ins.id)
            .map(|p| p + 1)
            .unwrap_or(0);
        let ext_configs = self
            .extension_configs
            .resolve_map_for_env(&env.id)
            .unwrap_or_else(|e| {
                tracing::warn!("扩展配置读取失败（按空配置继续）: {e}");
                Default::default()
            });
        let ext_paths = self
            .extensions
            .resolve_for_runtime(env, ins, chrome_index, &ext_configs);
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
        self.instances.update_runtime(
            &ins.id,
            RuntimeUpdate {
                pid: Some(pid),
                ..Default::default()
            },
        )?;

        // 5. 等 CDP 就绪（不能仅凭 spawn 成功判断 Runtime 成功）
        match cdp::client::wait_until_ready(ins.cdp_port.unwrap_or_default(), CDP_READY_TIMEOUT)
            .await
        {
            Ok(v) => {
                // 身份层注入：CDP 就绪后、置 running 前灌入保险库 Cookie。
                // 必须先于 agent 看到.running 后的首次 open_tab；失败降级——
                // 实例照常启动，仅丢失免登录（保证「实例永远能创建」）
                if let Err(e) = self.inject_session_cookies(ins).await {
                    tracing::warn!(
                        "[session] 实例 {} Cookie 注入失败（降级为空登录态启动）: {e}",
                        ins.id
                    );
                }
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
                // 清上轮导出残留（P0-3）：export 文件生命周期 = 本轮实例——
                // 崩溃场景（无新导出）不会把上轮 Cookie 当本轮新代提升
                let stale = self.paths.instance_cookies(&ins.environment_id, &ins.id);
                match std::fs::remove_file(&stale) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => tracing::warn!(
                        "[session] 清理上轮导出残留失败（忽略）: {} - {e}",
                        stale.display()
                    ),
                }
                // 常驻诊断 watcher（M6）：置 running 后 spawn，不阻塞启动返回路径；
                // 只订阅不操作，与外部 agent 的 CDP 会话共存
                self.diagnostics
                    .spawn(&ins.id, ins.cdp_port.unwrap_or_default());
                tracing::info!(
                    "实例启动成功: {} pid={pid} cdp={}",
                    ins.id,
                    ins.cdp_port.unwrap_or_default()
                );
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
            // 身份层挂钩 A：terminate 前经 CDP 导出实例 Cookie（尽力而为，不阻塞停止）
            self.sessions.on_instance_stopping(&ins).await;
            self.process.terminate(pid)?;
        }
        // CAS 停止置位：退出 watcher 可能已抢先（terminate 后 reaper 回调先到），
        // 抢不到则事件/promote 由 watcher 持有，本路径不重复执行
        if self
            .instances
            .mark_stopped_if_alive(id, chrono::Utc::now().timestamp_millis())?
        {
            self.emit("instance-stopped", id);
            tracing::info!("实例已停止: {id}");
            // 身份层挂钩 B：按 session_capture 策略升级环境身份（first-login/always）
            self.sessions
                .on_instance_stopped(&ins.environment_id, id)
                .await;
        }
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
            // 身份层挂钩（同 stop：导出 → 停止 → 策略升级）；CAS 置位防与
            // 退出 watcher 双 promote（见 stop 的注释）
            self.sessions.on_instance_stopping(&ins).await;
            if let Some(pid) = ins.pid {
                self.process.terminate(pid)?;
            }
            if self
                .instances
                .mark_stopped_if_alive(&ins.id, chrono::Utc::now().timestamp_millis())?
            {
                self.emit("instance-stopped", &ins.id);
                self.sessions
                    .on_instance_stopped(&ins.environment_id, &ins.id)
                    .await;
            }
            stopped.push(ins.id);
        }
        Ok(stopped)
    }

    /// restart = stop + start
    pub async fn restart(&self, id: &str) -> Result<Instance, AppError> {
        self.stop(id).await?;
        self.start(id).await
    }

    /// 页面诊断查询（M5）：对第一个 http(s) page target 做一次短连接 evaluate，
    /// 透传页面扩展挂载的 __chromeHostDiagnostics 快照。连接即用即弃（M1 形态，
    /// 常驻 console/network 订阅是 M6 范围）。
    /// 409 DIAGNOSTICS_UNAVAILABLE：实例未运行，或无 http(s) 页面目标；
    /// diagnostics: null = 页面未注入（实例启动早于扩展加载，或目标页不运行
    /// content script）。
    pub async fn diagnostics(&self, id: &str) -> Result<serde_json::Value, AppError> {
        let ins = self.reconciler.reconciled(id).await?;
        if ins.status != InstanceStatus::Running {
            return Err(AppError::conflict(
                "DIAGNOSTICS_UNAVAILABLE",
                format!("实例 {id} 未在运行，页面诊断仅对运行中实例可用"),
            ));
        }
        let port = ins
            .cdp_port
            .ok_or_else(|| AppError::internal("实例缺少 CDP 端口"))?;
        let targets = cdp::client::list_targets(port).await?;
        let target = pick_diagnostic_target(&targets).ok_or_else(|| {
            AppError::conflict(
                "DIAGNOSTICS_UNAVAILABLE",
                "无可诊断的页面目标（需先在实例内打开 http(s) 页面）",
            )
        })?;
        // 页面级 WS 端点为 ws://.../devtools/page/<targetId>（Chrome 稳定约定，
        // 与 /json 返回的 webSocketDebuggerUrl 同构），免扩 Target 结构
        let ws_url = format!("ws://127.0.0.1:{port}/devtools/page/{}", target.id);
        let conn = CdpConnection::connect_ws(&ws_url, Duration::from_secs(5)).await?;
        let resp: serde_json::Value = conn
            .send(
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": "__chromeHostDiagnostics",
                    "returnByValue": true
                }),
                None,
            )
            .await?;
        // result.value 缺席 = undefined（扩展未注入）；exceptionDetails 不可能
        // 出现（纯变量读取），出现连接级错误已在 send 内归为 CDP_COMMAND_FAILED
        let diagnostics = resp.get("result").and_then(|r| r.get("value")).cloned();
        Ok(serde_json::json!({
            "instanceId": ins.id,
            "targetId": target.id,
            "targetUrl": target.url,
            "diagnostics": diagnostics,
        }))
    }

    /// 身份层注入：把环境保险库 Cookie 灌入刚就绪的实例（browser 级，无需 page）。
    /// 分批防单帧超限；CookieRecord 与 Storage.setCookies 入参同构，
    /// session cookie 的 expires=-1 原样回传（这正是文件层拿不到的部分）。
    async fn inject_session_cookies(&self, ins: &Instance) -> Result<(), AppError> {
        let Some(port) = ins.cdp_port else {
            return Ok(());
        };
        let env_dir = self.paths.environment_dir(&ins.environment_id);
        let Some(vault) = session_vault::read_current(&env_dir) else {
            return Ok(()); // 从未捕获：空身份，正常路径
        };
        if vault.cookies.is_empty() {
            return Ok(());
        }
        let conn = CdpConnection::connect(port).await?;
        let params = session_vault::to_set_cookie_params(&vault.cookies);
        for chunk in params.chunks(100) {
            conn.send::<serde_json::Value>(
                "Storage.setCookies",
                serde_json::json!({ "cookies": chunk }),
                None,
            )
            .await?;
        }
        tracing::info!(
            "[session] 实例 {} 注入 {} 条身份 Cookie（gen={}）",
            ins.id,
            vault.cookies.len(),
            vault.generation
        );
        Ok(())
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
        let port = ins
            .cdp_port
            .ok_or_else(|| AppError::internal("实例缺少 CDP 端口"))?;
        let version = cdp::client::version(port).await.map_err(|_| {
            AppError::business(
                500,
                "CDP_CONNECTION_FAILED",
                format!("CDP 端口 {port} 不可达"),
            )
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
        let port = ins
            .cdp_port
            .ok_or_else(|| AppError::internal("实例缺少 CDP 端口"))?;

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
        let port = ins
            .cdp_port
            .ok_or_else(|| AppError::internal("实例缺少 CDP 端口"))?;
        Ok((ins, port))
    }

    /// 会话代理用：对账 + running 校验 + 返回 CDP 端口。与 get_cdp 的差异：
    /// 不额外请求 /json/version（代理路径每请求都会调，省一次往返；
    /// 会话发放与代理路由共用，404/409 语义与 get_cdp 一致）。
    pub async fn require_cdp_port(&self, id: &str) -> Result<u16, AppError> {
        let (_, port) = self.require_running(id).await?;
        Ok(port)
    }

    /// GET tabs：仅返回 type == page 的目标
    pub async fn list_tabs(&self, id: &str) -> Result<Vec<cdp::client::Target>, AppError> {
        let (_, port) = self.require_running(id).await?;
        let targets = cdp::client::list_targets(port).await?;
        Ok(targets
            .into_iter()
            .filter(|t| t.target_type == "page")
            .collect())
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
                tracing::warn!(
                    "清理实例 profile 失败（继续删除记录）: {} - {e}",
                    ins.profile_dir
                );
            }
        }
        // 物化扩展目录同步清理（与 profile 同生命周期；路径拼接与写入点同源）
        self.paths.remove_instance_generated_extensions(id);
        // 事件先于行删除：Activity 按 target_id 回填 environment_id，
        // 行已删则查不到（instance-deleted 的 environmentId 恒 null 的旧 bug）
        self.emit("instance-deleted", id);
        self.instances.delete(id)?;
        tracing::info!("实例已删除: {id}");
        Ok(())
    }

    /// 启动对账兑底：清 generated-extensions 下不在 instances 表的孤儿目录
    /// （删除编排补齐前的历史残留）。main 启动编排（reconcile_all 后）调用；
    /// 表读失败仅告警（对账兑底不应阻断启动）
    pub fn prune_orphan_generated_extensions(&self) {
        let live: Vec<String> = match self.instances.list_all() {
            Ok(rows) => rows.into_iter().map(|i| i.id).collect(),
            Err(e) => {
                tracing::warn!("[extensions] 实例表读取失败，跳过孤儿物化目录对账: {e}");
                return;
            }
        };
        let removed = self.paths.prune_orphan_generated_extensions(&live);
        if removed > 0 {
            tracing::info!("[extensions] 启动对账清理孤儿物化目录 {} 个", removed);
        }
    }

    /// 启动重挂常驻诊断 watcher（P0）：应用重启丢进程内 registry，对账确认
    /// 存活的实例补挂，否则 console/network 查询面假空数据。
    /// main 启动编排（reconcile_all 后）调用
    pub fn reattach_diagnostics_watchers(&self) {
        let rows = match self.instances.list_all() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("[diagnostics] 实例表读取失败，跳过 watcher 重挂: {e}");
                return;
            }
        };
        let mut n = 0;
        for ins in rows.iter().filter(|i| i.status.is_alive_status()) {
            if let Some(port) = ins.cdp_port {
                self.diagnostics.spawn(&ins.id, port);
                n += 1;
            }
        }
        if n > 0 {
            tracing::info!(
                "[diagnostics] 启动对账为 {} 个存活实例重挂常驻诊断 watcher",
                n
            );
        }
    }

    /// 进程退出监听：reaper 线程回收子进程（防僵尸）并事件化退出。
    /// 用户手动关闭 Chrome 窗口 → 此回调立即触发 → DB stopped + 事件 → UI 即时回正。
    /// pid 匹配防止旧实例的迟到回调污染 stop/restart 后的状态。
    fn attach_exit_watcher(&self, ins_id: &str, pid: u32) {
        let instances = self.instances.clone();
        let sessions = self.sessions.clone();
        let activity = self.activity.clone();
        let id = ins_id.to_string();
        let result = self.process.attach_exit_watcher(
            pid,
            Box::new(move |_code| {
                let Ok(row) = instances.get(&id) else { return };
                if row.pid == Some(pid) && row.status == InstanceStatus::Running {
                    let env_id = row.environment_id.clone();
                    // CAS 抢置位权：正常 stop 主路径可能已抢先（此时静默退场，
                    // 事件/promote 由主路径持有）；抢到 = 崩溃/用户关窗场景
                    let Ok(won) =
                        instances.mark_stopped_if_alive(&id, chrono::Utc::now().timestamp_millis())
                    else {
                        return;
                    };
                    if !won {
                        return;
                    }
                    // 事件统一走 Activity.record（SSE/落库/Tauri 三路同点；
                    // 此前直接 app.emit 导致 SSE/审计缺退出路径的 instance-stopped）
                    activity.record(
                        "info",
                        "instance-stopped",
                        Some(env_id.as_str()),
                        Some(id.as_str()),
                        "进程退出（事件驱动检测）",
                    );
                    tracing::info!("实例进程退出（事件驱动检测）: {id} pid={pid}");
                    // 身份层挂钩（P1）：仅走 on_instance_stopped 的 promote 判定——
                    // 进程已死无法导出；旧 export 已在实例启动时清除（见 launch_and_wait），
                    // 此处读到文件必为本轮 stop 主路径刚写的（正常停止），promote 照常；
                    // 崩溃场景无 export 自然跳过。reaper 同步回调经 async_runtime 调度。
                    let sessions = sessions.clone();
                    let env_id2 = row.environment_id.clone();
                    let id2 = id.clone();
                    tauri::async_runtime::spawn(async move {
                        sessions.on_instance_stopped(&env_id2, &id2).await;
                    });
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
            RuntimeUpdate {
                status: Some(InstanceStatus::Error),
                ..Default::default()
            },
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

/// insert 唯一约束错误 → 409 INSTANCE_NAME_CONFLICT（冻结契约码）。
/// 独立成色函数供单测：并发竞态兑底语义不依赖完整服务装配即可验证。
fn map_insert_unique_violation(e: AppError, name: &Option<String>) -> AppError {
    match &e {
        AppError::Db(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            let which = name.as_deref().unwrap_or("");
            AppError::conflict(
                "INSTANCE_NAME_CONFLICT",
                format!("实例名 {which} 在该环境内已存在"),
            )
        }
        _ => e,
    }
}

/// 诊断目标选择：第一个 http(s) page。过滤 chrome-extension:// 内部页（dev 环境
/// 代理扩展常驻 options 页，会抢占「第一个 page」）与 about:blank（content script
/// 不注入，读到的只有 undefined）——单页操作场景下首个 http 页即 agent 所在页面。
fn pick_diagnostic_target(targets: &[cdp::client::Target]) -> Option<&cdp::client::Target> {
    targets.iter().find(|t| {
        t.target_type == "page" && (t.url.starts_with("http://") || t.url.starts_with("https://"))
    })
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
            name: None,
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
            session_capture: Default::default(),
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
        ctx.instances
            .insert(&sample_ins("ins_a", "env_alive", 1))
            .unwrap();
        ctx.instances
            .insert(&sample_ins("ins_b", "env_gone", 2))
            .unwrap();
        // 绕过外键直接删环境行，复现「实例行的 environment_id 悬挂」的孤儿场景
        let guard = ctx.pool.get().unwrap();
        guard.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        guard
            .execute("DELETE FROM environments WHERE id = 'env_gone'", [])
            .unwrap();
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
        assert_eq!(json["name"], serde_json::Value::Null, "未命名实例 name 序列化为 null");
    }

    /// 兑底映射：唯一约束错误 → 409 INSTANCE_NAME_CONFLICT；
    /// 非约束的 DB 错误原样透传（不吞错、不误归类）。
    #[test]
    fn insert_unique_violation_maps_to_409_conflict() {
        let constraint = AppError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                extended_code: 19,
            },
            Some("UNIQUE constraint failed: instances.environment_id, instances.name".into()),
        ));
        let mapped = map_insert_unique_violation(constraint, &Some("task-1".into()));
        assert_eq!(mapped.status(), 409);
        assert_eq!(mapped.code(), "INSTANCE_NAME_CONFLICT");
        assert!(mapped.to_string().contains("task-1"), "message 携带冲突名");

        let other = AppError::Db(rusqlite::Error::InvalidColumnName("x".into()));
        let passed = map_insert_unique_violation(other, &Some("task-1".into()));
        assert_eq!(passed.status(), 500, "非约束错误原样透传（Db 默认 500）");
    }

    /// 诊断目标选择：扩展内部页与空白页让位，首个 http(s) 页胜出；
    /// 全非 http → None（调用方转 409 DIAGNOSTICS_UNAVAILABLE）
    #[test]
    fn pick_diagnostic_target_prefers_first_http_page() {
        let mk = |ty: &str, url: &str| cdp::client::Target {
            id: format!("t_{url}"),
            target_type: ty.into(),
            title: String::new(),
            url: url.into(),
        };
        let targets = vec![
            mk("page", "chrome-extension://abc/options.html"), // dev 环境代理扩展页
            mk("page", "about:blank"),
            mk("service_worker", "https://sw.example.com/sw.js"), // 非 page 类型不让位
            mk("page", "https://app.example.com/home"),
            mk("page", "http://other.example.com/late"),
        ];
        let picked = pick_diagnostic_target(&targets).unwrap();
        assert_eq!(picked.url, "https://app.example.com/home", "首个 http 页");

        assert!(pick_diagnostic_target(&[mk("page", "about:blank")]).is_none());
        assert!(pick_diagnostic_target(&[]).is_none(), "空列表 → None");
    }
}
