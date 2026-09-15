#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod application;
mod domain;
mod error;
mod infrastructure;

use std::sync::Arc;

use tauri::{Manager, WebviewUrl};

use api::server::ApiState;
use application::environment_service::EnvironmentService;
use application::extension_service::ExtensionService;
use application::health_service::HealthService;
use application::instance_service::InstanceService;
use application::keep_alive::KeepAliveWatcher;
use application::profile_service::ProfileService;
use application::activity::Activity;
use application::reconciler::Reconciler;
use application::settings_service::SettingsService;
use application::tray::{self, TrayState};
use error::AppError;
use infrastructure::db::client::DbPool;
use infrastructure::db::repositories::environment_repository::EnvironmentRepository;
use infrastructure::db::repositories::extension_repository::ExtensionRepository;
use infrastructure::db::repositories::instance_repository::InstanceRepository;
use infrastructure::db::repositories::login_profile_repository::LoginProfileRepository;
use infrastructure::kernel::KernelManager;
use infrastructure::paths::AppPaths;
use infrastructure::process::DefaultProcessManager;
use infrastructure::settings;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tauri::Builder::default()
        // 单实例保护：二次启动时唤起已有实例的主窗口并退出当前进程。
        // 必须最先注册；否则双开时后者 API 永久禁用（端口已被前者占用），行为令人困惑
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            use tauri::Manager;
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_clipboard_manager::init())
        // 应用级开机自启。自启拉起时进程带 --hidden 参数 → 主窗口不显示，仅托盘常驻
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        // Extensions 页的目录选择与 Finder 定位
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        // updater 自动更新（检查/下载/安装）+ 安装后 relaunch
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            let handle = app.handle().clone();

            // 路径 + 数据库
            let root = handle.path().app_data_dir()?;
            let paths = AppPaths::new(root);
            let pool: DbPool = infrastructure::db::client::init_pool(&paths.db_file())?;
            infrastructure::db::schema::init_schema(&pool)?;

            // 仓储
            let env_repo = Arc::new(EnvironmentRepository::new(pool.clone()));
            let ins_repo = Arc::new(InstanceRepository::new(pool.clone()));
            let lp_repo = Arc::new(LoginProfileRepository::new(pool.clone()));
            let service_pool = pool.clone(); // Activity / SettingsService 共用

            // 基础设施服务
            let kernel = Arc::new(KernelManager::new(paths.kernel_root()));
            let process: Arc<dyn infrastructure::process::ProcessManager> =
                Arc::new(DefaultProcessManager::new());
            let settings_service = Arc::new(SettingsService::new(service_pool.clone()));

            // 应用服务（reconciler 先行，env/instance 服务共享）
            let activity = Arc::new(Activity::new(handle.clone(), service_pool.clone()));
            activity.cleanup(); // app_events 超 5000 条清理旧行
            let reconciler = Arc::new(Reconciler::new(
                ins_repo.clone(),
                process.clone(),
                activity.clone(),
            ));
            let profile_service = Arc::new(ProfileService::new(
                env_repo.clone(),
                ins_repo.clone(),
                lp_repo,
                kernel.clone(),
                process.clone(),
                settings_service.clone(),
                paths.clone(),
                handle.clone(),
            ));
            let extension_service = Arc::new(ExtensionService::new(
                Arc::new(ExtensionRepository::new(service_pool.clone())),
                settings_service.clone(),
                paths.root.clone(),
                handle
                    .path()
                    .resource_dir()
                    .map_err(|e| AppError::internal(format!("resource_dir 不可用: {e}")))?,
            ));
            // 内置 system 扩展同步：目录自动发现注册 + prune 已移除目录（幂等）
            extension_service.sync_system_extensions()?;
            let keep_alive = Arc::new(KeepAliveWatcher::new(
                ins_repo.clone(),
                process.clone(),
                activity.clone(),
            ));
            // CLI 健康检查：聚合仓储计数与路径探测，
            // 仓储以 Arc 直传（服务层不反向依赖其他服务）；env_repo 随后 move 进
            // InstanceService，必须在此之前装配
            let app_version = handle.package_info().version.to_string();
            let health = Arc::new(HealthService::new(
                pool.clone(),
                kernel.clone(),
                paths.clone(),
                env_repo.clone(),
                ins_repo.clone(),
                Arc::new(ExtensionRepository::new(service_pool.clone())),
                app_version,
            ));
            let env_service = Arc::new(EnvironmentService::new(
                env_repo.clone(),
                ins_repo.clone(),
                reconciler.clone(),
                profile_service.clone(),
                activity.clone(),
                keep_alive.clone(),
            ));
            let instance_service = Arc::new(InstanceService::new(
                env_repo,
                ins_repo.clone(),
                kernel.clone(),
                process,
                reconciler.clone(),
                profile_service.clone(),
                activity.clone(),
                keep_alive.clone(),
                extension_service.clone(),
                settings_service.clone(),
                paths.clone(),
                handle.clone(),
            ));

            // 打破 Arc 循环：watcher 通过 Weak 回引服务执行重启
            keep_alive.set_service(Arc::downgrade(&instance_service));

            // 启动全量对账
            // 登录浏览器不在 instances 表，单独 reattach
            tauri::async_runtime::block_on(reconciler.reconcile_all());
            profile_service.reattach_all();
            keep_alive.scan_register();

            // 自启模式（--hidden）→ 启动即隐藏主窗口，仅托盘常驻
            if std::env::args().any(|a| a == "--hidden") {
                if let Some(main) = handle.get_webview_window("main") {
                    let _ = main.hide();
                    tracing::info!("自启模式：主窗口已隐藏，仅托盘常驻");
                }
            }

            // hide-to-tray（主窗口关闭 = 隐藏，Quit 仅在托盘菜单）
            let app_settings = settings::load(&paths.root);
            if let Some(main) = handle.get_webview_window("main") {
                let minimize_to_tray = app_settings.minimize_to_tray;
                let win = main.clone();
                main.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        if minimize_to_tray {
                            api.prevent_close();
                            let _ = win.hide();
                            tracing::info!("主窗口已隐藏到托盘（config.minimize_to_tray=true）");
                        }
                    }
                });
            }

            // 托盘（菜单数据走服务层对账路径，事件驱动防抖重建）。
            // 降级：托盘不可用（如 Linux 无 AppIndicator 环境）时主窗口入口不丢，应用照常运行
            handle.manage(TrayState {
                env_service: env_service.clone(),
                instance_service: instance_service.clone(),
            });
            if let Err(e) = tray::init(&handle) {
                tracing::warn!("托盘初始化失败（降级为主窗口入口，不影响其他功能）: {e}");
            }

            // Popover 窗口（隐藏常驻，JS 持续轮询保证弹出即最新；点击托盘图标时定位显示）。
            // 原生 Popover 材质（NSVisualEffectView）+ radius 圆角：CSS backdrop-blur 在透明窗上
            // 渲染不稳定，改用系统材质；shadow 关闭消除透明圆角处的黑边
            let popover = tauri::webview::WebviewWindowBuilder::new(
                &handle,
                tray::POPOVER_LABEL,
                WebviewUrl::App("index.html#/popover".into()),
            )
            .title("Chrome Host")
            .inner_size(380.0, 540.0)
            .resizable(false)
            .decorations(false)
            .transparent(true)
            .always_on_top(true)
            .skip_taskbar(true)
            .visible(false)
            .shadow(false)
            .on_page_load(|_window, payload| {
                if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                    tracing::info!("popover webview 加载完成");
                }
            })
            .effects(
                tauri::window::EffectsBuilder::new()
                    .effect(tauri::window::Effect::Popover)
                    .radius(12.0)
                    .build(),
            )
            .build()?;
            {
                let pop = popover.clone();
                popover.on_window_event(move |event| {
                    if let tauri::WindowEvent::Focused(false) = event {
                        let _ = pop.hide(); // 失焦自动隐藏
                    }
                });
            }

            // Agent API（axum，127.0.0.1:17890；端口占用时降级禁用）
            let api_state = ApiState {
                env_service,
                instance_service,
                profile_service,
                settings_service,
                extension_service,
                activity,
                health,
                kernel,
                app: handle,
            };
            tauri::async_runtime::spawn(async move {
                let started = api::server::serve(api_state).await;
                if !started {
                    tracing::warn!("Agent API 未启动，可在释放端口后重启应用");
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
