//! env 命令组 —— 命令层模式样板（其余命令组照此结构实现）。
//!
//! 固定 handler 结构：构造 [`OutputCtx`] → 调 [`AgentClient`] → 渲染，三步之外无他物。
//! 本文件禁止出现 reqwest / StatusCode 等 HTTP 细节（铁律见 client.rs 模块注释）；
//! 唯一例外是请求 body 允许用 serde_json 组装 —— PATCH「传入才更新、置空传 null」
//! 的语义映射必须发生在参数侧（client.rs 不收长参数表，见其 env_patch 注释）。

use std::io::IsTerminal;

use clap::Subcommand;

use super::confirm_or_yes;
use super::output::{OutputCtx, TableRow};
use super::GlobalArgs;
use crate::client::AgentClient;
use crate::error::CliError;
use crate::model::{AppEvent, EnvStatus, Environment, EnvironmentSummary};

/// env 子命令树。
#[derive(Debug, Subcommand)]
pub enum EnvCommands {
    /// 列出全部环境
    #[command(about = "列出全部环境")]
    List,

    /// 查看环境详情
    #[command(about = "查看环境详情")]
    Get {
        /// 环境 id
        id: String,
    },

    /// 创建环境
    #[command(about = "创建环境")]
    Create {
        /// 环境名称
        name: String,
        /// 动态 hosts 配置源 URL（可省略）
        #[arg(long, value_name = "URL")]
        hosts_source_url: Option<String>,
        /// 图标路径（可省略）
        #[arg(long, value_name = "PATH")]
        icon: Option<String>,
    },

    /// 更新环境（只更新传入的 flag；值为空串表示显式置空）
    #[command(about = "更新环境（只更新传入的 flag；值为空串表示显式置空）")]
    Update {
        /// 环境 id
        id: String,
        /// 新名称
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// hosts 配置源 URL；空串显式置空
        #[arg(long, value_name = "URL")]
        hosts_source_url: Option<String>,
        /// 图标路径；空串显式置空
        #[arg(long, value_name = "PATH")]
        icon: Option<String>,
        /// 附加启动参数；空串显式置空
        #[arg(long, value_name = "ARGS")]
        startup_args: Option<String>,
        /// 实例常驻保留（true/false）
        #[arg(long, value_name = "BOOL", value_parser = clap::value_parser!(bool))]
        keep_alive: Option<bool>,
    },

    /// 删除环境（同时删除其实例数据；有运行中实例时先停止）
    #[command(about = "删除环境（同时删除其实例数据；有运行中实例时先停止）")]
    Delete {
        /// 环境 id
        id: String,
        /// 跳过确认（非交互环境必需）
        #[arg(long)]
        yes: bool,
    },

    /// 查看环境活动事件流
    #[command(about = "查看环境活动事件流")]
    Activity {
        /// 环境 id
        id: String,
        /// 返回条数上限（1–200；缺省 50 与服务端一致，越界值由服务端 clamp，CLI 不做本地校验）
        #[arg(long, value_name = "N")]
        limit: Option<i64>,
    },

    /// 查看环境实例状态计数
    #[command(about = "查看环境实例状态计数")]
    Status {
        /// 环境 id
        id: String,
    },
}

/// env 组分发入口（main.rs dispatch 的唯一调用点）。
pub fn run(cmd: &EnvCommands, globals: &GlobalArgs, client: &AgentClient) -> Result<(), CliError> {
    match cmd {
        EnvCommands::List => list(globals, client),
        EnvCommands::Get { id } => get(globals, client, id),
        EnvCommands::Create {
            name,
            hosts_source_url,
            icon,
        } => create(globals, client, name, hosts_source_url.as_deref(), icon.as_deref()),
        EnvCommands::Update {
            id,
            name,
            hosts_source_url,
            icon,
            startup_args,
            keep_alive,
        } => update(
            globals,
            client,
            id,
            name.as_deref(),
            hosts_source_url.as_deref(),
            icon.as_deref(),
            startup_args.as_deref(),
            *keep_alive,
        ),
        EnvCommands::Delete { id, yes } => delete(globals, client, id, *yes),
        EnvCommands::Activity { id, limit } => activity(globals, client, id, *limit),
        EnvCommands::Status { id } => status(globals, client, id),
    }
}

/// `env activity` 的 `--limit` 缺省值：与服务端缺省一致；服务端对显式传入值会
/// clamp 到 [1, 200]，CLI 不重复校验（服务端是唯一校验源，沿 settings 同款透传原则）。
const ACTIVITY_DEFAULT_LIMIT: i64 = 50;

