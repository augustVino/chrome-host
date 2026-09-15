//! login-profile 命令组 —— 登录态生命周期管理（list/get/launch/capture/reset）。
//!
//! 生命周期一句话：`launch` 打开登录浏览器 → 人工登录 → 关闭浏览器 → `capture`
//! 捕获快照 → 之后 `instance create` 的实例自动克隆该快照。
//!
//! 命名：组名 `login-profile`，不采用 PRD cli.md 的
//! `profile` —— 实例域已有 profile_dir（浏览器用户数据目录）概念，裸 `profile`
//! 会在命令面与它混淆；API 路径本就是 /api/v1/login-profiles，CLI 跟随资源命名。
//!
//! 409 PROFILE_IN_USE 处置 vs EXTENSION_SYSTEM_LOCKED 翻译先例的对比：
//! - EXTENSION_SYSTEM_LOCKED 语义单一（只可能是「内置扩展被系统锁定」），CLI
//!   统一翻译为固定中文文案是安全的，不会丢失信息；
//! - PROFILE_IN_USE 则有多个子场景（登录浏览器运行中 / 捕获已在进行中，
//!   profile_service.rs 两个抛出点），统一翻译必然丢掉具体原因；服务端 message
//!   已是面向用户的中文 → 本组所有写命令对 409 一律**原样透传**（不匹配错误臂、
//!   不覆写 message），由 `?` 自然上抛 → exit 5。
//!
//! 固定 handler 结构与 env 组一致：构造 [`OutputCtx`] → 调 [`AgentClient`] →
//! 渲染，三步之外无他物；本文件禁止出现 reqwest / StatusCode 等 HTTP 细节
//! （铁律见 client.rs 模块注释）。

use clap::Subcommand;

use super::confirm_or_yes;
use super::output::{OutputCtx, TableRow};
use super::GlobalArgs;
use crate::client::AgentClient;
use crate::error::CliError;
use crate::model::{LoginProfileRow, LoginProfileStatus, LoginProfileView};

/// login-profile 子命令树。
#[derive(Debug, Subcommand)]
pub enum LoginProfileCommands {
    /// 列出全部环境登录态
    #[command(about = "列出全部环境登录态")]
    List,

    /// 查看环境登录态视图
    #[command(about = "查看环境登录态视图")]
    Get {
        /// 环境 id
        env_id: String,
    },

    /// 打开登录浏览器（人工登录后关闭，再用 capture 捕获快照）
    #[command(about = "打开登录浏览器（人工登录后关闭，再用 capture 捕获快照）")]
    Launch {
        /// 环境 id
        env_id: String,
    },

    /// 捕获登录态快照（覆盖已有快照时需确认；须先关闭登录浏览器）
    #[command(about = "捕获登录态快照（覆盖已有快照时需确认；须先关闭登录浏览器）")]
    Capture {
        /// 环境 id
        env_id: String,
        /// 跳过确认（非交互环境必需）
        #[arg(long)]
        yes: bool,
    },

    /// 清空登录态快照（危险操作，总是确认）
    #[command(about = "清空登录态快照（危险操作，总是确认）")]
    Reset {
        /// 环境 id
        env_id: String,
        /// 跳过确认（非交互环境必需）
        #[arg(long)]
        yes: bool,
    },
}

/// login-profile 组分发入口（main.rs dispatch 的唯一调用点）。
pub fn run(
    cmd: &LoginProfileCommands,
    globals: &GlobalArgs,
    client: &AgentClient,
) -> Result<(), CliError> {
    match cmd {
        LoginProfileCommands::List => list(globals, client),
        LoginProfileCommands::Get { env_id } => get(globals, client, env_id),
        LoginProfileCommands::Launch { env_id } => launch(globals, client, env_id),
        LoginProfileCommands::Capture { env_id, yes } => capture(globals, client, env_id, *yes),
        LoginProfileCommands::Reset { env_id, yes } => reset(globals, client, env_id, *yes),
    }
}

