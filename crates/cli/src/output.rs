//! 输出三态框架（Table / Json / Quiet）—— **stdout 纪律的唯一执行点**。
//!
//! # stdout 纪律（全 crate 硬规矩，review 时 grep `println!` 检查）
//! - 业务结果只写 stdout，而 `println!` / `print!` **只允许出现在本文件**。
//!   任何命令需要输出一律经由 [`OutputCtx`] 的 API；新形态先扩展本文件再使用。
//! - stderr 合法出口清单（全 crate 当前全集，新增出口须在此登记）：
//!   ① 错误渲染（[`OutputCtx::print_error`] 非 json 分支，eprintln!，本文件）；
//!   ② 危险操作确认提示（cli/mod.rs `confirm_with_reader`，eprint!）；
//!   ③ verbose 请求摘要（client.rs `send`，eprintln!，PRD cli.md §22.3 只含
//!     method/path/耗时，不含 query 与 body）；
//!   ④ runtime install 轮询进度与终态提示（cli/runtime.rs，eprintln!；--json 模式
//!     stderr 静默，stdout 只输出最终 CftStatus）。
//!
//! # 可测性取舍（纯函数 + 薄壳，why）
//! 所有渲染逻辑收敛为「输入 → String」的纯函数（`*_string` 后缀），单测直接断言
//! 返回值，不起子进程、不劫持真 stdout —— 用 `std::process::Command` 测打印既慢又脆，
//! 还要把 CLI 拆成可执行测试靶子，得不偿失（计划 §2.8 第 1 层单测的定位）。
//! [`OutputCtx`] 的公开方法只是「按模式选择纯函数 + 打印」的薄壳。

use comfy_table::Table;

use crate::cli::GlobalArgs;
use crate::error::CliError;

/// 输出上下文：三态中的两态开关（`--json` 与 `--quiet` 互斥，clap 已 conflicts_with）。
/// `no_color` 不建模：MVP 不输出任何 ANSI 颜色（计划 §2.5），该 flag 仅为契约保留。
#[derive(Debug, Clone, Copy)]
pub struct OutputCtx {
    pub json: bool,
    pub quiet: bool,
}

impl From<&GlobalArgs> for OutputCtx {
    fn from(g: &GlobalArgs) -> Self {
        Self {
            json: g.json,
            quiet: g.quiet,
        }
    }
}

/// 一行列表数据，三字段对应三态、信息不得互相矛盾（PRD §39：两模式无信息差）：
/// - `cells`：table 模式的单元格（列集与 JSON 字段一一对应）；
/// - `json`：该行完整 JSON（服务端原样，含 CJK 原文与 null），json 模式逐行收集成数组；
/// - `id`：quiet 模式每行输出的主实体 id。
#[derive(Debug, Clone)]
pub struct TableRow {
    pub id: String,
    pub cells: Vec<String>,
    pub json: serde_json::Value,
}

/// table 模式空列表的明确占位：不输出空表格框架（表头/边框会误导为「有零列结构」）。
const EMPTY_PLACEHOLDER: &str = "(empty)";

impl OutputCtx {
    /// 列表渲染薄壳（env/instance/extension 三组 list 共用）。
    /// quiet：每行一个 id；json：行 JSON 收集为数组整体 pretty（空列表输出 `[]`，
    /// jq/管道友好）；缺省 table：comfy-table（无行时输出占位文案）。
    pub fn render_list(&self, headers: &[&str], rows: &[TableRow]) {
        if self.quiet {
            let text = list_quiet_string(rows);
            if !text.is_empty() {
                println!("{text}");
            }
            return;
        }
        if self.json {
            println!("{}", list_json_string(rows));
        } else {
            println!("{}", list_table_string(headers, rows));
        }
    }

