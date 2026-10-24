//! page 命令组 —— 页面控制（P2-C/P2-G）：snap / click / fill / press / wait /
//! dialogs / eval / html / clickxy / loadall / type。
//!
//! 行为语义全部在服务层 page_service（P2-B）：本组只做参数透传 + 渲染。
//! 核心循环（对齐 SKILL.md 教学）：`snap` 拿 @eN 引用 → `click`/`fill`/
//! `press --target` 用引用操作 → 页面变化后重新 `snap`（旧引用报
//! PAGE_REF_STALE / PAGE_REF_NOT_FOUND，重 snap 而非对旧引用重试）。
//!
//! 渲染约定：
//! - snap：文本树（@eN 内联在行内，本身就是主产物）+ 一行元信息；
//! - click/fill/press/wait/eval/clickxy/loadall/type：一行 `result`（服务端
//!   人性化结果串，如 `Clicked <BUTTON> "Save" (native input @150,130)`）；
//!   命令期间自动应答了对话框时追加一行提示（dialogs_seen）；
//! - html：主产物在 `html` 字段（可能很大），text/quiet 原样到 stdout（与
//!   cdp.mjs 一致，不加元信息——dialogs_seen 只在 --json 里可见）；
//! - dialogs：SEQ TYPE MESSAGE 表 + cursor 行（增量拉取的 --since 值）；
//! - 三态：`--json` 响应原样透传（P2-F F3 修复：snap 与 result 型命令的
//!   shell 此前缺 --json 分支，文本树直接打到 stdout，管道侧拿不到合法
//!   JSON；分流收敛在 [`snap_out`]/[`result_out`] 纯函数）；`--quiet` 输出
//!   单行主结果（snap = targetId，其余 = result 行），供 `$(...)` 捕获。

use clap::Subcommand;

use super::output::{OutputCtx, TableRow};
use super::GlobalArgs;
use crate::client::AgentClient;
use crate::error::CliError;

/// page 子命令树。tab 参数（targetId 或唯一前缀）缺省 = 唯一页面标签自动
/// 选中，多标签 → 409 PAGE_TAB_AMBIGUOUS 且 message 列出全部 targetId
///（P2-F F1；显式前缀仍是无匹配 404 / 多匹配 409）。
#[derive(Debug, Subcommand)]
pub enum PageCommands {
    /// 可访问性树快照 + @eN 引用（操作循环起点；导航后引用整表失效）
    #[command(about = "可访问性树快照：可交互节点带 @e1..@eN 引用（操作循环起点）")]
    Snap {
        /// 实例 id
        id: String,
        /// 目标标签页（targetId 或唯一前缀，由 instance tabs 获得；缺省 = 唯一页面标签自动选中，多标签报错列出全部）
        #[arg(value_name = "TAB_ID")]
        tab: Option<String>,
    },

    /// 原生输入点击（选择器或 @eN 引用）
    #[command(about = "原生输入点击：<sel|@eN>（三段鼠标事件，非 el.click()）")]
    Click {
        /// 实例 id
        id: String,
        /// 点击目标：@eN 引用（上次 snap 产生）或 CSS 选择器
        target: String,
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签自动选中，多标签报错列出全部）
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
    },

