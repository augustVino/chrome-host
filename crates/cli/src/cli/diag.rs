//! 诊断命令 —— `chrome-host status` / `chrome-host doctor`。
//!
//! 数据源唯一：GET /api/v1/health（client::health）。
//! - `status`：人类可读摘要块（render_kv）；
//! - `doctor`：逐项 ✓/✗ 清单 + 修复建议（服务端已给 suggestion，CLI 不编文案）。
//!
//! 退出码语义（exit 契约：doctor 存在失败检查项 → 1）：doctor 的失败检查项
//! 是**命令的正常业务结果**而非进程错误 —— 不走 CliError（Local 会映射 exit 2，
//! 且会触发 print_error 的错误渲染，与「已输出完整诊断报告」的事实矛盾），而是
//! handler 显式返回 ExitCode 经 dispatch 带回 main。exit.rs 仍是 CliError →
//! ExitCode 的唯一映射点，本文件的退出码是命令层显式成功/失败码，不改映射规则。

use super::output::OutputCtx;
use super::GlobalArgs;
use crate::client::AgentClient;
use crate::error::CliError;
use crate::exit::ExitCode;
use crate::model::HealthReport;

// diag 命令在 clap 树中的形态：Status / Doctor 是 main.rs `Commands` 的独立无载荷臂
// （PRD 命令面就是裸的 `chrome-host status` / `chrome-host doctor`），本文件只提供
// 两个 handler，不需要组内子命令枚举（也就不需要 clap::Subcommand 导入）。

/// `chrome-host status`：健康摘要（table 模式 kv 块；json 模式 HealthReport 原样）。
pub fn run_status(globals: &GlobalArgs, client: &AgentClient) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let report: HealthReport = client.health()?;
    out.render_kv(
        &status_kv_pairs(&report),
        serde_json::to_value(&report).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// `chrome-host doctor`：逐项诊断清单。
///
/// 返回值即命令退出码（见模块注释）：全过 → SUCCESS(0)，存在失败项 → GENERAL(1)。
pub fn run_doctor(globals: &GlobalArgs, client: &AgentClient) -> Result<ExitCode, CliError> {
    let out = OutputCtx::from(globals);
    let report: HealthReport = client.health()?;
    // 渲染收敛为纯函数（返回 String），打印统一走 [`OutputCtx::render_text`] 薄壳
    // （stdout 纪律：println! 只存在于 output.rs，本文件零直接打印）。
    let text = if out.json {
        // json 模式：HealthReport 服务端原样（pretty），结尾不追加文案 ——
        // 「N problem(s) found.」是给人读的结论，机器消费方从 ok/checks 字段自取
        serde_json::to_string_pretty(&report).expect("纯数据 DTO 的序列化不会失败")
    } else {
        doctor_report_string(&report)
    };
    out.render_text(text);
    Ok(doctor_exit_code(&report))
}

// ---------------------------------------------------------------------------
// 纯函数层（参考 output.rs 的 `*_string` 模式：只渲染不打印，单测直接断言）
// ---------------------------------------------------------------------------

/// status 摘要块的 kv 对。键名与 JSON 字段一致（camelCase 单词，两模式无信息差）；
/// kernel 行取 checks 中 name == "kernel" 项的 detail（服务端已拼好人类可读文案，
/// CLI 不二次组装）；该项缺失（契约漂移/未来改名）显示 `-` 占位而非 panic。
fn status_kv_pairs(report: &HealthReport) -> Vec<(String, String)> {
    vec![
        ("version".into(), report.version.clone()),
        ("ok".into(), report.ok.to_string()),
        ("kernel".into(), check_detail(report, "kernel").to_string()),
        ("environments".into(), report.counts.environments.to_string()),
        ("instances".into(), report.counts.instances.to_string()),
        ("running".into(), report.counts.running.to_string()),
        ("extensions".into(), report.counts.extensions.to_string()),
    ]
}

/// 按 name 查检查项 detail，查不到返回 `-`（防御：服务端检查项改名时不让 status 崩）。
fn check_detail<'a>(report: &'a HealthReport, name: &str) -> &'a str {
    report
        .checks
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.detail.as_str())
        .unwrap_or("-")
}

/// doctor 清单渲染（table 模式全文）：
/// - ok 项：`✓ {name} — {detail}`
/// - 失败项：`✗ {name} — {detail}`，若服务端给了 suggestion，下一行缩进两格
///   `  suggestion: {suggestion}`（建议文案属服务端职责，CLI 原样转述不编造）
/// - 结尾：全过 → `No problems found.`；有失败 → `{N} problem(s) found.`
///
/// stdout 纪律注：本函数是纯渲染（返回 String），打印统一经
/// [`OutputCtx::render_text`] 薄壳，diag.rs 无任何直接 println!。
fn doctor_report_string(report: &HealthReport) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut failures = 0usize;
    for check in &report.checks {
        if check.ok {
            lines.push(format!("✓ {} — {}", check.name, check.detail));
        } else {
            failures += 1;
            lines.push(format!("✗ {} — {}", check.name, check.detail));
            if let Some(suggestion) = &check.suggestion {
                lines.push(format!("  suggestion: {suggestion}"));
            }
        }
    }
    if failures == 0 {
        lines.push("No problems found.".into());
    } else {
        lines.push(format!("{failures} problem(s) found."));
    }
    lines.join("\n")
}

