//! Agent API 响应 DTO（纯数据镜像层）。
//!
//! 规矩：
//! 1. 本文件只允许 serde 属性 + 字段，**零方法零逻辑** —— 行为只属于 client/cli 层；
//! 2. 每个结构体镜像 src-tauri 服务端的真实 serde 线上形态（来源文件见各条注释），
//!    服务端改字段时必须同步本文件，否则 tests/contract.rs 契约测试先红（防静默错解）；
//! 3. CLI 只消费响应，理论上只需 Deserialize；同时派生 Serialize 供
//!    output.rs 的 `--json` 模式把 DTO「服务端原样」回写 stdout（计划 §2.5：两种模式无信息差），
//!    serde 属性两侧一致时序列化是恒等往返，不会引入形状漂移。

use serde::{Deserialize, Serialize};

/// 来源：src-tauri/src/domain/environment.rs `Environment`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    pub id: String,
    pub name: String,
    /// 动态 hosts 配置源 URL，可空
    pub hosts_source_url: Option<String>,
    pub icon: Option<String>,
    /// 附加启动参数（高级逃生口）
    pub startup_args: Option<String>,
    pub keep_alive: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 来源：src-tauri/src/application/environment_service.rs `EnvironmentSummary`。
/// flatten 平铺 Environment 字段 + 运行摘要，与 GUI/MCP 消费的同源。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentSummary {
    #[serde(flatten)]
    pub environment: Environment,
    pub running_instances: i64,
    pub total_instances: i64,
}

/// 来源：src-tauri/src/domain/instance.rs `InstanceStatus`（lowercase 全集 7 变体）。
///
/// `Unknown` 兜底变体：服务端未来新增状态时，旧 CLI 反序列化为 Unknown 而**不是报错**
/// ——契约测试之外的运行期防线（CLI 不因服务端演进直接崩溃）。
/// 命令层渲染时应把 Unknown 显示为 "unknown"，且不参与「存活」（running/starting）判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceStatus {
    Created,
    Starting,
    Running,
    Stopping,
    Stopped,
    Error,
    Crashed,
    #[serde(other)]
    Unknown,
}

/// 来源：src-tauri/src/domain/instance.rs `Instance`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Instance {
    pub id: String,
    pub environment_id: String,
    pub login_profile_id: Option<String>,
    pub profile_dir: String,
    pub pid: Option<u32>,
    pub cdp_port: Option<u16>,
    pub status: InstanceStatus,
    /// 启动时固化的 hosts 映射快照 JSON（服务端原样字符串，CLI 不解析）
    pub host_rules: Option<String>,
    pub browser_version: Option<String>,
    pub started_at: Option<i64>,
    pub stopped_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 来源：src-tauri/src/application/instance_service.rs `InstanceView`（GET /api/v1/instances）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceView {
    #[serde(flatten)]
    pub instance: Instance,
    /// 孤儿实例（环境已删）时服务端回退为 environment_id
    pub environment_name: String,
}

/// 来源：src-tauri/src/application/instance_service.rs `CdpEndpoint`（GET /api/v1/instances/{id}/cdp）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CdpEndpoint {
    pub instance_id: String,
    pub host: String,
    pub port: u16,
    pub http_url: String,
    pub web_socket_url: Option<String>,
}

/// 来源：src-tauri/src/infrastructure/cdp/client.rs `Target`
/// （POST /api/v1/instances/{id}/tabs、GET /api/v1/instances/{id}/tabs 的元素）。
/// 线上形态 `{id, type, title, url}`：`type` 是服务端显式 rename，**不是** camelCase 的
/// `targetType` —— 以服务端代码为准。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CdpTarget {
    pub id: String,
    #[serde(rename = "type")]
    pub target_type: String,
    pub title: String,
    pub url: String,
}

/// 来源：src-tauri/src/domain/extension.rs `ExtensionType`（lowercase）。
/// `Unknown` 兜底语义同 [`InstanceStatus`]：服务端加新类型时 CLI 不崩。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionType {
    System,
    User,
    #[serde(other)]
    Unknown,
}

/// 来源：src-tauri/src/domain/extension.rs `ExtensionStatus`（lowercase，读取时按文件系统惰性计算）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionStatus {
    Ready,
    Missing,
    Invalid,
    #[serde(other)]
    Unknown,
}

/// 来源：src-tauri/src/domain/extension.rs `Extension`。
/// 注意 `extension_type` 线上字段名是 `"type"`（服务端显式 rename），非 `extensionType`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Extension {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub manifest_version: Option<i64>,
    #[serde(rename = "type")]
    pub extension_type: ExtensionType,
    pub source_path: String,
    pub enabled: bool,
    pub status: ExtensionStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 来源：src-tauri/src/infrastructure/kernel/manager.rs `CftStatus`（GET /api/v1/kernel/status）。
/// pinned 单版本策略：`version` 是本地已装版本，`pinned_version` 是期望锁定版本。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CftStatus {
    pub installed: bool,
    pub version: Option<String>,
    pub pinned_version: String,
    pub upgrade_available: bool,
    pub downloading: bool,
    pub binary_path: Option<String>,
}

/// 来源：src-tauri/src/application/health_service.rs `HealthCheck`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub suggestion: Option<String>,
}

