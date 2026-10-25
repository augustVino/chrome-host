//! CliError → ExitCode 的唯一映射点（PRD cli.md §18 × 服务端现有错误码，计划 §2.4 全表）。
//!
//! 规矩：服务端新增错误码时必须同步本文件的映射规则，并在
//! `tests::exit_code_mapping_table` 中补一行断言（与 client.rs 模块注释的同步义务对应）。

use crate::error::CliError;

/// 进程退出码（PRD cli.md §18 冻结的 0–9 契约值，改值即破坏对外契约）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitCode(pub u8);

impl ExitCode {
    /// 一切正常。
    pub const SUCCESS: ExitCode = ExitCode(0);
    /// 未分类错误：协议破坏、未知 5xx、doctor 存在失败检查项。
    pub const GENERAL: ExitCode = ExitCode(1);
    /// 参数错误：clap 解析失败、非 TTY 危险操作缺 --yes、本地校验不通过。
    pub const INVALID_ARGUMENT: ExitCode = ExitCode(2);
    /// 目标资源不存在（*_NOT_FOUND）。
    pub const NOT_FOUND: ExitCode = ExitCode(3);
    /// 资源已存在（预留：服务端当前无此码，映射规则先行）。
    pub const ALREADY_EXISTS: ExitCode = ExitCode(4);
    /// 运行时状态冲突 / 运行时错误（409 冲突族、500 运行时错误码族）。
    pub const RUNTIME: ExitCode = ExitCode(5);
    /// 权限拒绝（如内置扩展锁定 EXTENSION_SYSTEM_LOCKED）。
    pub const PERMISSION: ExitCode = ExitCode(6);
    /// 请求校验失败（400 族）。
    pub const VALIDATION: ExitCode = ExitCode(7);
    /// 服务不可达：Agent API 连接拒绝 / 超时（桌面应用未运行）。
    pub const UNAVAILABLE: ExitCode = ExitCode(8);
    /// CLI 侧等待超时 —— runtime install 轮询超过 `--timeout` 上限即显式返回
    /// 本码（命令层 ExitCode，不经 CliError —— Local 会误映射 2，与 doctor
    /// 同路径）。契约值 0–9 冻结（下方测试逐值断言）。
    pub const TIMEOUT: ExitCode = ExitCode(9);
    /// 未授权（401 UNAUTHORIZED）：远程接入令牌缺失/错误。**隧道与应用均在线**，
    /// 仅凭证问题——与 exit 8（不可达）形成三态诊断。0–9 冻结契约的**追加**项，
    /// 不改变既有语义（v2 远程接入方案 §8）。
    pub const UNAUTHORIZED: ExitCode = ExitCode(10);

    pub fn as_u8(self) -> u8 {
        self.0
    }
}

/// 5xx 中按「运行时错误」归类（→ 5）的服务端错误码表。
/// 其余 5xx（含 INTERNAL_ERROR 与未识别码）落 GENERAL（→ 1）兜底。
const RUNTIME_ERROR_CODES: &[&str] = &[
    "KERNEL_NOT_READY",
    "INSTANCE_START_FAILED",
    "CDP_PORT_UNAVAILABLE",
    "CDP_CONNECTION_FAILED",
    "HOSTS_FETCH_FAILED",
    // page 控制族（P2-B/P2-C）：Input 派发失败 / 工具预算超时 / 页内 eval 失败 /
    // CDP 发送失败——均为已知运行时故障，不落入 GENERAL 兜底；导航失败
    //（Page.navigate errorText，net::ERR_*）同为运行时故障
    "PAGE_DISPATCH_FAILED",
    "PAGE_TOOL_TIMEOUT",
    "PAGE_EVAL_FAILED",
    "PAGE_NAV_FAILED",
    "CDP_COMMAND_FAILED",
];

/// 预留的「资源已存在」错误码后缀：命中即 → 4，优先级高于 status 分支
/// （当前服务端无此码；规则先行，服务端加入后 CLI 无需改映射）。
const ALREADY_EXISTS_SUFFIX: &str = "_ALREADY_EXISTS";

