//! instance 命令组 —— 实例全生命周期
//! （list/get/status/create/start/stop/restart/open/tabs/navigate/focus/cdp/delete）。
//!
//! 固定 handler 结构与 cli/env.rs 一致：构造 [`OutputCtx`] → 调 [`AgentClient`] → 渲染，
//! 三步之外无他物；HTTP 细节止于 client.rs（铁律见其模块注释）。本组特有的本地编排：
//! - `open`/`navigate` 共享的 URL 协议前缀校验（[`validate_url`]）：本地拦截最高频
//!   输入错误，请求绝不发出；
//! - `delete` 的 409 `INSTANCE_ALREADY_RUNNING` 编排：先 stop 再删。

use std::io::IsTerminal;

use clap::Subcommand;

use super::confirm_or_yes;
use super::output::{OutputCtx, TableRow};
use super::GlobalArgs;
use crate::client::AgentClient;
use crate::error::CliError;
use crate::model::{
    CdpEndpoint, CdpSessionView, CdpTarget, Instance, InstanceStatus, InstanceStatusView,
    InstanceView,
};

/// instance 子命令树。
#[derive(Debug, Subcommand)]
pub enum InstanceCommands {
    /// 列出实例（缺省全量；--env 只看指定环境）
    #[command(about = "列出实例（缺省全量；--env 只看指定环境）")]
    List {
        /// 只列出该环境的实例
        #[arg(long, value_name = "ENV_ID")]
        env: Option<String>,
    },

    /// 查看实例详情
    #[command(about = "查看实例详情")]
    Get {
        /// 实例 id
        id: String,
    },

    /// 查看实例状态（轻量轮询视图；全量详情含 profileDir 用 get）
    #[command(about = "查看实例状态（轻量轮询视图；全量详情含 profileDir 用 get）")]
    Status {
        /// 实例 id
        id: String,
    },

    /// 页面诊断快照（JS 错误/失败资源/LCP/FCP；仅运行中实例）
    #[command(
        about = "页面诊断快照：JS 错误/失败资源/LCP/FCP（仅运行中实例，409 = 未运行或无 http 页面）"
    )]
    Diagnostics {
        /// 实例 id
        id: String,
    },

    /// 常驻订阅的 console 环（进程内态重启即清）
    #[command(
        about = "console 环形缓冲（500 条）：--level 过滤、--tab 按标签页过滤、--limit 尾部 N 条"
    )]
    Console {
        /// 实例 id
        id: String,
        /// 级别过滤：error/warning/info/debug/log
        #[arg(long)]
        level: Option<String>,
        /// 按标签页过滤（tab id，由 tabs 命令获得）
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
        /// 尾部条数（缺省 100）
        #[arg(long)]
        limit: Option<usize>,
    },

    /// 常驻订阅的请求环（进程内态重启即清）
    #[command(
        about = "请求环形缓冲（1000 条）：--failed-only 只看失败/HTTP≥400、--tab 按标签页过滤"
    )]
    Network {
        /// 实例 id
        id: String,
        /// 只看失败请求或 HTTP ≥400
        #[arg(long = "failed-only")]
        failed_only: bool,
        /// 按标签页过滤（tab id，由 tabs 命令获得）
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
        /// 尾部条数（缺省 100）
        #[arg(long)]
        limit: Option<usize>,
    },

    /// 当前页面 png 截图（复用常驻诊断连接）
    #[command(about = "当前页面 png 截图：--out 写文件（缺省 <实例id>.png），quiet 输出路径")]
    Screenshot {
        /// 实例 id
        id: String,
        /// 输出文件路径
        #[arg(long = "out")]
        out: Option<String>,
    },

    /// 请求环生成 HAR 1.2 文档
    #[command(about = "生成 HAR 1.2：--out 写文件（缺省 <实例id>.har），quiet 输出路径")]
    Har {
        /// 实例 id
        id: String,
        /// 输出文件路径
        #[arg(long = "out")]
        out: Option<String>,
    },

    /// 创建实例（创建即启动；首次运行可能阻塞于内核下载）
    #[command(about = "创建实例（创建即启动；首次运行可能阻塞于内核下载）")]
    Create {
        /// 环境 id
        env_id: String,
    },

    /// 启动实例
    #[command(about = "启动实例")]
    Start {
        /// 实例 id
        id: String,
    },

    /// 停止实例
    #[command(about = "停止实例")]
    Stop {
        /// 实例 id
        id: String,
    },

    /// 重启实例
    #[command(about = "重启实例")]
    Restart {
        /// 实例 id
        id: String,
    },

    /// 在实例中打开 URL（仅支持 http/https）
    #[command(about = "在实例中打开 URL（仅支持 http/https）")]
    Open {
        /// 实例 id
        id: String,
        /// 目标 URL（必须以 http:// 或 https:// 开头）
        url: String,
    },

    /// 列出实例当前标签页
    #[command(about = "列出实例当前标签页")]
    Tabs {
        /// 实例 id
        id: String,
    },

    /// 将指定标签页导航到新 URL（仅支持 http/https）
    #[command(about = "将指定标签页导航到新 URL（仅支持 http/https）")]
    Navigate {
        /// 实例 id
        id: String,
        /// 目标标签页 id（由 tabs 获得）
        tab_id: String,
        /// 目标 URL（必须以 http:// 或 https:// 开头）
        url: String,
    },

    /// 聚焦实例窗口
    #[command(about = "聚焦实例窗口")]
    Focus {
        /// 实例 id
        id: String,
    },

    /// 查看实例 CDP 端点
    #[command(about = "查看实例 CDP 端点")]
    Cdp {
        /// 实例 id
        id: String,
    },

    /// 发放 CDP 会话（远程/代理访问用，30 分钟有效）；--quiet 输出完整代理 URL
    #[command(about = "发放 CDP 会话（远程/代理访问用，30 分钟有效）；--quiet 输出完整代理 URL")]
    CdpSession {
        /// 实例 id
        id: String,
    },

    /// 删除实例（同时删除其 profile 数据；运行中实例会先停止）
    #[command(about = "删除实例（同时删除其 profile 数据；运行中实例会先停止）")]
    Delete {
        /// 实例 id
        id: String,
        /// 跳过确认（非交互环境必需）
        #[arg(long)]
        yes: bool,
    },
}

