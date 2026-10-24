//! page 命令组 —— 页面控制（P2-C）：snap / click / fill / press / wait / dialogs。
//!
//! 行为语义全部在服务层 page_service（P2-B）：本组只做参数透传 + 渲染。
//! 核心循环（对齐 SKILL.md 教学）：`snap` 拿 @eN 引用 → `click`/`fill`/
//! `press --target` 用引用操作 → 页面变化后重新 `snap`（旧引用报
//! PAGE_REF_STALE / PAGE_REF_NOT_FOUND，重 snap 而非对旧引用重试）。
//!
//! 渲染约定：
//! - snap：文本树（@eN 内联在行内，本身就是主产物）+ 一行元信息；
//! - click/fill/press/wait：一行 `result`（服务端人性化结果串，如
//!   `Clicked <BUTTON> "Save" (native input @150,130)`）；命令期间自动应答了
//!   对话框时追加一行提示（dialogs_seen）；
//! - dialogs：SEQ TYPE MESSAGE 表 + cursor 行（增量拉取的 --since 值）；
//! - 三态：`--json` 响应原样透传；`--quiet` 输出单行主结果（snap = targetId，
//!   其余 = result 行），供 `$(...)` 捕获。

use clap::Subcommand;

use super::output::{OutputCtx, TableRow};
use super::GlobalArgs;
use crate::client::AgentClient;
use crate::error::CliError;

/// page 子命令树。tab 参数（targetId 或唯一前缀）缺省 = 唯一 page tab
/// （多 tab 时服务端 409 PAGE_TAB_AMBIGUOUS，需显式指定）。
#[derive(Debug, Subcommand)]
pub enum PageCommands {
    /// 可访问性树快照 + @eN 引用（操作循环起点；导航后引用整表失效）
    #[command(about = "可访问性树快照：可交互节点带 @e1..@eN 引用（操作循环起点）")]
    Snap {
        /// 实例 id
        id: String,
        /// 目标标签页（targetId 或唯一前缀，由 instance tabs 获得；缺省 = 唯一页面标签）
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
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签）
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
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签）
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
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签）
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
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签）
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
        /// 目标标签页（targetId 或唯一前缀；缺省 = 唯一页面标签）
        #[arg(long, value_name = "TAB_ID")]
        tab: Option<String>,
        /// 游标：只返回 sequence 严格大于 N 的条目（缺省全部）
        #[arg(long)]
        since: Option<u64>,
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
    }
}

/// `page snap <ID> [TAB_ID]`：文本树 + 元信息行。quiet 输出 targetId
/// （后续命令 `--tab $(...)` 的取值源）。
fn snap(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    tab: Option<&str>,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let resp = client.page_snap(id, tab)?;
    if out.quiet {
        out.print_id(resp.get("targetId").and_then(|v| v.as_str()).unwrap_or(""));
        return Ok(());
    }
    out.render_text(snap_text(&resp));
    Ok(())
}

/// click/fill/press/wait 共用渲染：一行 result + 可选 dialogs_seen 提示。
/// quiet 输出 result 行（变更类无 id 可捕获，结果串即主产物）。
fn render_result(out: &OutputCtx, resp: &serde_json::Value) {
    if out.quiet {
        out.print_id(&result_line(resp));
        return;
    }
    out.render_text(result_line(resp));
    if let Some(note) = dialogs_seen_note(resp) {
        out.render_text(note);
    }
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

/// click/fill/press/wait 的主结果行（服务端 result 字段；缺字段时不装作成功）。
fn result_line(resp: &serde_json::Value) -> String {
    resp.get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("(响应缺少 result 字段)")
        .to_string()
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
}