/// doctor 退出码（exit 契约：doctor 存在失败检查项 → 1）。
/// 以 `report.ok` 为准 —— 它是服务端对全部检查项的聚合结论，与逐项计数互为印证；
/// 本地不重算 failures，避免与服务端聚合语义出现第二事实源。
fn doctor_exit_code(report: &HealthReport) -> ExitCode {
    if report.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::GENERAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Counts, HealthCheck};

    /// 构造 HealthReport 样本：四项检查（database/kernel/directories/extensions，
    /// name 取值与 health_service.rs 一致）按传入覆盖 ok/detail/suggestion。
    fn report(checks: Vec<HealthCheck>) -> HealthReport {
        HealthReport {
            version: "0.4.2".into(),
            ok: checks.iter().all(|c| c.ok),
            checks,
            counts: Counts {
                environments: 2,
                instances: 3,
                running: 1,
                extensions: 5,
            },
        }
    }

    fn check(name: &str, ok: bool, detail: &str, suggestion: Option<&str>) -> HealthCheck {
        HealthCheck {
            name: name.into(),
            ok,
            detail: detail.into(),
            suggestion: suggestion.map(str::to_string),
        }
    }

    fn all_ok() -> HealthReport {
        report(vec![
            check("database", true, "SELECT 1 + integrity_check 通过", None),
            check("kernel", true, "Chrome for Testing 131.0.6778.204 已就绪", None),
            check("directories", true, "3 个目录存在且可写", None),
            check("extensions", true, "5 个扩展全部就绪", None),
        ])
    }

    #[test]
    fn doctor_all_ok_renders_check_marks_and_clean_ending() {
        let s = doctor_report_string(&all_ok());
        let expected = "\
✓ database — SELECT 1 + integrity_check 通过
✓ kernel — Chrome for Testing 131.0.6778.204 已就绪
✓ directories — 3 个目录存在且可写
✓ extensions — 5 个扩展全部就绪
No problems found.";
        assert_eq!(s, expected);
    }

    #[test]
    fn doctor_failures_render_cross_suggestion_and_count() {
        let r = report(vec![
            check("database", true, "SELECT 1 + integrity_check 通过", None),
            check(
                "kernel",
                false,
                "内核未安装（期望 131.0.6778.204）",
                Some("运行 chrome-host runtime install 或在 GUI 设置页下载 Chrome for Testing"),
            ),
            check("directories", true, "3 个目录存在且可写", None),
            check(
                "extensions",
                false,
                "1 个扩展状态异常",
                Some("运行 chrome-host extension list 查看具体项"),
            ),
        ]);
        let s = doctor_report_string(&r);
        let expected = "\
✓ database — SELECT 1 + integrity_check 通过
✗ kernel — 内核未安装（期望 131.0.6778.204）
  suggestion: 运行 chrome-host runtime install 或在 GUI 设置页下载 Chrome for Testing
✓ directories — 3 个目录存在且可写
✗ extensions — 1 个扩展状态异常
  suggestion: 运行 chrome-host extension list 查看具体项
2 problem(s) found.";
        assert_eq!(s, expected);
    }

    #[test]
    fn doctor_failure_without_suggestion_omits_indented_line() {
        let r = report(vec![check(
            "kernel",
            false,
            "内核未安装",
            None, // 服务端未给建议 → 不渲染空 suggestion 行
        )]);
        let s = doctor_report_string(&r);
        assert!(s.contains("✗ kernel — 内核未安装\n1 problem(s) found."), "实际:\n{s}");
        assert!(!s.contains("suggestion:"), "无建议时不应出现 suggestion 行:\n{s}");
        assert_eq!(s.lines().count(), 2, "失败行 + 结论行，仅两行:\n{s}");
    }

    #[test]
    fn doctor_exit_code_follows_report_ok() {
        assert_eq!(doctor_exit_code(&all_ok()), ExitCode::SUCCESS);
        let failed = report(vec![check("kernel", false, "未安装", None)]);
        assert_eq!(doctor_exit_code(&failed), ExitCode::GENERAL);
    }

    #[test]
    fn status_kv_pairs_shape_and_kernel_detail() {
        let pairs = status_kv_pairs(&all_ok());
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["version", "ok", "kernel", "environments", "instances", "running", "extensions"]
        );
        // kernel 行取 checks 中 kernel 项的 detail
        assert_eq!(pairs[2], ("kernel".into(), "Chrome for Testing 131.0.6778.204 已就绪".into()));
        assert_eq!(pairs[3], ("environments".into(), "2".into()));
        assert_eq!(pairs[5], ("running".into(), "1".into()));
    }

    #[test]
    fn status_kv_missing_kernel_check_falls_back_to_dash() {
        // 契约漂移防御：checks 里没有 kernel 项时不 panic，占位 `-`
        let r = report(vec![check("database", true, "ok", None)]);
        let pairs = status_kv_pairs(&r);
        assert_eq!(pairs[2], ("kernel".into(), "-".into()));
    }
}