/// instance 组分发入口（main.rs dispatch 的唯一调用点）。
pub fn run(
    cmd: &InstanceCommands,
    globals: &GlobalArgs,
    client: &AgentClient,
) -> Result<(), CliError> {
    match cmd {
        InstanceCommands::List { env } => list(globals, client, env.as_deref()),
        InstanceCommands::Get { id } => get(globals, client, id),
        InstanceCommands::Status { id } => status(globals, client, id),
        InstanceCommands::Diagnostics { id } => diagnostics(globals, client, id),
        InstanceCommands::Console {
            id,
            level,
            tab,
            limit,
        } => console(
            globals,
            client,
            id,
            level.as_deref(),
            tab.as_deref(),
            *limit,
        ),
        InstanceCommands::Network {
            id,
            failed_only,
            tab,
            limit,
        } => network(globals, client, id, *failed_only, tab.as_deref(), *limit),
        InstanceCommands::Screenshot { id, out } => screenshot(globals, client, id, out.as_deref()),
        InstanceCommands::Har { id, out } => har(globals, client, id, out.as_deref()),
        InstanceCommands::Create { env_id } => create(globals, client, env_id),
        InstanceCommands::Start { id } => start(globals, client, id),
        InstanceCommands::Stop { id } => stop(globals, client, id),
        InstanceCommands::Restart { id } => restart(globals, client, id),
        InstanceCommands::Open { id, url } => open(globals, client, id, url),
        InstanceCommands::Tabs { id } => tabs(globals, client, id),
        InstanceCommands::Navigate { id, tab_id, url } => {
            navigate(globals, client, id, tab_id, url)
        }
        InstanceCommands::Focus { id } => focus(globals, client, id),
        InstanceCommands::Cdp { id } => cdp(globals, client, id),
        InstanceCommands::CdpSession { id } => cdp_session(globals, client, id),
        InstanceCommands::Delete { id, yes } => delete(globals, client, id, *yes),
    }
}

