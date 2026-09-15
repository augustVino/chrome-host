//! clap 命令树根 + 全局参数 + 危险操作确认流。
//!
//! 子命令树：env / instance / extension / login-profile / settings / runtime 六组，
//! 外加 status / doctor（实现分布见 cli/ 各文件）。
//! 全局参数封装在 [`GlobalArgs`]，子命令经 `#[command(flatten)]` 复用同一结构，
//! 保证 `--json` / `--quiet` / `--verbose` 等语义在所有命令上只定义一次。

use clap::{Args, Parser, Subcommand};

use crate::error::CliError;

/// output.rs 文件位于 src/ 根（计划 §2.3 的四层结构：cli/ → client → model → output → exit），
/// 但在此经 `#[path]` 挂载为 `cli::output` 子模块。why：tests/contract.rs 以 `#[path]`
/// 挂载本文件构建契约测试 crate，而该文件按「不得触碰」约束不挂载 output 模块 ——
/// 若 output.rs 改由 main.rs 顶层 `mod` 声明，env.rs 的 `crate::output` 引用在契约
/// 测试 crate 中无法解析。挂载点唯一（此处），bin 与契约测试共用同一模块，无重复编译。
#[path = "../output.rs"]
pub mod output;

pub mod diag;
pub mod env;
pub mod extension;
pub mod instance;
pub mod login_profile;
pub mod runtime;
pub mod settings;

/// Agent API 默认基地址。client.rs 构造复用此常量，避免两处硬编码漂移。
pub const DEFAULT_API_URL: &str = "http://127.0.0.1:17890";

/// `--api-url` 缺省时的环境变量回退名。
/// headless daemon 与集成测试的预留接入点（计划 §2.1）。
pub const ENV_API_URL: &str = "CHROME_HOST_API_URL";

/// CLI 根命令。
#[derive(Debug, Parser)]
#[command(
    name = "chrome-host",
    version,
    about = "chrome-host 命令行 — Developer-first Chrome Runtime Control Interface",
    // 无子命令声明时 bare 调用打印帮助并按 clap 默认退出（帮助 2 / --help 0），绝不静默空跑
    arg_required_else_help = true,
    after_help = "环境变量:\n  CHROME_HOST_API_URL  Agent API 基地址回退（优先级低于 --api-url，默认 http://127.0.0.1:17890）"
)]
pub struct Cli {
    /// 全局参数：flatten 在根上，位于子命令名之前。
    #[command(flatten)]
    pub globals: GlobalArgs,
    /// 子命令树：env / instance / extension / login-profile / settings / runtime
    /// 六组 + status / doctor。
    /// Option：`chrome-host --json` 这类「只有全局 flag」的调用落入 None，
    /// 由 main.rs dispatch 按用法错误（exit 2）处理。
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// 顶层子命令枚举：每组一个变体，内嵌该组的 clap 子命令枚举（`#[command(subcommand)]`
/// 声明嵌套 —— 元组变体形式会被 clap derive 当作 `Args` 载荷而非子命令嵌套）。
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// 环境管理
    Env {
        /// env 子命令（list/get/create/update/delete）
        #[command(subcommand)]
        command: env::EnvCommands,
    },
    /// 实例管理
    Instance {
        /// instance 子命令（list/get/create/start/stop/restart/open/cdp/delete）
        #[command(subcommand)]
        command: instance::InstanceCommands,
    },
    /// 扩展管理
    Extension {
        /// extension 子命令（list/get/add/enable/disable/remove）
        #[command(subcommand)]
        command: extension::ExtensionCommands,
    },
    /// 登录态生命周期管理（launch → 人工登录 → capture 捕获快照 → create 的实例自动克隆）
    LoginProfile {
        /// login-profile 子命令（list/get/launch/capture/reset）
        #[command(subcommand)]
        command: login_profile::LoginProfileCommands,
    },
    /// 全局应用设置（单例资源，无 id 路由）
    Settings {
        /// settings 子命令（get/update）
        #[command(subcommand)]
        command: settings::SettingsCommands,
    },
    /// 内核（Chrome for Testing）状态
    Runtime {
        /// runtime 子命令（version/list）
        #[command(subcommand)]
        command: runtime::RuntimeCommands,
    },
    /// 健康摘要（应用版本 / 内核 / 资源计数）
    Status,
    /// 全面诊断（逐项检查 + 修复建议；存在失败项时退出码 1，计划 §2.4）
    Doctor,
}

