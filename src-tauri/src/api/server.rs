//! axum HTTP 服务（Agent API）：双监听器共享同一路由集合。
//!
//! - 17890（本地）：免鉴权，现状不变——GUI / 本地 CLI / 本地 MCP 零回归；
//! - 17891（隧道落地）：全路由 Bearer token（`/cdp/` 会话路径豁免）。
//!
//! TCP 层无法区分本地与隧道流量（ssh 代连），监听端口是唯一判别器。
//! 端口被占时各自独立降级禁用并留痕，不 crash。
//! 路由为薄壳，业务全部走 application 服务层。

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tauri::AppHandle;
use tower_http::cors::CorsLayer;

/// CORS 白名单（P0 收敛）：仅 GUI webview 与本地 dev vite 源。
/// 无 Origin 头的客户端（curl/CLI/agent/MCP）不进 CORS 评定，不受影响；
/// 本机任意网页（恶意 Origin）不再拿到 allow-origin 回包，浏览器端拦截成立。
/// （此前 AGENT-API 接入注意事项第 5 条的「CORS 放行任意 Origin」风险面就此收敛）
const ALLOWED_ORIGINS: [&str; 5] = [
    "tauri://localhost",      // macOS GUI webview
    "http://tauri.localhost", // Windows GUI webview
    "http://localhost",       // Linux webview Origin（无端口形态）
    "http://localhost:1420",  // dev vite（tauri.conf devUrl）
    "http://127.0.0.1:1420",  // vite 备用回环源
];

use crate::application::activity::Activity;
use crate::application::cdp_session::CdpSessionService;
use crate::application::diagnostics_service::DiagnosticsService;
use crate::application::environment_service::EnvironmentService;
use crate::application::extension_service::ExtensionService;
use crate::application::health_service::HealthService;
use crate::application::instance_service::InstanceService;
use crate::application::page_service::PageService;
use crate::application::profile_service::ProfileService;
use crate::application::session_service::SessionService;
use crate::application::settings_service::SettingsService;
use crate::application::tunnel::TunnelService;
use crate::infrastructure::kernel::KernelManager;

pub const API_PORT: u16 = 17890;
/// 隧道落地监听端口（ssh -R 的本地目标，与应用层 FORWARD_SPEC 一致）
pub const REMOTE_API_PORT: u16 = 17891;

/// axum 共享状态（Arc 内部字段，clone 廉价）
#[derive(Clone)]
pub struct ApiState {
    pub env_service: Arc<EnvironmentService>,
    pub instance_service: Arc<InstanceService>,
    pub profile_service: Arc<ProfileService>,
    pub session_service: Arc<SessionService>,
    pub settings_service: Arc<SettingsService>,
    pub extension_service: Arc<ExtensionService>,
    /// 常驻诊断（M6）：console/network/screenshot/har 查询面
    pub diagnostics: Arc<DiagnosticsService>,
    /// page 控制（P2-C）：snap/click/fill/press/wait/dialogs 工具面
    pub page: Arc<PageService>,
    pub activity: Arc<Activity>,
    pub health: Arc<HealthService>,
    pub kernel: Arc<KernelManager>,
    pub tunnel: Arc<TunnelService>,
    pub sessions: Arc<CdpSessionService>,
    pub app: AppHandle,
}

fn cors() -> CorsLayer {
    // 前端 webview（tauri://localhost）与 dev vite 同源访问；回环服务仅本机可达。
    // 收敛为白名单（见 ALLOWED_ORIGINS 注释）；methods/headers 维持放行——
    // 鉴别面收敛在 Origin，方法/头枚举只增加无意义维护成本
    CorsLayer::new()
        .allow_origin(tower_http::cors::AllowOrigin::list(
            ALLOWED_ORIGINS
                .iter()
                .map(|o| o.parse().expect("常量 Origin 必可解析")),
        ))
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any)
}