/// `chrome-host instance list [--env <env_id>]`。table 列集冻结：
/// ID ENVIRONMENT STATUS PID CDP。
///
/// 两种数据源形状不同（服务端契约）：
/// - 缺省走全量端点 GET /api/v1/instances → `InstanceView`（含 environmentName，
///   孤儿实例服务端已回退为 env_id）；
/// - `--env` 走 GET /api/v1/environments/{id}/instances → `Instance`（响应无环境名
///   字段，ENVIRONMENT 列直接回显过滤参数 —— 环境上下文即用户输入）。
///
/// 行 JSON 始终是各自端点的服务端原样（View 含 environmentName），两模式无信息差。
fn list(globals: &GlobalArgs, client: &AgentClient, env: Option<&str>) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let rows: Vec<TableRow> = match env {
        Some(env_id) => {
            let instances: Vec<Instance> = client.instance_list_by_env(env_id)?;
            instances
                .iter()
                .map(|ins| TableRow {
                    id: ins.id.clone(),
                    cells: instance_cells(ins, env_id),
                    json: serde_json::to_value(ins).expect("纯数据 DTO 的序列化不会失败"),
                })
                .collect()
        }
        None => {
            let views: Vec<InstanceView> = client.instance_list()?;
            views
                .iter()
                .map(|view| TableRow {
                    id: view.instance.id.clone(),
                    cells: instance_cells(&view.instance, &view.environment_name),
                    json: serde_json::to_value(view).expect("纯数据 DTO 的序列化不会失败"),
                })
                .collect()
        }
    };
    out.render_list(&["ID", "ENVIRONMENT", "STATUS", "PID", "CDP"], &rows);
    Ok(())
}

/// table 五列单元格（ID ENVIRONMENT STATUS PID CDP）。ENVIRONMENT 列取值由调用方注入
/// （理由见 [`list`]）；PID/CDP 空值显示 `-` —— 仅 table 占位，json 原样保留 null
/// （两模式无信息差，PRD §39）。
fn instance_cells(instance: &Instance, environment: &str) -> Vec<String> {
    vec![
        instance.id.clone(),
        environment.to_string(),
        status_label(&instance.status),
        option_cell(instance.pid),
        option_cell(instance.cdp_port),
    ]
}

