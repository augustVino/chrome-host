//! Agent API 的类型化 HTTP 封装 —— CLI 进程内唯一的网络出口。
//!
//! # 同步义务（改动前必读）
//! 1. **新增服务端错误码时必须同步 exit.rs 映射表**，否则新码落入兜底分支
//!    （未知 4xx → 7 / 未知 5xx → 1），不崩但语义可能不准。
//! 2. 本文件的方法与 src-tauri/src/api/server.rs 路由**一一对应**，
//!    方法注释须引用路由原文；服务端改路由时 grep 本文件即可得全量受影响点。
//! 3. HTTP 细节（reqwest / Method / StatusCode）止于本文件，cli/ 命令层不得出现。
//!
//! 暂未被命令层调用的方法用逐方法的 `#[allow(dead_code)]` 抑制，不用 struct/impl
//! 级整块 `#[allow]`，避免掩盖真实死代码。
//!
//! # verbose 纪律（PRD cli.md §22.3：敏感信息不落日志）
//! `--verbose` 时 [`AgentClient::send`] 向 stderr 输出一行请求摘要：
//! `→ GET /api/v1/instances (23ms)` —— 只含 method、path、耗时三要素，
//! **不含 URL query、不含请求/响应 body**。path 由字面路由常量拼出，唯一例外是
//! activity 的 `?limit=N`（分页数字，非敏感）：verbose 行打印前
//! 已剥离 `?` 及之后内容（见 [`verbose_line`]），「verbose 不含 query」的承诺不破；
//! body 可能携带 hosts 源地址、扩展路径等敏感内容，从参数类型上就进不了日志行。
//! stderr 出口清单见 output.rs 模块注释。

use std::time::Duration;

use reqwest::Method;

use crate::error::CliError;
use crate::model::{
    AppEvent, AppSettingsView, CdpEndpoint, CdpTarget, CftStatus, Environment, EnvironmentSummary,
    EnvStatus, Extension, HealthReport, Instance, InstanceStatusView, InstanceView,
    KernelCancelResult, KernelDownloadResult, LoginProfileRow, LoginProfileView,
};

/// TCP 连接超时：API 绑定本机回环地址，3s 足够；连不上即按「服务不可达」（exit 8）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// 常规请求总超时：GET / DELETE 等读操作的安全上限。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Agent API 客户端。内部持有两个底层 HTTP 客户端，按次选用：
/// - `default_http`：带 30s 总超时，常规读写操作用；
/// - `long_op_http`：无总超时，长操作（instance create/start/restart）用。
pub struct AgentClient {
    /// 基地址，形如 `http://127.0.0.1:17890`（构造时去除尾部斜杠，拼接 path 永远安全）。
    base_url: String,
    /// verbose 开关（`--verbose`）：true 时每次请求向 stderr 追加一行摘要。
    verbose: bool,
    /// 常规操作客户端（30s 总超时）。
    default_http: reqwest::blocking::Client,
    /// 长操作客户端（无总超时）。
    long_op_http: reqwest::blocking::Client,
}