    /// 单对象渲染薄壳（env get / instance cdp / runtime / status 共用）。
    /// json：值整体 pretty；缺省 table：`key: value` 逐行。
    /// quiet 对单对象无冻结语义（§2.5 的 quiet 只给列表与变更类命令），按缺省 kv 输出。
    pub fn render_kv(&self, pairs: &[(String, String)], json: serde_json::Value) {
        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json).expect("json!/DTO 构造的 Value 序列化不会失败")
            );
            return;
        }
        let text = kv_table_string(pairs);
        if !text.is_empty() {
            println!("{text}");
        }
    }

    /// 多行自由文本渲染薄壳：doctor 清单等非 kv/json 形态的全文输出。
    /// 渲染逻辑仍在调用方以纯函数完成（返回 String），本方法只负责落 stdout ——
    /// `println!` 依旧收敛在本文件，stdout 纪律不被破坏。空串调用方自行短路（同 render_kv）。
    pub fn render_text(&self, s: String) {
        println!("{s}");
    }

    /// Quiet 契约（计划 §2.5）：变更类命令（create/delete 等）仅输出主实体 id,
    /// 供 `$(...)` 捕获。调用方负责先判 quiet 分支；本方法是「单行 id」的唯一 stdout 出口。
    pub fn print_id(&self, id: &str) {
        println!("{id}");
    }

    /// 顶层错误出口（main.rs 唯一调用）：json 模式错误体走 **stdout**（PRD §19：
    /// 管道另一侧永远拿到合法 JSON），非 json 走 stderr。exit code 由 main 按
    /// `exit::from_cli_error` 统一附加，本方法不退出进程。
    pub fn print_error(&self, err: &CliError) {
        if self.json {
            println!("{}", error_json_string(err));
        } else {
            eprintln!("{}", error_text_string(err));
        }
    }
}

// ---------------------------------------------------------------------------
// 纯函数层（`*_string` 后缀：只渲染不打印，单测直接断言）
// ---------------------------------------------------------------------------

/// table 模式：comfy-table 渲染。默认 ContentArrangement 按 unicode-width 计算列宽
/// （Cargo.lock 锁定的传递依赖），CJK 双宽字符参与对齐；本项目不设任何带 ANSI 颜色的
/// preset，管道（非 TTY）下输出纯文本。
pub(crate) fn list_table_string(headers: &[&str], rows: &[TableRow]) -> String {
    if rows.is_empty() {
        return EMPTY_PLACEHOLDER.to_string();
    }
    let mut table = Table::new();
    table.set_header(headers);
    for row in rows {
        table.add_row(row.cells.iter().map(String::as_str));
    }
    table.to_string()
}