/// status 枚举 → 线上 snake_case 字符串（STATUS 列「snake_case 原样」）。经 serde
/// 序列化取值（instance 组 status_label 同款）：与 DTO 的 `rename_all` 同一来源，
/// 服务端加状态变体时本函数零改动不漂移；Unknown 兜底同样序列化为 "unknown"。
fn status_label(status: &LoginProfileStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

/// Option 字段的 table/kv 占位：None → `-`（占位仅人类可读模式；json 模式经
/// DTO 原样序列化保留 null，两模式无信息差）。
fn option_cell<T: ToString>(value: Option<T>) -> String {
    value.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
}

/// `chrome-host login-profile list`。table 列集冻结：
/// ENVIRONMENT NAME STATUS SNAPSHOT INSTANCES LAST-CAPTURED
/// （environmentName/name/status 原样 / snapshotVersion / instancesUsing /
/// lastCapturedAt——epoch ms 原样不转可读时间，理由同 env 组：chrono 不在依赖
/// 清单且 json 模式输出的同为原始值，可读化交给消费侧；空值 `-`）。
/// json 模式输出 LoginProfileRow 数组原样；quiet 每行一个 id。
fn list(globals: &GlobalArgs, client: &AgentClient) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let profiles: Vec<LoginProfileRow> = client.login_profile_list()?;
    let rows: Vec<TableRow> = profiles
        .iter()
        .map(|p| TableRow {
            id: p.id.clone(),
            cells: vec![
                p.environment_name.clone(),
                p.name.clone(),
                status_label(&p.status),
                p.snapshot_version.to_string(),
                p.instances_using.to_string(),
                option_cell(p.last_captured_at),
            ],
            // json 字段 = 该行完整 JSON：DTO serde 属性两侧一致，序列化即服务端原样
            json: serde_json::to_value(p).expect("纯数据 DTO 的序列化不会失败"),
        })
        .collect();
    out.render_list(
        &[
            "ENVIRONMENT",
            "NAME",
            "STATUS",
            "SNAPSHOT",
            "INSTANCES",
            "LAST-CAPTURED",
        ],
        &rows,
    );
    Ok(())
}

/// LoginProfileView 的单对象 key: value 对，键名与 JSON 字段一致（camelCase 原样，
/// 两模式无信息差）。布尔原样 true/false（Rust bool Display 即小写字面量）；
/// lastCapturedAt/cdpPort 空值 `-`，时间戳 epoch ms 原样。
fn view_kv_pairs(view: &LoginProfileView) -> Vec<(String, String)> {
    vec![
        ("id".into(), view.id.clone()),
        ("name".into(), view.name.clone()),
        ("status".into(), status_label(&view.status)),
        ("snapshotVersion".into(), view.snapshot_version.to_string()),
        ("instancesUsing".into(), view.instances_using.to_string()),
        (
            "lastCapturedAt".into(),
            option_cell(view.last_captured_at),
        ),
        ("browserRunning".into(), view.browser_running.to_string()),
        ("cdpPort".into(), option_cell(view.cdp_port)),
    ]
}

/// get / launch / capture / reset 共用的单对象渲染：json 原样、缺省 kv。
fn render_view(out: &OutputCtx, view: &LoginProfileView) {
    out.render_kv(
        &view_kv_pairs(view),
        serde_json::to_value(view).expect("纯数据 DTO 的序列化不会失败"),
    );
}

/// launch/capture/reset 成功后的统一出口：quiet 输出主实体 id（变更类命令契约，
/// 与 env create/update 同约定）；其余渲染更新后的完整 LoginProfileView。
fn render_updated(out: &OutputCtx, view: &LoginProfileView) {
    if out.quiet {
        out.print_id(&view.id);
        return;
    }
    render_view(out, view);
}

/// `chrome-host login-profile get <ENV_ID>`：登录态视图（只读）。
/// 对尚无登录态的环境，服务端惰性建行并返回 not_configured 视图（而非 404），
/// 本命令因此可对任意存在的环境直接调用。
fn get(globals: &GlobalArgs, client: &AgentClient, env_id: &str) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let view = client.login_profile_get(env_id)?;
    render_view(&out, &view);
    Ok(())
}

/// `chrome-host login-profile launch <ENV_ID>`：用母本目录打开登录浏览器
/// （非破坏性，不确认）。渲染更新后的视图。
fn launch(globals: &GlobalArgs, client: &AgentClient, env_id: &str) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    // 重复 launch（浏览器已开）→ 409 PROFILE_IN_USE 原样透传（见模块注释的多子
    // 场景对比）：不在此匹配错误臂，`?` 直接把服务端中文 message 带给用户 → exit 5
    let view = client.login_profile_launch(env_id)?;
    if out.quiet {
        out.print_id(&view.id);
        return Ok(());
    }
    render_view(&out, &view);
    // launch 区别于其他命令的价值点：打开的登录浏览器带 CDP 调试端口，自动化
    // 登录（Playwright / Selenium connect_over_cdp）以它为入口 —— 该行把「下一
    // 步怎么接」直接给到调用方。仅人类模式追加：json 消费方按字段编程、quiet
    // 契约是单行 id，提示行都会污染机器可读输出（与 render_kv 的 json 短路同理）。
    if let Some(port) = view.cdp_port {
        out.render_text(format!("自动化登录可用 CDP: 127.0.0.1:{port}"));
    }
    Ok(())
}