impl AgentClient {
    /// 基础构造：同时准备「常规 30s 总超时」与「长操作无总超时」两个底层客户端。
    ///
    /// 为什么长操作不能设总超时：`instance create` 是「创建即启动」的复合语义，
    /// 首次调用可能在服务端阻塞于 Chrome for Testing 内核下载（~150MB，见计划 §1.1），
    /// 30s 总超时会把正常下载误判为失败。是否用长超时由每次调用的 `long` 参数决定。
    ///
    /// 基地址由调用方传入（main.rs 用 `GlobalArgs::api_url()` 解析：flag > env > 默认）。
    pub fn new(base_url: &str) -> Result<Self, CliError> {
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            verbose: false,
            default_http: Self::build_http(true)?,
            long_op_http: Self::build_http(false)?,
        })
    }

    /// verbose 开关：消费式 builder，main.rs 以 `globals.verbose` 注入
    /// （`AgentClient::new(...)?.with_verbose(globals.verbose)`）。true 时每次请求
    /// 向 stderr 追加一行请求摘要（见模块注释「verbose 纪律」）；默认 false，
    /// tests/contract.rs 走裸 [`AgentClient::new`] 不受影响。
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// 构建底层 blocking 客户端。连接超时恒为 3s；总超时按用途可选。
    ///
    /// 为什么 `no_proxy()`：Agent API 目标恒为本机回环地址（127.0.0.1），任何系统/环境
    /// 代理都不得劫持 —— 否则在配了系统代理的机器上，服务未运行时用户拿到的是代理的
    /// 502（被误判为协议错误）而非 exit 8 的「桌面应用未运行」指引。与 src-tauri
    /// CDP 客户端（infrastructure/cdp/client.rs）的 no_proxy 取舍同源。
    /// 另：reqwest 若因 workspace 特性合并启用 macos-system-configuration，会读系统代理，
    /// 此行是唯一可靠防线（tests/contract.rs 的 Unreachable 用例即靶点）。
    fn build_http(with_total_timeout: bool) -> Result<reqwest::blocking::Client, CliError> {
        let builder = reqwest::blocking::Client::builder()
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT);
        let builder = if with_total_timeout {
            builder.timeout(DEFAULT_TIMEOUT)
        } else {
            builder
        };
        // 固定参数构造失败仅可能是 TLS 后端等环境级问题，归为本地失败（exit 2）
        builder
            .build()
            .map_err(|e| CliError::Local(format!("HTTP 客户端构造失败: {e}")))
    }

    /// 统一请求出口：所有 HTTP 调用必须经此方法，错误在此集中解包为 CliError。
    ///
    /// 解包规则（tests/contract.rs 契约测试的靶点）：
    /// - 传输层失败（连接拒绝 / 超时等 reqwest 错误）→ `CliError::Unreachable`（exit 8）；
    /// - 响应体为 `{"error":{"code","message"}}` 形状 → `CliError::Api`（携带 status）。
    ///   解包**不限于非 2xx**：2xx 带错误形状同样视为服务端业务错误（防御性）；
    /// - 响应非合法 JSON，或非 2xx 却无标准错误形状 → `CliError::Protocol`（exit 1）；
    /// - 2xx 且无错误形状 → 原样返回 JSON。
    ///
    /// `long`: 是否走无总超时客户端。GET/DELETE 传 false；create/start/restart 传 true。
    fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
        long: bool,
    ) -> Result<serde_json::Value, CliError> {
        let http = if long {
            &self.long_op_http
        } else {
            &self.default_http
        };
        // method 字符串先取走（转为 owned，借用随语句结束）：Method 会被 request()
        // 移动，verbose 行在响应后还要用
        let method_str = method.to_string();
        let mut request = http.request(method, format!("{}{}", self.base_url, path));
        if let Some(json) = body {
            request = request.json(&json);
        }

        // 本机 HTTP 传输层只有「连不上」一种合理归类，统一按不可达处理（exit 8）。
        // verbose 行在错误解包**之前**打印：传输失败恰恰是 verbose 最该诊断的时刻。
        let start = std::time::Instant::now();
        let response = request.send();
        if self.verbose {
            // stderr 纪律（PRD cli.md §22.3）：只输出 method/path/耗时，不含 query 与
            // body —— 敏感信息（hosts 源、扩展路径等）不落日志。query 剥离在
            // verbose_line 内做（本行传入的 path 可能是 activity 的带 query 版本）。
            let elapsed = start.elapsed();
            eprintln!("{}", verbose_line(&method_str, path, elapsed));
        }
        let response = response.map_err(|e| CliError::Unreachable(e.to_string()))?;
        let status = response.status().as_u16();

        let text = response
            .text()
            .map_err(|e| CliError::Protocol(format!("读取响应体失败: {e}")))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| CliError::Protocol(format!("响应不是合法 JSON: {e}")))?;

        // 错误解包：形状与 src-tauri/src/api/error.rs 的 ErrorBody 严格对齐
        if let Some(err) = value.get("error").filter(|e| e.is_object()) {
            if let (Some(code), Some(message)) = (
                err.get("code").and_then(|v| v.as_str()),
                err.get("message").and_then(|v| v.as_str()),
            ) {
                return Err(CliError::Api {
                    status,
                    code: code.to_string(),
                    message: message.to_string(),
                });
            }
        }

        if (200..300).contains(&status) {
            Ok(value)
        } else {
            // 非 2xx 却不带标准错误形状：契约破坏，按协议错误处理（exit 1）
            Err(CliError::Protocol(format!(
                "HTTP {status} 响应缺少 {{\"error\":{{\"code\",\"message\"}}}} 形状"
            )))
        }
    }

    /// `send()` 之后的 JSON → 类型化 DTO 收口：数组/对象形状的反序列化失败在此
    /// **显式暴露**为 `CliError::Protocol`（exit 1）—— 契约漂移（字段改名/类型变更）
    /// 不允许静默错解，另由 tests/contract.rs 在 CI 阶段拦截。
    fn decode<T: serde::de::DeserializeOwned>(
        response: Result<serde_json::Value, CliError>,
    ) -> Result<T, CliError> {
        serde_json::from_value(response?).map_err(|e| {
            CliError::Protocol(format!("响应与 DTO 契约不符（服务端可能已改字段）: {e}"))
        })
    }
}

