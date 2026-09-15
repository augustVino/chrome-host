//! Agent API 契约测试（防服务端契约漂移的主力）。
//!
//! 覆盖点：DTO 反序列化（含 CJK / flatten / rename）、`{"error":{...}}` 错误解包、
//! 非 JSON 响应与 DTO 形状漂移 → Protocol、连接拒绝 → Unreachable、
//! 每个错误样本的 ExitCode 映射。
//!
//! # 为什么不用 `#[tokio::test]`（重要，改动前必读）
//!
//! `chrome-host-cli` 是纯 binary crate（无 lib target），集成测试无法 `use` 其内部
//! 模块 —— 用 `#[path]` 把 src/ 下被测模块直接挂载进本测试 crate；模块内部全部走
//! `crate::` 相对路径，因此**零改动被测文件**。
//!
//! 运行模型上，blocking 的 reqwest 若在 tokio runtime 上下文里发请求会直接 panic
//! （blocking 内部要起自己的单线程 runtime 等待响应，runtime 嵌套不被允许）。因此：
//! - server 侧：独立 `std::thread` + 私有 tokio runtime（axum 是 async 框架，必须
//!   跑在 runtime 里）；
//! - client 侧：留在测试主线程 —— 测试函数全部是普通 `#[test]`，主线程无 runtime，
//!   blocking 调用合法。两侧跨线程隔离，绝不把 blocking client 放进 tokio runtime。
//!
//! 挂载进来的 src 模块含有测试未触达的项（如 GlobalArgs 的多数字段），
//! 在测试 crate 的 dead_code 视角下会误报 —— 仅在本测试 crate 抑制，
//! 不影响被测代码本身的 lint 纪律。
#![allow(dead_code)]

#[path = "../src/cli/mod.rs"]
mod cli;
#[path = "../src/client.rs"]
mod client;
#[path = "../src/error.rs"]
mod error;
#[path = "../src/exit.rs"]
mod exit;
#[path = "../src/model.rs"]
mod model;

use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde_json::json;

use crate::client::AgentClient;
use crate::error::CliError;
use crate::model::{ExtensionStatus, ExtensionType, InstanceStatus, LoginProfileStatus};

// ---------------------------------------------------------------------------
// canned 数据：与 src-tauri 服务端真实 serde 线上形态逐字段对齐（含 CJK、null、flatten）
// ---------------------------------------------------------------------------

fn error_body(code: &str) -> serde_json::Value {
    json!({ "error": { "code": code, "message": "x" } })
}

fn canned_env() -> serde_json::Value {
    json!({
        "id": "env_1",
        "name": "测试环境",
        "hostsSourceUrl": "https://example.com/hosts.txt",
        "icon": null,
        "startupArgs": "--lang=zh-CN",
        "keepAlive": false,
        "createdAt": 1,
        "updatedAt": 2
    })
}

fn canned_env_summary() -> serde_json::Value {
    // EnvironmentSummary = flatten(env) + 摘要计数（environment_service.rs）
    let mut v = canned_env();
    v["runningInstances"] = json!(1);
    v["totalInstances"] = json!(2);
    v
}

fn canned_instance() -> serde_json::Value {
    json!({
        "id": "ins_1",
        "environmentId": "env_1",
        "loginProfileId": null,
        "profileDir": "/tmp/chrome-host/env_1/ins_1",
        "pid": 4321,
        "cdpPort": 9333,
        "status": "running",
        "hostRules": null,
        "browserVersion": "131.0.6778.204",
        "startedAt": 100,
        "stoppedAt": null,
        "createdAt": 1,
        "updatedAt": 2
    })
}

fn canned_instance_view() -> serde_json::Value {
    let mut v = canned_instance();
    v["environmentName"] = json!("测试环境");
    v
}

fn canned_cdp_endpoint() -> serde_json::Value {
    json!({
        "instanceId": "ins_1",
        "host": "127.0.0.1",
        "port": 9333,
        "httpUrl": "http://127.0.0.1:9333",
        "webSocketUrl": null
    })
}

fn canned_target() -> serde_json::Value {
    // CDP Target：`type` 为服务端显式 rename，非 targetType（infrastructure/cdp/client.rs）
    json!({
        "id": "tab_1",
        "type": "page",
        "title": "标题页",
        "url": "https://example.com/"
    })
}

