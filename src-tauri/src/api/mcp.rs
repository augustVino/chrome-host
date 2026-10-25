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
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    schemars,
    serde_json::json,
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpService,
    },
    ErrorData, ServerHandler,
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
    #[schemars(
        description = "可选 hosts 配置源地址（返回 hosts 格式文本的 http(s) URL）；不传则不做 hosts 注入"
    )]
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
    #[schemars(description = "get=查状态/快照版本; capture=捕获快照; reset=清空快照")]
    action: LoginProfileAction,
    #[schemars(
        description = "capture 可选：从指定实例捕获登录态（实例即登录源，实例须已停止）。缺省 = 传统母本捕获"
    )]
    from_instance_id: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum LoginProfileAction {
    Get,
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
struct ExtensionConfigArgs {
    #[schemars(description = "扩展 id（ext_ 前缀）")]
    extension_id: String,
    #[schemars(description = "环境 id；省略 = 全局默认配置（被 per-env 配置覆盖）")]
    environment_id: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ConsoleArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(description = "级别过滤：error/warning/info/debug/log；省略全量")]
    level: Option<String>,
    #[schemars(
        description = "按标签页过滤（targetId，由 tabs 获得）；环中存在无归属条目时不过滤并以 meta.tabFilterUnavailable 标注"
    )]
    tab: Option<String>,
    #[schemars(description = "尾部条数；缺省 100，上限 1000")]
    limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct NetworkArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(description = "true = 只看失败请求或 HTTP≥400")]
    failed_only: Option<bool>,
    #[schemars(
        description = "按标签页过滤（targetId，由 tabs 获得）；环中存在无归属条目时不过滤并以 meta.tabFilterUnavailable 标注"
    )]
    tab: Option<String>,
    #[schemars(description = "尾部条数；缺省 100，上限 1000")]
    limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SetExtensionConfigArgs {
    #[schemars(description = "扩展 id（ext_ 前缀）")]
    extension_id: String,
    #[schemars(description = "环境 id；省略 = 全局默认配置（被 per-env 配置覆盖）")]
    environment_id: Option<String>,
    #[schemars(description = "配置 JSON 对象（整体覆盖；net-rules 形状见工具描述）")]
    config: serde_json::Value,
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

// ---------- page 控制参数（P2-C，与 REST /instances/{id}/page/* 对齐） ----------

/// tab 路由参数的统一描述：targetId 或唯一前缀；省略 = 唯一 page target
/// 自动选中，多 target 409 并在 message 列出全部 targetId（P2-F F1，不猜
/// 活跃 tab）。targetId 由 list_tabs 获得。
const TAB_ARG_DESC: &str =
    "目标标签页（targetId 或唯一前缀，由 list_tabs 获得）；省略 = 唯一页面标签自动选中，多标签时报错列出全部";

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PageSnapArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PageClickArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(
        description = "点击目标：@eN 引用（上次 page_snap 产生，导航后失效需重新 snap）或 CSS 选择器"
    )]
    target: String,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PageFillArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(
        description = "填写目标：@eN 引用（上次 page_snap 产生，导航后失效需重新 snap）或 CSS 选择器"
    )]
    target: String,
    #[schemars(description = "填入文本；空串 = 仅清空")]
    value: String,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PagePressArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(
        description = "键名：Enter/Tab/Escape/单字符/组合键（Control+A、Meta+V 等）；未知 → 400 PAGE_KEY_UNKNOWN"
    )]
    key: String,
    #[schemars(description = "可选聚焦目标：@eN 引用或 CSS 选择器（先 focus 再按键）")]
    target: Option<String>,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PageWaitArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(description = "CSS 选择器")]
    selector: String,
    #[schemars(description = "等待状态：visible（缺省）/ hidden / attached / detached")]
    state: Option<String>,
    #[schemars(description = "超时时长字符串：如 \"10s\"、\"250ms\"，裸数字 = 毫秒；缺省 10s")]
    timeout: Option<String>,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PageDialogsArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
    #[schemars(
        description = "游标：只返回 sequence 严格大于 N 的条目；缺省 0 = 全部（缓冲上限 32 条）"
    )]
    since: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PageEvalArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(
        description = "任意 JS 表达式（Runtime.evaluate，returnByValue + awaitPromise）。⚠ 高权限：可执行任意 JS，与退役前的 cdp.mjs 同权限——仅在 page 原语（snap/click/fill/press/wait/html）不够用时使用"
    )]
    expression: String,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PageRawArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(
        description = "CDP 方法名（如 \"Emulation.setDeviceMetricsOverride\"、\"Target.getTargets\"）。⚠ 最高权限逃生舱：仅在 page 原语不够用时使用"
    )]
    method: String,
    #[schemars(description = "命令参数对象；缺省 = {}")]
    params: Option<serde_json::Value>,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
    #[schemars(description = "true = 发 browser 级（Target 域等）；缺省 false = 发到 tab 的 page session（Runtime/DOM/Input/Emulation）")]
    browser: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PageHtmlArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(
        description = "CSS 选择器：命中元素的 outerHTML；省略 = 整文档 documentElement.outerHTML。未命中 → 404 PAGE_ELEMENT_NOT_FOUND"
    )]
    selector: Option<String>,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PageClickxyArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(description = "视口 CSS 像素横坐标")]
    x: f64,
    #[schemars(description = "视口 CSS 像素纵坐标")]
    y: f64,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PageLoadallArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(
        description = "CSS 选择器：待循环点击的「加载更多」类按钮（每次点击前带可见性/遮挡校验）"
    )]
    selector: String,
    #[schemars(
        description = "轮询间隔时长字符串：如 \"1500ms\"、\"1.5s\"，裸数字 = 毫秒；缺省 1500ms"
    )]
    interval: Option<String>,
    #[schemars(description = "点击上限；缺省 200。达上限仍未消失时结果串明示 still present")]
    max_clicks: Option<u32>,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PageTypeArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(
        description = "非空文本（空串 → 400）：直接输入到当前焦点元素（Input.insertText，无定位无校验）"
    )]
    text: String,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PageScreenshotArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(
        description = "截图目标：@eN 引用（上次 page_snap 产生）或 CSS 选择器；省略 = 当前视口整幅。元素路径先滚动入视口再截其包围盒（跨行元素取并集）"
    )]
    target: Option<String>,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PagePdfArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(description = "横向打印；缺省 false（纵向）")]
    landscape: Option<bool>,
    #[schemars(description = "打印背景色/背景图；缺省 true")]
    print_background: Option<bool>,
    #[schemars(description = "纸宽（英寸）；缺省 8.5（Letter）")]
    paper_width: Option<f64>,
    #[schemars(description = "纸高（英寸）；缺省 11（Letter）")]
    paper_height: Option<f64>,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PageEmulateViewport {
    #[schemars(description = "视口宽（CSS 像素，正整数）")]
    width: i64,
    #[schemars(description = "视口高（CSS 像素，正整数）")]
    height: i64,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PageEmulateArgs {
    #[schemars(description = "实例 id（ins_ 前缀）")]
    instance_id: String,
    #[schemars(
        description = "预设名：iphone-14 / iphone-se / pixel-7 / galaxy-s23 / ipad-air / desktop-1080p；未知 → 422 PAGE_EMULATE_UNKNOWN_PRESET"
    )]
    preset: Option<String>,
    #[schemars(description = "自定义视口（与 preset 互斥；mobile/touch 缺省 false）")]
    viewport: Option<PageEmulateViewport>,
    #[schemars(description = "覆盖 UA（仅 viewport 形态可配；预设自带真实 UA）")]
    user_agent: Option<String>,
    #[schemars(description = "mobile 语义；viewport 形态缺省 false")]
    mobile: Option<bool>,
    #[schemars(description = "触摸仿真；viewport 形态缺省 false")]
    touch: Option<bool>,
    #[schemars(description = "true = 清除全部仿真（viewport / UA / touch），与其他字段互斥")]
    off: Option<bool>,
    #[schemars(description = TAB_ARG_DESC)]
    tab: Option<String>,
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
        Self {
            tool_router: Self::tool_router(),
            state,
        }
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

    #[tool(
        description = "创建环境（name + 可选 hosts 配置源）。实例起始页由 Settings 默认起始页决定，未配置则打开 about:blank"
    )]
    async fn create_environment(
        &self,
        Parameters(a): Parameters<CreateEnvironmentArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let input = CreateEnvironmentInput {
            name: a.name,
            hosts_source_url: a.hosts_source_url,
            icon: None,
            session_capture: None,
        };
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
        run(|| {
            self.state
                .instance_service
                .stop_all_for_env(&a.environment_id)
        })
        .await
    }

    #[tool(description = "查询环境最近事件流（创建/启动/停止/hosts 解析/快照等审计记录）")]
    async fn activity(
        &self,
        Parameters(a): Parameters<ActivityArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| {
            self.state
                .activity
                .list_by_env(&a.environment_id, a.limit.unwrap_or(50))
        })
    }

    // ── 实例 ────────────────────────────────────────────────────────────

    #[tool(
        description = "为环境创建并启动完全隔离的 Chrome 实例（独立 profile + hosts 注入 + 登录态克隆）。首次运行可能阻塞于内核下载。错误时先查 kernel_status"
    )]
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

    #[tool(
        description = "获取实例 CDP endpoint（含 WebSocket URL，可直接连 Chrome DevTools Protocol 做页面自动化）"
    )]
    async fn get_cdp(
        &self,
        Parameters(a): Parameters<InstanceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.instance_service.get_cdp(&a.instance_id)).await
    }

    #[tool(
        description = "发放实例的 CDP 会话（远程/代理访问用，30 分钟有效）：返回 sessionId 与 baseUrl。远程客户端以 baseUrl 代理访问 CDP（GET /json/version 与 /devtools/* WebSocket，webSocketDebuggerUrl 已改写指向代理）；本地直连场景优先 get_cdp。实例须运行中（否则 409）"
    )]
    async fn create_cdp_session(
        &self,
        Parameters(a): Parameters<InstanceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| async {
            self.state
                .instance_service
                .require_cdp_port(&a.instance_id)
                .await?;
            Ok(self.state.sessions.create(&a.instance_id))
        })
        .await
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
        run(|| {
            self.state
                .instance_service
                .navigate(&a.instance_id, &a.tab_id, &a.url)
        })
        .await
    }

    // ── 页面诊断 ──────────────────────────────────────────────────

    #[tool(
        description = "读取实例当前页面诊断快照：JS 错误（error/unhandledrejection）与失败资源（status≥400 或 transferSize==0）各环形 100 条 + LCP/FCP。对第一个 http(s) 页面目标短连接求值 page-diagnostics 扩展挂载的 window.__chromeHostDiagnostics；实例未运行或无 http 页面 → 409 DIAGNOSTICS_UNAVAILABLE；diagnostics: null = 页面未注入（实例启动早于扩展加载或目标页不运行 content script）"
    )]
    async fn get_diagnostics(
        &self,
        Parameters(a): Parameters<InstanceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.instance_service.diagnostics(&a.instance_id)).await
    }

    #[tool(
        description = "常驻订阅的 console 环形缓冲（500 条，进程内态重启即清）：console API 输出 + 页面日志（含资源加载失败）。可选 level（error/warning/info/debug/log）、tab（targetId 按标签页过滤，由 tabs 获得）与 limit（缺省 100）"
    )]
    async fn get_console(
        &self,
        Parameters(a): Parameters<ConsoleArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| {
            self.state.diagnostics.console(
                &a.instance_id,
                a.level.as_deref(),
                a.tab.as_deref(),
                a.limit,
            )
        })
    }

    #[tool(
        description = "常驻订阅的请求环（1000 条，进程内态重启即清；请求完成/失败时入环）。可选 failedOnly（失败或 HTTP≥400）、tab（targetId 按标签页过滤，由 tabs 获得）与 limit；完整时序与耗时用 get_har"
    )]
    async fn get_network(
        &self,
        Parameters(a): Parameters<NetworkArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| {
            self.state.diagnostics.network(
                &a.instance_id,
                a.failed_only.unwrap_or(false),
                a.tab.as_deref(),
                a.limit,
            )
        })
    }

    #[tool(
        description = "当前页面 png 截图（base64，复用常驻诊断连接的第一个 http(s) 页面 session）。实例未运行/连接未就绪/无 http 页 → 409 DIAGNOSTICS_UNAVAILABLE"
    )]
    async fn get_screenshot(
        &self,
        Parameters(a): Parameters<InstanceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| self.state.diagnostics.screenshot(&a.instance_id)).await
    }

    #[tool(
        description = "从请求环生成 HAR 1.2 文档（har-recorder 落地点，可直接存档/回放分析）；未采集字段按规范以 -1/空集占位，_targetUrl 标注条目归属页面"
    )]
    async fn get_har(
        &self,
        Parameters(a): Parameters<InstanceArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| self.state.diagnostics.har(&a.instance_id))
    }

    // ── 登录态 ──────────────────────────────────────────────────────────

    #[tool(
        description = "环境登录态管理（查询/捕获/重置）。登录主路径 = 实例里登录后关闭实例自动保存（SessionService）；capture 用于显式固化（可选 fromInstanceId 从已停止实例捕获）"
    )]
    async fn login_profile(
        &self,
        Parameters(a): Parameters<LoginProfileArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        match a.action {
            LoginProfileAction::Get => {
                run(|| self.state.profile_service.view(&a.environment_id)).await
            }
            LoginProfileAction::Capture => match a.from_instance_id {
                Some(ins) => {
                    let env = a.environment_id.clone();
                    run(move || async move {
                        self.state
                            .session_service
                            .capture_from_instance(&env, &ins)
                            .await
                    })
                    .await
                }
                None => run(|| self.state.profile_service.capture(&a.environment_id)).await,
            },
            LoginProfileAction::Reset => {
                run(|| self.state.session_service.reset(&a.environment_id)).await
            }
        }
    }

    #[tool(description = "查环境会话身份层：策略/当前代/来源实例/Cookie 数/历史代（回滚观测面）")]
    async fn session_status(
        &self,
        Parameters(a): Parameters<EnvironmentArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| self.state.session_service.status(&a.environment_id))
    }

    // ── 扩展 ────────────────────────────────────────────────────────────

    #[tool(
        description = "列出已注册扩展（含状态 ready/missing/invalid；system=内置锁定，user=用户注册）"
    )]
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

    #[tool(
        description = "注册用户扩展（服务端读 manifest 提取元数据；注册后对新启动实例生效）。安全边界：只接受显式注册，无按任意路径加载"
    )]
    async fn register_extension(
        &self,
        Parameters(a): Parameters<RegisterExtensionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| self.state.extension_service.register(&a.path))
    }

    #[tool(
        description = "启用/禁用扩展（system 内置 → 403 EXTENSION_SYSTEM_LOCKED；配置变化只影响之后启动的实例）"
    )]
    async fn set_extension_enabled(
        &self,
        Parameters(a): Parameters<SetExtensionEnabledArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| {
            self.state
                .extension_service
                .set_enabled(&a.extension_id, a.enabled)
        })
    }

    #[tool(description = "移除扩展注册项（不删源文件；system 内置 → 403）")]
    async fn delete_extension(
        &self,
        Parameters(a): Parameters<ExtensionIdArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| self.state.extension_service.delete(&a.extension_id))
    }

    #[tool(
        description = "读取扩展运行时配置（可选 environmentId，省略 = 全局默认；未配置 → config: null）。仅声明 configFile 的 system 扩展可配置（list_extensions 的 configurable 字段）"
    )]
    async fn get_extension_config(
        &self,
        Parameters(a): Parameters<ExtensionConfigArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| {
            self.state
                .extension_service
                .get_config(&a.extension_id, a.environment_id.as_deref())
        })
    }

    #[tool(
        description = "写入扩展运行时配置（整体覆盖；可选 environmentId 省略 = 全局默认；对新启动实例生效）。net-rules 形状：{\"block\":[\"*.google-analytics.com\"],\"redirect\":[{\"from\":\"https://api.foo.com/*\",\"to\":\"http://localhost:3000/$1\"}],\"headers\":[{\"url\":\"https://app.foo.com/*\",\"set\":{\"X-Debug-Uid\":\"42\"}}]}（block/headers 域锚定可带路径通配；redirect 的 * 依序为 $1 $2…）；；语法参考（DNR）：https://developer.chrome.com/docs/extensions/reference/api/declarativeNetRequest 与匹配模式 https://developer.chrome.com/docs/extensions/develop/concepts/match-patterns（extension get 的 configDocsUrl 同源）；非法 → 400 EXTENSION_CONFIG_INVALID"
    )]
    async fn set_extension_config(
        &self,
        Parameters(a): Parameters<SetExtensionConfigArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run_sync(|| {
            let body = a.config.to_string();
            self.state.extension_service.set_config(
                &a.extension_id,
                a.environment_id.as_deref(),
                &body,
            )
        })
    }

    // ── 页面控制（P2-C）───────────────────────────────────────

    #[tool(
        description = "可访问性树快照：返回精简文本树（可交互节点带 @e1..@eN 引用，内联 role/name/value）+ refs_count + generation。操作循环的起点：snap → 用 @eN 引用 click/fill/press → 页面变化后重新 snap（导航会使全部引用失效 → 409 PAGE_REF_STALE，需重 snap 而非重试）"
    )]
    async fn page_snap(
        &self,
        Parameters(a): Parameters<PageSnapArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| {
            self.state
                .page
                .snap(&a.instance_id, a.tab.as_deref().unwrap_or(""))
        })
        .await
    }

    #[tool(
        description = "原生输入点击（mouseMoved→mousePressed→mouseReleased，非 el.click()）：target 支持 @eN 引用（上次 page_snap 产生，导航后失效需重新 snap）与 CSS 选择器两种形态；selector 路径带可见性/遮挡校验（422 PAGE_ELEMENT_NOT_VISIBLE / 409 PAGE_ELEMENT_BLOCKED）。实例须运行中（409 PAGE_SERVICE_UNAVAILABLE）"
    )]
    async fn page_click(
        &self,
        Parameters(a): Parameters<PageClickArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| {
            self.state
                .page
                .click(&a.instance_id, a.tab.as_deref().unwrap_or(""), &a.target)
        })
        .await
    }

    #[tool(
        description = "填写输入框：target 支持 @eN 引用（上次 page_snap 产生，导航后失效需重新 snap）与 CSS 选择器；聚焦→全选清空→Input.insertText→回读校验，value 空串 = 仅清空。不可编辑 → 422 PAGE_TARGET_NOT_FILLABLE；回读不一致 → 409 PAGE_FILL_VALUE_MISMATCH（写入可能已部分生效，先观察再决定）"
    )]
    async fn page_fill(
        &self,
        Parameters(a): Parameters<PageFillArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| {
            self.state.page.fill(
                &a.instance_id,
                a.tab.as_deref().unwrap_or(""),
                &a.target,
                &a.value,
            )
        })
        .await
    }

    #[tool(
        description = "按键（rawKeyDown/char/keyUp 三段，IME 安全）：key 支持 Enter/Tab/Escape/Backspace/Delete/Home/End/PageUp/PageDown/ArrowUp…/Space/F1-F12、单字符、组合键（Control+A / Meta+V / Shift+ArrowDown）；未知键名 → 400 PAGE_KEY_UNKNOWN。可选 target 先聚焦（@eN 引用或 CSS 选择器）再按键"
    )]
    async fn page_press(
        &self,
        Parameters(a): Parameters<PagePressArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| {
            self.state.page.press(
                &a.instance_id,
                a.tab.as_deref().unwrap_or(""),
                &a.key,
                a.target.as_deref(),
            )
        })
        .await
    }

    #[tool(
        description = "轮询等待元素状态（250ms 间隔）：state = visible（缺省）/ hidden / attached / detached，timeout 时长字符串（如 \"10s\"、\"250ms\"，裸数字 = 毫秒，缺省 10s）。超时 → 409 PAGE_WAIT_TIMEOUT（附最后观察态与轮询数）"
    )]
    async fn page_wait(
        &self,
        Parameters(a): Parameters<PageWaitArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| {
            self.state.page.wait(
                &a.instance_id,
                a.tab.as_deref().unwrap_or(""),
                &a.selector,
                a.state.as_deref(),
                a.timeout.as_deref(),
            )
        })
        .await
    }

    #[tool(
        description = "查询已自动应答的对话框日志（alert/confirm/prompt/beforeunload 均无条件 accept，prompt 提交默认文本）：返回 cursor + entries（sequence/type/message/handled）；since 游标增量拉取，缓冲上限 32 条。命令期间新弹的对话框会附在成功响应的 dialogs_seen 字段"
    )]
    async fn page_dialogs(
        &self,
        Parameters(a): Parameters<PageDialogsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| {
            self.state
                .page
                .dialogs(&a.instance_id, a.tab.as_deref().unwrap_or(""), a.since)
        })
        .await
    }

    #[tool(
        description = "任意 JS 求值（Runtime.evaluate，returnByValue + awaitPromise）：对象/数组 → pretty JSON 字符串，字符串原样，undefined/null → \"undefined\"/\"null\"，数字/布尔 → JS String 形态；页面抛错 → 500 PAGE_EVAL_FAILED。⚠ 高权限：可执行任意 JS（与退役前的 cdp.mjs 同权限）——仅在 page 原语（snap/click/fill/press/wait/html）不够用时使用，优先用专用原语"
    )]
    async fn page_eval(
        &self,
        Parameters(a): Parameters<PageEvalArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| {
            self.state.page.eval(
                &a.instance_id,
                a.tab.as_deref().unwrap_or(""),
                &a.expression,
            )
        })
        .await
    }

    #[tool(
        description = "任意 CDP 命令透传（D5 逃生舱）：method + params 原样发送，返回 CDP 原始 result。browser=true 发 browser 级（Target.getTargets 等），缺省发到 tab 的 page session（Runtime/DOM/Input/Emulation 全域可达）。⚠ 最高权限：仅在 page 原语不够用时使用"
    )]
    async fn page_raw(
        &self,
        Parameters(a): Parameters<PageRawArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| {
            self.state.page.raw(
                &a.instance_id,
                a.tab.as_deref().unwrap_or(""),
                &a.method,
                a.params.clone().unwrap_or(serde_json::json!({})),
                a.browser.unwrap_or(false),
            )
        })
        .await
    }

    #[tool(
        description = "读页面 HTML：selector 省略 = 整文档 documentElement.outerHTML；有 = querySelector 命中元素的 outerHTML（未命中 → 404 PAGE_ELEMENT_NOT_FOUND）。大页面响应会很大——优先 page_snap 定位后用 selector 缩小范围"
    )]
    async fn page_html(
        &self,
        Parameters(a): Parameters<PageHtmlArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| {
            self.state.page.html(
                &a.instance_id,
                a.tab.as_deref().unwrap_or(""),
                a.selector.as_deref(),
            )
        })
        .await
    }

    #[tool(
        description = "视口坐标点击（CSS 像素，三段原生鼠标事件）：无定位/可见性/遮挡校验——坐标即意图，点错不拦。跨域 iframe 内元素（selector/@eN 均不可达）的兜底点击路径，配合 page_type 输入"
    )]
    async fn page_clickxy(
        &self,
        Parameters(a): Parameters<PageClickxyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| {
            self.state
                .page
                .clickxy(&a.instance_id, a.tab.as_deref().unwrap_or(""), a.x, a.y)
        })
        .await
    }

    #[tool(
        description = "循环点击「加载更多」类按钮：selector 存在 → 原生 click（带可见性/遮挡校验）→ 等间隔（缺省 1500ms）→ 直到消失或达 maxClicks（缺省 200）；达上限仍未消失时结果串明示 still present，由调用方决定是否继续"
    )]
    async fn page_loadall(
        &self,
        Parameters(a): Parameters<PageLoadallArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| {
            self.state.page.loadall(
                &a.instance_id,
                a.tab.as_deref().unwrap_or(""),
                &a.selector,
                a.interval.as_deref(),
                a.max_clicks,
            )
        })
        .await
    }

    #[tool(
        description = "向当前焦点元素直接输入文本（Input.insertText，无定位无校验）：跨域 iframe 输入的唯一路径（eval/fill 的 selector 均不可达跨域 frame）——先用 page_clickxy 点入 iframe 获得焦点再 type；空文本 → 400"
    )]
    async fn page_type(
        &self,
        Parameters(a): Parameters<PageTypeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| {
            self.state
                .page
                .r#type(&a.instance_id, a.tab.as_deref().unwrap_or(""), &a.text)
        })
        .await
    }

    #[tool(
        description = "png 截图：target 省略 = 当前视口整幅；target 为 @eN 引用或 CSS 选择器时截该元素（与 page_click 同定位链，先滚动入视口，clip 取元素包围盒）。响应 data 为 base64 png；未命中 → 404 PAGE_ELEMENT_NOT_FOUND，几何退化 → 409 PAGE_TARGET_MOVED"
    )]
    async fn page_screenshot(
        &self,
        Parameters(a): Parameters<PageScreenshotArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| {
            self.state.page.screenshot(
                &a.instance_id,
                a.tab.as_deref().unwrap_or(""),
                a.target.as_deref(),
            )
        })
        .await
    }

    #[tool(
        description = "整页 PDF（Page.printToPDF）：landscape 缺省纵向、printBackground 缺省 true、纸张缺省 Letter 8.5×11 英寸（paperWidth/paperHeight 可调，如 A4 = 8.27×11.69）。响应 data 为 base64 pdf"
    )]
    async fn page_pdf(
        &self,
        Parameters(a): Parameters<PagePdfArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        run(|| {
            self.state.page.pdf(
                &a.instance_id,
                a.tab.as_deref().unwrap_or(""),
                a.landscape,
                a.print_background,
                a.paper_width,
                a.paper_height,
            )
        })
        .await
    }

    #[tool(
        description = "设备仿真：preset（iphone-14/iphone-se/pixel-7/galaxy-s23/ipad-air/desktop-1080p，自带真实 UA 与 mobile/touch）或 viewport+可选 userAgent/mobile/touch；off=true 恢复。只作用于该 tab 的 page session（与实例级无关）；同一 tab 重复 emulate = 覆盖（未给字段恢复默认）；连接重建/实例重启自然清除。仿真后 viewport 截图尺寸随之变化；page_pdf 输出也按仿真视口排页"
    )]
    async fn page_emulate(
        &self,
        Parameters(a): Parameters<PageEmulateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let req = crate::domain::emulate::EmulateRequest {
            preset: a.preset,
            viewport: a
                .viewport
                .map(|v| crate::domain::emulate::Viewport {
                    width: v.width,
                    height: v.height,
                }),
            user_agent: a.user_agent,
            mobile: a.mobile,
            touch: a.touch,
            off: a.off.unwrap_or(false),
        };
        run(|| self.state.page.emulate(&a.instance_id, a.tab.as_deref().unwrap_or(""), req)).await
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
                 需要登录态时在实例里人工登录后关闭实例即自动保存。\
                 业务错误以 isError + {code,message,status} 结构化返回，可据此重试或换路径。",
            )
    }
}
