//! runtime 命令组 —— Chrome for Testing 内核状态查询与安装。
//!
//! 数据源唯一：GET /api/v1/kernel/status（client::kernel_status）；写操作为
//! POST kernel/download 与 kernel/cancel。
//! 内核为 pinned 单版本策略（CFT_VERSION 锁定），PRD
//! cli.md §14 的多版本表不适用 —— `runtime list` 渲染单行版本信息，与
//! `runtime version` 同源同形，差别仅在命令语义位（about 文案），共享同一实现。
//!
//! 退出码契约：install 轮询超过
//! `--timeout` → 9（`ExitCode::TIMEOUT` 首次真实启用，doctor 同款「命令层显式
//! ExitCode」路径）；轮询观测到 downloading true→false 中止且仍未装 → 5
//! （服务端 CftStatus 无 error 字段，只能启发式判定，文案保留两可语义）。

use std::time::{Duration, Instant};

use clap::Subcommand;

use super::confirm_or_yes;
use super::output::OutputCtx;
use super::GlobalArgs;
use crate::client::AgentClient;
use crate::error::CliError;
use crate::exit::ExitCode;
use crate::model::CftStatus;

/// runtime 子命令树。
#[derive(Debug, Subcommand)]
pub enum RuntimeCommands {
    /// 当前内核版本状态（pinned 期望版本 / 已装版本 / 是否下载中 / 二进制路径）
    #[command(about = "当前内核版本状态（pinned 期望版本 / 已装版本 / 是否下载中 / 二进制路径）")]
    Version,

    /// 内核版本列表（单 pinned 版本策略，详见 runtime version）
    #[command(about = "内核版本列表（单 pinned 版本策略，详见 runtime version）")]
    List,

    /// 安装内核（已装则幂等短路；--cancel 中止进行中的下载）
    #[command(about = "安装内核（已装则幂等短路；--cancel 中止进行中的下载）")]
    Install {
        /// 最长等待秒数，0 = 无限等待；超时 → exit 9。
        /// 与 --cancel 互斥（clap conflicts_with）：取消语义下没有「等待时长」可言
        #[arg(
            long,
            value_name = "SEC",
            default_value_t = DEFAULT_INSTALL_TIMEOUT_SECS,
            conflicts_with = "cancel"
        )]
        timeout: u64,

        /// 中止进行中的内核下载（危险操作：总是确认，非 TTY 缺 --yes → exit 2）
        #[arg(long)]
        cancel: bool,

        /// 跳过 --cancel 的确认流（非交互环境必需）。子命令级与全局 --yes 等效：
        /// 任一放行即跳过确认（env delete / login-profile capture·reset 同一约定）
        #[arg(long)]
        yes: bool,
    },
}

/// runtime 组分发入口（main.rs dispatch 的唯一调用点）。
/// 整组 handler 统一返回 `Result<ExitCode, CliError>`：install 需要显式带回
/// 5/9 业务退出码（doctor 同款，不走 CliError —— Local 会误映射 exit 2）；
/// version/list 末尾 `Ok(ExitCode::SUCCESS)` 同型对齐，main.rs 的 Runtime 臂
/// 得以零适配透传。
pub fn run(
    cmd: &RuntimeCommands,
    globals: &GlobalArgs,
    client: &AgentClient,
) -> Result<ExitCode, CliError> {
    match cmd {
        // pinned 单版本策略下 list 与 version 同数据源同渲染，
        // list 只是 version 的语义别名（列表位 vs 查询位）
        RuntimeCommands::Version | RuntimeCommands::List => version(globals, client),
        RuntimeCommands::Install { timeout, cancel, yes } => {
            install(globals, client, *timeout, *cancel, *yes)
        }
    }
}