fn canned_extension() -> serde_json::Value {
    json!({
        "id": "ext_user",
        "name": "用户脚本助手",
        "description": null,
        "version": "1.0.0",
        "manifestVersion": 3,
        "type": "user",
        "sourcePath": "/tmp/extensions/user-helper",
        "enabled": true,
        "status": "ready",
        "createdAt": 1,
        "updatedAt": 2
    })
}

fn canned_kernel_status() -> serde_json::Value {
    json!({
        "installed": true,
        "version": "131.0.6778.204",
        "pinnedVersion": "131.0.6778.204",
        "upgradeAvailable": false,
        "downloading": false,
        "binaryPath": "/tmp/kernel/chrome"
    })
}

fn canned_health() -> serde_json::Value {
    json!({
        "version": "0.0.0-test",
        "ok": true,
        "checks": [
            { "name": "kernel", "ok": true, "detail": "Chrome for Testing 就绪", "suggestion": null },
            { "name": "database", "ok": true, "detail": "SQLite 完整性校验通过（integrity_check: ok）", "suggestion": null }
        ],
        "counts": { "environments": 1, "instances": 2, "running": 1, "extensions": 1 }
    })
}

// —— 扩展方法面端点的 canned 数据 ——

fn canned_instance_status() -> serde_json::Value {
    // GET /api/v1/instances/{id}/status 精简视图（api/instances.rs status handler 内联形状）
    json!({
        "id": "ins_x",
        "status": "running",
        "pid": 123,
        "cdpPort": 9333,
        "browserVersion": "131.0.6778.204"
    })
}

fn canned_activity_events() -> serde_json::Value {
    // AppEvent 数组：有值一例 + null 可空字段一例（孤儿事件的 environmentId/targetId 均 null）
    json!([
        {
            "ts": 1700000000000i64,
            "level": "info",
            "event": "instance.started",
            "environmentId": "env_1",
            "targetId": "tab_1",
            "message": "实例已启动"
        },
        {
            "ts": 1700000001000i64,
            "level": "warn",
            "event": "cdp.detached",
            "environmentId": null,
            "targetId": null,
            "message": "检测到崩溃（孤儿事件）"
        }
    ])
}

fn canned_env_status() -> serde_json::Value {
    json!({
        "environmentId": "env_1",
        "instances": { "total": 2, "running": 1, "starting": 0, "stopped": 1, "error": 0 }
    })
}

fn canned_settings() -> serde_json::Value {
    json!({
        "developerMode": false,
        "envLabelPosition": "top-left",
        "envLabelColor": "red",
        "defaultStartUrl": "https://example.com/"
    })
}

fn canned_login_profile_view() -> serde_json::Value {
    // LoginProfileView：Ready 态 + 快照已捕获 + 登录浏览器运行中（launch 之后的形态）
    json!({
        "id": "env_1",
        "name": "env_1 母本",
        "status": "ready",
        "snapshotVersion": 3,
        "instancesUsing": 1,
        "lastCapturedAt": 1700000000000i64,
        "browserRunning": true,
        "cdpPort": 9223
    })
}

fn canned_login_profile_row() -> serde_json::Value {
    // LoginProfileRow：无 cdpPort 字段（列表视图），含环境名；lastCapturedAt=null = 从未捕获
    json!({
        "id": "env_2",
        "environmentId": "env_2",
        "environmentName": "第二环境",
        "name": "env_2 母本",
        "status": "not_configured",
        "snapshotVersion": 0,
        "instancesUsing": 0,
        "lastCapturedAt": null,
        "browserRunning": false
    })
}

// ---------------------------------------------------------------------------
// fixture server：随机端口 + 独立线程私有 runtime（理由见文件头注释）
// ---------------------------------------------------------------------------

