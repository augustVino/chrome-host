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

use axum::extract::Query;
use axum::http::StatusCode;
use axum::routing::{get, patch, post, put};
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
        "configurable": false,
        "configDocsUrl": null,
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

fn canned_session_status() -> serde_json::Value {
    json!({
        "environmentId": "env_1",
        "policy": "first-login",
        "generation": 3,
        "exportedAt": 1700000000000i64,
        "sourceInstanceId": "ins_9",
        "cookieCount": 12,
        "history": [
            { "generation": 2, "exportedAt": 1690000000000i64, "sourceInstanceId": "ins_4", "cookieCount": 9 },
            { "generation": 1, "exportedAt": 1680000000000i64, "sourceInstanceId": null, "cookieCount": 2 }
        ]
    })
}

fn canned_session_capture_result() -> serde_json::Value {
    json!({
        "environmentId": "env_2",
        "instanceId": "ins_7",
        "generation": 4,
        "cookieCount": 15
    })
}

fn canned_login_profile_view() -> serde_json::Value {
    // LoginProfileView：Ready 态 + 快照已捕获（0.3.0 起无 browserRunning/cdpPort）
    json!({
        "id": "env_1",
        "name": "env_1 母本",
        "status": "ready",
        "snapshotVersion": 3,
        "instancesUsing": 1,
        "lastCapturedAt": 1700000000000i64
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
        "lastCapturedAt": null
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
        // —— extension config（可配置扩展的运行时配置读写）——
        .route(
            "/api/v1/extensions/ext_cfg/config",
            get(|Query(q): Query<std::collections::HashMap<String, String>>| async move {
                Json(json!({
                    "environmentId": q.get("environmentId"),
                    "config": {"block": ["a.com"]}
                }))
            })
                .put(|| async { Json(json!({ "ok": true })) }),
        )
        .route(
            "/api/v1/extensions/ext_bad/config",
            put(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    Json(error_body("EXTENSION_CONFIG_INVALID")),
                )
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
        // 页面诊断快照（透传自由形状 JSON）+ 409 不可用路径
        .route(
            "/api/v1/instances/ins_x/diagnostics",
            get(|| async {
                Json(json!({
                    "instanceId": "ins_x",
                    "targetId": "t1",
                    "targetUrl": "https://app.example.com/home",
                    "diagnostics": {
                        "startedAt": 1,
                        "url": "https://app.example.com/home",
                        "errors": [{ "type": "error", "message": "boom" }],
                        "resources": [],
                        "lcp": 731,
                        "fcp": 120
                    }
                }))
            }),
        )
        // —— 常驻诊断（M6）：console/network 环 + screenshot + har ——
        // console/network 的 tab 透传验证用 echo 路由（ins_tab，见下）
        .route(
            "/api/v1/instances/ins_x/console",
            get(|| async {
                Json(json!({ "instanceId": "ins_x", "entries": [
                    { "ts": 1, "level": "error", "text": "boom", "source": "console", "targetUrl": "https://a/" },
                    { "ts": 2, "level": "warning", "text": "w", "source": "network", "targetUrl": "https://a/" }
                ] }))
            }),
        )
        .route(
            "/api/v1/instances/ins_x/network",
            get(|| async {
                Json(json!({ "instanceId": "ins_x", "entries": [
                    { "ts": 1, "requestId": "r1", "method": "GET", "url": "https://a/404", "status": 404, "failed": false, "targetUrl": "https://a/" }
                ] }))
            }),
        )
        // tab 过滤透传验证：echo 收到的 query 为 meta（同 extension config 的 echo 先例）
        .route(
            "/api/v1/instances/ins_tab/console",
            get(|Query(q): Query<std::collections::HashMap<String, String>>| async move {
                Json(json!({
                    "instanceId": "ins_tab",
                    "entries": [],
                    "meta": {
                        "filteredByTab": q.get("tab").cloned().unwrap_or_default(),
                        "tabFilterUnavailable": false,
                        "before": 2,
                        "after": 1
                    }
                }))
            }),
        )
        .route(
            "/api/v1/instances/ins_tab/network",
            get(|Query(q): Query<std::collections::HashMap<String, String>>| async move {
                Json(json!({
                    "instanceId": "ins_tab",
                    "entries": [],
                    "meta": {
                        "filteredByTab": q.get("tab").cloned().unwrap_or_default(),
                        "tabFilterUnavailable": false,
                        "before": 0,
                        "after": 0
                    }
                }))
            }),
        )
        .route(
            "/api/v1/instances/ins_x/screenshot",
            get(|| async {
                // 1x1 透明 png（真实魔数，验证客户端解码校验链）
                Json(json!({ "instanceId": "ins_x", "format": "png", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==" }))
            }),
        )
        .route(
            "/api/v1/instances/ins_x/har",
            get(|| async {
                Json(json!({ "log": { "version": "1.2", "creator": { "name": "chrome-host", "version": "0.2.3" }, "pages": [], "entries": [
                    { "request": { "method": "GET", "url": "https://a/x" }, "response": { "status": 200 } }
                ] } }))
            }),
        )
        .route(
            "/api/v1/instances/err_409_diag/console",
            get(|| async {
                (
                    StatusCode::CONFLICT,
                    Json(error_body("DIAGNOSTICS_UNAVAILABLE")),
                )
            }),
        )
        .route(
            "/api/v1/instances/err_409_diag/diagnostics",
            get(|| async {
                (
                    StatusCode::CONFLICT,
                    Json(error_body("DIAGNOSTICS_UNAVAILABLE")),
                )
            }),
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
            "/api/v1/environments/env_1/session",
            get(|| async { Json(canned_session_status()) }),
        )
        .route(
            "/api/v1/environments/env_1/session/restore",
            post(|| async { Json(canned_session_status()) }),
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
            "/api/v1/environments/env_2/login-profile/capture",
            // 按 body 分支：带 fromInstanceId → 从实例捕获结果；无 body → 传统母本 View
            post(|body: Option<Json<serde_json::Value>>| async move {
                match body {
                    Some(Json(v)) if v.get("fromInstanceId").is_some() => {
                        axum::response::IntoResponse::into_response(Json(canned_session_capture_result()))
                    }
                    _ => axum::response::IntoResponse::into_response(Json(canned_login_profile_view())),
                }
            }),
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
        // —— page 控制命令族（P2-C）：正常响应 + 参数透传 echo + 错误样本 ——
        .route(
            "/api/v1/instances/ins_x/page/snap",
            post(|| async {
                Json(json!({
                    "instanceId": "ins_x",
                    "targetId": "TAB9F",
                    "text": "[WebArea] Home\n  @e1 [button] OK\n  @e2 [textbox] q",
                    "refs_count": 2,
                    "generation": 1
                }))
            }),
        )
        .route(
            "/api/v1/instances/ins_x/page/click",
            post(|| async { Json(json!({ "result": "Clicked @e1 \"OK\" at CSS (100, 50)", "instanceId": "ins_x", "targetId": "TAB9F" })) }),
        )
        .route(
            "/api/v1/instances/ins_x/page/fill",
            post(|| async { Json(json!({ "result": "Filled <INPUT> \"q\" with \"\" (0 chars)", "instanceId": "ins_x", "targetId": "TAB9F" })) }),
        )
        .route(
            "/api/v1/instances/ins_x/page/press",
            post(|| async { Json(json!({ "result": "Pressed Enter", "instanceId": "ins_x", "targetId": "TAB9F", "dialogs_seen": [
                { "tab_id": "TAB9F", "type": "alert", "message": "hi", "handled": "accepted", "sequence": 1 }
            ] })) }),
        )
        .route(
            "/api/v1/instances/ins_x/page/wait",
            post(|| async { Json(json!({ "result": "Waited: .ready is visible (after 120ms, 1 polls)", "instanceId": "ins_x", "targetId": "TAB9F" })) }),
        )
        .route(
            "/api/v1/instances/ins_x/page/dialogs",
            get(|Query(q): Query<std::collections::HashMap<String, String>>| async move {
                Json(json!({
                    "instanceId": "ins_x",
                    "targetId": "TAB9F",
                    "cursor": 2,
                    "entries": [
                        { "tab_id": "TAB9F", "type": "prompt", "message": "名字？", "handled": "accepted", "sequence": 1, "default_prompt": "Bob" },
                        { "tab_id": "TAB9F", "type": "alert", "message": "m2", "handled": "accepted", "sequence": 2 }
                    ],
                    "meta": { "since": q.get("since"), "tab": q.get("tab") }
                }))
            }),
        )
        // —— page 控制命令族（P2-G）：eval/html/clickxy/loadall/type 正常响应 ——
        .route(
            "/api/v1/instances/ins_x/page/eval",
            post(|| async {
                Json(json!({ "result": "{\n  \"ok\": true\n}", "instanceId": "ins_x", "targetId": "TAB9F" }))
            }),
        )
        .route(
            "/api/v1/instances/ins_x/page/html",
            post(|| async {
                Json(json!({ "html": "<!DOCTYPE html><html></html>", "instanceId": "ins_x", "targetId": "TAB9F" }))
            }),
        )
        .route(
            "/api/v1/instances/ins_x/page/clickxy",
            post(|| async {
                Json(json!({ "result": "Clicked at CSS (150, 130)", "instanceId": "ins_x", "targetId": "TAB9F" }))
            }),
        )
        .route(
            "/api/v1/instances/ins_x/page/loadall",
            post(|| async {
                Json(json!({ "result": "Clicked \".more\" 3 time(s) until it disappeared", "instanceId": "ins_x", "targetId": "TAB9F" }))
            }),
        )
        .route(
            "/api/v1/instances/ins_x/page/type",
            post(|| async {
                Json(json!({ "result": "Typed 5 characters", "instanceId": "ins_x", "targetId": "TAB9F" }))
            }),
        )
        // 参数透传 echo：请求体原样回显（验证 client 拼的 body 形状，同 capture_from 先例）
        .route(
            "/api/v1/instances/ins_pg/page/eval",
            post(|body: Option<Json<serde_json::Value>>| async move {
                Json(json!({ "echo": body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null) }))
            }),
        )
        .route(
            "/api/v1/instances/ins_pg/page/html",
            post(|body: Option<Json<serde_json::Value>>| async move {
                Json(json!({ "echo": body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null) }))
            }),
        )
        .route(
            "/api/v1/instances/ins_pg/page/clickxy",
            post(|body: Option<Json<serde_json::Value>>| async move {
                Json(json!({ "echo": body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null) }))
            }),
        )
        .route(
            "/api/v1/instances/ins_pg/page/loadall",
            post(|body: Option<Json<serde_json::Value>>| async move {
                Json(json!({ "echo": body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null) }))
            }),
        )
        .route(
            "/api/v1/instances/ins_pg/page/type",
            post(|body: Option<Json<serde_json::Value>>| async move {
                Json(json!({ "echo": body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null) }))
            }),
        )
        // 参数透传 echo：请求体原样回显（验证 client 拼的 body 形状，同 capture_from 先例）
        .route(
            "/api/v1/instances/ins_pg/page/click",
            post(|body: Option<Json<serde_json::Value>>| async move {
                Json(json!({ "echo": body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null) }))
            }),
        )
        .route(
            "/api/v1/instances/ins_pg/page/fill",
            post(|body: Option<Json<serde_json::Value>>| async move {
                Json(json!({ "echo": body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null) }))
            }),
        )
        .route(
            "/api/v1/instances/ins_pg/page/press",
            post(|body: Option<Json<serde_json::Value>>| async move {
                Json(json!({ "echo": body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null) }))
            }),
        )
        .route(
            "/api/v1/instances/ins_pg/page/wait",
            post(|body: Option<Json<serde_json::Value>>| async move {
                Json(json!({ "echo": body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null) }))
            }),
        )
        // 错误样本：page 族四类代表映射（404→3 / 409→5 / 422→7 / 504→5 运行时码表）
        .route(
            "/api/v1/instances/err_tab/page/click",
            post(|| async { (StatusCode::NOT_FOUND, Json(error_body("PAGE_TAB_NOT_FOUND"))) }),
        )
        .route(
            "/api/v1/instances/err_stale/page/click",
            post(|| async { (StatusCode::CONFLICT, Json(error_body("PAGE_REF_STALE"))) }),
        )
        .route(
            "/api/v1/instances/err_vis/page/click",
            post(|| async {
                (StatusCode::UNPROCESSABLE_ENTITY, Json(error_body("PAGE_ELEMENT_NOT_VISIBLE")))
            }),
        )
        .route(
            "/api/v1/instances/err_busy/page/click",
            post(|| async {
                (
                    StatusCode::from_u16(504).unwrap(),
                    Json(error_body("PAGE_TOOL_TIMEOUT")),
                )
            }),
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
                    tx.send(format!("http://{addr}"))
                        .expect("通知测试主线程失败");
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
        .env_create(
            "测试环境",
            Some("https://example.com/hosts"),
            None,
            Some("first-login"),
        )
        .expect("env_create 应成功");
    // fixture 是 canned 契约（不回显 body）：断言点在 DTO 的 Some/null 双向解码，
    // 即 Option<String> 对字符串值与 null 各自正确落位
    assert_eq!(env.id, "env_1");
    assert_eq!(env.name, "测试环境");
    assert_eq!(
        env.hosts_source_url.as_deref(),
        Some("https://example.com/hosts.txt")
    );
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

    let created = client
        .instance_create("env_1")
        .expect("instance_create 应成功");
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

    client
        .instance_delete("ins_1")
        .expect("instance_delete 应成功");
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

    let toggled = client
        .extension_set_enabled("ext_user", false)
        .expect("应成功");
    assert_eq!(toggled.id, "ext_user");

    client
        .extension_delete("ext_user")
        .expect("extension_delete 应成功");
}

