//! settings 命令组 —— 全局应用设置（GET / PUT /api/v1/settings）。
//!
//! 与 env / instance 组的本质差异（组级约定）：settings 是**全局单例资源**
//! —— 一个 Agent 实例只有一份设置，API 无 id 路由（`/api/v1/settings` 不带
//! `{id}` 段），因此本组没有 `<ID>` 位置参数；quiet 的「变更类命令输出主实体
//! id」契约在此退化为固定字面量（见 update 的 quiet 分支注释）。
//!
//! 校验边界：`--env-label-position` /
//! `--env-label-color` / `--start-url` 一律透传，CLI 不做本地校验。服务端是
//! 唯一校验源（src-tauri/src/application/settings_service.rs：position / color
//! 白名单 + start URL 空值合法、非空须 http(s) 前缀），非法 → 400
//! INVALID_REQUEST → exit 7。理由：CLI 重复实现校验会形成双源规则漂移 ——
//! 服务端调整白名单后，旧 CLI 会误拦本已合法的请求；透传则天然跟随服务端
//! 演进，无需发版同步。
//!
//! 固定 handler 结构与 env 组一致：构造 [`OutputCtx`] → 调 [`AgentClient`] →
//! 渲染，三步之外无他物；本文件禁止出现 reqwest / StatusCode 等 HTTP 细节
//! （铁律见 client.rs 模块注释），请求 body 允许用 serde_json 组装（「传入
//! flag 才进 body」的语义映射必须发生在参数侧，与 env update 同理）。

use clap::Subcommand;

use super::output::OutputCtx;
use super::GlobalArgs;
use crate::client::AgentClient;
use crate::error::CliError;
use crate::model::AppSettingsView;

/// settings 子命令树。
#[derive(Debug, Subcommand)]
pub enum SettingsCommands {
    /// 查看全局设置
    #[command(about = "查看全局设置")]
    Get,

    /// 更新全局设置（只更新传入的 flag；至少传一个）
    #[command(about = "更新全局设置（只更新传入的 flag；至少传一个）")]
    Update {
        /// 开发者模式（true/false）
        #[arg(long, value_name = "BOOL", value_parser = clap::value_parser!(bool))]
        developer_mode: Option<bool>,
        /// 环境标签位置（取值白名单由服务端定义，非法 → exit 7）
        #[arg(long, value_name = "POSITION")]
        env_label_position: Option<String>,
        /// 环境标签颜色（取值白名单由服务端定义，非法 → exit 7）
        #[arg(long, value_name = "COLOR")]
        env_label_color: Option<String>,
        /// 全局默认起始页；空串合法（= 未配置，启动打开 about:blank）
        #[arg(long, value_name = "URL")]
        start_url: Option<String>,
    },
}

/// settings 组分发入口（main.rs dispatch 的唯一调用点）。
pub fn run(
    cmd: &SettingsCommands,
    globals: &GlobalArgs,
    client: &AgentClient,
) -> Result<(), CliError> {
    match cmd {
        SettingsCommands::Get => get(globals, client),
        SettingsCommands::Update {
            developer_mode,
            env_label_position,
            env_label_color,
            start_url,
        } => update(
            globals,
            client,
            *developer_mode,
            env_label_position.as_deref(),
            env_label_color.as_deref(),
            start_url.as_deref(),
        ),
    }
}

/// get / update 共用的单对象 key: value 对，键名与 JSON 字段一致（camelCase
/// 原样，两模式无信息差，PRD §39）。
fn settings_kv_pairs(s: &AppSettingsView) -> Vec<(String, String)> {
    vec![
        ("developerMode".into(), s.developer_mode.to_string()),
        ("envLabelPosition".into(), s.env_label_position.clone()),
        ("envLabelColor".into(), s.env_label_color.clone()),
        ("defaultStartUrl".into(), s.default_start_url.clone()),
    ]
}

/// `chrome-host settings get`：读全局设置；json 模式输出 AppSettingsView 原样。
fn get(globals: &GlobalArgs, client: &AgentClient) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let settings = client.settings_get()?;
    out.render_kv(
        &settings_kv_pairs(&settings),
        serde_json::to_value(&settings).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// `chrome-host settings update`：只把传入 flag 组进 PUT body（serde_json::Map
/// 按需组装，env update 同款模式）。
///
/// 「一个 flag 都没传」在本地拦截（→ exit 2，请求不发出）：PUT 空 body 虽可被
/// 服务端解释为「无更新」，但对 CLI 调用方而言几乎必然是意图错误（flag 名打错
/// 会被 clap 拦截，能走到这里的空跑没有业务意义），显式报错优于静默成功。
fn update(
    globals: &GlobalArgs,
    client: &AgentClient,
    developer_mode: Option<bool>,
    env_label_position: Option<&str>,
    env_label_color: Option<&str>,
    start_url: Option<&str>,
) -> Result<(), CliError> {
    if developer_mode.is_none()
        && env_label_position.is_none()
        && env_label_color.is_none()
        && start_url.is_none()
    {
        return Err(CliError::Local(
            "至少提供一个要更新的字段：--developer-mode / --env-label-position / \
             --env-label-color / --start-url"
                .into(),
        ));
    }

    let out = OutputCtx::from(globals);
    let mut body = serde_json::Map::new();
    if let Some(v) = developer_mode {
        body.insert("developerMode".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = env_label_position {
        body.insert(
            "envLabelPosition".into(),
            serde_json::Value::String(v.to_string()),
        );
    }
    if let Some(v) = env_label_color {
        body.insert(
            "envLabelColor".into(),
            serde_json::Value::String(v.to_string()),
        );
    }
    if let Some(v) = start_url {
        // 空串原样透传（服务端语义「空 = 未配置，打开 about:blank」）：与 env
        // update 的 nullable() 置空映射不同——AppSettings 字段是非 Option String，
        // 空串本身即合法值，无需 null 中转
        body.insert(
            "defaultStartUrl".into(),
            serde_json::Value::String(v.to_string()),
        );
    }

    // 透传不校验，非法值由服务端 400 INVALID_REQUEST → exit 7
    let settings = client.settings_update(serde_json::Value::Object(body))?;
    if out.quiet {
        // quiet 契约「变更类命令输出主实体 id」在无 id 资源上的退化：settings
        // 全局单例、无 id 语义，输出固定字面量 "settings" —— 保证脚本 $(...)
        // 捕获不空、成功失败按退出码判定、quiet 的单行输出契约不破
        out.print_id("settings");
        return Ok(());
    }
    // 成功渲染更新后的完整 AppSettingsView（服务端 PUT 响应即更新后全量视图）
    out.render_kv(
        &settings_kv_pairs(&settings),
        serde_json::to_value(&settings).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}