fn fixture_router() -> Router {
    Router::new()
        // —— env ——
        .route(
            "/api/v1/environments",
            get(|| async { Json(json!([canned_env_summary()])) })
                .post(|| async { (StatusCode::CREATED, Json(canned_env())) }),
        )
        .route(
            "/api/v1/environments/env_1",
            get(|| async { Json(canned_env()) })
                .patch(|| async { Json(canned_env()) })
                .delete(|| async { Json(json!({ "ok": true })) }),
        )
        .route(
            "/api/v1/environments/err_400",
            patch(|| async { (StatusCode::BAD_REQUEST, Json(error_body("INVALID_REQUEST"))) }),
        )
        .route(
            "/api/v1/environments/bad_shape",
            // 合法 JSON 但形状与 Environment DTO 不符 → decode 层的 Protocol（契约漂移样本）
            get(|| async { Json(json!({ "foo": 1 })) }),
        )
        .route(
            "/api/v1/environments/env_1/stop-all",
            post(|| async {
                Json(json!({ "environmentId": "env_1", "stopped": ["ins_1", "ins_2"] }))
            }),
        )
        .route(
            "/api/v1/environments/env_1/instances",
            get(|| async { Json(json!([canned_instance()])) })
                .post(|| async { (StatusCode::CREATED, Json(canned_instance())) }),
        )
        // —— instance ——
        .route(
            "/api/v1/instances",
            get(|| async { Json(json!([canned_instance_view()])) }),
        )
        .route(
            "/api/v1/instances/ins_1",
            get(|| async { Json(canned_instance()) })
                .delete(|| async { Json(json!({ "ok": true })) }),
        )
        .route("/api/v1/instances/ins_1/start", post(|| async { Json(canned_instance()) }))
        .route("/api/v1/instances/ins_1/stop", post(|| async { Json(canned_instance()) }))
        .route("/api/v1/instances/ins_1/restart", post(|| async { Json(canned_instance()) }))
        .route(
            "/api/v1/instances/ins_1/tabs",
            post(|| async { (StatusCode::CREATED, Json(canned_target())) }),
        )
        .route("/api/v1/instances/ins_1/cdp", get(|| async { Json(canned_cdp_endpoint()) }))
        // 协议破坏样本：2xx 但响应体是纯文本（非 JSON）→ send() 层的 Protocol
        .route(
            "/api/v1/instances/plain",
            get(|| async { "THIS IS NOT JSON" }),
        )
        // —— 错误形状（四个代表样本）——
        .route(
            "/api/v1/instances/err_404",
            get(|| async { (StatusCode::NOT_FOUND, Json(error_body("INSTANCE_NOT_FOUND"))) }),
        )
        .route(
            "/api/v1/instances/err_409/start",
            post(|| async {
                (StatusCode::CONFLICT, Json(error_body("INSTANCE_ALREADY_RUNNING")))
            }),
        )
        // —— extension ——
        .route(
            "/api/v1/extensions",
            get(|| async { Json(json!([canned_extension()])) })
                .post(|| async { Json(canned_extension()) }),
        )
        .route(
            "/api/v1/extensions/ext_user",
            get(|| async { Json(canned_extension()) })
                .patch(|| async { Json(canned_extension()) })
                .delete(|| async { Json(json!({ "ok": true })) }),
        )
        .route(
            "/api/v1/extensions/ext_system",
            patch(|| async {
                (StatusCode::FORBIDDEN, Json(error_body("EXTENSION_SYSTEM_LOCKED")))
            }),
        )
        // —— runtime / health ——
        .route("/api/v1/kernel/status", get(|| async { Json(canned_kernel_status()) }))
        .route("/api/v1/health", get(|| async { Json(canned_health()) }))
        // —— 扩展方法面端点 ——
        // instance 精简视图 + 标签页自动化（tabs 正常数组 / 空数组两路 + navigate + focus）
        .route(
            "/api/v1/instances/ins_x/status",
            get(|| async { Json(canned_instance_status()) }),
        )
        .route(
            "/api/v1/instances/ins_x/tabs",
            get(|| async { Json(json!([canned_target()])) }),
        )
        .route(
            "/api/v1/instances/ins_none/tabs",
            get(|| async { Json(json!([])) }),
        )
        .route(
            "/api/v1/instances/ins_1/navigate",
            post(|| async { Json(canned_target()) }),
        )
        .route(
            "/api/v1/instances/ins_1/focus",
            post(|| async { Json(canned_instance()) }),
        )
        .route(
            "/api/v1/instances/err_409/focus",
            post(|| async { (StatusCode::CONFLICT, Json(error_body("INSTANCE_NOT_RUNNING"))) }),
        )
        // activity 唯一带 query 的 path（?limit=N 由 axum 路由自动忽略）+ env status
        .route(
            "/api/v1/environments/env_1/activity",
            get(|| async { Json(canned_activity_events()) }),
        )
        .route(
            "/api/v1/environments/env_1/status",
            get(|| async { Json(canned_env_status()) }),
        )
        // settings GET + PUT（fixture 是 canned 契约，PUT 不回显 body）
        .route(
            "/api/v1/settings",
            get(|| async { Json(canned_settings()) }).put(|| async { Json(canned_settings()) }),
        )
        // login-profile：列表 Row + 单环境 View + 三个动作（fixture 不回显 body，
        // 动作路径统一回 View canned；capture 的 409 子场景另用 env_1 路径）
        .route(
            "/api/v1/login-profiles",
            get(|| async { Json(json!([canned_login_profile_row()])) }),
        )
        .route(
            "/api/v1/environments/env_1/login-profile",
            get(|| async { Json(canned_login_profile_view()) }),
        )
        .route(
            "/api/v1/environments/env_1/login-profile/capture",
            post(|| async { (StatusCode::CONFLICT, Json(error_body("PROFILE_IN_USE"))) }),
        )
        .route(
            "/api/v1/environments/env_2/login-profile/launch",
            post(|| async { Json(canned_login_profile_view()) }),
        )
        .route(
            "/api/v1/environments/env_2/login-profile/capture",
            post(|| async { Json(canned_login_profile_view()) }),
        )
        .route(
            "/api/v1/environments/env_2/login-profile/reset",
            post(|| async { Json(canned_login_profile_view()) }),
        )
        // kernel 下载控制：cancel 只回 {"ok"}（无 downloading 字段）—— 形状契约锁的靶点
        .route(
            "/api/v1/kernel/download",
            post(|| async { Json(json!({ "ok": true, "downloading": true })) }),
        )
        .route(
            "/api/v1/kernel/cancel",
            post(|| async { Json(json!({ "ok": true })) }),
        )
}