/// extension config：GET 往返（含 environmentId query 路径）、PUT 成功、
/// 400 EXTENSION_CONFIG_INVALID 错误解包与 exit 映射（未知 400 码 → VALIDATION 兜底）
#[test]
fn extension_config_endpoints_roundtrip_and_error() {
    let client = test_client();

    let resp = client
        .extension_config_get("ext_cfg", None)
        .expect("全局默认配置读取应成功");
    assert_eq!(resp["environmentId"], serde_json::json!(null));
    assert_eq!(resp["config"]["block"][0], "a.com");

    let resp = client
        .extension_config_get("ext_cfg", Some("env_1"))
        .expect("per-env 配置读取应成功");
    assert_eq!(resp["environmentId"], "env_1");

    let config = serde_json::json!({"block": ["*.google-analytics.com"]});
    let ok = client
        .extension_config_set("ext_cfg", None, &config)
        .expect("写入应成功");
    assert_eq!(ok["ok"], true);

    let err = client
        .extension_config_set("ext_bad", None, &serde_json::json!({"block": [42]}))
        .unwrap_err();
    match &err {
        CliError::Api { status, code, .. } => {
            assert_eq!(*status, 400);
            assert_eq!(code, "EXTENSION_CONFIG_INVALID");
        }
        other => panic!("应为 Api 变体: {other:?}"),
    }
    assert_eq!(
        exit::from_cli_error(&err).as_u8(),
        7,
        "400 未特判码 EXTENSION_CONFIG_INVALID → VALIDATION 兜底 exit 7"
    );
}