/// 来源：src-tauri/src/application/health_service.rs `Counts`。
/// 服务端为 u64；CLI 统一 i64（计数值恒为非负小整数，反序列化兼容，
/// 避免无符号类型跨层传染到后续的表格/JSON 渲染）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub environments: i64,
    pub instances: i64,
    pub running: i64,
    pub extensions: i64,
}

/// 来源：src-tauri/src/application/health_service.rs `HealthReport`（GET /api/v1/health）。
/// `status` / `doctor` 两个命令共用的数据源。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthReport {
    /// chrome-host 应用版本
    pub version: String,
    /// 全部检查通过
    pub ok: bool,
    pub checks: Vec<HealthCheck>,
    pub counts: Counts,
}

// ---------------------------------------------------------------------------
// 能力补齐端点的响应 DTO（来源见各条注释）。
// ---------------------------------------------------------------------------

/// 来源：src-tauri/src/application/activity.rs `AppEvent`
/// （GET /api/v1/environments/{id}/activity 的元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppEvent {
    pub ts: i64,
    pub level: String,
    pub event: String,
    /// 孤儿事件（环境已删）时为 null
    pub environment_id: Option<String>,
    pub target_id: Option<String>,
    pub message: String,
}

/// 来源：src-tauri/src/api/environments.rs `status` handler 内联的计数对象
/// （GET /api/v1/environments/{id}/status）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvInstanceCounts {
    pub total: i64,
    pub running: i64,
    pub starting: i64,
    pub stopped: i64,
    /// 服务端把 Error 与 Crashed 合并计入本档（match 分支即如此）
    pub error: i64,
}

/// 来源：src-tauri/src/api/environments.rs `status` handler 的响应形状
/// （GET /api/v1/environments/{id}/status）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvStatus {
    pub environment_id: String,
    pub instances: EnvInstanceCounts,
}

/// 来源：src-tauri/src/application/settings_service.rs `AppSettingsView`
/// （GET / PUT /api/v1/settings）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettingsView {
    pub developer_mode: bool,
    pub env_label_position: String,
    pub env_label_color: String,
    /// 全局默认起始页（空 = 打开 about:blank）
    pub default_start_url: String,
    /// 远程接入（v2 新增；旧版应用无此字段 → serde default 保证 CLI 兼容不破）
    #[serde(default)]
    pub remote_access_enabled: bool,
    #[serde(default)]
    pub remote_access_ssh_target: String,
    #[serde(default)]
    pub remote_access_token: String,
}

/// 来源：src-tauri/src/domain/login_profile.rs `LoginProfileStatus`（snake_case 全集 4 变体）。
/// `Unknown` 兜底语义同 [`InstanceStatus`]：服务端未来新增状态时旧 CLI 反序列化为
/// Unknown 而**不是报错**（服务端演进不直接击穿 CLI 进程）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginProfileStatus {
    NotConfigured,
    Ready,
    Capturing,
    Error,
    #[serde(other)]
    Unknown,
}

/// 来源：src-tauri/src/application/profile_service.rs `LoginProfileView`
/// （GET/POST /api/v1/environments/{id}/login-profile[/launch|/capture|/reset]）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginProfileView {
    pub id: String,
    pub name: String,
    pub status: LoginProfileStatus,
    pub snapshot_version: i64,
    pub instances_using: i64,
    pub last_captured_at: Option<i64>,
    /// 派生运行态（登录浏览器 pid 探活），不落库、不属于 [`LoginProfileStatus`] 状态机
    pub browser_running: bool,
    /// 登录浏览器 CDP 端口（运行中才有；Agent 自动化登录的入口）
    pub cdp_port: Option<u16>,
}

/// 来源：src-tauri/src/application/profile_service.rs `LoginProfileRow`
/// （GET /api/v1/login-profiles 的元素）。与 [`LoginProfileView`] 的差异：
/// 多环境名、**无 cdpPort**（列表页不需要自动化入口字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginProfileRow {
    pub id: String,
    pub environment_id: String,
    pub environment_name: String,
    pub name: String,
    pub status: LoginProfileStatus,
    pub snapshot_version: i64,
    pub instances_using: i64,
    pub last_captured_at: Option<i64>,
    pub browser_running: bool,
}

/// 来源：src-tauri/src/api/instances.rs `status` handler 的精简视图
/// （GET /api/v1/instances/{id}/status）。与全量 [`Instance`] 的分工：
/// 轻量轮询（本视图无 profileDir 等重字段）vs 完整详情（instance_get）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceStatusView {
    pub id: String,
    /// 与全量 Instance 同源的 lowercase 状态枚举（复用而非另立，防双源漂移）
    pub status: InstanceStatus,
    pub pid: Option<u32>,
    pub cdp_port: Option<u16>,
    pub browser_version: Option<String>,
}

/// 来源：src-tauri/src/api/kernel.rs `download` handler：`{"ok": bool, "downloading": true}`。
/// `ok=false` 表示服务端已在下载中（单飞语义，非错误）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelDownloadResult {
    pub ok: bool,
    pub downloading: bool,
}

/// 来源：src-tauri/src/api/kernel.rs `cancel` handler：**只有 `{"ok": bool}` 一个字段**。
/// 刻意不复用 [`KernelDownloadResult`]：cancel 响应没有 downloading 字段，误用联合 DTO
/// 会在 decode 层报缺字段 → Protocol(exit 1)；tests/contract.rs 的
/// kernel_download_and_cancel_decode 用例锁死此形状契约。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelCancelResult {
    pub ok: bool,
}