/// verbose 请求行的纯格式化：`→ {method} {path} ({ms}ms)`。
/// 抽成纯函数以便单测（stderr 是进程级全局资源，打印薄壳不测 —— 与 output.rs 同一取舍）。
/// path 存在唯一带 query 的调用（activity 的 `?limit=N`，非敏感分页数字）：
/// 打印前剥离 `?` 及之后内容，守住「verbose 不含 query」承诺（limit 虽非敏感但纪律不开口子，
/// PRD cli.md §22.3）；本函数签名只收 method/path/elapsed，body 从类型上就进不来。
fn verbose_line(method: &str, path: &str, elapsed: Duration) -> String {
    let path = path.split('?').next().unwrap_or(path);
    format!("→ {method} {path} ({}ms)", elapsed.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbose_line_formats_method_path_and_elapsed() {
        let line = verbose_line("GET", "/api/v1/instances", Duration::from_millis(23));
        assert_eq!(line, "→ GET /api/v1/instances (23ms)");
    }

    #[test]
    fn verbose_line_zero_elapsed_renders_zero_ms() {
        let line = verbose_line(
            "POST",
            "/api/v1/instances/ins_x/start",
            Duration::from_micros(400),
        );
        assert_eq!(line, "→ POST /api/v1/instances/ins_x/start (0ms)");
    }

    #[test]
    fn verbose_line_strips_query_string() {
        // activity 是全 client 唯一带 query 的 path，verbose 行必须剥离 query：
        // 若摘要行泄漏 query，本用例先红，守住「verbose 不含 query」承诺
        let line = verbose_line(
            "GET",
            "/api/v1/environments/env_1/activity?limit=5",
            Duration::from_millis(12),
        );
        assert_eq!(line, "→ GET /api/v1/environments/env_1/activity (12ms)");
    }
}

impl AgentClient {
    // -----------------------------------------------------------------------
    // env 命令组
    // -----------------------------------------------------------------------

    /// GET /api/v1/environments
    pub fn env_list(&self) -> Result<Vec<EnvironmentSummary>, CliError> {
        Self::decode(self.send(Method::GET, "/api/v1/environments", None, false))
    }

    /// GET /api/v1/environments/{id}
    pub fn env_get(&self, id: &str) -> Result<Environment, CliError> {
        Self::decode(self.send(
            Method::GET,
            &format!("/api/v1/environments/{id}"),
            None,
            false,
        ))
    }

    /// POST /api/v1/environments —— body 镜像 `CreateEnvironmentInput`（camelCase）。
    /// 轻操作，走 30s 总超时客户端（long = false）。
    pub fn env_create(
        &self,
        name: &str,
        hosts_source_url: Option<&str>,
        icon: Option<&str>,
    ) -> Result<Environment, CliError> {
        let body = serde_json::json!({
            "name": name,
            "hostsSourceUrl": hosts_source_url,
            "icon": icon,
        });
        Self::decode(self.send(Method::POST, "/api/v1/environments", Some(body), false))
    }

    /// PATCH /api/v1/environments/{id} —— body 由调用方（cli/env.rs）按「传入才进 body、
    /// 置空传 null」组装（PATCH 部分更新语义），避免本层长参数表。
    pub fn env_patch(&self, id: &str, body: serde_json::Value) -> Result<Environment, CliError> {
        Self::decode(self.send(
            Method::PATCH,
            &format!("/api/v1/environments/{id}"),
            Some(body),
            false,
        ))
    }

    /// DELETE /api/v1/environments/{id} —— 响应 `{"ok": true}`，CLI 契约只关心成败，body 丢弃。
    pub fn env_delete(&self, id: &str) -> Result<(), CliError> {
        self.send(
            Method::DELETE,
            &format!("/api/v1/environments/{id}"),
            None,
            false,
        )?;
        Ok(())
    }

    /// POST /api/v1/environments/{id}/stop-all —— 响应 `{environmentId, stopped: [instanceId]}`，
    /// CLI 只消费 `stopped`（env delete 409 编排流用，计划 §2.7）。
    pub fn env_stop_all(&self, id: &str) -> Result<Vec<String>, CliError> {
        let value = self.send(
            Method::POST,
            &format!("/api/v1/environments/{id}/stop-all"),
            None,
            false,
        )?;
        let stopped = value
            .get("stopped")
            .ok_or_else(|| CliError::Protocol("stop-all 响应缺少 stopped 字段".into()))?
            .clone();
        serde_json::from_value(stopped)
            .map_err(|e| CliError::Protocol(format!("stop-all 响应与 DTO 契约不符: {e}")))
    }

    // -----------------------------------------------------------------------
    // instance 命令组
    // -----------------------------------------------------------------------

    /// GET /api/v1/instances —— 全量实例列表
    pub fn instance_list(&self) -> Result<Vec<InstanceView>, CliError> {
        Self::decode(self.send(Method::GET, "/api/v1/instances", None, false))
    }

    /// GET /api/v1/environments/{environment_id}/instances
    pub fn instance_list_by_env(&self, env_id: &str) -> Result<Vec<Instance>, CliError> {
        Self::decode(self.send(
            Method::GET,
            &format!("/api/v1/environments/{env_id}/instances"),
            None,
            false,
        ))
    }

    /// GET /api/v1/instances/{id}
    pub fn instance_get(&self, id: &str) -> Result<Instance, CliError> {
        Self::decode(self.send(Method::GET, &format!("/api/v1/instances/{id}"), None, false))
    }

    /// POST /api/v1/environments/{environment_id}/instances —— 创建即启动的复合语义，
    /// 首次可能阻塞于内核下载，**long = true**（无总超时，见 [`AgentClient::new`]）。
    pub fn instance_create(&self, env_id: &str) -> Result<Instance, CliError> {
        Self::decode(self.send(
            Method::POST,
            &format!("/api/v1/environments/{env_id}/instances"),
            None,
            true,
        ))
    }

    /// POST /api/v1/instances/{id}/start —— 启动含内核/CDP 等待，**long = true**。
    pub fn instance_start(&self, id: &str) -> Result<Instance, CliError> {
        Self::decode(self.send(
            Method::POST,
            &format!("/api/v1/instances/{id}/start"),
            None,
            true,
        ))
    }

    /// POST /api/v1/instances/{id}/stop —— 优雅退出，常规超时内可完成（long = false）。
    pub fn instance_stop(&self, id: &str) -> Result<Instance, CliError> {
        Self::decode(self.send(
            Method::POST,
            &format!("/api/v1/instances/{id}/stop"),
            None,
            false,
        ))
    }

    /// POST /api/v1/instances/{id}/restart —— stop + start 复合，**long = true**。
    pub fn instance_restart(&self, id: &str) -> Result<Instance, CliError> {
        Self::decode(self.send(
            Method::POST,
            &format!("/api/v1/instances/{id}/restart"),
            None,
            true,
        ))
    }

    /// DELETE /api/v1/instances/{id} —— 运行中实例服务端返回 409（409 编排流见计划 §2.7）。
    pub fn instance_delete(&self, id: &str) -> Result<(), CliError> {
        self.send(
            Method::DELETE,
            &format!("/api/v1/instances/{id}"),
            None,
            false,
        )?;
        Ok(())
    }

    /// POST /api/v1/instances/{id}/tabs —— body `{url}`，返回新建标签页 Target。
    pub fn instance_open(&self, id: &str, url: &str) -> Result<CdpTarget, CliError> {
        let body = serde_json::json!({ "url": url });
        Self::decode(self.send(
            Method::POST,
            &format!("/api/v1/instances/{id}/tabs"),
            Some(body),
            false,
        ))
    }

    /// GET /api/v1/instances/{id}/cdp
    pub fn instance_cdp(&self, id: &str) -> Result<CdpEndpoint, CliError> {
        Self::decode(self.send(
            Method::GET,
            &format!("/api/v1/instances/{id}/cdp"),
            None,
            false,
        ))
    }

    // -----------------------------------------------------------------------
    // extension 命令组
    // -----------------------------------------------------------------------

    /// GET /api/v1/extensions
    pub fn extension_list(&self) -> Result<Vec<Extension>, CliError> {
        Self::decode(self.send(Method::GET, "/api/v1/extensions", None, false))
    }

    /// GET /api/v1/extensions/{id}
    pub fn extension_get(&self, id: &str) -> Result<Extension, CliError> {
        Self::decode(self.send(
            Method::GET,
            &format!("/api/v1/extensions/{id}"),
            None,
            false,
        ))
    }

    /// POST /api/v1/extensions —— body `{path}`（服务端读 manifest 提取元数据）。
    pub fn extension_register(&self, path: &str) -> Result<Extension, CliError> {
        let body = serde_json::json!({ "path": path });
        Self::decode(self.send(Method::POST, "/api/v1/extensions", Some(body), false))
    }

    /// PATCH /api/v1/extensions/{id} —— body `{enabled}`；system 扩展服务端返回 403。
    pub fn extension_set_enabled(&self, id: &str, enabled: bool) -> Result<Extension, CliError> {
        let body = serde_json::json!({ "enabled": enabled });
        Self::decode(self.send(
            Method::PATCH,
            &format!("/api/v1/extensions/{id}"),
            Some(body),
            false,
        ))
    }

    /// DELETE /api/v1/extensions/{id} —— 仅移除注册项，不删源文件；system 扩展返回 403。
    pub fn extension_delete(&self, id: &str) -> Result<(), CliError> {
        self.send(
            Method::DELETE,
            &format!("/api/v1/extensions/{id}"),
            None,
            false,
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // runtime / status / doctor
    // -----------------------------------------------------------------------

    /// GET /api/v1/kernel/status
    pub fn kernel_status(&self) -> Result<CftStatus, CliError> {
        Self::decode(self.send(Method::GET, "/api/v1/kernel/status", None, false))
    }

    /// GET /api/v1/health
    pub fn health(&self) -> Result<HealthReport, CliError> {
        Self::decode(self.send(Method::GET, "/api/v1/health", None, false))
    }

    // -----------------------------------------------------------------------
    // 能力补齐组（instance 轻量轮询 / 标签页自动化 / activity /
    // env status / settings / login-profile / kernel 下载控制）
    // -----------------------------------------------------------------------

    /// GET /api/v1/instances/{id}/status —— 精简视图（轻量轮询用，区别于全量
    /// [`AgentClient::instance_get`]）。
    pub fn instance_status(&self, id: &str) -> Result<InstanceStatusView, CliError> {
        Self::decode(self.send(
            Method::GET,
            &format!("/api/v1/instances/{id}/status"),
            None,
            false,
        ))
    }

    /// GET /api/v1/instances/{id}/tabs —— 当前标签页列表（与 POST 同路径不同 method）。
    pub fn instance_tabs(&self, id: &str) -> Result<Vec<CdpTarget>, CliError> {
        Self::decode(self.send(
            Method::GET,
            &format!("/api/v1/instances/{id}/tabs"),
            None,
            false,
        ))
    }

    /// POST /api/v1/instances/{id}/navigate —— body `{tabId, url}`（serde key 是 tabId，
    /// 服务端 NavigateBody 显式 rename，非 camelCase 默认的 tab_id）。
    pub fn instance_navigate(
        &self,
        id: &str,
        tab_id: &str,
        url: &str,
    ) -> Result<CdpTarget, CliError> {
        let body = serde_json::json!({ "tabId": tab_id, "url": url });
        Self::decode(self.send(
            Method::POST,
            &format!("/api/v1/instances/{id}/navigate"),
            Some(body),
            false,
        ))
    }

    /// POST /api/v1/instances/{id}/focus —— 聚焦窗口；实例未运行 → 409
    /// INSTANCE_NOT_RUNNING（exit 5，非破坏性不确认）。
    pub fn instance_focus(&self, id: &str) -> Result<Instance, CliError> {
        Self::decode(self.send(
            Method::POST,
            &format!("/api/v1/instances/{id}/focus"),
            None,
            false,
        ))
    }

    /// GET /api/v1/environments/{id}/activity?limit=N —— **全 client 唯一带 query 的
    /// path**：limit 是分页数字非敏感，但「verbose 不含 query」纪律不开口子，
    /// verbose 行由 [`verbose_line`] 剥离 query。
    pub fn env_activity(&self, id: &str, limit: i64) -> Result<Vec<AppEvent>, CliError> {
        Self::decode(self.send(
            Method::GET,
            &format!("/api/v1/environments/{id}/activity?limit={limit}"),
            None,
            false,
        ))
    }

    /// GET /api/v1/environments/{id}/status —— 实例状态计数视图。
    pub fn env_status(&self, id: &str) -> Result<EnvStatus, CliError> {
        Self::decode(self.send(
            Method::GET,
            &format!("/api/v1/environments/{id}/status"),
            None,
            false,
        ))
    }

    /// GET /api/v1/settings
    pub fn settings_get(&self) -> Result<AppSettingsView, CliError> {
        Self::decode(self.send(Method::GET, "/api/v1/settings", None, false))
    }

    /// PUT /api/v1/settings —— body 由命令层按「传入 flag 才进 body」组装（与
    /// env_patch 同模式）；本层不校验，服务端是唯一校验源。
    pub fn settings_update(&self, body: serde_json::Value) -> Result<AppSettingsView, CliError> {
        Self::decode(self.send(Method::PUT, "/api/v1/settings", Some(body), false))
    }

    /// GET /api/v1/login-profiles —— Login Profiles 页全量列表（含环境名）。
    pub fn login_profile_list(&self) -> Result<Vec<LoginProfileRow>, CliError> {
        Self::decode(self.send(Method::GET, "/api/v1/login-profiles", None, false))
    }

    /// GET /api/v1/environments/{id}/login-profile —— 单环境登录态视图。
    pub fn login_profile_get(&self, env_id: &str) -> Result<LoginProfileView, CliError> {
        Self::decode(self.send(
            Method::GET,
            &format!("/api/v1/environments/{env_id}/login-profile"),
            None,
            false,
        ))
    }

    /// POST /api/v1/environments/{id}/login-profile/launch —— 用母本目录启动
    /// 登录浏览器（非破坏性，不确认）。
    pub fn login_profile_launch(&self, env_id: &str) -> Result<LoginProfileView, CliError> {
        Self::decode(self.send(
            Method::POST,
            &format!("/api/v1/environments/{env_id}/login-profile/launch"),
            None,
            false,
        ))
    }

    /// POST /api/v1/environments/{id}/login-profile/capture —— 停机捕获快照。
    /// 409 PROFILE_IN_USE 有多个子场景（登录浏览器运行中 / 捕获已在进行），
    /// 服务端 message 由命令层**原样透传不覆写**（统一翻译会误导）。
    pub fn login_profile_capture(&self, env_id: &str) -> Result<LoginProfileView, CliError> {
        Self::decode(self.send(
            Method::POST,
            &format!("/api/v1/environments/{env_id}/login-profile/capture"),
            None,
            false,
        ))
    }

    /// POST /api/v1/environments/{id}/login-profile/reset —— 清空母本
    /// （破坏性，命令层总是确认）。
    pub fn login_profile_reset(&self, env_id: &str) -> Result<LoginProfileView, CliError> {
        Self::decode(self.send(
            Method::POST,
            &format!("/api/v1/environments/{env_id}/login-profile/reset"),
            None,
            false,
        ))
    }

    /// POST /api/v1/kernel/download —— 触发后台下载；ok=false 表示已在下载中
    /// （服务端单飞语义，非错误）。
    pub fn kernel_download(&self) -> Result<KernelDownloadResult, CliError> {
        Self::decode(self.send(Method::POST, "/api/v1/kernel/download", None, false))
    }

    /// POST /api/v1/kernel/cancel —— 响应**只有 {"ok"} 一个字段**（独立 DTO，
    /// 缘由见 model.rs [`crate::model::KernelCancelResult`] 注释）。
    pub fn kernel_cancel(&self) -> Result<KernelCancelResult, CliError> {
        Self::decode(self.send(Method::POST, "/api/v1/kernel/cancel", None, false))
    }
}
