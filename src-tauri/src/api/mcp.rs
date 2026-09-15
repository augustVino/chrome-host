//! MCP over Streamable HTTP：内嵌 axum `/mcp`，随 app 启动，与 REST 同端口同进程。
//!
//! 形态变更记录：原设计为独立 Node stdio 包（scripts/mcp/，已移除）——stdio 的 server 进程
//! 由客户端拉起，结构上无法"随 app 启动"；Streamable HTTP 让客户端只配一个 url，
//! 零 node 依赖、无版本漂移。协议细节（握手/会话/SSE/版本协商）由 rmcp 官方 SDK 兜底。
//!
//! 分层纪律：本模块是 REST 之外的另一个协议视图——tool 直接调用 application 服务层
//! （与 api/*.rs handler 同级同职责），不绕过服务层；业务错误经 `tool_error` 以
//! isError + 结构化 JSON 透传 REST 语义（code/message/status），AI 可据此自行决策。

use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    serde_json::json,
    transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde::Deserialize;
use std::sync::Arc;

use crate::domain::environment::CreateEnvironmentInput;
use crate::error::AppError;

use super::server::ApiState;

/// MCP over Streamable HTTP 的 endpoint 路径（与 REST 同端口）
pub const MCP_PATH: &str = "/mcp";

/// AppError → tool 执行错误：isError 语义（tool 已执行但业务失败），
/// 非 JSON-RPC 协议错误；REST 错误码原样透传给 AI。
fn tool_error(e: AppError) -> CallToolResult {
    let (status, code, message) = match &e {
        AppError::Business(b) => (b.status, b.code.to_string(), b.message.clone()),
        other => (500u16, "INTERNAL_ERROR".to_string(), other.to_string()),
    };
    let body = json!({ "error": { "code": code, "message": message, "status": status } });
    CallToolResult::error(vec![ContentBlock::json(body).expect("序列化错误体必成功")])
}

fn json_ok<T: serde::Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let block = ContentBlock::json(value)?;
    Ok(CallToolResult::success(vec![block]))
}

/// 统一包装：服务调用成功 → 文本化 JSON；业务失败 → Ok(tool_error)（isError 语义）
async fn run<T, F, Fut>(f: F) -> Result<CallToolResult, ErrorData>
where
    T: serde::Serialize,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, AppError>>,
{
    match f().await {
        Ok(v) => json_ok(&v),
        Err(e) => Ok(tool_error(e)),
    }
}

/// 同步服务调用版（extensions/settings 等非 async 路径）
fn run_sync<T, F>(f: F) -> Result<CallToolResult, ErrorData>
where
    T: serde::Serialize,
    F: FnOnce() -> Result<T, AppError>,
{
    match f() {
        Ok(v) => json_ok(&v),
        Err(e) => Ok(tool_error(e)),
    }
}