/// instance diagnostics：透传快照解包 + 409 DIAGNOSTICS_UNAVAILABLE 的
/// code/exit 映射（409 泛化 → RUNTIME exit 5，无特判码）
#[test]
fn instance_diagnostics_passthrough_and_unavailable() {
    let client = test_client();

    let resp = client.instance_diagnostics("ins_x").expect("应成功");
    assert_eq!(resp["targetUrl"], "https://app.example.com/home");
    assert_eq!(resp["diagnostics"]["errors"][0]["message"], "boom");
    assert_eq!(resp["diagnostics"]["lcp"], 731);

    let err = client.instance_diagnostics("err_409_diag").unwrap_err();
    match &err {
        CliError::Api { status, code, .. } => {
            assert_eq!(*status, 409);
            assert_eq!(code, "DIAGNOSTICS_UNAVAILABLE");
        }
        other => panic!("应为 Api 变体: {other:?}"),
    }
    assert_eq!(
        exit::from_cli_error(&err).as_u8(),
        5,
        "409 → RUNTIME exit 5"
    );
}

/// 常驻诊断（M6）四方法面：console/network 环解包（含 query 透传）、
/// screenshot 的 base64+PNG 魔数解码链、har 文档解包、409 → exit 5、
/// P2-D 的 tab 过滤参数透传（echo 路由验证服务端确实收到 tab）
#[test]
fn resident_diagnostics_endpoints() {
    let client = test_client();

    let console = client
        .instance_console("ins_x", Some("error"), None, Some(10))
        .expect("console 应成功");
    assert_eq!(console["entries"].as_array().unwrap().len(), 2);

    let network = client
        .instance_network("ins_x", true, None, None)
        .expect("network 应成功");
    assert_eq!(network["entries"][0]["status"], 404);

    // P2-D：tab 参数由 client 原样拼入 query（echo 路由回显为 meta.filteredByTab）
    let console = client
        .instance_console("ins_tab", None, Some("tab-9f"), None)
        .expect("console tab 透传应成功");
    assert_eq!(console["meta"]["filteredByTab"], "tab-9f");
    assert_eq!(console["meta"]["tabFilterUnavailable"], false);

    let network = client
        .instance_network("ins_tab", false, Some("tab-9f"), None)
        .expect("network tab 透传应成功");
    assert_eq!(network["meta"]["filteredByTab"], "tab-9f");

    let shot = client
        .instance_screenshot("ins_x")
        .expect("screenshot 应成功");
    assert_eq!(shot["format"], "png");
    // 客户端解码链（base64 + PNG 魔数）由截图命令内部使用，此处至少验 base64 可解
    let data = shot["data"].as_str().unwrap();
    assert!(data.len() > 40, "非空截图数据");

    let har = client.instance_har("ins_x").expect("har 应成功");
    assert_eq!(har["log"]["version"], "1.2");
    assert_eq!(har["log"]["entries"].as_array().unwrap().len(), 1);

    let err = client
        .instance_console("err_409_diag", None, None, None)
        .unwrap_err();
    match &err {
        CliError::Api { status, code, .. } => {
            assert_eq!(*status, 409);
            assert_eq!(code, "DIAGNOSTICS_UNAVAILABLE");
        }
        other => panic!("应为 Api 变体: {other:?}"),
    }
    assert_eq!(exit::from_cli_error(&err).as_u8(), 5);
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
    let status = client
        .instance_status("ins_x")
        .expect("instance_status 应成功");
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
    let focused = client
        .instance_focus("ins_1")
        .expect("instance_focus 应成功");
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
    assert_eq!(
        exit::from_cli_error(&err).as_u8(),
        5,
        "409 运行时冲突 → exit 5"
    );
}