/// 全局 flag 集（PRD cli.md §16/§19/§22 契约）。
///
/// 设计为独立 `Args` 结构以便子命令 `#[command(flatten)]` 复用；flag 语义只在此定义一次。
/// 输出/行为类 flag 加 `global = true`：既可放在子命令前（`chrome-host --json env list`），
/// 也可跟在子命令后（`chrome-host env list --json`，PRD 的管道用例范式）。
/// 注意 `--yes` **不加** global：env delete 有自己的子命令级 `--yes`，同名全局传播
/// 会触发 clap 重复参数冲突；且 delete 级 flag 已覆盖「命令后置」的常用位。
#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// 机器可读输出：stdout 输出纯 JSON，错误体也走 stdout（PRD §19）
    #[arg(long, global = true, conflicts_with = "quiet")]
    pub json: bool,

    /// 安静模式：仅输出主实体 id，供 $(...) 捕获
    #[arg(long, global = true)]
    pub quiet: bool,

    /// 诊断模式：向 stderr 追加请求方法/路径/耗时（不含 query 与敏感内容，PRD §22）
    #[arg(long, global = true)]
    pub verbose: bool,

    /// 禁用 ANSI 颜色。MVP 本就不输出颜色（纯文本表格），保留 flag 以兼容 PRD
    /// 契约与未来上色后的管道场景
    #[arg(long, global = true)]
    pub no_color: bool,

    /// 跳过危险操作确认（非交互环境必需，缺失时 exit 2，绝不静默执行）
    #[arg(long)]
    pub yes: bool,

    /// Agent API 基地址。隐藏出帮助正文（运维参数，非日常使用），
    /// 解析顺序见 [`GlobalArgs::api_url`]；环境变量回退在 after_help 中说明。
    #[arg(long, hide = true, value_name = "URL")]
    pub api_url: Option<String>,
}

impl GlobalArgs {
    /// 解析最终 API 基地址：flag > 环境变量 > 默认值。
    ///
    /// 不用 clap 内建 `env` 特性：那需要给 clap 加 "env" feature，
    /// 计划 §2.2 的依赖清单已冻结，不为单一参数加计划外 feature。
    pub fn api_url(&self) -> String {
        self.api_url
            .clone()
            .or_else(|| std::env::var(ENV_API_URL).ok())
            .unwrap_or_else(|| DEFAULT_API_URL.to_string())
    }
}

// ---------------------------------------------------------------------------
// 危险操作确认流（计划 §2.7）
// ---------------------------------------------------------------------------

/// 危险操作统一确认入口：`--yes` 放行；非交互（stdin 非 TTY）且无 `--yes` 直接拒绝
/// （→ exit 2，请求绝不发出）；交互 TTY 打印提示读一行，`y`/`yes` 才放行（默认 N）。
/// 内核在 [`confirm_with_reader`]：yes / is_tty / 输入源全部参数化，交互分支可单测。
pub fn confirm_or_yes(globals: &GlobalArgs, msg: &str) -> Result<(), CliError> {
    use std::io::IsTerminal;
    // 先判 TTY 再锁 stdin：IsTerminal 探测不消费缓冲区，与后续读行互不影响
    let is_tty = std::io::stdin().is_terminal();
    let mut stdin = std::io::stdin().lock();
    confirm_with_reader(globals.yes, is_tty, msg, &mut stdin)
}