    /// 聚焦 + 清空 + 输入 + 回读校验（表单填写用它，别 eval 手写）
    #[command(
        about = "填写输入框：<sel|@eN> <value>（聚焦→清空→输入→回读校验；空 value = 仅清空）"
    )]
    Fill {
        /// 实例 id
        id: String,
        /// 填写目标：@eN 引用或 CSS 选择器
        target: String,
        /// 填入文本（空串 = 仅清空）
        value: String,
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签自动选中，多标签报错列出全部）
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
    },

    /// 按键（三段键盘事件；--target 先聚焦）
    #[command(about = "按键：<key>（如 Enter / Control+A；--target 先聚焦再按）")]
    Press {
        /// 实例 id
        id: String,
        /// 键名：Enter/Tab/单字符/组合键（Control+A 等）
        key: String,
        /// 可选聚焦目标：@eN 引用或 CSS 选择器
        #[arg(long, value_name = "SEL|@eN")]
        target: Option<String>,
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签自动选中，多标签报错列出全部）
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
    },

    /// 轮询等待元素状态
    #[command(
        about = "等待元素状态：<selector>（--state visible|hidden|attached|detached，缺省 visible）"
    )]
    Wait {
        /// 实例 id
        id: String,
        /// CSS 选择器
        selector: String,
        /// 等待状态：visible（缺省）/ hidden / attached / detached
        #[arg(long)]
        state: Option<String>,
        /// 超时时长：10s / 250ms / 裸数字=ms（缺省 10s）
        #[arg(long)]
        timeout: Option<String>,
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签自动选中，多标签报错列出全部）
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
    },

    /// 已自动应答的对话框日志（增量游标拉取）
    #[command(
        about = "对话框日志：alert/confirm/prompt/beforeunload 均已自动 accept（--since 增量拉取）"
    )]
    Dialogs {
        /// 实例 id
        id: String,
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签自动选中，多标签报错列出全部）
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
        /// 游标：只返回 sequence 严格大于 N 的条目（缺省全部）
        #[arg(long)]
        since: Option<u64>,
    },

    /// 任意 JS 求值（高权限，最后手段）
    #[command(
        about = "任意 JS 求值：<expr>（⚠ 高权限：优先 snap/click 等专用原语，仅在不够用时用）"
    )]
    Eval {
        /// 实例 id
        id: String,
        /// JS 表达式（开头是 - 时需用 -- 分隔）
        expression: String,
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签自动选中，多标签报错列出全部）
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
    },

    /// 任意 CDP 命令透传（最高权限逃生舱）
    #[command(
        about = "CDP 命令透传：<method> [json params]（⚠ 最高权限逃生舱：仅在 page 原语不够用时用；--browser 发 browser 级）"
    )]
    Raw {
        /// 实例 id
        id: String,
        /// CDP 方法名（如 "Emulation.setDeviceMetricsOverride"）
        method: String,
        /// 命令参数 JSON（缺省 = {}；开头是 - 时需用 -- 分隔）
        params: Option<String>,
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签自动选中，多标签报错列出全部）
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
        /// 发 browser 级（Target.getTargets 等）；缺省发到 tab 的 page session
        #[arg(long)]
        browser: bool,
    },

    /// 读页面 HTML（整文档或选择器命中元素）
    #[command(about = "读 HTML：[selector]（缺省 = 整文档 outerHTML；未命中 → 404 exit 3）")]
    Html {
        /// 实例 id
        id: String,
        /// CSS 选择器（缺省 = 整文档 documentElement.outerHTML）
        selector: Option<String>,
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签自动选中，多标签报错列出全部）
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
    },

    /// 视口坐标点击（无定位/遮挡校验，坐标即意图）
    #[command(about = "坐标点击：<x> <y>（视口 CSS 像素；无校验，点错不拦）")]
    Clickxy {
        /// 实例 id
        id: String,
        /// 视口 CSS 像素横坐标
        x: f64,
        /// 视口 CSS 像素纵坐标
        y: f64,
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签自动选中，多标签报错列出全部）
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
    },

    /// 循环点击「加载更多」类按钮直到消失
    #[command(
        about = "循环点击：<selector>（直到消失；--interval 缺省 1500ms，--max-clicks 缺省 200）"
    )]
    Loadall {
        /// 实例 id
        id: String,
        /// CSS 选择器（待循环点击的按钮）
        selector: String,
        /// 轮询间隔：1500ms / 1.5s / 裸数字=ms（缺省 1500ms）
        #[arg(long)]
        interval: Option<String>,
        /// 点击上限（缺省 200；达上限仍未消失时结果明示 still present）
        #[arg(long)]
        max_clicks: Option<u32>,
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签自动选中，多标签报错列出全部）
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
    },

    /// 向当前焦点元素输入文本（跨域 iframe 唯一输入路径）
    #[command(
        about = "输入文本到当前焦点元素：<text>（跨域 iframe 输入唯一路径；先 clickxy 点入）"
    )]
    Type {
        /// 实例 id
        id: String,
        /// 非空文本（空串 → 400 exit 7）
        text: String,
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签自动选中，多标签报错列出全部）
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
    },
}