/// 启动 fixture server 并阻塞等待其就绪（经 channel 传回实际绑定地址），返回基地址。
/// server 线程常驻到测试进程退出（JoinHandle drop 不会终止线程），多测试共享一个实例。
fn fixture_base_url() -> String {
    use std::sync::mpsc;
    use std::sync::OnceLock;

    static BASE_URL: OnceLock<String> = OnceLock::new();
    BASE_URL
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel::<String>();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("fixture tokio runtime 构建失败");
                rt.block_on(async move {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                        .await
                        .expect("fixture 端口绑定失败");
                    let addr = listener.local_addr().expect("fixture local_addr 失败");
                    tx.send(format!("http://{addr}")).expect("通知测试主线程失败");
                    axum::serve(listener, fixture_router())
                        .await
                        .expect("fixture server 运行失败");
                });
            });
            // 阻塞至就绪：拿到地址即 server 已可 accept，测试无需 sleep 轮询
            rx.recv().expect("fixture server 启动失败（channel 关闭）")
        })
        .clone()
}

fn test_client() -> AgentClient {
    AgentClient::new(&fixture_base_url()).expect("测试客户端构造失败")
}

// ---------------------------------------------------------------------------
// DTO 反序列化（含 CJK / flatten / rename）
// ---------------------------------------------------------------------------

/// env list：中文名 + flatten 平铺 + 摘要计数
#[test]
fn env_list_decodes_cjk_summary_and_flatten() {
    let list = test_client().env_list().expect("env_list 应成功");
    assert_eq!(list.len(), 1);
    let summary = &list[0];
    // flatten 验证：Environment 字段平铺在顶层（id 来自内层 environment）
    assert_eq!(summary.environment.id, "env_1");
    assert_eq!(summary.environment.name, "测试环境");
    assert_eq!(summary.environment.created_at, 1);
    assert_eq!(summary.running_instances, 1);
    assert_eq!(summary.total_instances, 2);
}

/// env CRUD 面：create / get / patch / stop-all / delete
#[test]
fn env_crud_and_stop_all_contract() {
    let client = test_client();
    let env = client
        .env_create("测试环境", Some("https://example.com/hosts"), None)
        .expect("env_create 应成功");
    // fixture 是 canned 契约（不回显 body）：断言点在 DTO 的 Some/null 双向解码，
    // 即 Option<String> 对字符串值与 null 各自正确落位
    assert_eq!(env.id, "env_1");
    assert_eq!(env.name, "测试环境");
    assert_eq!(env.hosts_source_url.as_deref(), Some("https://example.com/hosts.txt"));
    assert_eq!(env.startup_args.as_deref(), Some("--lang=zh-CN"));
    assert!(env.icon.is_none(), "null 应解码为 None");

    let got = client.env_get("env_1").expect("env_get 应成功");
    assert_eq!(got.id, "env_1");

    let patched = client
        .env_patch("env_1", json!({ "name": "改名" }))
        .expect("env_patch 应成功");
    assert_eq!(patched.id, "env_1");

    let stopped = client.env_stop_all("env_1").expect("env_stop_all 应成功");
    assert_eq!(stopped, vec!["ins_1".to_string(), "ins_2".to_string()]);

    client.env_delete("env_1").expect("env_delete 应成功");
}