/// 全量业务路由（无鉴权层）：两个监听器共享，仅中间件差异。
fn routes(state: ApiState) -> Router {
    Router::new()
        // MCP：rmcp Streamable HTTP，客户端配 http://127.0.0.1:17890/mcp 即用
        .route_service(
            crate::api::mcp::MCP_PATH,
            crate::api::mcp::mcp_service(state.clone()),
        )
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
            "/api/v1/environments/{id}/login-profile/capture",
            post(crate::api::login_profiles::capture),
        )
        .route(
            "/api/v1/environments/{id}/login-profile/reset",
            post(crate::api::login_profiles::reset),
        )
        .route(
            "/api/v1/environments/{id}/session",
            get(crate::api::session::status),
        )
        .route(
            "/api/v1/environments/{id}/session/restore",
            post(crate::api::session::restore),
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
        .route(
            "/api/v1/instances/{id}/status",
            get(crate::api::instances::status),
        )
        .route(
            "/api/v1/instances/{id}/start",
            post(crate::api::instances::start),
        )
        .route(
            "/api/v1/instances/{id}/stop",
            post(crate::api::instances::stop),
        )
        .route(
            "/api/v1/instances/{id}/restart",
            post(crate::api::instances::restart),
        )
        .route(
            "/api/v1/instances/{id}/focus",
            post(crate::api::instances::focus),
        )
        .route(
            "/api/v1/instances/{id}/cdp",
            get(crate::api::instances::cdp),
        )
        .route(
            "/api/v1/instances/{id}/diagnostics",
            get(crate::api::instances::diagnostics),
        )
        .route(
            "/api/v1/instances/{id}/console",
            get(crate::api::instances::console),
        )
        .route(
            "/api/v1/instances/{id}/network",
            get(crate::api::instances::network),
        )
        .route(
            "/api/v1/instances/{id}/screenshot",
            get(crate::api::instances::screenshot),
        )
        .route(
            "/api/v1/instances/{id}/har",
            get(crate::api::instances::har),
        )
        .route(
            "/api/v1/instances/{id}/tabs",
            get(crate::api::instances::tabs).post(crate::api::instances::open_tab),
        )
        .route(
            "/api/v1/instances/{id}/navigate",
            post(crate::api::instances::navigate),
        )
        // page 控制命令族（P2-C）：handler 薄透传，语义全在 page_service
        .route(
            "/api/v1/instances/{id}/page/snap",
            post(crate::api::pages::snap),
        )
        .route(
            "/api/v1/instances/{id}/page/click",
            post(crate::api::pages::click),
        )
        .route(
            "/api/v1/instances/{id}/page/fill",
            post(crate::api::pages::fill),
        )
        .route(
            "/api/v1/instances/{id}/page/press",
            post(crate::api::pages::press),
        )
        .route(
            "/api/v1/instances/{id}/page/wait",
            post(crate::api::pages::wait),
        )
        .route(
            "/api/v1/instances/{id}/page/dialogs",
            get(crate::api::pages::dialogs),
        )
        // page 控制命令族（P2-G）：eval/html/clickxy/loadall/type——cdp.mjs
        // 退役前置补齐，handler 同样薄透传
        .route(
            "/api/v1/instances/{id}/page/eval",
            post(crate::api::pages::eval),
        )
        .route(
            "/api/v1/instances/{id}/page/html",
            post(crate::api::pages::html),
        )
        .route(
            "/api/v1/instances/{id}/page/clickxy",
            post(crate::api::pages::clickxy),
        )
        .route(
            "/api/v1/instances/{id}/page/loadall",
            post(crate::api::pages::loadall),
        )
        .route(
            "/api/v1/instances/{id}/page/type",
            post(crate::api::pages::r#type),
        )
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
            "/api/v1/extensions/{id}/config",
            get(crate::api::extensions::get_config).put(crate::api::extensions::put_config),
        )
        .route(
            "/api/v1/environments/{id}/activity",
            get(crate::api::environments::activity),
        )
        .route("/api/v1/kernel/status", get(crate::api::kernel::status))
        .route(
            "/api/v1/kernel/download",
            post(crate::api::kernel::download),
        )
        .route("/api/v1/kernel/cancel", post(crate::api::kernel::cancel))
        // 远程接入：隧道状态与令牌轮换（17891 上受 token 中间件保护）
        .route(
            "/api/v1/remote-access/status",
            get(crate::api::remote_access::status),
        )
        .route(
            "/api/v1/remote-access/token",
            post(crate::api::remote_access::rotate_token),
        )
        // CDP 会话代理（两个监听器都挂：17890 免鉴权本地可用，17891 豁免 Bearer、
        // 会话 key 即凭证——见 auth.rs；非 /api/v1 前缀：与改写后的 WS 路径一致）
        .route(
            "/cdp/{instance_id}/{session_id}/{*rest}",
            get(crate::api::cdp_proxy::proxy),
        )
        // CDP 会话发放（需 token，17891 上）
        .route(
            "/api/v1/instances/{id}/cdp/sessions",
            post(crate::api::instances::create_cdp_session),
        )
        // 业务事件 SSE 流（17890 免鉴权；17891 全路由 Bearer 天然覆盖）
        .route("/api/v1/events", get(crate::api::events::events))
        // CLI status/doctor 共用健康快照
        .route("/api/v1/health", get(crate::api::health::get))
        .layer(cors())
        .with_state(state)
}