/// quiet 模式：每行一个 id。空列表返回空串（薄壳据此什么都不打印）。
pub(crate) fn list_quiet_string(rows: &[TableRow]) -> String {
    rows.iter()
        .map(|r| r.id.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// json 模式：行 JSON 收集为数组，整体 pretty。空列表输出 `[]`（合法 JSON，
/// `| jq` 不因空结果报错）。
pub(crate) fn list_json_string(rows: &[TableRow]) -> String {
    let array = serde_json::Value::Array(rows.iter().map(|r| r.json.clone()).collect());
    serde_json::to_string_pretty(&array).expect("Value 数组序列化不会失败")
}

/// 单对象 table 模式：`key: value` 逐行。空键值对返回空串（薄壳据此不打印）。
pub(crate) fn kv_table_string(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// json 模式的错误体形状，与服务端 `src-tauri/src/api/error.rs` 的 ErrorBody 同构：
/// `{"error":{"code","message"}}`。code 取值：
/// - `CliError::Api` → 服务端机器码原样透传（CLI 只透传不造码）；
/// - 非 Api 错误用 CLI 侧语义命名，绝不与服务端 15 个码撞名：
///   Local → `CLI_ERROR`（本地校验失败/取消）、Unreachable → `UNREACHABLE`、
///   Protocol → `PROTOCOL_ERROR`（协议破坏）。
pub(crate) fn error_json_string(err: &CliError) -> String {
    let (code, message) = error_parts(err);
    let body = serde_json::json!({ "error": { "code": code, "message": message } });
    serde_json::to_string_pretty(&body).expect("json! 构造的 Value 序列化不会失败")
}

/// 非 json 模式的错误文案（stderr）：`Error: {err}`，err 的 Display 已带指引
/// （如 Unreachable 的「请确认桌面应用已运行」）。
pub(crate) fn error_text_string(err: &CliError) -> String {
    format!("Error: {err}")
}

/// (code, message) 提取：Api 用服务端 code 与 message；其余按上注释的语义命名。
fn error_parts(err: &CliError) -> (String, String) {
    match err {
        CliError::Api { code, message, .. } => (code.clone(), message.clone()),
        CliError::Unreachable(m) => ("UNREACHABLE".into(), m.clone()),
        CliError::Protocol(m) => ("PROTOCOL_ERROR".into(), m.clone()),
        CliError::Local(m) => ("CLI_ERROR".into(), m.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_rows() -> Vec<TableRow> {
        vec![
            TableRow {
                id: "env_a".into(),
                cells: vec![
                    "env_a".into(),
                    "开发环境（CJK 列宽）".into(),
                    "-".into(),
                    "1".into(),
                    "2".into(),
                ],
                json: json!({ "id": "env_a", "name": "开发环境（CJK 列宽）", "hostsSourceUrl": null }),
            },
            TableRow {
                id: "env_b".into(),
                cells: vec![
                    "env_b".into(),
                    "qa".into(),
                    "https://example.com/hosts".into(),
                    "0".into(),
                    "0".into(),
                ],
                json: json!({ "id": "env_b", "name": "qa" }),
            },
        ]
    }

    fn test_globals(json: bool, quiet: bool) -> GlobalArgs {
        GlobalArgs {
            json,
            quiet,
            verbose: false,
            no_color: false,
            yes: false,
            api_url: None,
            token: None,
        }
    }

    #[test]
    fn list_table_with_cjk_renders_rows_completely() {
        let s = list_table_string(&["ID", "NAME", "HOSTS-SOURCE", "RUNNING", "TOTAL"], &sample_rows());
        // 行为契约：CJK 双宽字符参与列宽计算不 panic、内容完整、表头与每行数据独立成行
        assert!(s.contains("开发环境（CJK 列宽）"), "CJK 内容丢失:\n{s}");
        assert!(s.contains("HOSTS-SOURCE"), "表头丢失:\n{s}");
        assert!(s.contains("env_b"), "第二行丢失:\n{s}");
        assert!(s.lines().count() >= 5, "表头 + 分隔 + 2 数据行应至少 5 行:\n{s}");
    }

    #[test]
    fn list_table_empty_outputs_placeholder_not_frame() {
        assert_eq!(list_table_string(&["ID"], &[]), "(empty)");
    }

    #[test]
    fn list_quiet_outputs_id_per_line() {
        assert_eq!(list_quiet_string(&sample_rows()), "env_a\nenv_b");
        assert_eq!(list_quiet_string(&[]), "", "空列表应输出空串（薄壳据此静默）");
    }

    #[test]
    fn list_json_is_pretty_array_equivalent_to_input_values() {
        let s = list_json_string(&sample_rows());
        let parsed: serde_json::Value = serde_json::from_str(&s).expect("应为合法 JSON");
        let expected = serde_json::Value::Array(sample_rows().into_iter().map(|r| r.json).collect());
        assert_eq!(parsed, expected, "数组应与输入行的 json 字段等价");
        assert!(s.contains('\n'), "应为 pretty 多行输出");
        assert_eq!(list_json_string(&[]), "[]", "空列表应输出合法 JSON 空数组");
    }

    #[test]
    fn kv_table_is_key_value_lines() {
        let pairs = vec![
            ("name".to_string(), "测试".to_string()),
            ("keepAlive".to_string(), "true".to_string()),
        ];
        assert_eq!(kv_table_string(&pairs), "name: 测试\nkeepAlive: true");
        assert_eq!(kv_table_string(&[]), "");
    }

    #[test]
    fn error_json_carries_code_per_variant() {
        // Api：服务端机器码原样透传
        let api = CliError::Api {
            status: 404,
            code: "ENVIRONMENT_NOT_FOUND".into(),
            message: "not found".into(),
        };
        let v: serde_json::Value = serde_json::from_str(&error_json_string(&api)).expect("应为合法 JSON");
        assert_eq!(v["error"]["code"], "ENVIRONMENT_NOT_FOUND");
        assert_eq!(v["error"]["message"], "not found");

        // 非 Api：CLI 侧语义命名，不与服务端码撞名
        for (err, want) in [
            (CliError::Local("取消".into()), "CLI_ERROR"),
            (CliError::Unreachable("refused".into()), "UNREACHABLE"),
            (CliError::Protocol("bad".into()), "PROTOCOL_ERROR"),
        ] {
            let v: serde_json::Value =
                serde_json::from_str(&error_json_string(&err)).expect("应为合法 JSON");
            assert_eq!(v["error"]["code"], want, "variant: {err:?}");
        }
    }

    #[test]
    fn error_text_is_stderr_style_line() {
        let err = CliError::Local("已取消".into());
        let text = error_text_string(&err);
        assert!(text.starts_with("Error: "), "实际: {text}");
        assert!(text.contains("已取消"));
    }

    #[test]
    fn output_ctx_from_globals_maps_flags() {
        let ctx = OutputCtx::from(&test_globals(true, false));
        assert!(ctx.json);
        assert!(!ctx.quiet);
        let ctx = OutputCtx::from(&test_globals(false, true));
        assert!(!ctx.json);
        assert!(ctx.quiet);
    }
}