/// CliError → ExitCode 唯一入口。映射规则见计划 §2.4：
/// 未知 4xx → 7、未知 5xx → 1 兜底，保证服务端加码时 CLI 不崩、语义不错得离谱。
pub fn from_cli_error(e: &CliError) -> ExitCode {
    match e {
        CliError::Unreachable(_) => ExitCode::UNAVAILABLE,
        CliError::Protocol(_) => ExitCode::GENERAL,
        CliError::Local(_) => ExitCode::INVALID_ARGUMENT,
        CliError::Api { status, code, .. } => from_api(*status, code),
    }
}

/// (HTTP status, server code) → ExitCode。
/// 先查精确码表，再按 status 分支，最后落兜底 —— 顺序即优先级。
fn from_api(status: u16, code: &str) -> ExitCode {
    if (400..500).contains(&status) {
        // 预留规则优先于 status 分支：409 + *_ALREADY_EXISTS 也应 → 4 而非 5
        if code.ends_with(ALREADY_EXISTS_SUFFIX) {
            return ExitCode::ALREADY_EXISTS;
        }
        return match status {
            401 => ExitCode::UNAUTHORIZED,
            403 => ExitCode::PERMISSION,
            404 => ExitCode::NOT_FOUND,
            409 => ExitCode::RUNTIME,
            // 400 与其余未知 4xx：一律按校验错误兜底
            _ => ExitCode::VALIDATION,
        };
    }
    if (500..600).contains(&status) {
        if RUNTIME_ERROR_CODES.contains(&code) {
            return ExitCode::RUNTIME;
        }
        // INTERNAL_ERROR 与未识别 5xx：兜底 General
        return ExitCode::GENERAL;
    }
    // 4xx/5xx 之外的状态（理论不应出现）：按未分类错误兜底
    ExitCode::GENERAL
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 Api 错误样本的辅助函数。
    fn api(status: u16, code: &str) -> CliError {
        CliError::Api {
            status,
            code: code.to_string(),
            message: "msg".to_string(),
        }
    }

    /// 表驱动全分支覆盖：每个 (场景名, 错误样本, 期望退出码) 一行。
    /// 服务端新增错误码时必须在此补行（同步义务见模块注释）。
    #[test]
    fn exit_code_mapping_table() {
        let cases: &[(&str, CliError, ExitCode)] = &[
            // —— 传输层不可达 → 8 ——
            (
                "连接拒绝",
                CliError::Unreachable("connection refused".into()),
                ExitCode::UNAVAILABLE,
            ),
            (
                "connect timeout",
                CliError::Unreachable("operation timed out".into()),
                ExitCode::UNAVAILABLE,
            ),
            // —— 协议破坏 → 1 ——
            (
                "响应非合法 JSON",
                CliError::Protocol("not json".into()),
                ExitCode::GENERAL,
            ),
            // —— 本地失败 → 2 ——
            (
                "本地校验失败（缺 --yes）",
                CliError::Local("非交互环境需要 --yes".into()),
                ExitCode::INVALID_ARGUMENT,
            ),
            // —— Api 400：校验错 → 7 ——
            (
                "400 INVALID_REQUEST",
                api(400, "INVALID_REQUEST"),
                ExitCode::VALIDATION,
            ),
            (
                "400 HOSTS_SOURCE_INVALID",
                api(400, "HOSTS_SOURCE_INVALID"),
                ExitCode::VALIDATION,
            ),
            // —— 未知 4xx 兜底 → 7 ——
            (
                "418 未知 4xx 兜底",
                api(418, "SOME_FUTURE_CODE"),
                ExitCode::VALIDATION,
            ),
            // —— Api 401：未授权 → 10（远程接入令牌缺失/错误；应用与隧道均在线）——
            (
                "401 UNAUTHORIZED",
                api(401, "UNAUTHORIZED"),
                ExitCode::UNAUTHORIZED,
            ),
            // —— Api 403 → 6 ——
            (
                "403 EXTENSION_SYSTEM_LOCKED",
                api(403, "EXTENSION_SYSTEM_LOCKED"),
                ExitCode::PERMISSION,
            ),
            // —— Api 404 → 3（*_NOT_FOUND 全家族，按 status 映射）——
            (
                "404 ENVIRONMENT_NOT_FOUND",
                api(404, "ENVIRONMENT_NOT_FOUND"),
                ExitCode::NOT_FOUND,
            ),
            (
                "404 INSTANCE_NOT_FOUND",
                api(404, "INSTANCE_NOT_FOUND"),
                ExitCode::NOT_FOUND,
            ),
            (
                "404 EXTENSION_NOT_FOUND",
                api(404, "EXTENSION_NOT_FOUND"),
                ExitCode::NOT_FOUND,
            ),
            // —— Api 409：运行时状态冲突 → 5 ——
            (
                "409 ENVIRONMENT_HAS_RUNNING_INSTANCES",
                api(409, "ENVIRONMENT_HAS_RUNNING_INSTANCES"),
                ExitCode::RUNTIME,
            ),
            (
                "409 INSTANCE_ALREADY_RUNNING",
                api(409, "INSTANCE_ALREADY_RUNNING"),
                ExitCode::RUNTIME,
            ),
            (
                "409 INSTANCE_NOT_RUNNING",
                api(409, "INSTANCE_NOT_RUNNING"),
                ExitCode::RUNTIME,
            ),
            (
                "409 PROFILE_IN_USE",
                api(409, "PROFILE_IN_USE"),
                ExitCode::RUNTIME,
            ),
            // —— Api 500：运行时错误码族 → 5 ——
            (
                "500 KERNEL_NOT_READY",
                api(500, "KERNEL_NOT_READY"),
                ExitCode::RUNTIME,
            ),
            (
                "500 INSTANCE_START_FAILED",
                api(500, "INSTANCE_START_FAILED"),
                ExitCode::RUNTIME,
            ),
            (
                "500 CDP_PORT_UNAVAILABLE",
                api(500, "CDP_PORT_UNAVAILABLE"),
                ExitCode::RUNTIME,
            ),
            (
                "500 CDP_CONNECTION_FAILED",
                api(500, "CDP_CONNECTION_FAILED"),
                ExitCode::RUNTIME,
            ),
            (
                "500 HOSTS_FETCH_FAILED",
                api(500, "HOSTS_FETCH_FAILED"),
                ExitCode::RUNTIME,
            ),
            // —— page 控制族（P2-C）：按 status 归类（404→3 / 409→5 / 400·422→7）——
            (
                "404 PAGE_TAB_NOT_FOUND（tab 路由未命中）",
                api(404, "PAGE_TAB_NOT_FOUND"),
                ExitCode::NOT_FOUND,
            ),
            (
                "404 PAGE_REF_NOT_FOUND（引用不在上次 snap 表中）",
                api(404, "PAGE_REF_NOT_FOUND"),
                ExitCode::NOT_FOUND,
            ),
            (
                "404 PAGE_ELEMENT_NOT_FOUND（选择器未命中）",
                api(404, "PAGE_ELEMENT_NOT_FOUND"),
                ExitCode::NOT_FOUND,
            ),
            (
                "409 PAGE_REF_STALE（导航后引用失效，重 snap）",
                api(409, "PAGE_REF_STALE"),
                ExitCode::RUNTIME,
            ),
            (
                "409 PAGE_TAB_AMBIGUOUS（多 tab 且未指定）",
                api(409, "PAGE_TAB_AMBIGUOUS"),
                ExitCode::RUNTIME,
            ),
            (
                "409 PAGE_ELEMENT_BLOCKED（点击点被遮挡）",
                api(409, "PAGE_ELEMENT_BLOCKED"),
                ExitCode::RUNTIME,
            ),
            (
                "409 PAGE_FILL_VALUE_MISMATCH（回读不一致，先观察再重试）",
                api(409, "PAGE_FILL_VALUE_MISMATCH"),
                ExitCode::RUNTIME,
            ),
            (
                "409 PAGE_WAIT_TIMEOUT（等待超时）",
                api(409, "PAGE_WAIT_TIMEOUT"),
                ExitCode::RUNTIME,
            ),
            (
                "409 PAGE_NAV_NO_HISTORY（back/forward 已在历史边界）",
                api(409, "PAGE_NAV_NO_HISTORY"),
                ExitCode::RUNTIME,
            ),
            (
                "409 PAGE_SERVICE_UNAVAILABLE（实例未运行/无 CDP 端口）",
                api(409, "PAGE_SERVICE_UNAVAILABLE"),
                ExitCode::RUNTIME,
            ),
            (
                "400 PAGE_KEY_UNKNOWN（键名不识别）",
                api(400, "PAGE_KEY_UNKNOWN"),
                ExitCode::VALIDATION,
            ),
            (
                "422 PAGE_ELEMENT_NOT_VISIBLE（元素不可见）",
                api(422, "PAGE_ELEMENT_NOT_VISIBLE"),
                ExitCode::VALIDATION,
            ),
            (
                "422 PAGE_TARGET_NOT_FILLABLE（目标不可编辑）",
                api(422, "PAGE_TARGET_NOT_FILLABLE"),
                ExitCode::VALIDATION,
            ),
            // —— page 控制族：5xx 运行时码表（非 GENERAL 兑底）——
            (
                "500 PAGE_DISPATCH_FAILED（Input 派发失败，附 effect_state）",
                api(500, "PAGE_DISPATCH_FAILED"),
                ExitCode::RUNTIME,
            ),
            (
                "500 PAGE_NAV_FAILED（Page.navigate errorText，net::ERR_*）",
                api(500, "PAGE_NAV_FAILED"),
                ExitCode::RUNTIME,
            ),
            (
                "504 PAGE_TOOL_TIMEOUT（工具预算超时）",
                api(504, "PAGE_TOOL_TIMEOUT"),
                ExitCode::RUNTIME,
            ),
            // —— Api 500：INTERNAL_ERROR / 未知 5xx 兜底 → 1 ——
            (
                "500 INTERNAL_ERROR",
                api(500, "INTERNAL_ERROR"),
                ExitCode::GENERAL,
            ),
            (
                "503 未知 5xx 兜底",
                api(503, "SOME_FUTURE_CODE"),
                ExitCode::GENERAL,
            ),
            // —— 预留：*_ALREADY_EXISTS → 4（后缀规则优先于 status 分支）——
            (
                "409 PROFILE_ALREADY_EXISTS（预留）",
                api(409, "PROFILE_ALREADY_EXISTS"),
                ExitCode::ALREADY_EXISTS,
            ),
            // —— 状态码越出 4xx/5xx 范围 → 1 兜底 ——
            (
                "302 非预期状态兜底",
                api(302, "REDIRECT"),
                ExitCode::GENERAL,
            ),
        ];

        for (name, err, expected) in cases {
            assert_eq!(from_cli_error(err), *expected, "case: {name}, err: {err:?}");
        }
    }

    /// 0–9 契约值冻结（PRD cli.md §18）：防止无意改动破坏对外承诺。
    #[test]
    fn exit_code_values_match_prd() {
        assert_eq!(ExitCode::SUCCESS.as_u8(), 0);
        assert_eq!(ExitCode::GENERAL.as_u8(), 1);
        assert_eq!(ExitCode::INVALID_ARGUMENT.as_u8(), 2);
        assert_eq!(ExitCode::NOT_FOUND.as_u8(), 3);
        assert_eq!(ExitCode::ALREADY_EXISTS.as_u8(), 4);
        assert_eq!(ExitCode::RUNTIME.as_u8(), 5);
        assert_eq!(ExitCode::PERMISSION.as_u8(), 6);
        assert_eq!(ExitCode::VALIDATION.as_u8(), 7);
        assert_eq!(ExitCode::UNAVAILABLE.as_u8(), 8);
        assert_eq!(ExitCode::TIMEOUT.as_u8(), 9);
    }
}