/// `chrome-host env activity <id> [--limit n]`：环境活动事件流（只读）。
///
/// table 列集冻结：TIME LEVEL EVENT TARGET MESSAGE。
/// TIME 输出原始 epoch 毫秒、不转可读时间（why）：① chrono 不在依赖清单，
/// 不为本地格式化加依赖；② json 模式输出的同样是原始毫秒值——两模式
/// 展示同一原始值、无信息差，可读化交给消费侧（jq / date）。
/// TARGET = targetId，空值显示 `-`（占位仅 table 模式，json 原样保留 null）。
fn activity(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    limit: Option<i64>,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let events: Vec<AppEvent> = client.env_activity(id, limit.unwrap_or(ACTIVITY_DEFAULT_LIMIT))?;
    let rows: Vec<TableRow> = events
        .iter()
        .map(|e| TableRow {
            // quiet 的既有契约是列表类命令「每行一个 id」，但 AppEvent 无业务 id 字段
            // ——取 ts 作行标识：事件流里时间戳（epoch ms）即该条事件的锚点，脚本仍可
            // 按行捕获。这是 quiet 语义在无 id 数据上的最小妥协，不为此新开输出形态。
            id: e.ts.to_string(),
            cells: vec![
                e.ts.to_string(),
                e.level.clone(),
                e.event.clone(),
                e.target_id.clone().unwrap_or_else(|| "-".into()),
                e.message.clone(),
            ],
            json: serde_json::to_value(e).expect("纯数据 DTO 的序列化不会失败"),
        })
        .collect();
    // 空事件流：render_list 的 table 分支输出 (empty) 占位、json 分支输出 []
    out.render_list(&["TIME", "LEVEL", "EVENT", "TARGET", "MESSAGE"], &rows);
    Ok(())
}

/// `chrome-host env status <id>`：环境实例状态计数视图（只读）。
///
/// kv 键名与 JSON 字段一致（camelCase 原样，两模式无信息差）；total/running/
/// starting/stopped/error 是服务端嵌套 counts 对象的平面展开，单对象 kv 风格与
/// `env get` 一致（该命令定位是计数速览，嵌套结构在 json 模式仍原样保留）。
fn status(globals: &GlobalArgs, client: &AgentClient, id: &str) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let status: EnvStatus = client.env_status(id)?;
    let counts = &status.instances;
    out.render_kv(
        &[
            ("environmentId".into(), status.environment_id.clone()),
            ("total".into(), counts.total.to_string()),
            ("running".into(), counts.running.to_string()),
            ("starting".into(), counts.starting.to_string()),
            ("stopped".into(), counts.stopped.to_string()),
            ("error".into(), counts.error.to_string()),
        ],
        serde_json::to_value(&status).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// `chrome-host env list`。table 列集冻结：
/// ID NAME HOSTS-SOURCE RUNNING TOTAL（hosts-source 空值显示 `-`）；
/// json 模式输出 EnvironmentSummary 数组原样；quiet 模式每行一个 id。
fn list(globals: &GlobalArgs, client: &AgentClient) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let summaries: Vec<EnvironmentSummary> = client.env_list()?;
    let rows: Vec<TableRow> = summaries
        .iter()
        .map(|s| TableRow {
            id: s.environment.id.clone(),
            cells: vec![
                s.environment.id.clone(),
                s.environment.name.clone(),
                // `-` 仅是 table 模式的占位显示；json 原样保留 null（两模式无信息差）
                s.environment
                    .hosts_source_url
                    .clone()
                    .unwrap_or_else(|| "-".into()),
                s.running_instances.to_string(),
                s.total_instances.to_string(),
            ],
            // json 字段 = 该行完整 JSON：DTO 的 serde 属性两侧一致，序列化是恒等往返，
            // 即服务端原样（含 CJK 原文，PRD §39）
            json: serde_json::to_value(s).expect("纯数据 DTO 的序列化不会失败"),
        })
        .collect();
    out.render_list(&["ID", "NAME", "HOSTS-SOURCE", "RUNNING", "TOTAL"], &rows);
    Ok(())
}