/// 本地监听器路由（17890，免鉴权——现状不变）
pub fn router(state: ApiState) -> Router {
    routes(state)
}

/// 隧道落地监听器路由（17891，全路由 Bearer token；`/cdp/` 豁免见 auth.rs）
pub fn remote_router(state: ApiState) -> Router {
    let auth = axum::middleware::from_fn_with_state(state.clone(), crate::api::auth::require_token);
    routes(state).layer(auth)
}

/// 带重试的端口绑定：dev watcher 重启窗口期老进程释放有延迟，立即降级会让本进程
/// 永久无 API。≤30s 后仍失败返回 None（调用方降级禁用并留痕，不 crash）。
async fn bind_with_retry(port: u16) -> Option<tokio::net::TcpListener> {
    const MAX_ATTEMPTS: usize = 60;
    for attempt in 1..=MAX_ATTEMPTS {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => return Some(l),
            Err(e) if attempt < MAX_ATTEMPTS => {
                if attempt == 1 || attempt % 20 == 0 {
                    tracing::warn!(
                        "端口 {port} 暂被占用（第 {attempt}/{MAX_ATTEMPTS} 次），500ms 后重试: {e}"
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(e) => {
                tracing::error!("端口 {port} 监听失败（重试 {MAX_ATTEMPTS} 次仍占用）: {e}");
                return None;
            }
        }
    }
    None
}

/// 启动本地 Agent API（127.0.0.1:17890）。失败 → false（降级禁用，调用方留痕）。
pub async fn serve(state: ApiState) -> bool {
    let Some(listener) = bind_with_retry(API_PORT).await else {
        return false;
    };
    tracing::info!("Agent API 已启动: http://127.0.0.1:{API_PORT}");
    let _ = axum::serve(listener, router(state)).await;
    true
}

/// 启动隧道落地监听器（127.0.0.1:17891）。失败 → false：仅远程接入不可用，
/// 本地 17890 不受影响（独立降级）。
pub async fn serve_remote(state: ApiState) -> bool {
    let Some(listener) = bind_with_retry(REMOTE_API_PORT).await else {
        return false;
    };
    tracing::info!("远程接入监听器已启动: http://127.0.0.1:{REMOTE_API_PORT}（需 Bearer token）");
    let _ = axum::serve(listener, remote_router(state)).await;
    true
}