/// page 组分发入口（main.rs dispatch 的唯一调用点）。
pub fn run(cmd: &PageCommands, globals: &GlobalArgs, client: &AgentClient) -> Result<(), CliError> {
    match cmd {
        PageCommands::Snap { id, tab } => snap(globals, client, id, tab.as_deref()),
        PageCommands::Click { id, target, tab } => {
            click(globals, client, id, target, tab.as_deref())
        }
        PageCommands::Fill {
            id,
            target,
            value,
            tab,
        } => fill(globals, client, id, target, value, tab.as_deref()),
        PageCommands::Press {
            id,
            key,
            target,
            tab,
        } => press(globals, client, id, key, target.as_deref(), tab.as_deref()),
        PageCommands::Wait {
            id,
            selector,
            state,
            timeout,
            tab,
        } => wait(
            globals,
            client,
            id,
            selector,
            state.as_deref(),
            timeout.as_deref(),
            tab.as_deref(),
        ),
        PageCommands::Dialogs { id, tab, since } => {
            dialogs(globals, client, id, tab.as_deref(), *since)
        }
        PageCommands::Eval {
            id,
            expression,
            tab,
        } => eval(globals, client, id, expression, tab.as_deref()),
        PageCommands::Raw {
            id,
            method,
            params,
            tab,
            browser,
        } => raw(
            globals,
            client,
            id,
            method,
            params.as_deref(),
            tab.as_deref(),
            *browser,
        ),
        PageCommands::Html { id, selector, tab } => {
            html(globals, client, id, selector.as_deref(), tab.as_deref())
        }
        PageCommands::Clickxy { id, x, y, tab } => {
            clickxy(globals, client, id, *x, *y, tab.as_deref())
        }
        PageCommands::Loadall {
            id,
            selector,
            interval,
            max_clicks,
            tab,
        } => loadall(
            globals,
            client,
            id,
            selector,
            interval.as_deref(),
            *max_clicks,
            tab.as_deref(),
        ),
        PageCommands::Type { id, text, tab } => r#type(globals, client, id, text, tab.as_deref()),
    }
}

/// `page snap <ID> [TAB_ID]`：文本树 + 元信息行。quiet 输出 targetId
/// （后续命令 `--tab $(...)` 的取值源）；json 响应原样透传（F3）。
fn snap(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    tab: Option<&str>,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let resp = client.page_snap(id, tab)?;
    print_page_out(&out, snap_out(&resp, &out));
    Ok(())
}

/// click/fill/press/wait 共用渲染：quiet 输出 result 行（变更类无 id 可捕获，
/// 结果串即主产物）；json 响应原样透传；table = result 行 + 可选
/// dialogs_seen 提示行。
fn render_result(out: &OutputCtx, resp: &serde_json::Value) {
    print_page_out(out, result_out(resp, out));
}

/// `page click <ID> <sel|@eN> [--tab T]`
fn click(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    target: &str,
    tab: Option<&str>,
) -> Result<(), CliError> {
    let resp = client.page_click(id, target, tab)?;
    render_result(&OutputCtx::from(globals), &resp);
    Ok(())
}

/// `page fill <ID> <sel|@eN> <value> [--tab T]`
fn fill(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    target: &str,
    value: &str,
    tab: Option<&str>,
) -> Result<(), CliError> {
    let resp = client.page_fill(id, target, value, tab)?;
    render_result(&OutputCtx::from(globals), &resp);
    Ok(())
}

/// `page press <ID> <key> [--target <sel|@eN>] [--tab T]`
fn press(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    key: &str,
    target: Option<&str>,
    tab: Option<&str>,
) -> Result<(), CliError> {
    let resp = client.page_press(id, key, target, tab)?;
    render_result(&OutputCtx::from(globals), &resp);
    Ok(())
}

/// `page wait <ID> <selector> [--state V] [--timeout 10s] [--tab T]`
fn wait(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    selector: &str,
    state: Option<&str>,
    timeout: Option<&str>,
    tab: Option<&str>,
) -> Result<(), CliError> {
    let resp = client.page_wait(id, selector, state, timeout, tab)?;
    render_result(&OutputCtx::from(globals), &resp);
    Ok(())
}

/// `page eval <ID> <expr> [--tab T]`：result 型渲染（对象结果 = pretty JSON
/// 多行串）。高权限工具，服务端语义已由契约文档披露。
fn eval(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    expression: &str,
    tab: Option<&str>,
) -> Result<(), CliError> {
    let resp = client.page_eval(id, expression, tab)?;
    render_result(&OutputCtx::from(globals), &resp);
    Ok(())
}