// ---------- 参数结构（camelCase 暴露，与 REST 字段一致） ----------

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct EnvironmentArgs {
    #[schemars(description = "环境 id（env_ 前缀）")]
    environment_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct InstanceArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct CreateEnvironmentArgs {
    #[schemars(description = "环境名，如 qapub")]
    name: String,
    #[schemars(description = "可选 hosts 配置源地址（返回 hosts 格式文本的 http(s) URL）；不传则不做 hosts 注入")]
    hosts_source_url: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SetKeepAliveArgs {
    environment_id: String,
    #[schemars(description = "开启后实例意外退出自动重启（60s 窗口 3 次熔断）")]
    keep_alive: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ActivityArgs {
    environment_id: String,
    #[schemars(description = "返回条数，默认 50")]
    limit: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct OpenTabArgs {
    instance_id: String,
    #[schemars(description = "目标地址（须含协议）")]
    url: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct NavigateArgs {
    instance_id: String,
    #[schemars(description = "被替换的标签页 id")]
    tab_id: String,
    url: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct LoginProfileArgs {
    environment_id: String,
    #[schemars(description = "get=查状态/快照版本; launch=打开登录浏览器(人工登录); capture=捕获快照(浏览器须已关); reset=清空快照")]
    action: LoginProfileAction,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum LoginProfileAction {
    Get,
    Launch,
    Capture,
    Reset,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ExtensionIdArgs {
    #[schemars(description = "扩展 id（ext_ 前缀）")]
    extension_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct RegisterExtensionArgs {
    #[schemars(description = "扩展源目录的绝对路径（须含合法 manifest.json）")]
    path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SetExtensionEnabledArgs {
    extension_id: String,
    #[schemars(description = "启用/禁用；配置变化只影响之后启动的实例")]
    enabled: bool,
}

// ---------- MCP Server ----------

#[derive(Clone)]
pub struct ChromeHostMcp {
    tool_router: ToolRouter<Self>,
    state: ApiState,
}

/// 挂载到 axum `/mcp` 的 Streamable HTTP 服务（rmcp tower Service，Error=Infallible）
pub type ChromeHostMcpService = StreamableHttpService<ChromeHostMcp, LocalSessionManager>;

pub fn mcp_service(state: ApiState) -> ChromeHostMcpService {
    StreamableHttpService::new(
        move || Ok(ChromeHostMcp::new(state.clone())),
        Arc::new(LocalSessionManager::default()),
        Default::default(),
    )
}

#[tool_router]
impl ChromeHostMcp {
    pub fn new(state: ApiState) -> Self {
        Self { tool_router: Self::tool_router(), state }
    }

    // ── 内核 ────────────────────────────────────────────────────────────

    /// create_instance 的前置判断：内核未安装时实例创建会阻塞于首次下载（~200MB）
    #[tool(description = "查询浏览器内核状态（pinned 版本/本地版本/是否在下载）")]
    async fn kernel_status(&self) -> Result<CallToolResult, ErrorData> {
        run_sync(|| Ok(self.state.kernel.status()))
    }

    // ── 环境 ────────────────────────────────────────────────────────────

    #[tool(description = "列出全部环境（含运行中实例摘要）")]
    async fn list_environments(&self) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.env_service.list_summary()).await
    }

    #[tool(description = "创建环境（name + 可选 hosts 配置源）。实例起始页由 Settings 默认起始页决定，未配置则打开 about:blank")]
    async fn create_environment(
        &self,
        Parameters(a): Parameters<CreateEnvironmentArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let input = CreateEnvironmentInput { name: a.name, hosts_source_url: a.hosts_source_url, icon: None };
        run_sync(|| self.state.env_service.create(input))
    }

    #[tool(description = "删除环境（有运行中实例时 409，可先 stop_all_for_environment）")]
    async fn delete_environment(
        &self,
        Parameters(a): Parameters<EnvironmentArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| self.state.env_service.delete(&a.environment_id))
    }

    #[tool(description = "设置环境 keepAlive：实例意外退出自动重启（60s 窗口 3 次熔断）")]
    async fn set_keep_alive(
        &self,
        Parameters(a): Parameters<SetKeepAliveArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| {
            self.state.env_service.update(
                &a.environment_id,
                crate::domain::environment::UpdateEnvironmentInput {
                    keep_alive: Some(a.keep_alive),
                    ..Default::default()
                },
            )
        })
    }

    #[tool(description = "停止环境全部运行中实例（删除环境前置操作）")]
    async fn stop_all_for_environment(
        &self,
        Parameters(a): Parameters<EnvironmentArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.instance_service.stop_all_for_env(&a.environment_id)).await
    }

    #[tool(description = "查询环境最近事件流（创建/启动/停止/hosts 解析/快照等审计记录）")]
    async fn activity(
        &self,
        Parameters(a): Parameters<ActivityArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| self.state.activity.list_by_env(&a.environment_id, a.limit.unwrap_or(50)))
    }

    // ── 实例 ────────────────────────────────────────────────────────────

    #[tool(description = "为环境创建并启动完全隔离的 Chrome 实例（独立 profile + hosts 注入 + 登录态克隆）。首次运行可能阻塞于内核下载。错误时先查 kernel_status")]
    async fn create_instance(
        &self,
        Parameters(a): Parameters<EnvironmentArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.instance_service.create(&a.environment_id)).await
    }

    #[tool(description = "列出环境下的全部实例（含状态/端口/pid）")]
    async fn list_instances(
        &self,
        Parameters(a): Parameters<EnvironmentArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.instance_service.list_by_env(&a.environment_id)).await
    }

    #[tool(description = "查询单个实例详情（状态/pid/CDP 端口/profile 目录）")]
    async fn get_instance(
        &self,
        Parameters(a): Parameters<InstanceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.instance_service.get(&a.instance_id)).await
    }

    #[tool(description = "启动已停止的实例（重复 start → 409 INSTANCE_ALREADY_RUNNING）")]
    async fn start_instance(
        &self,
        Parameters(a): Parameters<InstanceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.instance_service.start(&a.instance_id)).await
    }

    #[tool(description = "停止实例（优雅终止 Chrome 进程）")]
    async fn stop_instance(
        &self,
        Parameters(a): Parameters<InstanceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.instance_service.stop(&a.instance_id)).await
    }

    #[tool(description = "重启实例（stop + start，hosts 重新拉取）")]
    async fn restart_instance(
        &self,
        Parameters(a): Parameters<InstanceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.instance_service.restart(&a.instance_id)).await
    }

    #[tool(description = "删除实例（运行中 → 409，先停止）")]
    async fn delete_instance(
        &self,
        Parameters(a): Parameters<InstanceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.instance_service.delete(&a.instance_id)).await
    }

    #[tool(description = "获取实例 CDP endpoint（含 WebSocket URL，可直接连 Chrome DevTools Protocol 做页面自动化）")]
    async fn get_cdp(
        &self,
        Parameters(a): Parameters<InstanceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.instance_service.get_cdp(&a.instance_id)).await
    }

    // ── 标签页 ──────────────────────────────────────────────────────────

    #[tool(description = "列出实例的全部页面标签（id/url/title，仅 page 类型）")]
    async fn list_tabs(
        &self,
        Parameters(a): Parameters<InstanceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.instance_service.list_tabs(&a.instance_id)).await
    }

    #[tool(description = "在实例中新开标签页并导航到指定 URL")]
    async fn open_tab(
        &self,
        Parameters(a): Parameters<OpenTabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.instance_service.open_tab(&a.instance_id, &a.url)).await
    }

    #[tool(description = "导航既有标签页到新 URL（new+close 组合近似：丢失旧页历史）")]
    async fn navigate(
        &self,
        Parameters(a): Parameters<NavigateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.instance_service.navigate(&a.instance_id, &a.tab_id, &a.url)).await
    }

    // ── 登录态 ──────────────────────────────────────────────────────────

    #[tool(description = "环境登录态管理。capture 前 login 浏览器须已关闭（运行中 → 409 PROFILE_IN_USE）")]
    async fn login_profile(
        &self,
        Parameters(a): Parameters<LoginProfileArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        match a.action {
            LoginProfileAction::Get => run(|| self.state.profile_service.view(&a.environment_id)).await,
            LoginProfileAction::Launch => run(|| self.state.profile_service.launch(&a.environment_id)).await,
            LoginProfileAction::Capture => run(|| self.state.profile_service.capture(&a.environment_id)).await,
            LoginProfileAction::Reset => run(|| self.state.profile_service.reset(&a.environment_id)).await,
        }
    }

    // ── 扩展 ────────────────────────────────────────────────────────────

    #[tool(description = "列出已注册扩展（含状态 ready/missing/invalid；system=内置锁定，user=用户注册）")]
    async fn list_extensions(&self) -> Result<CallToolResult, ErrorData> {
        run_sync(|| self.state.extension_service.list())
    }

    #[tool(description = "查询单个扩展详情（manifest 元数据）")]
    async fn get_extension(
        &self,
        Parameters(a): Parameters<ExtensionIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| self.state.extension_service.get(&a.extension_id))
    }

    #[tool(description = "注册用户扩展（服务端读 manifest 提取元数据；注册后对新启动实例生效）。安全边界：只接受显式注册，无按任意路径加载")]
    async fn register_extension(
        &self,
        Parameters(a): Parameters<RegisterExtensionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| self.state.extension_service.register(&a.path))
    }

    #[tool(description = "启用/禁用扩展（system 内置 → 403 EXTENSION_SYSTEM_LOCKED；配置变化只影响之后启动的实例）")]
    async fn set_extension_enabled(
        &self,
        Parameters(a): Parameters<SetExtensionEnabledArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| self.state.extension_service.set_enabled(&a.extension_id, a.enabled))
    }

    #[tool(description = "移除扩展注册项（不删源文件；system 内置 → 403）")]
    async fn delete_extension(
        &self,
        Parameters(a): Parameters<ExtensionIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| self.state.extension_service.delete(&a.extension_id))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ChromeHostMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                "chrome-host",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "chrome-host：为 AI Agent 提供完全隔离的 Chrome 实例编排。\
                 典型闭环：create_instance → open_tab → list_tabs 读页面标题；\
                 需要登录态时先 login_profile(launch) 人工登录再 capture。\
                 业务错误以 isError + {code,message,status} 结构化返回，可据此重试或换路径。",
            )
    }
}
