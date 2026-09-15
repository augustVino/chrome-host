//! CLI 入口：parse → dispatch → render → exit。
//!
//! 全局 tracing 不初始化（MVP 无日志；诊断信息只经 `--verbose` 走 stderr，见计划 §2.3）。
//! 顶层错误出口唯一：dispatch 返回 Err → `output.print_error` 渲染（json → stdout，
//! 否则 stderr）→ `exit::from_cli_error` 映射退出码。除 clap 自身的 help/version/usage
//! 退出外，进程退出码只在本文件决定。

mod cli;
mod client;
mod error;
mod exit;
mod model;

use clap::Parser;

use crate::cli::output::OutputCtx;
use crate::cli::{Cli, Commands};
use crate::client::AgentClient;
use crate::error::CliError;
use crate::exit::ExitCode;

fn main() {
    // clap 解析失败 / --help / --version 由 clap 自行退出（usage 错误 2、帮助/版本 0），不经过 dispatch
    let cli = Cli::parse();
    // OutputCtx 在 dispatch 前构造：错误渲染（print_error）也要按三态模式分流
    let output = OutputCtx::from(&cli.globals);
    match dispatch(&cli) {
        // 命令层显式返回退出码（doctor 失败检查项 → 1；其余命令统一 SUCCESS）
        Ok(code) => std::process::exit(i32::from(code.as_u8())),
        Err(err) => {
            output.print_error(&err);
            std::process::exit(i32::from(exit::from_cli_error(&err).as_u8()));
        }
    }
}

/// 命令分发：构造 AgentClient（唯一网络出口，client.rs）→ 匹配子命令 → handler。
/// HTTP 与渲染细节不进本文件。
///
/// 返回 `Result<ExitCode, CliError>`（why）：doctor 的「存在失败检查项 → 1」
/// （计划 §2.4）是命令的正常业务结果而非进程错误 —— 不经 CliError（Local 会误映射
/// exit 2，且触发 print_error 的错误渲染，与「已输出完整诊断报告」的事实矛盾），
/// 而由 handler 显式带回退出码。其余命令的 `Ok(())` 统一包装为 ExitCode::SUCCESS；
/// exit.rs 仍是 CliError → ExitCode 的唯一映射点。
fn dispatch(cli: &Cli) -> Result<ExitCode, CliError> {
    let globals = &cli.globals;
    // 基地址解析：flag > 环境变量 > 默认（GlobalArgs::api_url）；
    // 构造失败（TLS 后端等环境级问题）→ CliError::Local → exit 2。
    // verbose：--verbose 时 client.send() 向 stderr 输出请求摘要行。
    let client = AgentClient::new(&globals.api_url())?.with_verbose(globals.verbose);
    match cli.command.as_ref() {
        Some(Commands::Env { command }) => {
            cli::env::run(command, globals, &client).map(|_| ExitCode::SUCCESS)
        }
        Some(Commands::Instance { command }) => {
            cli::instance::run(command, globals, &client).map(|_| ExitCode::SUCCESS)
        }
        Some(Commands::Extension { command }) => {
            cli::extension::run(command, globals, &client).map(|_| ExitCode::SUCCESS)
        }
        Some(Commands::LoginProfile { command }) => {
            cli::login_profile::run(command, globals, &client).map(|_| ExitCode::SUCCESS)
        }
        Some(Commands::Settings { command }) => {
            cli::settings::run(command, globals, &client).map(|_| ExitCode::SUCCESS)
        }
        // runtime / doctor 显式带回业务退出码：install 超时 → 9、下载中止 → 5、
        // doctor 存在失败检查项 → 1
        Some(Commands::Runtime { command }) => cli::runtime::run(command, globals, &client),
        Some(Commands::Status) => {
            cli::diag::run_status(globals, &client).map(|_| ExitCode::SUCCESS)
        }
        Some(Commands::Doctor) => cli::diag::run_doctor(globals, &client),
        // 传了全局 flag 但没给子命令：用法错误（bare 无参调用已由 arg_required_else_help 拦截）
        None => Err(CliError::Local(
            "缺少子命令：运行 chrome-host --help 查看用法".into(),
        )),
    }
}