/// `page raw <ID> <method> [json] [--tab T] [--browser]`：CDP 透传，result
/// 原样 pretty JSON。params 非法 JSON 在本地报参数错（对齐 cdp.mjs evalraw）。
fn raw(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    method: &str,
    params: Option<&str>,
    tab: Option<&str>,
    browser: bool,
) -> Result<(), CliError> {
    let params: serde_json::Value = match params {
        None => serde_json::json!({}),
        Some(raw) => serde_json::from_str(raw)
            .map_err(|e| CliError::Local(format!("params 不是合法 JSON: {e}")))?,
    };
    let resp = client.page_raw(id, method, params, tab, browser)?;
    render_result(&OutputCtx::from(globals), &resp);
    Ok(())
}

/// `page html <ID> [selector] [--tab T]`：html 原样到 stdout（主产物即输出物，
/// 与 cdp.mjs 一致）；--json 响应原样（dialogs_seen 只在该分支可见）。
fn html(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    selector: Option<&str>,
    tab: Option<&str>,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let resp = client.page_html(id, selector, tab)?;
    print_page_out(&out, html_out(&resp, &out));
    Ok(())
}

/// `page clickxy <ID> <x> <y> [--tab T]`
fn clickxy(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    x: f64,
    y: f64,
    tab: Option<&str>,
) -> Result<(), CliError> {
    let resp = client.page_clickxy(id, x, y, tab)?;
    render_result(&OutputCtx::from(globals), &resp);
    Ok(())
}

/// `page loadall <ID> <selector> [--interval 1500ms] [--max-clicks 200] [--tab T]`
fn loadall(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    selector: &str,
    interval: Option<&str>,
    max_clicks: Option<u32>,
    tab: Option<&str>,
) -> Result<(), CliError> {
    let resp = client.page_loadall(id, selector, interval, max_clicks, tab)?;
    render_result(&OutputCtx::from(globals), &resp);
    Ok(())
}

/// `page type <ID> <text> [--tab T]`
fn r#type(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    text: &str,
    tab: Option<&str>,
) -> Result<(), CliError> {
    let resp = client.page_type(id, text, tab)?;
    render_result(&OutputCtx::from(globals), &resp);
    Ok(())
}

/// `page dialogs <ID> [--tab T] [--since N]`：SEQ TYPE MESSAGE 表；
/// 表前一行 cursor（下次 --since 增量值）。quiet 每行 `seq\ttype\tmessage`。
fn dialogs(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    tab: Option<&str>,
    since: Option<u64>,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let resp = client.page_dialogs(id, tab, since)?;
    let rows = dialogs_rows(&resp);
    if out.quiet {
        for row in &rows {
            out.print_id(&row.cells.join("\t"));
        }
        return Ok(());
    }
    if !out.json {
        out.render_text(format!(
            "cursor: {}（下次增量拉取：--since {}）",
            resp.get("cursor").and_then(|v| v.as_u64()).unwrap_or(0),
            resp.get("cursor").and_then(|v| v.as_u64()).unwrap_or(0),
        ));
    }
    out.render_list(&["SEQ", "TYPE", "MESSAGE"], &rows);
    Ok(())
}

// ---------------------------------------------------------------------------
// 渲染纯函数（`*_text` / `*_rows` / `*_note`：只渲染不打印，单测直接断言；
// 服务端响应为自由形状 Value（同 diagnostics 先例），提取全部防御式缺省）
// ---------------------------------------------------------------------------

/// page 组三态分流产物：命令 shell 只调 [`print_page_out`] 打印，不自行决策
///（F3 根因即 shell 漏判 --json，分流收敛为纯函数后单测可断言）。
#[derive(Debug)]
enum PageOut {
    /// quiet 单行（snap = targetId；click/fill/press/wait = result 行）
    Quiet(String),
    /// json：服务端响应原样（pretty 落地归 OutputCtx::render_kv，同 instance
    /// screenshot/har 的 json 分支先例）
    Json(serde_json::Value),
    /// table：自由文本（snap = 元信息 + 树；其余 = result + 可选附注）
    Text(String),
}

/// 三态分流薄壳：按模式落 stdout（println 仍收敛在 output.rs）。
fn print_page_out(out: &OutputCtx, rendered: PageOut) {
    match rendered {
        PageOut::Quiet(line) => out.print_id(&line),
        PageOut::Json(resp) => out.render_kv(&[], resp),
        PageOut::Text(text) => out.render_text(text),
    }
}