/// `InstanceStatus` → 线上小写字符串（STATUS 列「小写原样」）。经 serde 序列化取值：
/// 与 DTO 的 `#[serde(rename_all = "lowercase")]` 同一来源，服务端加状态变体时
/// 本函数零改动不漂移；`Unknown` 兜底变体同样序列化为 "unknown"。
fn status_label(status: &InstanceStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

/// Option 数值字段的 table 占位：None → `-`（json 模式不受影响，保留 null）。
fn option_cell<T: ToString>(value: Option<T>) -> String {
    value.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
}

/// `chrome-host instance get <id>`：单对象渲染 —— json 原样；table 模式 key: value。
/// quiet 对单对象无冻结语义（同 env get），按缺省 kv 输出。
fn get(globals: &GlobalArgs, client: &AgentClient, id: &str) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let instance = client.instance_get(id)?;
    out.render_kv(
        &instance_kv_pairs(&instance),
        serde_json::to_value(&instance).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// instance get/create/start/stop/restart 共用的单对象 key: value 对，键名与
/// JSON 字段一致（PRD §39 两模式无信息差）。冻结字段集：
/// id/status/pid/cdpPort/browserVersion/profileDir/createdAt/updatedAt。
/// 时间戳输出原始 epoch 毫秒（同 env_kv_pairs：不为本地格式化加 chrono 依赖）；
/// Option 字段 table 模式空值显示 `-`，json 原样保留 null。
fn instance_kv_pairs(instance: &Instance) -> Vec<(String, String)> {
    vec![
        ("id".into(), instance.id.clone()),
        ("status".into(), status_label(&instance.status)),
        ("pid".into(), option_cell(instance.pid)),
        ("cdpPort".into(), option_cell(instance.cdp_port)),
        (
            "browserVersion".into(),
            instance
                .browser_version
                .clone()
                .unwrap_or_else(|| "-".into()),
        ),
        ("profileDir".into(), instance.profile_dir.clone()),
        ("createdAt".into(), instance.created_at.to_string()),
        ("updatedAt".into(), instance.updated_at.to_string()),
    ]
}

/// `chrome-host instance status <id>`：轻量轮询视图（与 get 的分工已写入 about：
/// status 只含 id/status/pid/cdpPort/browserVersion 五键，无 profileDir 等重字段，
/// 适合脚本高频轮询；get 是全量详情）。
fn status(globals: &GlobalArgs, client: &AgentClient, id: &str) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let view = client.instance_status(id)?;
    out.render_kv(
        &status_kv_pairs(&view),
        serde_json::to_value(&view).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// `chrome-host instance console <ID> [--level] [--tab] [--limit]`：console 环渲染。
/// table：LEVEL SOURCE TEXT 列表（TEXT 截断到 80 列，json 模式完整保留）；
/// quiet：每行一条 `level\ttext`；tab 过滤条件在列表前标注（仅 table 模式）。
fn console(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    level: Option<&str>,
    tab: Option<&str>,
    limit: Option<usize>,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let resp = client.instance_console(id, level, tab, limit)?;
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if out.quiet {
        for e in &entries {
            out.print_id(&format!(
                "{}\t{}",
                e.get("level").and_then(|v| v.as_str()).unwrap_or("?"),
                e.get("text").and_then(|v| v.as_str()).unwrap_or("")
            ));
        }
        return Ok(());
    }
    let rows: Vec<TableRow> = entries
        .iter()
        .map(|e| TableRow {
            id: e
                .get("ts")
                .and_then(|v| v.as_i64())
                .map(|v| v.to_string())
                .unwrap_or_default(),
            cells: vec![
                e.get("level")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string(),
                e.get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string(),
                truncate(e.get("text").and_then(|v| v.as_str()).unwrap_or(""), 80),
            ],
            json: e.clone(),
        })
        .collect();
    print_tab_filter_note(&out, &resp);
    out.render_list(&["LEVEL", "SOURCE", "TEXT"], &rows);
    Ok(())
}

/// `chrome-host instance network <ID> [--failed-only] [--tab] [--limit]`：请求环渲染。
/// table：METHOD STATUS URL 列表；quiet：每行 `status\turl`；tab 过滤条件同 console。
fn network(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    failed_only: bool,
    tab: Option<&str>,
    limit: Option<usize>,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let resp = client.instance_network(id, failed_only, tab, limit)?;
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if out.quiet {
        for e in &entries {
            out.print_id(&format!(
                "{}\t{}",
                e.get("status")
                    .and_then(|v| v.as_i64())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "FAIL".into()),
                e.get("url").and_then(|v| v.as_str()).unwrap_or("")
            ));
        }
        return Ok(());
    }
    let rows: Vec<TableRow> = entries
        .iter()
        .map(|e| TableRow {
            id: e
                .get("ts")
                .and_then(|v| v.as_i64())
                .map(|v| v.to_string())
                .unwrap_or_default(),
            cells: vec![
                e.get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string(),
                e.get("status")
                    .and_then(|v| v.as_i64())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| {
                        e.get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("FAIL")
                            .to_string()
                    }),
                truncate(e.get("url").and_then(|v| v.as_str()).unwrap_or(""), 70),
            ],
            json: e.clone(),
        })
        .collect();
    print_tab_filter_note(&out, &resp);
    out.render_list(&["METHOD", "STATUS", "URL"], &rows);
    Ok(())
}

/// tab 过滤条件标注（仅 table 模式；json 模式响应原样含 meta，quiet 保持纯行输出）。
/// 服务端过滤降级（存在无归属条目）时明确告知，避免「过滤了但没完全过滤」的误读。
fn print_tab_filter_note(out: &OutputCtx, resp: &serde_json::Value) {
    let Some(m) = resp.get("meta") else { return };
    if out.quiet || out.json {
        return;
    }
    let tab = m
        .get("filteredByTab")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    if m.get("tabFilterUnavailable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        out.render_text(format!("tab 过滤 {tab} 不可用（存在无归属条目），返回全量"));
    } else {
        let before = m.get("before").and_then(|v| v.as_u64()).unwrap_or(0);
        let after = m.get("after").and_then(|v| v.as_u64()).unwrap_or(0);
        out.render_text(format!("tab 过滤 {tab}：{before} → {after} 条"));
    }
}

/// `chrome-host instance screenshot <ID> [--out]`：解码 base64 写 png。
/// json 模式输出响应原样（含 data），不落盘——落盘路径仅 table 模式输出。
fn screenshot(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    out_path: Option<&str>,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let resp = client.instance_screenshot(id)?;
    let data = resp.get("data").and_then(|v| v.as_str()).unwrap_or("");
    let bytes = decode_base64_png(data)?;
    if !out.json {
        let path = out_path
            .map(str::to_string)
            .unwrap_or_else(|| format!("{id}.png"));
        std::fs::write(&path, &bytes)
            .map_err(|e| CliError::Local(format!("写截图文件失败 {path}: {e}")))?;
        if out.quiet {
            out.print_id(&path);
        } else {
            out.render_kv(
                &[
                    ("file".into(), path.clone()),
                    ("bytes".into(), bytes.len().to_string()),
                ],
                serde_json::json!({ "file": path, "bytes": bytes.len(), "format": "png" }),
            );
        }
    } else {
        out.render_kv(&[], resp);
    }
    Ok(())
}

/// `chrome-host instance har <ID> [--out]`：HAR 1.2 文档。
/// json 模式输出 HAR 原样；table 模式写文件 + 摘要（条目数）。
fn har(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    out_path: Option<&str>,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let resp = client.instance_har(id)?;
    if out.json {
        out.render_kv(&[], resp);
        return Ok(());
    }
    let path = out_path
        .map(str::to_string)
        .unwrap_or_else(|| format!("{id}.har"));
    let pretty = serde_json::to_string_pretty(&resp)
        .map_err(|e| CliError::Local(format!("HAR 序列化失败: {e}")))?;
    std::fs::write(&path, pretty)
        .map_err(|e| CliError::Local(format!("写 HAR 文件失败 {path}: {e}")))?;
    let entries = resp
        .pointer("/log/entries")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    if out.quiet {
        out.print_id(&path);
    } else {
        out.render_kv(
            &[
                ("file".into(), path.clone()),
                ("entries".into(), entries.to_string()),
            ],
            serde_json::json!({ "file": path, "entries": entries }),
        );
    }
    Ok(())
}

/// base64 解码 + PNG 魔数校验（服务端形状漂移在客户端拦下，不写坏文件）
fn decode_base64_png(data: &str) -> Result<Vec<u8>, CliError> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| CliError::Local(format!("截图 base64 解码失败: {e}")))?;
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(CliError::Protocol(
            "截图数据不是 PNG（魔数校验失败）".into(),
        ));
    }
    Ok(bytes)
}

