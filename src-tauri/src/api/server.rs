//! axum HTTP 服务（Agent API）：
//! 仅绑定 127.0.0.1:17890；端口被占时降级为禁用并留痕，不 crash。
//! 路由为薄壳，业务全部走 application 服务层。

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tauri::AppHandle;
use tower_http::cors::{Any, CorsLayer};

use crate::application::environment_service::EnvironmentService;
use crate::application::extension_service::ExtensionService;
use crate::application::health_service::HealthService;
use crate::application::instance_service::InstanceService;
use crate::application::activity::Activity;
use crate::application::profile_service::ProfileService;
use crate::application::settings_service::SettingsService;
use crate::infrastructure::kernel::KernelManager;

pub const API_PORT: u16 = 17890;

/// axum 共享状态（Arc 内部字段，clone 廉价）
#[derive(Clone)]
pub struct ApiState {
    pub env_service: Arc<EnvironmentService>,
    pub instance_service: Arc<InstanceService>,
    pub profile_service: Arc<ProfileService>,
    pub settings_service: Arc<SettingsService>,
    pub extension_service: Arc<ExtensionService>,
    pub activity: Arc<Activity>,
    pub health: Arc<HealthService>,
    pub kernel: Arc<KernelManager>,
    pub app: AppHandle,
}

pub fn router(state: ApiState) -> Router {
    // 前端 webview（tauri://localhost）与 dev vite 同源访问；回环服务仅本机可达
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        // MCP：rmcp Streamable HTTP，客户端配 http://127.0.0.1:17890/mcp 即用
        .route_service(crate::api::mcp::MCP_PATH, crate::api::mcp::mcp_service(state.clone()))
        .route(
            "/api/v1/environments",
            get(crate::api::environments::list).post(crate::api::environments::create),
        )
        .route(
            "/api/v1/environments/{id}",
            get(crate::api::environments::get)
                .patch(crate::api::environments::patch)
                .delete(crate::api::environments::delete),
        )
        .route(
            "/api/v1/environments/{id}/status",
            get(crate::api::environments::status),
        )
        .route(
            "/api/v1/environments/{id}/login-profile",
            get(crate::api::login_profiles::get),
        )
        .route(
            "/api/v1/environments/{id}/login-profile/launch",
            post(crate::api::login_profiles::launch),
        )
        .route(
            "/api/v1/environments/{id}/login-profile/capture",
            post(crate::api::login_profiles::capture),
        )
        .route(
            "/api/v1/environments/{id}/login-profile/reset",
            post(crate::api::login_profiles::reset),
        )
        .route(
            "/api/v1/environments/{environment_id}/instances",
            get(crate::api::instances::list_by_env).post(crate::api::instances::create),
        )
        .route(
            "/api/v1/environments/{environment_id}/open",
            post(crate::api::instances::open),
        )
        .route(
            "/api/v1/environments/{environment_id}/stop-all",
            post(crate::api::instances::stop_all),
        )
        .route("/api/v1/instances", get(crate::api::instances::list_all))
        .route(
            "/api/v1/instances/{id}",
            get(crate::api::instances::get).delete(crate::api::instances::delete),
        )
        .route("/api/v1/instances/{id}/status", get(crate::api::instances::status))
        .route("/api/v1/instances/{id}/start", post(crate::api::instances::start))
        .route("/api/v1/instances/{id}/stop", post(crate::api::instances::stop))
        .route("/api/v1/instances/{id}/restart", post(crate::api::instances::restart))
        .route("/api/v1/instances/{id}/focus", post(crate::api::instances::focus))
        .route("/api/v1/instances/{id}/cdp", get(crate::api::instances::cdp))
        .route(
            "/api/v1/instances/{id}/tabs",
            get(crate::api::instances::tabs).post(crate::api::instances::open_tab),
        )
        .route("/api/v1/instances/{id}/navigate", post(crate::api::instances::navigate))
        .route(
            "/api/v1/login-profiles",
            get(crate::api::login_profiles::list),
        )
        .route(
            "/api/v1/settings",
            get(crate::api::settings::get).put(crate::api::settings::put),
        )
        .route(
            "/api/v1/extensions",
            get(crate::api::extensions::list).post(crate::api::extensions::register),
        )
        .route(
            "/api/v1/extensions/{id}",
            get(crate::api::extensions::get)
                .patch(crate::api::extensions::patch)
                .delete(crate::api::extensions::delete),
        )
        .route(
            "/api/v1/environments/{id}/activity",
            get(crate::api::environments::activity),
        )
        .route("/api/v1/kernel/status", get(crate::api::kernel::status))
        .route("/api/v1/kernel/download", post(crate::api::kernel::download))
        .route("/api/v1/kernel/cancel", post(crate::api::kernel::cancel))
        // CLI status/doctor 共用健康快照
        .route("/api/v1/health", get(crate::api::health::get))
        .layer(cors)
        .with_state(state)
}

/// 启动服务。端口被占 → 重试 ≤30s（dev watcher 重启窗口期老进程释放有延迟，
/// 立即降级会让本进程永久无 API、前端所有轮询失败）→ 仍失败才降级禁用并留痕。
pub async fn serve(state: ApiState) -> bool {
    // 使用 tokio 原生异步绑定（std 阻塞 listener 注册进 tokio 会 panic）
    const MAX_ATTEMPTS: usize = 60;
    let mut listener = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match tokio::net::TcpListener::bind(("127.0.0.1", API_PORT)).await {
            Ok(l) => {
                listener = Some(l);
                break;
            }
            Err(e) if attempt < MAX_ATTEMPTS => {
                if attempt == 1 || attempt % 20 == 0 {
                    tracing::warn!(
                        "Agent API 端口 {API_PORT} 暂被占用（第 {attempt}/{MAX_ATTEMPTS} 次），500ms 后重试: {e}"
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(e) => {
                tracing::error!(
                    "Agent API 端口 {API_PORT} 监听失败（重试 {MAX_ATTEMPTS} 次仍占用），服务已禁用: {e}"
                );
                return false;
            }
        }
    }
    let listener = listener.expect("重试循环结束后 listener 必存在");
    tracing::info!("Agent API 已启动: http://127.0.0.1:{API_PORT}");
    let _ = axum::serve(listener, router(state)).await;
    true
}