/// `chrome-host runtime version`（及 `runtime list`）。
///
/// table 模式 key: value，键名用 JSON 字段的 camelCase 原样 —— 「两模式无信息差」
/// （PRD §39）的字面执行：table 键即 json 键，零翻译。冻结五键：
/// pinnedVersion/version/installed/downloading/binaryPath。json 模式输出 CftStatus
/// 服务端原样（含 upgradeAvailable 全部字段）。quiet 对只读单对象无冻结语义，
/// 按缺省 kv 输出（output.rs 的既定取舍）。
fn version(globals: &GlobalArgs, client: &AgentClient) -> Result<ExitCode, CliError> {
    let out = OutputCtx::from(globals);
    let status: CftStatus = client.kernel_status()?;
    out.render_kv(
        &cft_kv_pairs(&status),
        serde_json::to_value(&status).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// runtime install（本组唯一含轮询等待逻辑的命令）
// ---------------------------------------------------------------------------

/// 轮询间隔 2s：150MB 下载是分钟级操作，2s 跟手且不打爆 status 接口。
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// `--timeout` 缺省 900s：覆盖 150MB 内核下载的常见时长上限；0 = 无限等待。
const DEFAULT_INSTALL_TIMEOUT_SECS: u64 = 900;

/// 轮询判定四分支（互斥穷尽）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollVerdict {
    /// installed=true：安装完成 → exit 0
    Installed,
    /// downloading true→false 转变沿且仍未安装：下载中止（失败或被取消，两可）→ exit 5
    Stalled,
    /// 超过 `--timeout` 截止仍未完成 → exit 9
    Timeout,
    /// 继续下一轮
    Continue,
}

/// 单轮判定（纯函数，真实轮询循环与下方测试回放共用 —— 决策逻辑单一事实源）。
///
/// 判定优先级 Installed > Stalled > Timeout：已装即成功（最后一轮恰好装完、
/// 截止同拍到达是正常时序，不得误报）；中止沿比超时更具体，两者同时命中时
/// 报告更准确的那个。
///
/// 转变沿（启发式判定）：服务端 CftStatus 无 error 字段（见 manager.rs），
/// 「上一拍 downloading=true、本拍 false 且仍未装」是失败的唯一可观测信号；
/// 但用户主动 --cancel 也产生同一信号，文案必须保留「失败或被取消」两可语义。
/// 首轮无上一拍（None）不构成沿：下载刚触发尚未置位属正常时序，不得误报。
fn poll_once(
    prev_downloading: Option<bool>,
    cur: &CftStatus,
    deadline_reached: bool,
) -> PollVerdict {
    if cur.installed {
        return PollVerdict::Installed;
    }
    if prev_downloading == Some(true) && !cur.downloading {
        return PollVerdict::Stalled;
    }
    if deadline_reached {
        return PollVerdict::Timeout;
    }
    PollVerdict::Continue
}

/// `chrome-host runtime install [--timeout SEC] [--cancel]`。
///
/// 流程：幂等短路 → （--cancel）确认后取消 → 触发下载 → 2s 轮询至终态。
/// stdout 纪律：轮询进度与终态提示一律走 stderr（output.rs 出口清单 ④），
/// stdout 只输出最终结果（CftStatus 或 cancel 的 {ok}）；--json 模式 stderr
/// 全程静默，机器消费方靠退出码 + stdout 的最终 CftStatus 判定。
fn install(
    globals: &GlobalArgs,
    client: &AgentClient,
    timeout_secs: u64,
    cancel: bool,
    sub_yes: bool,
) -> Result<ExitCode, CliError> {
    let out = OutputCtx::from(globals);

    // —— --cancel 分支：取消不存在的下载不是错误（服务端幂等返回
    // ok=false），仍 exit 0 —— 重复取消与「无下载时取消」都不该惊动调用方 ——
    if cancel {
        // 子命令级 --yes 与全局 --yes 等效，任一放行即跳过确认流
        if !(sub_yes || globals.yes) {
            confirm_or_yes(globals, "将中止进行中的内核下载，继续?")?;
        }
        let result = client.kernel_cancel()?;
        if out.json {
            // json 契约：stdout 输出服务端原样 {ok}，机器消费方自取成败；ok=false
            // 是正常业务结果而非进程错误，不进 print_error（那会渲染 error 形状）
            out.render_kv(
                &[("ok".into(), result.ok.to_string())],
                serde_json::to_value(&result).expect("纯数据 DTO 的序列化不会失败"),
            );
        } else if result.ok {
            out.render_text("已中止内核下载。".into());
        } else {
            out.render_text("当前无下载进行。".into());
        }
        return Ok(ExitCode::SUCCESS);
    }

    // —— 幂等短路：已装则渲染当前状态即返回，重复 install 零副作用 ——
    let status = client.kernel_status()?;
    if status.installed {
        out.render_kv(
            &cft_kv_pairs(&status),
            serde_json::to_value(&status).expect("纯数据 DTO 的序列化不会失败"),
        );
        return Ok(ExitCode::SUCCESS);
    }

    // —— 触发下载：ok=false = 服务端已在下载中（单飞槽 DOWNLOAD_SLOT，
    // 非错误），通常是 GUI 设置页先触发；CLI 提示后照常轮询，等的是同一个下载 ——
    let trigger = client.kernel_download()?;
    if !trigger.ok && !out.json {
        eprintln!("服务端已在下载中（可能由 GUI 触发），继续等待…"); // stderr 出口④
    }

    // —— 轮询循环：间隔 POLL_INTERVAL；timeout_secs=0 表示无限等待。
    // 截止时刻只算一次（起点 = 触发下载之后）：等待语义是「从现在起最多 N 秒」，
    // 不把此前短路/触发请求的耗时计入 ——
    let deadline = (timeout_secs > 0).then(|| Instant::now() + Duration::from_secs(timeout_secs));
    let started = Instant::now();
    let mut prev_downloading: Option<bool> = None;
    loop {
        let status = client.kernel_status()?;
        let deadline_reached = deadline.is_some_and(|d| Instant::now() >= d);
        match poll_once(prev_downloading, &status, deadline_reached) {
            PollVerdict::Installed => {
                // 成功终态：stdout 输出最终 CftStatus（kv/json 两模式无信息差）
                out.render_kv(
                    &cft_kv_pairs(&status),
                    serde_json::to_value(&status).expect("纯数据 DTO 的序列化不会失败"),
                );
                return Ok(ExitCode::SUCCESS);
            }
            PollVerdict::Stalled => {
                // 失败探测是启发式，人类文案保留「失败或被取消」两可语义；
                // json 模式在 stdout 给出最后快照供机器复核（人类可用 runtime version）
                if out.json {
                    out.render_kv(
                        &cft_kv_pairs(&status),
                        serde_json::to_value(&status).expect("纯数据 DTO 的序列化不会失败"),
                    );
                } else {
                    eprintln!("下载已中止且内核仍未安装（失败或被取消）。可用 `runtime version` 复核状态。"); // stderr 出口④
                }
                return Ok(ExitCode::RUNTIME);
            }
            PollVerdict::Timeout => {
                // ExitCode::TIMEOUT（exit 9）：服务端下载是后台
                // 任务，CLI 超时只是不再等待，下载仍在进行 —— 提示取消路径而非报错
                if out.json {
                    out.render_kv(
                        &cft_kv_pairs(&status),
                        serde_json::to_value(&status).expect("纯数据 DTO 的序列化不会失败"),
                    );
                } else {
                    eprintln!("等待内核安装超时（--timeout {timeout_secs}s），服务端下载仍在后台进行。可用 `runtime install --cancel` 中止服务端下载。"); // stderr 出口④
                }
                return Ok(ExitCode::TIMEOUT);
            }
            PollVerdict::Continue => {
                prev_downloading = Some(status.downloading);
                if !out.json {
                    // stderr 出口④：进度行走 stderr（每轮一行，选简单者），stdout 只留最终结果
                    eprintln!("下载中… {}s", started.elapsed().as_secs());
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }
}

/// CftStatus → kv 对。bool 输出 true/false；Option 空值显示 `-`
/// （仅 table 占位，json 模式原样保留 null —— 两模式无信息差）。
fn cft_kv_pairs(status: &CftStatus) -> Vec<(String, String)> {
    vec![
        ("pinnedVersion".into(), status.pinned_version.clone()),
        (
            "version".into(),
            status.version.clone().unwrap_or_else(|| "-".into()),
        ),
        ("installed".into(), status.installed.to_string()),
        ("downloading".into(), status.downloading.to_string()),
        (
            "binaryPath".into(),
            status.binary_path.clone().unwrap_or_else(|| "-".into()),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_status() -> CftStatus {
        CftStatus {
            installed: true,
            version: Some("131.0.6778.204".into()),
            pinned_version: "131.0.6778.204".into(),
            upgrade_available: false,
            downloading: false,
            binary_path: Some("/opt/cft/chrome".into()),
        }
    }

    #[test]
    fn cft_kv_uses_camel_case_keys_in_frozen_order() {
        let pairs = cft_kv_pairs(&sample_status());
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        // 冻结五键 + camelCase 原样：table 键与 json 字段零信息差
        assert_eq!(keys, ["pinnedVersion", "version", "installed", "downloading", "binaryPath"]);
    }

    #[test]
    fn cft_kv_option_none_renders_dash() {
        let mut status = sample_status();
        status.version = None;
        status.binary_path = None;
        let pairs = cft_kv_pairs(&status);
        assert_eq!(pairs[1], ("version".into(), "-".into()));
        assert_eq!(pairs[4], ("binaryPath".into(), "-".into()));
    }

    // -----------------------------------------------------------------------
    // 轮询状态机（poll_once + replay）：四分支 + true→false 转变沿
    // -----------------------------------------------------------------------

    /// 构造轮询快照：判定只消费 installed / downloading 两个字段，其余置默认。
    fn snap(installed: bool, downloading: bool) -> CftStatus {
        CftStatus {
            installed,
            version: None,
            pinned_version: "131.0.6778.204".into(),
            upgrade_available: false,
            downloading,
            binary_path: None,
        }
    }

    #[test]
    fn poll_once_installed_takes_precedence_over_deadline_and_edge() {
        // 已装即成功，优先于超时与中止沿：最后一轮恰好装完（截止同拍到达）是正常
        // 时序，绝不能因先到截止而误报失败 —— 成功判定必须最优先
        assert_eq!(
            poll_once(Some(true), &snap(true, false), true),
            PollVerdict::Installed
        );
    }

    #[test]
    fn poll_once_true_to_false_edge_is_stalled_even_at_deadline() {
        // downloading true→false 且未装 = 下载中止（失败或被取消两可）→ exit 5；
        // 与截止同拍命中时仍取 Stalled —— 更具体的诊断优先于笼统超时
        assert_eq!(
            poll_once(Some(true), &snap(false, false), false),
            PollVerdict::Stalled
        );
        assert_eq!(
            poll_once(Some(true), &snap(false, false), true),
            PollVerdict::Stalled
        );
    }

    #[test]
    fn poll_once_edge_requires_a_previous_true_snapshot() {
        // 沿检测必须以「上一拍 true」为前提：首轮（None）或持续未开始（Some(false)）
        // 都不构成中止 —— 下载刚触发尚未置位属正常时序，误报会打断正常安装
        let cur = snap(false, false);
        assert_eq!(poll_once(None, &cur, false), PollVerdict::Continue);
        assert_eq!(poll_once(Some(false), &cur, false), PollVerdict::Continue);
        // 不构成沿时只剩超时可判：截止到达 → exit 9 语义
        assert_eq!(poll_once(None, &cur, true), PollVerdict::Timeout);
    }

    #[test]
    fn poll_once_downloading_continues_until_deadline() {
        // 下载进行中（无论上一拍形态）：继续轮询
        let cur = snap(false, true);
        assert_eq!(poll_once(None, &cur, false), PollVerdict::Continue);
        assert_eq!(poll_once(Some(false), &cur, false), PollVerdict::Continue);
        assert_eq!(poll_once(Some(true), &cur, false), PollVerdict::Continue);
        // 截止先于完成 → exit 9 语义
        assert_eq!(poll_once(Some(true), &cur, true), PollVerdict::Timeout);
    }

    /// 测试脚手架：按真实循环的时序回放快照序列 —— 第 i 轮虚拟时刻 = i × interval，
    /// 每轮先判定再推进，不真睡眠；序列耗尽仍 Continue（对应真实循环「继续等下一轮」）。
    /// 与真实循环共用 [`poll_once`]，判定逻辑单一事实源。
    fn replay(snapshots: &[CftStatus], timeout_secs: u64, interval_secs: u64) -> PollVerdict {
        let mut prev: Option<bool> = None;
        for (round, cur) in snapshots.iter().enumerate() {
            let deadline_reached =
                timeout_secs > 0 && round as u64 * interval_secs >= timeout_secs;
            match poll_once(prev, cur, deadline_reached) {
                PollVerdict::Continue => prev = Some(cur.downloading),
                verdict => return verdict,
            }
        }
        PollVerdict::Continue
    }

    #[test]
    fn replay_returns_installed_on_final_snapshot() {
        // 正常安装时序：两轮下载中 → 第三轮装完，无论第几轮完成都报成功
        let snaps = [snap(false, true), snap(false, true), snap(true, false)];
        assert_eq!(replay(&snaps, 60, 2), PollVerdict::Installed);
    }

    #[test]
    fn replay_detects_stall_in_the_middle_of_sequence() {
        // 下载中 → 中止且未装：转变沿出现在序列中段（失败/被取消场景的最小时序）
        let snaps = [snap(false, true), snap(false, false)];
        assert_eq!(replay(&snaps, 60, 2), PollVerdict::Stalled);
    }

    #[test]
    fn replay_timeout_only_after_deadline_accumulates() {
        let snaps = [snap(false, true), snap(false, true), snap(false, true)];
        // 3 轮虚拟时刻 0/2/4s 均未到 5s 截止：序列耗尽仍 Continue（真实循环会继续等）
        assert_eq!(replay(&snaps, 5, 2), PollVerdict::Continue);
        // 截止 4s：第 3 轮（4s >= 4s）命中 → Timeout
        assert_eq!(replay(&snaps, 4, 2), PollVerdict::Timeout);
    }

    #[test]
    fn replay_with_zero_timeout_never_times_out() {
        // --timeout 0 = 无限等待：永不产生 Timeout，只剩成功/中止/继续
        let snaps = [snap(false, true), snap(false, true)];
        assert_eq!(replay(&snaps, 0, 2), PollVerdict::Continue);
    }
}