/// 确认流内核。stdout 纪律注：提示走 stderr（eprint!），为全 crate stderr 出口清单
/// （output.rs 模块注释）中的 ② 号出口；其余出口（①③④）以 output.rs 清单为准。
fn confirm_with_reader<R: std::io::BufRead>(
    yes: bool,
    is_tty: bool,
    msg: &str,
    input: &mut R,
) -> Result<(), CliError> {
    use std::io::Write;
    if yes {
        return Ok(());
    }
    if !is_tty {
        // 非交互环境绝不静默执行危险操作（PRD DoD 4）：失败发生在请求发出之前
        return Err(CliError::Local(format!(
            "非交互环境执行危险操作需要 --yes：{msg}"
        )));
    }
    eprint!("{msg} [y/N] ");
    std::io::stderr()
        .flush()
        .map_err(|e| CliError::Local(format!("写入确认提示失败: {e}")))?;
    let mut line = String::new();
    input
        .read_line(&mut line)
        .map_err(|e| CliError::Local(format!("读取确认输入失败: {e}")))?;
    let answer = line.trim();
    if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") {
        Ok(())
    } else {
        // 其余任何输入（含空行 = 默认 N）一律取消，exit 2
        Err(CliError::Local("已取消".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn globals(yes: bool) -> GlobalArgs {
        GlobalArgs {
            json: false,
            quiet: false,
            verbose: false,
            no_color: false,
            yes,
            api_url: None,
        }
    }

    /// 经注入 BufRead 驱动确认内核，不碰真实 stdin（交互终端下跑测试不会挂起）。
    fn confirm(yes: bool, is_tty: bool, input: &str) -> Result<(), CliError> {
        let mut reader = input.as_bytes();
        confirm_with_reader(yes, is_tty, "确认?", &mut reader)
    }

    #[test]
    fn yes_flag_skips_confirmation_entirely() {
        // --yes 下连 stdin 形态都不影响结果（非 TTY 也放行）
        assert!(confirm(true, false, "").is_ok());
        assert!(confirm(true, true, "n\n").is_ok());
        assert!(confirm_or_yes(&globals(true), "确认?").is_ok());
    }

    /// 非 TTY 且无 --yes：直接拒绝（exit 2 语义），不读输入。
    /// 真实进程中该分支来自 `stdin().is_terminal()`（管道 / CI 恒为 false）。
    #[test]
    fn non_tty_without_yes_is_rejected() {
        let err = confirm(false, false, "").unwrap_err();
        assert!(
            matches!(&err, CliError::Local(m) if m.contains("--yes")),
            "应提示需要 --yes，实际 {err:?}"
        );
        let err = confirm_or_yes(&globals(false), "确认?").unwrap_err();
        assert!(matches!(err, CliError::Local(_)));
    }

    /// TTY 交互分支（输入注入）：y/yes 放行，其余含空行（默认 N）一律取消。
    #[test]
    fn tty_answers_accept_only_y_and_yes() {
        assert!(confirm(false, true, "y\n").is_ok());
        assert!(confirm(false, true, "Y\n").is_ok());
        assert!(confirm(false, true, "yes\n").is_ok());
        assert!(confirm(false, true, "  yes  \n").is_ok());

        assert!(matches!(
            &confirm(false, true, "n\n").unwrap_err(),
            CliError::Local(m) if m == "已取消"
        ));
        assert!(matches!(
            &confirm(false, true, "\n").unwrap_err(),
            CliError::Local(m) if m == "已取消"
        ));
        assert!(matches!(
            &confirm(false, true, "ok\n").unwrap_err(),
            CliError::Local(m) if m == "已取消"
        ));
    }

    // 真实 TTY 的端到端交互（eprint 提示 + 终端读行）不自动化：依赖真终端，
    // 留给 scripts/smoke-cli.sh 人工 smoke（计划 §2.8 第 4 层）。
}