/// table 列截断（ascii 截断即可：中文按字符数截，不追求显示宽度对齐）
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}
/// `chrome-host instance diagnostics <ID>`：页面诊断快照（M5 短连接透传）。
/// 响应是服务端透传的自由形状 JSON（页面扩展挂载的快照），不建 DTO ——
/// 诊断面随扩展演进，CLI 只做摘要提取 + 原样透传（json 模式完整保留）。
/// quiet：输出 targetUrl（供脚本捕获定位被诊断页面）。
fn diagnostics(globals: &GlobalArgs, client: &AgentClient, id: &str) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let resp = client.instance_diagnostics(id)?;
    if out.quiet {
        out.print_id(resp.get("targetUrl").and_then(|v| v.as_str()).unwrap_or(""));
        return Ok(());
    }
    out.render_kv(&diagnostics_kv_pairs(&resp), resp);
    Ok(())
}

/// 诊断摘要 kv：目标页 + 两类计数 + 性能指标；diagnostics 为 null 时明示未注入
/// （旧实例/非 content script 页），而不是把空对象渲染成误导性的 0）。
fn diagnostics_kv_pairs(resp: &serde_json::Value) -> Vec<(String, String)> {
    let mut pairs = vec![(
        "targetUrl".into(),
        resp.get("targetUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("-")
            .to_string(),
    )];
    match resp.get("diagnostics") {
        Some(d) if d.is_object() => {
            let count = |key: &str| {
                d.get(key)
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0)
                    .to_string()
            };
            let ms = |key: &str| {
                d.get(key)
                    .and_then(|v| v.as_i64())
                    .map(|v| format!("{v}ms"))
                    .unwrap_or_else(|| "-".into())
            };
            pairs.push(("errors".into(), count("errors")));
            pairs.push(("badResources".into(), count("resources")));
            pairs.push(("lcp".into(), ms("lcp")));
            pairs.push(("fcp".into(), ms("fcp")));
        }
        _ => pairs.push((
            "diagnostics".into(),
            "null（页面未注入：实例早于扩展启动，或目标页不运行 content script）".into(),
        )),
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 摘要提取：对象快照 → 计数/指标；null/缺失 → 明示未注入而非伪 0
    #[test]
    fn diagnostics_kv_summarizes_object_and_flags_null() {
        let resp = json!({
            "instanceId": "ins_1",
            "targetId": "t1",
            "targetUrl": "https://app.example.com/home",
            "diagnostics": {
                "startedAt": 1,
                "url": "https://app.example.com/home",
                "errors": [{ "type": "error", "message": "a" }, { "type": "unhandledrejection", "message": "b" }],
                "resources": [{ "url": "https://x/404", "status": 404 }],
                "lcp": 731,
                "fcp": 120
            }
        });
        let pairs = diagnostics_kv_pairs(&resp);
        assert_eq!(
            pairs[0],
            ("targetUrl".into(), "https://app.example.com/home".into())
        );
        assert!(
            pairs.iter().any(|(k, v)| k == "errors" && v == "2"),
            "{pairs:?}"
        );
        assert!(
            pairs.iter().any(|(k, v)| k == "badResources" && v == "1"),
            "{pairs:?}"
        );
        assert!(
            pairs.iter().any(|(k, v)| k == "lcp" && v == "731ms"),
            "{pairs:?}"
        );
        assert!(
            pairs.iter().any(|(k, v)| k == "fcp" && v == "120ms"),
            "{pairs:?}"
        );

        let null_resp = json!({ "targetUrl": "https://a.com", "diagnostics": null });
        let pairs = diagnostics_kv_pairs(&null_resp);
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == "diagnostics" && v.contains("未注入")),
            "null 明示原因: {pairs:?}"
        );
    }
}