/// env activity（含 null 可空字段一例）+ env status 计数视图
#[test]
fn env_activity_and_status_contract() {
    let client = test_client();

    let events = client
        .env_activity("env_1", 2)
        .expect("env_activity 应成功");
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
fn session_endpoints_contract() {
    let client = test_client();

    // 状态：policy/generation/历史代（新 → 旧），可空 sourceInstanceId=null 兜底
    let s = client
        .session_status("env_1")
        .expect("session_status 应成功");
    assert_eq!(s.policy, "first-login");
    assert_eq!(s.generation, 3);
    assert_eq!(s.cookie_count, 12);
    assert_eq!(s.source_instance_id.as_deref(), Some("ins_9"));
    assert_eq!(s.history.len(), 2);
    assert_eq!(s.history[0].generation, 2);
    assert!(s.history[1].source_instance_id.is_none());

    // 回滚：响应仍是 SessionStatus
    let restored = client.session_restore("env_1", 2).expect("restore 应成功");
    assert_eq!(restored.environment_id, "env_1");

    // 从实例捕获：独立响应形状（environmentId/instanceId/generation/cookieCount）
    let cap = client
        .login_profile_capture_from("env_2", "ins_7")
        .expect("capture_from 应成功");
    assert_eq!(cap.instance_id, "ins_7");
    assert_eq!(cap.generation, 4);
    assert_eq!(cap.cookie_count, 15);
}

#[test]
fn login_profile_endpoints_contract() {
    let client = test_client();

    // 列表 Row：含环境名、无 cdpPort 字段、lastCapturedAt=null（从未捕获）
    let rows = client
        .login_profile_list()
        .expect("login_profile_list 应成功");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].environment_id, "env_2");
    assert_eq!(rows[0].environment_name, "第二环境");
    assert_eq!(rows[0].status, LoginProfileStatus::NotConfigured);
    assert_eq!(rows[0].snapshot_version, 0);
    assert!(rows[0].last_captured_at.is_none());

    // 单环境 View（0.3.0 起 browserRunning/cdpPort 随 launch 移除而删除）
    let view = client.login_profile_get("env_1").expect("应成功");
    assert_eq!(view.status, LoginProfileStatus::Ready);
    assert_eq!(view.snapshot_version, 3);
    assert_eq!(view.instances_using, 1);
    assert_eq!(view.last_captured_at, Some(1_700_000_000_000));

    // 两个动作统一回 View（fixture 是 canned 契约，不回显请求语境）
    for view in [
        client
            .login_profile_capture("env_2")
            .expect("capture 应成功"),
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
    assert_eq!(
        exit::from_cli_error(&err).as_u8(),
        5,
        "409 运行时冲突 → exit 5"
    );
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

/// page 控制命令族（P2-C）全方法面：正常响应解包、请求体/query 透传
/// （echo 路由验证服务端确实收到 tab/target/value/key/timeout/since）、
/// 四类代表错误码的 exit 映射（404→3 / 409→5 / 422→7 / 504→5 运行时码表）
#[test]
fn page_endpoints_contract() {
    let client = test_client();

    // snap：文本树 + 引用计数 + 代次（@eN 操作循环的起点）
    let snap = client.page_snap("ins_x", None).expect("snap 应成功");
    assert_eq!(snap["targetId"], "TAB9F");
    assert_eq!(snap["refs_count"], 2);
    assert_eq!(snap["generation"], 1);
    assert!(snap["text"].as_str().unwrap().contains("@e1 [button] OK"));

    // click：result 行 + 参数透传（target 与显式 tab 均进 body）
    let click = client
        .page_click("ins_x", "@e1", Some("TAB9F"))
        .expect("click 应成功");
    assert_eq!(click["result"], "Clicked @e1 \"OK\" at CSS (100, 50)");
    let echo = client
        .page_click("ins_pg", "#save", Some("TAB"))
        .expect("click echo 应成功");
    assert_eq!(echo["echo"]["target"], "#save");
    assert_eq!(echo["echo"]["tab"], "TAB");

    // fill：空串 value 必须原样透传（空串 = 仅清空的服务端语义）
    let echo = client
        .page_fill("ins_pg", "#q", "", None)
        .expect("fill echo 应成功");
    assert_eq!(echo["echo"]["target"], "#q");
    assert_eq!(echo["echo"]["value"], "");
    assert_eq!(
        echo["echo"]["tab"],
        serde_json::json!(null),
        "缺省 tab 序列化为 null 而非省略"
    );

    // press：key + 可选 target；wait：selector/state/timeout（时长字符串原样透传）
    let echo = client
        .page_press("ins_pg", "Control+A", Some("@e2"), Some("TAB"))
        .expect("press echo 应成功");
    assert_eq!(echo["echo"]["key"], "Control+A");
    assert_eq!(echo["echo"]["target"], "@e2");
    let echo = client
        .page_wait("ins_pg", ".ready", Some("visible"), Some("10s"), None)
        .expect("wait echo 应成功");
    assert_eq!(echo["echo"]["selector"], ".ready");
    assert_eq!(echo["echo"]["state"], "visible");
    assert_eq!(echo["echo"]["timeout"], "10s");

    // press 正常路径：dialogs_seen 附在成功响应（snake_case 条目，同 dialogs 端点）
    let press = client
        .page_press("ins_x", "Enter", None, None)
        .expect("应成功");
    assert_eq!(press["result"], "Pressed Enter");
    assert_eq!(press["dialogs_seen"][0]["type"], "alert");

    // wait 正常路径：人性化结果串
    let wait = client
        .page_wait("ins_x", ".ready", None, None, None)
        .expect("wait 应成功");
    assert!(wait["result"].as_str().unwrap().contains("is visible"));

    // dialogs：cursor/entries + query 透传（tab/since，同 console 的 meta echo 先例）
    let dialogs = client
        .page_dialogs("ins_x", Some("TAB9F"), Some(1))
        .expect("dialogs 应成功");
    assert_eq!(dialogs["cursor"], 2);
    let entries = dialogs["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["type"], "prompt");
    assert_eq!(entries[0]["default_prompt"], "Bob");
    assert_eq!(entries[1]["sequence"], 2);
    assert_eq!(dialogs["meta"]["tab"], "TAB9F");
    assert_eq!(dialogs["meta"]["since"], "1");

    // P2-G 五件：eval/html/clickxy/loadall/type —— 正常响应 + 透传 body 形状
    // （maxClicks 为首个多词 body 键，camelCase 对齐 tabId/failedOnly 先例）
    let eval = client
        .page_eval("ins_x", "({ok:true})", None)
        .expect("eval 应成功");
    assert_eq!(eval["result"], "{\n  \"ok\": true\n}");
    let html = client.page_html("ins_x", None, None).expect("html 应成功");
    assert_eq!(html["html"], "<!DOCTYPE html><html></html>");
    let clickxy = client
        .page_clickxy("ins_x", 150.0, 130.0, None)
        .expect("clickxy 应成功");
    assert_eq!(clickxy["result"], "Clicked at CSS (150, 130)");
    let loadall = client
        .page_loadall("ins_x", ".more", None, None, None)
        .expect("loadall 应成功");
    assert_eq!(
        loadall["result"],
        "Clicked \".more\" 3 time(s) until it disappeared"
    );
    let typed = client
        .page_type("ins_x", "hello", None)
        .expect("type 应成功");
    assert_eq!(typed["result"], "Typed 5 characters");

    let echo = client
        .page_eval("ins_pg", "document.title", Some("TAB"))
        .expect("eval echo 应成功");
    assert_eq!(echo["echo"]["expression"], "document.title");
    assert_eq!(echo["echo"]["tab"], "TAB");
    let echo = client
        .page_html("ins_pg", Some("#main"), None)
        .expect("html echo 应成功");
    assert_eq!(echo["echo"]["selector"], "#main");
    let echo = client
        .page_clickxy("ins_pg", 1.5, 2.0, None)
        .expect("clickxy echo 应成功");
    assert_eq!(echo["echo"]["x"], 1.5);
    assert_eq!(echo["echo"]["y"], 2.0);
    let echo = client
        .page_loadall("ins_pg", ".more", Some("1500ms"), Some(50), None)
        .expect("loadall echo 应成功");
    assert_eq!(echo["echo"]["interval"], "1500ms");
    assert_eq!(echo["echo"]["maxClicks"], 50);
    let echo = client
        .page_type("ins_pg", "hi", None)
        .expect("type echo 应成功");
    assert_eq!(echo["echo"]["text"], "hi");

    // 错误映射：404 tab 路由未命中 → exit 3
    let err = client.page_click("err_tab", "@e1", None).unwrap_err();
    match &err {
        CliError::Api { status, code, .. } => {
            assert_eq!((*status, code.as_str()), (404, "PAGE_TAB_NOT_FOUND"));
        }
        other => panic!("应为 Api 变体: {other:?}"),
    }
    assert_eq!(exit::from_cli_error(&err).as_u8(), 3);

    // 409 引用失效 → exit 5（重 snap 而非重试）
    let err = client.page_click("err_stale", "@e1", None).unwrap_err();
    assert_eq!(exit::from_cli_error(&err).as_u8(), 5);

    // 422 元素不可见 → exit 7（未知 4xx → VALIDATION 兑底）
    let err = client.page_click("err_vis", ".x", None).unwrap_err();
    assert_eq!(exit::from_cli_error(&err).as_u8(), 7);

    // 504 工具预算超时 → exit 5（RUNTIME_ERROR_CODES 显式条目，非 GENERAL 兑底）
    let err = client.page_click("err_busy", ".x", None).unwrap_err();
    match &err {
        CliError::Api { status, code, .. } => {
            assert_eq!((*status, code.as_str()), (504, "PAGE_TOOL_TIMEOUT"));
        }
        other => panic!("应为 Api 变体: {other:?}"),
    }
    assert_eq!(exit::from_cli_error(&err).as_u8(), 5);
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
        CliError::Api {
            status,
            code,
            message,
        } => {
            assert_eq!(*status, 404);
            assert_eq!(code, "INSTANCE_NOT_FOUND");
            assert_eq!(message, "x");
        }
        other => panic!("404 应解包为 CliError::Api，实际 {other:?}"),
    }
    assert_eq!(
        exit::from_cli_error(&err).as_u8(),
        3,
        "404 *_NOT_FOUND → exit 3"
    );

    // 403：EXTENSION_SYSTEM_LOCKED（对 system 扩展 set_enabled）
    let err = client
        .extension_set_enabled("ext_system", false)
        .unwrap_err();
    match &err {
        CliError::Api { status, code, .. } => {
            assert_eq!(*status, 403);
            assert_eq!(code, "EXTENSION_SYSTEM_LOCKED");
        }
        other => panic!("403 应解包为 CliError::Api，实际 {other:?}"),
    }
    assert_eq!(
        exit::from_cli_error(&err).as_u8(),
        6,
        "403 内置扩展锁定 → exit 6"
    );

    // 409：INSTANCE_ALREADY_RUNNING
    let err = client.instance_start("err_409").unwrap_err();
    match &err {
        CliError::Api { status, code, .. } => {
            assert_eq!(*status, 409);
            assert_eq!(code, "INSTANCE_ALREADY_RUNNING");
        }
        other => panic!("409 应解包为 CliError::Api，实际 {other:?}"),
    }
    assert_eq!(
        exit::from_cli_error(&err).as_u8(),
        5,
        "409 运行时冲突 → exit 5"
    );

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
    assert!(
        matches!(err, CliError::Protocol(_)),
        "纯文本应得 Protocol，实际 {err:?}"
    );
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
    assert!(
        matches!(err, CliError::Unreachable(_)),
        "连接拒绝应得 Unreachable，实际 {err:?}"
    );
    assert_eq!(exit::from_cli_error(&err).as_u8(), 8, "不可达 → exit 8");
}