/// snap 三态：quiet → targetId；json → 响应原样；table → 元信息 + 文本树。
fn snap_out(resp: &serde_json::Value, out: &OutputCtx) -> PageOut {
    if out.quiet {
        return PageOut::Quiet(
            resp.get("targetId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );
    }
    if out.json {
        return PageOut::Json(resp.clone());
    }
    PageOut::Text(snap_text(resp))
}

/// result 型命令（click/fill/press/wait/eval/clickxy/loadall/type）三态：json
/// 原样（dialogs_seen 已在响应体内，透传自然携带）；table = result 行 +
/// 附注行（单字符串两行，与逐行 println 字节等价）。
fn result_out(resp: &serde_json::Value, out: &OutputCtx) -> PageOut {
    if out.quiet {
        return PageOut::Quiet(result_line(resp));
    }
    if out.json {
        return PageOut::Json(resp.clone());
    }
    let mut text = result_line(resp);
    if let Some(note) = dialogs_seen_note(resp) {
        text.push('\n');
        text.push_str(&note);
    }
    PageOut::Text(text)
}

/// html 三态：text/quiet = html 字段原样到 stdout（html 本身就是主产物，与
/// cdp.mjs 一致——不加元信息行；dialogs_seen 只在 json 分支可见）；json =
/// 响应原样。缺字段时不装作成功（空串落 stdout 便于发现契约漂移）。
fn html_out(resp: &serde_json::Value, out: &OutputCtx) -> PageOut {
    let html = || {
        resp.get("html")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    if out.quiet {
        return PageOut::Quiet(html());
    }
    if out.json {
        return PageOut::Json(resp.clone());
    }
    PageOut::Text(html())
}

/// snap 的 table 模式输出：元信息行 + 文本树。树是主产物（@eN 引用内联），
/// 元信息行放前面供脚本 grep（targetId / refs_count / generation）。
fn snap_text(resp: &serde_json::Value) -> String {
    let str_or = |key: &str| {
        resp.get(key)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into())
    };
    format!(
        "targetId: {} · refs: {} · generation: {}\n{}",
        str_or("targetId"),
        str_or("refs_count"),
        str_or("generation"),
        resp.get("text").and_then(|v| v.as_str()).unwrap_or("")
    )
}

/// click/fill/press/wait/eval/clickxy/loadall/type 的主结果行（服务端 result
/// 字段；缺字段时不装作成功）。raw 的 result 是对象（CDP 原始 result 透传）
/// → pretty JSON 多行串；其余命令 result 恒为字符串。
fn result_line(resp: &serde_json::Value) -> String {
    match resp.get("result") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(v @ serde_json::Value::Object(_) | v @ serde_json::Value::Array(_)) => {
            serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
        }
        Some(other) => other.to_string(),
        None => "(响应缺少 result 字段)".to_string(),
    }
}

/// 命令期间自动应答了对话框时的提示行（仅 table 模式调用；None = 无附注）。
/// 服务端仅在成功响应且确有新对话框时附 dialogs_seen（snake_case 条目同
/// dialogs 端点）。
fn dialogs_seen_note(resp: &serde_json::Value) -> Option<String> {
    let seen = resp.get("dialogs_seen")?.as_array()?;
    if seen.is_empty() {
        return None;
    }
    let parts: Vec<String> = seen
        .iter()
        .map(|e| {
            format!(
                "{}: {}",
                e.get("type").and_then(|v| v.as_str()).unwrap_or("?"),
                truncate(e.get("message").and_then(|v| v.as_str()).unwrap_or(""), 60)
            )
        })
        .collect();
    Some(format!(
        "dialogs_seen: {} 个对话框已被自动应答（{}）；历史详见 page dialogs",
        seen.len(),
        parts.join(" / ")
    ))
}

/// dialogs 表行：SEQ TYPE MESSAGE（与 JSON 字段同源；message 截断 80 列，
/// json 模式完整保留——同 console 的 TEXT 列约定）。
fn dialogs_rows(resp: &serde_json::Value) -> Vec<TableRow> {
    resp.get("entries")
        .and_then(|v| v.as_array())
        .map(|entries| {
            entries
                .iter()
                .map(|e| TableRow {
                    id: e
                        .get("sequence")
                        .and_then(|v| v.as_u64())
                        .unwrap_or_default()
                        .to_string(),
                    cells: vec![
                        e.get("sequence")
                            .and_then(|v| v.as_u64())
                            .unwrap_or_default()
                            .to_string(),
                        e.get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?")
                            .to_string(),
                        truncate(e.get("message").and_then(|v| v.as_str()).unwrap_or(""), 80),
                    ],
                    json: e.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// table 列截断（按字符数，同 instance.rs 的 truncate 约定）
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// snap 渲染：元信息行在前（脚本 grep targetId/refs/generation），树体原样。
    /// refs_count 是数字字段（to_string 呈现，非 as_str）——数字与字符串两种
    /// 服务端形态都不得丟失。
    #[test]
    fn snap_text_leads_with_meta_then_tree() {
        let resp = json!({
            "instanceId": "ins_1",
            "targetId": "TAB9F",
            "text": "[WebArea] Home\n  @e1 [button] OK",
            "refs_count": 2,
            "generation": 1
        });
        let text = snap_text(&resp);
        assert!(
            text.starts_with("targetId: \"TAB9F\" · refs: 2 · generation: 1"),
            "{text}"
        );
        assert!(text.contains("@e1 [button] OK"), "树体必须原样保留: {text}");
    }

    /// F3（真机冒烟修复）：snap 三态分流——json 模式必须是响应原样 JSON
    ///（`--json | jq` 可用），修复前 shell 无 --json 分支，输出的是元信息行 +
    /// 文本树（非合法 JSON，管道侧表现为拿不到数据）。
    #[test]
    fn snap_out_routes_three_modes() {
        let resp = json!({
            "instanceId": "ins_1",
            "targetId": "TAB9F",
            "text": "[WebArea] Home\n  @e1 [button] OK",
            "refs_count": 1,
            "generation": 1
        });
        match snap_out(
            &resp,
            &OutputCtx {
                json: true,
                quiet: false,
            },
        ) {
            PageOut::Json(v) => assert_eq!(v, resp, "json 模式 = 响应原样透传"),
            other => panic!("json 模式应走 Json 分支: {other:?}"),
        }
        match snap_out(
            &resp,
            &OutputCtx {
                json: false,
                quiet: true,
            },
        ) {
            PageOut::Quiet(line) => assert_eq!(line, "TAB9F", "quiet = targetId"),
            other => panic!("quiet 模式应输出 targetId: {other:?}"),
        }
        match snap_out(
            &resp,
            &OutputCtx {
                json: false,
                quiet: false,
            },
        ) {
            PageOut::Text(t) => assert!(
                t.starts_with("targetId: \"TAB9F\"") && t.contains("@e1 [button] OK"),
                "table = 元信息 + 树: {t}"
            ),
            other => panic!("table 模式应输出文本树: {other:?}"),
        }
    }

    /// result 型命令同补三态：json 原样（dialogs_seen 在响应体内自然携带）；
    /// table = result 行 + 附注行两行（与逐行 println 字节等价）；quiet =
    /// result 行。
    #[test]
    fn result_out_routes_json_and_merges_note() {
        let resp = json!({
            "result": "Pressed Enter",
            "dialogs_seen": [ { "type": "alert", "message": "hi" } ]
        });
        match result_out(
            &resp,
            &OutputCtx {
                json: true,
                quiet: false,
            },
        ) {
            PageOut::Json(v) => assert_eq!(v, resp, "json 模式 = 响应原样透传"),
            other => panic!("json 模式应走 Json 分支: {other:?}"),
        }
        match result_out(
            &resp,
            &OutputCtx {
                json: false,
                quiet: false,
            },
        ) {
            PageOut::Text(t) => {
                assert!(t.starts_with("Pressed Enter"), "{t}");
                assert!(t.contains("dialogs_seen: 1"), "{t}");
                assert_eq!(t.lines().count(), 2, "result + 附注两行: {t}");
            }
            other => panic!("table 模式应输出 result + 附注: {other:?}"),
        }
        match result_out(
            &resp,
            &OutputCtx {
                json: false,
                quiet: true,
            },
        ) {
            PageOut::Quiet(line) => assert_eq!(line, "Pressed Enter"),
            other => panic!("quiet 模式应输出 result 行: {other:?}"),
        }
    }

    /// result 行：正常提取；缺 result 字段时显式暴露而非空串（防契约漂移静默）。
    #[test]
    fn result_line_extracts_and_flags_missing() {
        let resp = json!({ "result": "Clicked <BUTTON> \"Save\" (native input @150,130)" });
        assert_eq!(
            result_line(&resp),
            "Clicked <BUTTON> \"Save\" (native input @150,130)"
        );
        assert_eq!(
            result_line(&json!({ "instanceId": "ins_1" })),
            "(响应缺少 result 字段)"
        );
    }

    /// dialogs_seen：条目拼装 + 截断；无字段 / 空数组 → None（table 模式不输出）。
    #[test]
    fn dialogs_seen_note_formats_entries() {
        let resp = json!({
            "result": "Pressed Enter",
            "dialogs_seen": [
                { "type": "prompt", "message": "你的名字？" },
                { "type": "alert", "message": "a very long message that should be truncated at sixty characters indeed" }
            ]
        });
        let note = dialogs_seen_note(&resp).expect("应生成提示");
        assert!(
            note.starts_with("dialogs_seen: 2 个对话框已被自动应答"),
            "{note}"
        );
        assert!(note.contains("prompt: 你的名字？"), "{note}");
        assert!(note.contains("…"), "超长 message 截断: {note}");

        assert!(dialogs_seen_note(&json!({ "result": "ok" })).is_none());
        assert!(dialogs_seen_note(&json!({ "dialogs_seen": [] })).is_none());
    }

    /// dialogs 行：SEQ/TYPE/MESSAGE 三列同源，长 message 截断，json 原样。
    #[test]
    fn dialogs_rows_map_entries_to_table() {
        let resp = json!({
            "cursor": 2,
            "entries": [
                { "tab_id": "T1", "type": "alert", "message": "m1", "handled": "accepted", "sequence": 1 },
                { "tab_id": "T1", "type": "prompt", "message": "很长的消息".repeat(30), "sequence": 2 }
            ]
        });
        let rows = dialogs_rows(&resp);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].cells, vec!["1", "alert", "m1"]);
        assert_eq!(rows[0].id, "1");
        assert!(
            rows[1].cells[2].ends_with('…'),
            "长消息截断: {}",
            rows[1].cells[2]
        );
        assert_eq!(rows[0].json["type"], "alert", "行 json 原样保留");

        assert!(dialogs_rows(&json!({ "cursor": 0, "entries": [] })).is_empty());
        assert!(
            dialogs_rows(&json!({})).is_empty(),
            "缺 entries 键防御为空表"
        );
    }

    /// html 三态：text/quiet = html 字段原样（不加元信息行——html 本身就是
    /// 输出物，与 cdp.mjs 一致，$(...) 捕获不受污染）；json = 响应原样。
    #[test]
    fn html_out_routes_three_modes_pure_payload() {
        let payload = "<!DOCTYPE html><html><body>hi</body></html>";
        let resp = json!({
            "instanceId": "ins_1",
            "targetId": "TAB9F",
            "html": payload,
            "dialogs_seen": [ { "type": "alert", "message": "x" } ]
        });
        match html_out(
            &resp,
            &OutputCtx {
                json: false,
                quiet: false,
            },
        ) {
            PageOut::Text(t) => assert_eq!(t, payload, "table 模式 = html 原样，无任何前缀行"),
            other => panic!("table 模式应输出 html 原样: {other:?}"),
        }
        match html_out(
            &resp,
            &OutputCtx {
                json: false,
                quiet: true,
            },
        ) {
            PageOut::Quiet(line) => assert_eq!(line, payload),
            other => panic!("quiet 模式应输出 html 原样: {other:?}"),
        }
        match html_out(
            &resp,
            &OutputCtx {
                json: true,
                quiet: false,
            },
        ) {
            PageOut::Json(v) => {
                assert_eq!(v, resp, "json 模式 = 响应原样（dialogs_seen 在此可见）")
            }
            other => panic!("json 模式应原样透传: {other:?}"),
        }
        // 缺 html 字段防御：空串而非装作成功
        match html_out(
            &json!({ "instanceId": "ins_1" }),
            &OutputCtx {
                json: false,
                quiet: false,
            },
        ) {
            PageOut::Text(t) => assert_eq!(t, ""),
            other => panic!("缺字段防御路径: {other:?}"),
        }
    }
}