/// status 的单对象 kv：键名与 `InstanceStatusView` JSON 字段一致（两模式无信息差）。
/// status 复用 [`status_label`]（与全量 Instance 同一 serde 来源，状态枚举不双源漂移）；
/// Option 字段空值 `-`，json 原样保留 null（同 [`instance_kv_pairs`] 约定）。
fn status_kv_pairs(view: &InstanceStatusView) -> Vec<(String, String)> {
    vec![
        ("id".into(), view.id.clone()),
        ("status".into(), status_label(&view.status)),
        ("pid".into(), option_cell(view.pid)),
        ("cdpPort".into(), option_cell(view.cdp_port)),
        (
            "browserVersion".into(),
            view.browser_version.clone().unwrap_or_else(|| "-".into()),
        ),
    ]
}

/// `chrome-host instance create <env_id>`：创建即启动 —— 服务端内部走完
/// 内核就绪 → hosts → spawn → CDP 等待全链路后才返回；首次运行可能阻塞于
/// Chrome for Testing 内核下载（~150MB，本调用在 client.rs 走无总超时客户端，
/// Ctrl-C 安全：下载槽服务端幂等）。quiet 输出新实例 id，table 模式
/// 输出创建后快照 kv。
fn create(globals: &GlobalArgs, client: &AgentClient, env_id: &str) -> Result<(), CliError> {
    let instance = client.instance_create(env_id)?;
    render_mutated(globals, instance)
}

/// `chrome-host instance start <id>`：启动含内核/CDP 等待，长操作。
fn start(globals: &GlobalArgs, client: &AgentClient, id: &str) -> Result<(), CliError> {
    let instance = client.instance_start(id)?;
    render_mutated(globals, instance)
}

/// `chrome-host instance stop <id>`：优雅退出，常规超时内完成。
fn stop(globals: &GlobalArgs, client: &AgentClient, id: &str) -> Result<(), CliError> {
    let instance = client.instance_stop(id)?;
    render_mutated(globals, instance)
}

/// `chrome-host instance restart <id>`：stop + start 复合，长操作。
fn restart(globals: &GlobalArgs, client: &AgentClient, id: &str) -> Result<(), CliError> {
    let instance = client.instance_restart(id)?;
    render_mutated(globals, instance)
}

/// 变更类命令（create/start/stop/restart）共用渲染：服务端返回操作后的 Instance
/// 快照 —— quiet 仅输出实例 id（供 `$(...)` 捕获）；table 模式输出操作后快照 kv。
fn render_mutated(globals: &GlobalArgs, instance: Instance) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    if out.quiet {
        out.print_id(&instance.id);
        return Ok(());
    }
    out.render_kv(
        &instance_kv_pairs(&instance),
        serde_json::to_value(&instance).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// URL 协议前缀本地校验（`open` 与 `navigate` 共享，why：同一规则收敛为一个函数，
/// 防两份内联校验随迭代漂移）。仅放行 http/https —— CDP 建页/导航语义就是网页
/// 导航，file:// 等协议不在服务端支持面内。校验失败 → `CliError::Local`
/// （本地校验不通过，请求绝不发出），不发请求。
fn validate_url(url: &str) -> Result<(), CliError> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(CliError::Local(format!(
            "URL 必须以 http:// 或 https:// 开头（收到: {url}）"
        )));
    }
    Ok(())
}