/// `chrome-host login-profile capture <ENV_ID> [--yes]`：停机捕获登录态快照。
///
/// 确认条件：仅 snapshotVersion > 0（覆盖已有快照）时确认；
/// version 0 = 首次捕获无破坏性，不确认直接执行（与 env delete「有数据才确认」
/// 同一精神）。因此先 GET 只读视图再决策——该 GET 即使随后被确认流拦下（exit 2）
/// 也无副作用；确认提示携带当前版本号，让用户明确知道要覆盖的是什么。
fn capture(
    globals: &GlobalArgs,
    client: &AgentClient,
    env_id: &str,
    sub_yes: bool,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    // 子命令级 --yes 与全局 --yes 等效：任一放行即跳过确认流（env delete 同约定）
    let yes = sub_yes || globals.yes;

    let current = client.login_profile_get(env_id)?;
    if !yes && current.snapshot_version > 0 {
        confirm_or_yes(
            globals,
            &format!(
                "捕获将覆盖现有快照（当前版本 {}），继续?",
                current.snapshot_version
            ),
        )?;
    }

    // 409 PROFILE_IN_USE 原样透传不翻译（多子场景：登录浏览器运行中 / 捕获已在
    // 进行中——见模块注释对比；服务端 message 已面向用户）→ exit 5
    let view = client.login_profile_capture(env_id)?;
    render_updated(&out, &view);
    Ok(())
}

/// `chrome-host login-profile reset <ENV_ID> [--yes]`：清空登录态快照。
///
/// 总是确认（不同于 capture 的条件确认）：reset 没有「首次 vs 覆盖」之分——
/// 清空动作对任何版本的快照都是不可逆丢失，且影响面是之后创建的全部实例。
/// 409 PROFILE_IN_USE 同 capture：原样透传。
fn reset(
    globals: &GlobalArgs,
    client: &AgentClient,
    env_id: &str,
    sub_yes: bool,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let yes = sub_yes || globals.yes;

    if !yes {
        confirm_or_yes(
            globals,
            "清空登录态快照，该环境之后创建的实例将不再携带登录态，继续?",
        )?;
    }

    let view = client.login_profile_reset(env_id)?;
    render_updated(&out, &view);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// status_label 经 serde 取 snake_case 原样（STATUS 列「原样」契约的护栏）。
    #[test]
    fn status_label_renders_snake_case() {
        assert_eq!(status_label(&LoginProfileStatus::NotConfigured), "not_configured");
        assert_eq!(status_label(&LoginProfileStatus::Ready), "ready");
        assert_eq!(status_label(&LoginProfileStatus::Capturing), "capturing");
        assert_eq!(status_label(&LoginProfileStatus::Error), "error");
        assert_eq!(status_label(&LoginProfileStatus::Unknown), "unknown");
    }

    /// kv 键名与 JSON 字段一致（camelCase 原样）；布尔 true/false；空值 `-`。
    #[test]
    fn view_kv_pairs_matches_json_fields() {
        let view = LoginProfileView {
            id: "lp_1".into(),
            name: "env-1 主目录".into(),
            status: LoginProfileStatus::Ready,
            snapshot_version: 3,
            instances_using: 2,
            last_captured_at: Some(1_700_000_000_000),
            browser_running: true,
            cdp_port: Some(9223),
        };
        let pairs = view_kv_pairs(&view);
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get("id"), "lp_1");
        assert_eq!(get("status"), "ready");
        assert_eq!(get("snapshotVersion"), "3");
        assert_eq!(get("instancesUsing"), "2");
        assert_eq!(get("lastCapturedAt"), "1700000000000");
        assert_eq!(get("browserRunning"), "true");
        assert_eq!(get("cdpPort"), "9223");

        // 空值占位：lastCapturedAt/cdpPort 为 None → `-`（布尔仍原样输出）
        let mut empty = view.clone();
        empty.last_captured_at = None;
        empty.cdp_port = None;
        empty.browser_running = false;
        let pairs = view_kv_pairs(&empty);
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get("lastCapturedAt"), "-");
        assert_eq!(get("cdpPort"), "-");
        assert_eq!(get("browserRunning"), "false");
    }

    /// LoginProfileRow 反序列化（snake_case 状态 + null 时间戳）往返护栏，
    /// 与 tests/contract.rs 的 fixture 路径互补（模型层直测，不依赖 HTTP）。
    #[test]
    fn login_profile_row_deserializes_snake_case_status() {
        let row: LoginProfileRow = serde_json::from_value(serde_json::json!({
            "id": "lp_2",
            "environmentId": "env_2",
            "environmentName": "qa",
            "name": "qa 主目录",
            "status": "not_configured",
            "snapshotVersion": 0,
            "instancesUsing": 0,
            "lastCapturedAt": null,
            "browserRunning": false
        }))
        .expect("服务端行 JSON 应可反序列化");
        assert_eq!(row.status, LoginProfileStatus::NotConfigured);
        assert_eq!(row.snapshot_version, 0);
        assert!(row.last_captured_at.is_none());
        assert!(!row.browser_running);
    }
}