/// instance 全方法面：list / list_by_env / get / create / start / stop / restart /
/// open(tabs) / cdp / delete
#[test]
fn instance_endpoints_full_surface() {
    let client = test_client();

    let views = client.instance_list().expect("instance_list 应成功");
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].environment_name, "测试环境");
    assert_eq!(views[0].instance.id, "ins_1");
    assert_eq!(views[0].instance.status, InstanceStatus::Running);
    assert_eq!(views[0].instance.cdp_port, Some(9333));
    assert_eq!(views[0].instance.pid, Some(4321));

    let by_env = client.instance_list_by_env("env_1").expect("应成功");
    assert_eq!(by_env.len(), 1);
    assert_eq!(by_env[0].environment_id, "env_1");

    let created = client.instance_create("env_1").expect("instance_create 应成功");
    assert_eq!(created.id, "ins_1");

    let got = client.instance_get("ins_1").expect("instance_get 应成功");
    assert_eq!(got.profile_dir, "/tmp/chrome-host/env_1/ins_1");

    for status in [
        client.instance_start("ins_1").expect("start 应成功"),
        client.instance_stop("ins_1").expect("stop 应成功"),
        client.instance_restart("ins_1").expect("restart 应成功"),
    ] {
        assert_eq!(status.id, "ins_1");
        assert_eq!(status.status, InstanceStatus::Running);
    }

    let target = client
        .instance_open("ins_1", "https://example.com/")
        .expect("instance_open 应成功");
    assert_eq!(target.id, "tab_1");
    assert_eq!(target.target_type, "page"); // `type` 字段的显式 rename 契约
    assert_eq!(target.title, "标题页");
    assert_eq!(target.url, "https://example.com/");

    let endpoint = client.instance_cdp("ins_1").expect("instance_cdp 应成功");
    assert_eq!(endpoint.instance_id, "ins_1");
    assert_eq!(endpoint.port, 9333);
    assert_eq!(endpoint.web_socket_url, None);

    client.instance_delete("ins_1").expect("instance_delete 应成功");
}

/// extension 全方法面：list / get / register / set_enabled / delete；
/// 重点验证 `type` 字段（服务端显式 rename，非 camelCase 的 extensionType）
#[test]
fn extension_endpoints_and_type_rename() {
    let client = test_client();

    let list = client.extension_list().expect("extension_list 应成功");
    assert_eq!(list.len(), 1);
    let ext = &list[0];
    assert_eq!(ext.name, "用户脚本助手");
    assert_eq!(ext.extension_type, ExtensionType::User);
    assert_eq!(ext.status, ExtensionStatus::Ready);
    assert_eq!(ext.manifest_version, Some(3));
    assert!(ext.enabled);

    let got = client.extension_get("ext_user").expect("应成功");
    assert_eq!(got.id, "ext_user");

    let registered = client
        .extension_register("/tmp/extensions/user-helper")
        .expect("extension_register 应成功");
    assert_eq!(registered.id, "ext_user");

    let toggled = client.extension_set_enabled("ext_user", false).expect("应成功");
    assert_eq!(toggled.id, "ext_user");

    client.extension_delete("ext_user").expect("extension_delete 应成功");
}

/// runtime + health：CftStatus / HealthReport（checks + counts）
#[test]
fn kernel_status_and_health_decode() {
    let client = test_client();

    let kernel = client.kernel_status().expect("kernel_status 应成功");
    assert!(kernel.installed);
    assert_eq!(kernel.pinned_version, "131.0.6778.204");
    assert_eq!(kernel.version.as_deref(), Some("131.0.6778.204"));
    assert!(!kernel.downloading);
    assert!(kernel.binary_path.is_some());

    let report = client.health().expect("health 应成功");
    assert!(report.ok);
    assert_eq!(report.version, "0.0.0-test");
    assert_eq!(report.checks.len(), 2);
    assert_eq!(report.checks[0].name, "kernel");
    assert!(report.checks[0].suggestion.is_none());
    assert_eq!(report.counts.environments, 1);
    assert_eq!(report.counts.instances, 2);
    assert_eq!(report.counts.running, 1);
    assert_eq!(report.counts.extensions, 1);
}