/// `chrome-host env get <id>`：单对象渲染 —— json 原样；table 模式 key: value。
/// quiet 对单对象无冻结语义（见 output.rs render_kv 注释），按缺省 kv 输出。
fn get(globals: &GlobalArgs, client: &AgentClient, id: &str) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let env = client.env_get(id)?;
    out.render_kv(
        &env_kv_pairs(&env),
        serde_json::to_value(&env).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// env get / create / update 共用的单对象 key: value 对，键名与 JSON 字段一致
/// （PRD §39 两模式无信息差）。
/// 时间戳输出原始 epoch 毫秒：chrono 不在依赖清单（不为本地格式化加依赖），
/// 且服务端 created_at/updated_at 本就是毫秒值（domain/environment.rs）。
fn env_kv_pairs(env: &Environment) -> Vec<(String, String)> {
    vec![
        ("id".into(), env.id.clone()),
        ("name".into(), env.name.clone()),
        (
            "hostsSourceUrl".into(),
            env.hosts_source_url.clone().unwrap_or_else(|| "-".into()),
        ),
        ("icon".into(), env.icon.clone().unwrap_or_else(|| "-".into())),
        ("keepAlive".into(), env.keep_alive.to_string()),
        ("createdAt".into(), env.created_at.to_string()),
        ("updatedAt".into(), env.updated_at.to_string()),
    ]
}

/// `chrome-host env create <name>`：POST /environments；quiet 只输出新 env_id。
fn create(
    globals: &GlobalArgs,
    client: &AgentClient,
    name: &str,
    hosts_source_url: Option<&str>,
    icon: Option<&str>,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let env = client.env_create(name, hosts_source_url, icon)?;
    if out.quiet {
        out.print_id(&env.id);
        return Ok(());
    }
    out.render_kv(
        &env_kv_pairs(&env),
        serde_json::to_value(&env).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// `chrome-host env update <id>`：只把用户传入的 flag 放进 PATCH body。
///
/// PATCH 语义锚定（src-tauri/src/domain/environment.rs `UpdateEnvironmentInput`）：
/// - 字段缺省 = 不更新（`#[serde(default)]` 的 `Option<T>`）；
/// - `Some(None)` = 显式置空（服务端自定义反序列化接受 `"x" | null`）。
///
/// CLI 侧映射约定：flag 未传 → 字段不出现在 body；flag 传了空串（`--xxx ""`）→
/// JSON `null` → 服务端 `Some(None)` → 置空；其余传值原样。
#[allow(clippy::too_many_arguments)] // 参数与 clap flag 一一对应；为消参数再造 struct 会引入第二份字段表（漂移面）
fn update(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    name: Option<&str>,
    hosts_source_url: Option<&str>,
    icon: Option<&str>,
    startup_args: Option<&str>,
    keep_alive: Option<bool>,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let mut body = serde_json::Map::new();
    if let Some(v) = name {
        body.insert("name".into(), serde_json::Value::String(v.to_string()));
    }
    if let Some(v) = hosts_source_url {
        body.insert("hostsSourceUrl".into(), nullable(v));
    }
    if let Some(v) = icon {
        body.insert("icon".into(), nullable(v));
    }
    if let Some(v) = startup_args {
        body.insert("startupArgs".into(), nullable(v));
    }
    if let Some(v) = keep_alive {
        body.insert("keepAlive".into(), serde_json::Value::Bool(v));
    }

    let env = client.env_patch(id, serde_json::Value::Object(body))?;
    if out.quiet {
        // update 属变更类命令，quiet 输出主实体 id（与 create/delete 同约定）
        out.print_id(&env.id);
        return Ok(());
    }
    out.render_kv(
        &env_kv_pairs(&env),
        serde_json::to_value(&env).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// PATCH 可空字符串字段的 CLI → JSON 映射：空串 → `null`（显式置空，对应服务端
/// `Some(None)`），否则原值。
fn nullable(v: &str) -> serde_json::Value {
    if v.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(v.to_string())
    }
}

/// `chrome-host env delete <id> [--yes]`：确认流 + 409 复合编排。
///
/// 409 编排（服务端语义 `ENVIRONMENT_HAS_RUNNING_INSTANCES` = 有运行中实例必须先停）：
/// - `--yes`：直接 stop-all → 重试 DELETE；
/// - 交互 TTY：增量确认「停止全部并删除?」后 stop-all → 重试；
/// - 非 TTY 无 `--yes`：已在第一步确认流被拦（exit 2），此分支兜底防御，绝不静默级联；
/// - stop-all 或重试 DELETE 失败 → 错误原样上抛（保留 409/运行时语义，绝不 SIGKILL 兜底）。
fn delete(globals: &GlobalArgs, client: &AgentClient, id: &str, sub_yes: bool) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    // 子命令级 --yes 与全局 --yes 等效：任一放行即跳过确认流
    let yes = sub_yes || globals.yes;

    // 顺序决策（why 确认在请求之前）：若先 DELETE 再在 409 分支补确认，交互用户会被
    // 连问两次（「删除?」→ 409 →「停止并删除?」），体验割裂；且非交互场景会在许可
    // 校验完成前就发出请求。先确认 + 缺 --yes 即拦（exit 2）保证：请求发出前完成全部
    // 授权；409 后的二次确认只承载「停止实例」这一增量语义，不重复问删除。
    if !yes {
        confirm_or_yes(globals, "删除环境将同时删除其全部实例数据，继续?")?;
    }

    match client.env_delete(id) {
        Ok(()) => {}
        // 原 409 错误在此臂不透传：确认流取消 → exit 2「已取消」，否则重试 DELETE
        // （重试失败原样上抛），都携带比原始 409 更明确的终态信息
        Err(CliError::Api { status: 409, code, .. })
            if code == "ENVIRONMENT_HAS_RUNNING_INSTANCES" =>
        {
            if yes {
                // 已显式授权级联操作
            } else if std::io::stdin().is_terminal() {
                confirm_or_yes(globals, "环境有运行中实例，停止全部并删除?")?;
            } else {
                // 正常流程不可达：非 TTY 无 --yes 已在第一步被拦。保留兜底：
                // 极端时序（如确认后终端被挂起转后台）下也绝不静默执行级联停止
                return Err(CliError::Local(
                    "环境存在运行中实例，非交互环境需要 --yes 才能停止全部实例并删除".into(),
                ));
            }
            client.env_stop_all(id)?;
            client.env_delete(id)?;
        }
        Err(err) => return Err(err),
    }

    if out.quiet {
        out.print_id(id);
    }
    Ok(())
}