/// `chrome-host instance open <id> <url>`：在实例中新开标签页。
///
/// 协议前缀本地校验优先于请求（why：协议缺失是本命令最高频输入错误，提前拦截
/// 省一次注定失败的网络往返，错误即时反馈），规则见 [`validate_url`]。
fn open(globals: &GlobalArgs, client: &AgentClient, id: &str, url: &str) -> Result<(), CliError> {
    validate_url(url)?;
    let out = OutputCtx::from(globals);
    let target = client.instance_open(id, url)?;
    out.render_kv(
        &target_kv_pairs(&target),
        serde_json::to_value(&target).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// `chrome-host instance tabs <id>`：标签页列表。table 列集冻结 ID TITLE URL
/// （cells 复用 [`target_kv_pairs`]：三键 id/title/url 与三列同源同序，防两处
/// 漂移）；空列表由 output.rs 的 `(empty)` 占位机制兜底（list_table_string）。
/// json 原样 Vec&lt;CdpTarget&gt;；quiet 每行 tab id（供 navigate `$(...)` 取用）。
fn tabs(globals: &GlobalArgs, client: &AgentClient, id: &str) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let targets = client.instance_tabs(id)?;
    let rows: Vec<TableRow> = targets
        .iter()
        .map(|t| TableRow {
            id: t.id.clone(),
            cells: target_kv_pairs(t).into_iter().map(|(_, v)| v).collect(),
            json: serde_json::to_value(t).expect("纯数据 DTO 的序列化不会失败"),
        })
        .collect();
    out.render_list(&["ID", "TITLE", "URL"], &rows);
    Ok(())
}

/// `chrome-host instance navigate <id> <tab_id> <url>`：标签页导航。协议前缀校验
/// 复用 [`validate_url`]（与 open 同一规则）；成功渲染导航后标签页快照 kv（同
/// open 的三键形态，服务端返回的就是目标标签页的最新状态）。
fn navigate(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    tab_id: &str,
    url: &str,
) -> Result<(), CliError> {
    validate_url(url)?;
    let out = OutputCtx::from(globals);
    let target = client.instance_navigate(id, tab_id, url)?;
    out.render_kv(
        &target_kv_pairs(&target),
        serde_json::to_value(&target).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// `chrome-host instance focus <id>`：聚焦窗口。非破坏性操作不确认；
/// 实例未运行 → 409 `INSTANCE_NOT_RUNNING` 由 client 原样上抛为 `CliError::Api`
/// （exit 5，无翻译义务——该码语义单一），CLI 不做二次包装。成功走
/// [`render_mutated`]：服务端返回操作后实例快照，与 start/stop 同为变更类渲染。
fn focus(globals: &GlobalArgs, client: &AgentClient, id: &str) -> Result<(), CliError> {
    let instance = client.instance_focus(id)?;
    render_mutated(globals, instance)
}

/// open 成功后的单对象 kv：冻结三键 id/title/url（`type` 字段在新开标签页场景
/// 恒为 page，无信息量，不进 CLI 输出）。
fn target_kv_pairs(target: &CdpTarget) -> Vec<(String, String)> {
    vec![
        ("id".into(), target.id.clone()),
        ("title".into(), target.title.clone()),
        ("url".into(), target.url.clone()),
    ]
}

/// `chrome-host instance cdp <id>`：CDP 端点信息。table 模式 key: value，
/// webSocketUrl 可空显示 `-`（json 原样保留 null）。
fn cdp(globals: &GlobalArgs, client: &AgentClient, id: &str) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let endpoint = client.instance_cdp(id)?;
    out.render_kv(
        &cdp_kv_pairs(&endpoint),
        serde_json::to_value(&endpoint).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// `chrome-host instance cdp-session <id>`：发放 CDP 会话。
/// **quiet 新契约**：输出完整代理 URL（base + baseUrl 路径，供 `CDP_BASE=$(...)` 直接捕获；
/// render_kv 的 quiet 对单对象无冻结语义，此处按变更类命令 print_id 先例显式定义）。
/// json 模式在服务端 DTO 基础上补 fullUrl 字段，与 kv 无信息差。
fn cdp_session(globals: &GlobalArgs, client: &AgentClient, id: &str) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let view = client.instance_cdp_session(id)?;
    let full_url = format!("{}{}", client.base_url(), view.base_url);
    if out.quiet {
        out.print_id(&full_url);
        return Ok(());
    }
    let mut value = serde_json::to_value(&view).expect("纯数据 DTO 的序列化不会失败");
    value["fullUrl"] = serde_json::Value::String(full_url);
    out.render_kv(
        &[
            ("sessionId".into(), view.session_id.clone()),
            ("baseUrl".into(), view.base_url.clone()),
            ("expiresAt".into(), view.expires_at.to_string()),
            (
                "expiresInMin".into(),
                ((view.expires_at - chrono_now_ms()) / 60_000).to_string(),
            ),
        ],
        value,
    );
    Ok(())
}

/// 当前时刻（epoch ms）。CLI 侧只为呈现「剩余分钟」，精度无要求。
fn chrono_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// cdp 的单对象 kv：键名与 `CdpEndpoint` JSON 字段一致（两模式无信息差）。
fn cdp_kv_pairs(endpoint: &CdpEndpoint) -> Vec<(String, String)> {
    vec![
        ("instanceId".into(), endpoint.instance_id.clone()),
        ("host".into(), endpoint.host.clone()),
        ("port".into(), endpoint.port.to_string()),
        ("httpUrl".into(), endpoint.http_url.clone()),
        (
            "webSocketUrl".into(),
            endpoint
                .web_socket_url
                .clone()
                .unwrap_or_else(|| "-".into()),
        ),
    ]
}

/// `chrome-host instance delete <id> [--yes]`：确认流 + 409 复合编排。
///
/// 409 编排（服务端语义 `INSTANCE_ALREADY_RUNNING` = 运行中实例不允许直接删除）：
/// - `--yes`：直接 stop → 重试 DELETE；
/// - 交互 TTY：增量确认「实例运行中，先停止并删除?」后 stop → 重试；
/// - 非 TTY 无 `--yes`：已在第一步确认流被拦（exit 2），此分支兜底防御，绝不静默级联；
/// - stop 或重试 DELETE 失败 → 错误原样上抛（保留 409/运行时语义，绝不 SIGKILL 兜底）。
fn delete(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    sub_yes: bool,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    // 子命令级 --yes 与全局 --yes 等效：任一放行即跳过确认流
    let yes = sub_yes || globals.yes;

    // 顺序决策同 env delete（why 确认在请求之前）：若先 DELETE 再在 409 分支补确认，
    // 交互用户会被连问两次（「删除?」→ 409 →「停止并删除?」），体验割裂；先确认 +
    // 缺 --yes 即拦（exit 2）保证请求发出前完成全部授权，409 后的二次确认只承载
    // 「停止实例」这一增量语义，不重复问删除。
    if !yes {
        confirm_or_yes(globals, "删除实例将同时删除其 profile 数据，继续?")?;
    }

    match client.instance_delete(id) {
        Ok(()) => {}
        // 原 409 错误在此臂不透传：确认流取消 → exit 2「已取消」，否则 stop 后重试
        // DELETE（失败原样上抛），都携带比原始 409 更明确的终态信息
        Err(CliError::Api {
            status: 409, code, ..
        }) if code == "INSTANCE_ALREADY_RUNNING" => {
            if yes {
                // 已显式授权级联操作（stop → delete）
            } else if std::io::stdin().is_terminal() {
                confirm_or_yes(globals, "实例运行中，先停止并删除?")?;
            } else {
                // 正常流程不可达：非 TTY 无 --yes 已在第一步被拦。保留兜底：
                // 极端时序（如确认后终端被挂起转后台）下也绝不静默执行级联停止
                return Err(CliError::Local(
                    "实例运行中，非交互环境需要 --yes 才能停止并删除".into(),
                ));
            }
            client.instance_stop(id)?;
            client.instance_delete(id)?;
        }
        Err(err) => return Err(err),
    }

    if out.quiet {
        out.print_id(id);
    }
    Ok(())
}