// ---------------------------------------------------------------------------
// 扩展方法面：instance 轻量轮询/标签页自动化/activity/env status/
// settings/login-profile/kernel 下载控制
// ---------------------------------------------------------------------------

/// instance 扩展面：status 精简视图 / tabs（正常 + 空数组）/ navigate / focus + 409
#[test]
fn instance_status_tabs_navigate_focus_contract() {
    let client = test_client();

    // 精简视图：与全量 Instance 的字段分工（轻量轮询 vs 完整详情）
    let status = client.instance_status("ins_x").expect("instance_status 应成功");
    assert_eq!(status.id, "ins_x");
    assert_eq!(status.status, InstanceStatus::Running);
    assert_eq!(status.pid, Some(123));
    assert_eq!(status.cdp_port, Some(9333));
    assert_eq!(status.browser_version.as_deref(), Some("131.0.6778.204"));

    // tabs 正常数组
    let tabs = client.instance_tabs("ins_x").expect("instance_tabs 应成功");
    assert_eq!(tabs.len(), 1);
    assert_eq!(tabs[0].id, "tab_1");
    assert_eq!(tabs[0].target_type, "page");

    // tabs 空数组：显式覆盖 Vec<T> 对 [] 的解码
    let empty = client.instance_tabs("ins_none").expect("空 tabs 应成功");
    assert!(empty.is_empty());

    // navigate：body serde key 是 tabId（服务端 NavigateBody 显式 rename）
    let navigated = client
        .instance_navigate("ins_1", "tab_1", "https://example.com/next")
        .expect("instance_navigate 应成功");
    assert_eq!(navigated.id, "tab_1");
    assert_eq!(navigated.url, "https://example.com/");

    // focus：返回全量 Instance
    let focused = client.instance_focus("ins_1").expect("instance_focus 应成功");
    assert_eq!(focused.id, "ins_1");
    assert_eq!(focused.profile_dir, "/tmp/chrome-host/env_1/ins_1");

    // focus 对未运行实例 → 409 INSTANCE_NOT_RUNNING → exit 5
    let err = client.instance_focus("err_409").unwrap_err();
    match &err {
        CliError::Api { status, code, .. } => {
            assert_eq!(*status, 409);
            assert_eq!(code, "INSTANCE_NOT_RUNNING");
        }
        other => panic!("focus 409 应解包为 CliError::Api，实际 {other:?}"),
    }
    assert_eq!(exit::from_cli_error(&err).as_u8(), 5, "409 运行时冲突 → exit 5");
}

/// env activity（含 null 可空字段一例）+ env status 计数视图
#[test]
fn env_activity_and_status_contract() {
    let client = test_client();

    let events = client.env_activity("env_1", 2).expect("env_activity 应成功");
    assert_eq!(events.len(), 2);
    // 有值一例：可空字段落位
    assert_eq!(events[0].ts, 1_700_000_000_000);
    assert_eq!(events[0].event, "instance.started");
    assert_eq!(events[0].environment_id.as_deref(), Some("env_1"));
    assert_eq!(events[0].target_id.as_deref(), Some("tab_1"));
    // null 一例：Option 双向解码（孤儿事件）
    assert_eq!(events[1].level, "warn");
    assert!(events[1].environment_id.is_none(), "null 应解码为 None");
    assert!(events[1].target_id.is_none(), "null 应解码为 None");

    let status = client.env_status("env_1").expect("env_status 应成功");
    assert_eq!(status.environment_id, "env_1");
    assert_eq!(status.instances.total, 2);
    assert_eq!(status.instances.running, 1);
    assert_eq!(status.instances.starting, 0);
    assert_eq!(status.instances.stopped, 1);
    assert_eq!(status.instances.error, 0);
}

/// settings GET + PUT（body 由调用方组装的透传契约，服务端是唯一校验源）
#[test]
fn settings_get_and_update_contract() {
    let client = test_client();

    let view = client.settings_get().expect("settings_get 应成功");
    assert!(!view.developer_mode);
    assert_eq!(view.env_label_position, "top-left");
    assert_eq!(view.env_label_color, "red");
    assert_eq!(view.default_start_url, "https://example.com/");

    // PUT body 由命令层组装，client 层只透传（fixture 是 canned 契约，不回显 body）
    let updated = client
        .settings_update(json!({ "defaultStartUrl": "https://example.net/" }))
        .expect("settings_update 应成功");
    assert_eq!(updated.default_start_url, "https://example.com/");
}

/// login-profile 全方法面：list(Row) / get(View) / launch / capture / reset + 409
#[test]
fn login_profile_endpoints_contract() {
    let client = test_client();

    // 列表 Row：含环境名、无 cdpPort 字段、lastCapturedAt=null（从未捕获）
    let rows = client.login_profile_list().expect("login_profile_list 应成功");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].environment_id, "env_2");
    assert_eq!(rows[0].environment_name, "第二环境");
    assert_eq!(rows[0].status, LoginProfileStatus::NotConfigured);
    assert_eq!(rows[0].snapshot_version, 0);
    assert!(rows[0].last_captured_at.is_none());
    assert!(!rows[0].browser_running);

    // 单环境 View：比 Row 多 cdpPort（自动化登录入口）
    let view = client.login_profile_get("env_1").expect("应成功");
    assert_eq!(view.status, LoginProfileStatus::Ready);
    assert_eq!(view.snapshot_version, 3);
    assert_eq!(view.instances_using, 1);
    assert_eq!(view.last_captured_at, Some(1_700_000_000_000));
    assert!(view.browser_running);
    assert_eq!(view.cdp_port, Some(9223));

    // 三个动作统一回 View（fixture 是 canned 契约，不回显请求语境）
    for view in [
        client.login_profile_launch("env_2").expect("launch 应成功"),
        client.login_profile_capture("env_2").expect("capture 应成功"),
        client.login_profile_reset("env_2").expect("reset 应成功"),
    ] {
        assert_eq!(view.id, "env_1");
        assert_eq!(view.status, LoginProfileStatus::Ready);
    }

    // capture 对「登录浏览器运行中」环境 → 409 PROFILE_IN_USE → exit 5
    // （该码有多个子场景，服务端 message 原样透传是命令层义务；client 层只保证解包）
    let err = client.login_profile_capture("env_1").unwrap_err();
    match &err {
        CliError::Api { status, code, .. } => {
            assert_eq!(*status, 409);
            assert_eq!(code, "PROFILE_IN_USE");
        }
        other => panic!("capture 409 应解包为 CliError::Api，实际 {other:?}"),
    }
    assert_eq!(exit::from_cli_error(&err).as_u8(), 5, "409 运行时冲突 → exit 5");
}

/// LoginProfileStatus 的 snake_case 四值 + 未知值 #[serde(other)] 兑底回归
#[test]
fn login_profile_status_snake_case_and_unknown_fallback() {
    for (raw, expected) in [
        ("not_configured", LoginProfileStatus::NotConfigured),
        ("ready", LoginProfileStatus::Ready),
        ("capturing", LoginProfileStatus::Capturing),
        ("error", LoginProfileStatus::Error),
    ] {
        let decoded: LoginProfileStatus =
            serde_json::from_value(json!(raw)).expect("snake_case 值应可解码");
        assert_eq!(decoded, expected, "值 {raw} 应解码为 {expected:?}");
    }
    // 未知值兑底：服务端新增状态时旧 CLI 反序列化为 Unknown 而不是报错
    let unknown: LoginProfileStatus =
        serde_json::from_value(json!("migrating")).expect("未知值应落入兑底变体");
    assert_eq!(unknown, LoginProfileStatus::Unknown);
}

/// kernel download + cancel —— cancel 响应**只回 {"ok"}（无 downloading 字段）**，
/// 这是形状契约锁：若误用带 downloading 的联合 DTO，缺字段 decode → Protocol，本测试必红
#[test]
fn kernel_download_and_cancel_decode() {
    let client = test_client();

    let download = client.kernel_download().expect("kernel_download 应成功");
    assert!(download.ok);
    assert!(download.downloading);

    // 只含 ok 的响应体能正常解码（联合 DTO 假设下此行必炸 Protocol）
    let cancelled = client.kernel_cancel().expect("kernel_cancel 应成功");
    assert!(cancelled.ok);
}

// ---------------------------------------------------------------------------
// 错误路径：Api 解包 / Protocol / Unreachable + ExitCode 映射
// ---------------------------------------------------------------------------

/// 四个标准错误形状 → `CliError::Api` 且 status/code 逐一正确，ExitCode 映射到位
#[test]
fn api_error_shapes_carry_status_code_and_exit() {
    let client = test_client();

    // 404：{error:{code:INSTANCE_NOT_FOUND}}
    let err = client.instance_get("err_404").unwrap_err();
    match &err {
        CliError::Api { status, code, message } => {
            assert_eq!(*status, 404);
            assert_eq!(code, "INSTANCE_NOT_FOUND");
            assert_eq!(message, "x");
        }
        other => panic!("404 应解包为 CliError::Api，实际 {other:?}"),
    }
    assert_eq!(exit::from_cli_error(&err).as_u8(), 3, "404 *_NOT_FOUND → exit 3");

    // 403：EXTENSION_SYSTEM_LOCKED（对 system 扩展 set_enabled）
    let err = client.extension_set_enabled("ext_system", false).unwrap_err();
    match &err {
        CliError::Api { status, code, .. } => {
            assert_eq!(*status, 403);
            assert_eq!(code, "EXTENSION_SYSTEM_LOCKED");
        }
        other => panic!("403 应解包为 CliError::Api，实际 {other:?}"),
    }
    assert_eq!(exit::from_cli_error(&err).as_u8(), 6, "403 内置扩展锁定 → exit 6");

    // 409：INSTANCE_ALREADY_RUNNING
    let err = client.instance_start("err_409").unwrap_err();
    match &err {
        CliError::Api { status, code, .. } => {
            assert_eq!(*status, 409);
            assert_eq!(code, "INSTANCE_ALREADY_RUNNING");
        }
        other => panic!("409 应解包为 CliError::Api，实际 {other:?}"),
    }
    assert_eq!(exit::from_cli_error(&err).as_u8(), 5, "409 运行时冲突 → exit 5");

    // 400：INVALID_REQUEST
    let err = client.env_patch("err_400", json!({})).unwrap_err();
    match &err {
        CliError::Api { status, code, .. } => {
            assert_eq!(*status, 400);
            assert_eq!(code, "INVALID_REQUEST");
        }
        other => panic!("400 应解包为 CliError::Api，实际 {other:?}"),
    }
    assert_eq!(exit::from_cli_error(&err).as_u8(), 7, "400 校验错 → exit 7");
}

/// 协议破坏 · 形状一：2xx 但响应体是纯文本（非 JSON）→ send() 层的 `CliError::Protocol`
#[test]
fn plain_text_response_is_protocol_error() {
    let client = test_client();
    let err = client.instance_get("plain").unwrap_err();
    assert!(matches!(err, CliError::Protocol(_)), "纯文本应得 Protocol，实际 {err:?}");
    assert_eq!(exit::from_cli_error(&err).as_u8(), 1);
}

/// 协议破坏 · 形状二：合法 JSON 但字段与 DTO 契约不符 → decode 层的 `CliError::Protocol`
/// （这是「服务端改字段、CLI 静默错解」防线在运行期的显式暴露点）
#[test]
fn dto_shape_drift_is_protocol_error() {
    let client = test_client();
    let err = client.env_get("bad_shape").unwrap_err();
    match &err {
        CliError::Protocol(msg) => assert!(
            msg.contains("契约不符"),
            "decode 层错误应说明契约漂移，实际 {msg}"
        ),
        other => panic!("形状漂移应得 CliError::Protocol，实际 {other:?}"),
    }
    assert_eq!(exit::from_cli_error(&err).as_u8(), 1);
}

/// 连接拒绝：无服务监听的随机端口 → `CliError::Unreachable`（exit 8）
///
/// 曾经的 flake 根因备忘：reqwest 在特定特性组合下会读 macOS 系统代理，把回环请求
/// 送进代理并拿到 502（误判为 Protocol）。client.rs build_http 的 no_proxy() 修复后
/// 本测试即确定性通过；若再红，优先怀疑代理劫持回归而非端口抢占。
#[test]
fn unreachable_when_no_server_listening() {
    // 先占一个随机端口再释放：拿到「几乎必然无人监听」的端口号，比硬编码抗撞车
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("探针端口绑定失败")
        .local_addr()
        .expect("探针 local_addr 失败")
        .port();
    let client = AgentClient::new(&format!("http://127.0.0.1:{port}")).expect("构造应成功");
    let err = client.env_list().unwrap_err();
    assert!(matches!(err, CliError::Unreachable(_)), "连接拒绝应得 Unreachable，实际 {err:?}");
    assert_eq!(exit::from_cli_error(&err).as_u8(), 8, "不可达 → exit 8");
}
