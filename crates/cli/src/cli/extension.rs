//! extension 命令组 —— 扩展注册与生命周期（list/get/add/enable/disable/remove）。
//!
//! 固定 handler 结构与 cli/env.rs 一致：构造 [`OutputCtx`] → 调 [`AgentClient`] → 渲染，
//! 三步之外无他物；HTTP 细节止于 client.rs（铁律见其模块注释）。本组特有的本地编排：
//! - `add` 的目录/manifest 客户端校验 + 路径绝对化：最高频输入错误本地拦截，请求绝不发出；
//! - enable/disable/remove 收到 403 `EXTENSION_SYSTEM_LOCKED` 时把服务端语义翻译成
//!   终端用户可读文案（机器码仍透传 --json，exit 6 映射不受影响，见
//!   [`system_locked_translated`]）。

use clap::Subcommand;

use super::confirm_or_yes;
use super::output::{OutputCtx, TableRow};
use super::GlobalArgs;
use crate::client::AgentClient;
use crate::error::CliError;
use crate::model::Extension;

/// extension 子命令树。
#[derive(Debug, Subcommand)]
pub enum ExtensionCommands {
    /// 列出全部扩展（system 内置 + user 注册）
    #[command(about = "列出全部扩展（system 内置 + user 注册）")]
    List,

    /// 查看扩展详情
    #[command(about = "查看扩展详情")]
    Get {
        /// 扩展 id
        id: String,
    },

    /// 注册本地扩展目录（须含 manifest.json；只存路径引用，不复制文件）
    #[command(about = "注册本地扩展目录（须含 manifest.json；只存路径引用，不复制文件）")]
    Add {
        /// 扩展源目录路径
        path: String,
    },

    /// 启用扩展（仅 user 扩展；system 内置扩展锁定）
    #[command(about = "启用扩展（仅 user 扩展；system 内置扩展锁定）")]
    Enable {
        /// 扩展 id
        id: String,
    },

    /// 停用扩展（仅 user 扩展；system 内置扩展锁定）
    #[command(about = "停用扩展（仅 user 扩展；system 内置扩展锁定）")]
    Disable {
        /// 扩展 id
        id: String,
    },

    /// 移除扩展注册项（不删除源目录文件）
    #[command(about = "移除扩展注册项（不删除源目录文件）")]
    Remove {
        /// 扩展 id
        id: String,
        /// 跳过确认（非交互环境必需）
        #[arg(long)]
        yes: bool,
    },
}

/// extension 组分发入口（main.rs dispatch 的唯一调用点）。
pub fn run(
    cmd: &ExtensionCommands,
    globals: &GlobalArgs,
    client: &AgentClient,
) -> Result<(), CliError> {
    match cmd {
        ExtensionCommands::List => list(globals, client),
        ExtensionCommands::Get { id } => get(globals, client, id),
        ExtensionCommands::Add { path } => add(globals, client, path),
        ExtensionCommands::Enable { id } => set_enabled(globals, client, id, true),
        ExtensionCommands::Disable { id } => set_enabled(globals, client, id, false),
        ExtensionCommands::Remove { id, yes } => remove(globals, client, id, *yes),
    }
}

/// `chrome-host extension list`。table 列集冻结：NAME VERSION TYPE STATUS。
/// type/status 经 serde 序列化取小写原样（与 instance.rs status_label 同源策略，服务端
/// 加变体时零改动）；version 空值显示 `-`（description 无冻结 table 列，可空字段占位
/// 规则只作用于有列的字段）；json 模式输出 Extension 数组原样；quiet 每行一个 id。
fn list(globals: &GlobalArgs, client: &AgentClient) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let extensions: Vec<Extension> = client.extension_list()?;
    let rows: Vec<TableRow> = extensions
        .iter()
        .map(|ext| TableRow {
            id: ext.id.clone(),
            cells: vec![
                ext.name.clone(),
                option_cell(ext.version.clone()),
                enum_label(&ext.extension_type),
                enum_label(&ext.status),
            ],
            // json 字段 = 该行完整 JSON：DTO serde 属性与服务端一致，序列化是恒等往返
            json: serde_json::to_value(ext).expect("纯数据 DTO 的序列化不会失败"),
        })
        .collect();
    out.render_list(&["NAME", "VERSION", "TYPE", "STATUS"], &rows);
    Ok(())
}

/// `chrome-host extension get <id>`：单对象渲染 —— json 原样；table 模式 key: value。
/// quiet 对单对象无冻结语义（同 env/instance get），按缺省 kv 输出。
fn get(globals: &GlobalArgs, client: &AgentClient, id: &str) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    let ext = client.extension_get(id)?;
    out.render_kv(
        &extension_kv_pairs(&ext),
        serde_json::to_value(&ext).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// 单对象 key: value 对，键名与 JSON 字段一致（PRD §39 两模式无信息差）。
/// 冻结字段集：id/name/type/status/version/manifestVersion/
/// sourcePath/enabled/createdAt/updatedAt。description 不进 kv（同 instance get
/// 省略 hostRules 等字段的先例：kv 是面向人的精选子集，json 模式始终携带全集）。
/// 时间戳输出原始 epoch 毫秒（同 env/instance：不为本地格式化加 chrono 依赖）。
fn extension_kv_pairs(ext: &Extension) -> Vec<(String, String)> {
    vec![
        ("id".into(), ext.id.clone()),
        ("name".into(), ext.name.clone()),
        ("type".into(), enum_label(&ext.extension_type)),
        ("status".into(), enum_label(&ext.status)),
        ("version".into(), option_cell(ext.version.clone())),
        ("manifestVersion".into(), option_cell(ext.manifest_version)),
        ("sourcePath".into(), ext.source_path.clone()),
        ("enabled".into(), ext.enabled.to_string()),
        ("createdAt".into(), ext.created_at.to_string()),
        ("updatedAt".into(), ext.updated_at.to_string()),
    ]
}

/// `chrome-host extension add <path>`：注册本地扩展目录。
///
/// 客户端先校验（目录存在 + 含 manifest.json），失败 → `CliError::Local`，请求绝不
/// 发出。why 不交给服务端：路径写错 / 漏 manifest 是本命令最高频输入错误，本地拦截
/// 省一次注定失败的网络往返，且错误反馈不依赖桌面应用是否运行。
///
/// 校验通过后把路径规范化为绝对路径再发送：服务端按收到的字符串原样落库并直引该目录
/// （`extension_service.rs` register 的 `source_path: path.to_string()`，相对路径会以
/// GUI 进程 cwd 解析）——CLI 的 cwd 与 GUI 不同，相对路径会在服务端读不到。canonicalize
/// 失败（理论上目录刚校验过存在，不应发生）则回退原路径，不因防御逻辑阻断主流程。
fn add(globals: &GlobalArgs, client: &AgentClient, path: &str) -> Result<(), CliError> {
    let dir = std::path::Path::new(path);
    if !dir.is_dir() {
        return Err(CliError::Local(format!("扩展目录不存在: {path}")));
    }
    if !dir.join("manifest.json").is_file() {
        return Err(CliError::Local(format!(
            "扩展目录缺少 manifest.json: {path}（Chrome 扩展清单是注册前置条件）"
        )));
    }
    let absolute = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());

    let out = OutputCtx::from(globals);
    let ext = client.extension_register(absolute.to_string_lossy().as_ref())?;
    if out.quiet {
        // 注册属变更类命令，quiet 输出新注册扩展 id
        out.print_id(&ext.id);
        return Ok(());
    }
    out.render_kv(
        &extension_kv_pairs(&ext),
        serde_json::to_value(&ext).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// `chrome-host extension enable/disable <id>`：PATCH `{enabled}`（服务端语义：配置
/// 只影响之后启动的实例）。403 `EXTENSION_SYSTEM_LOCKED` 翻译语义后上抛。
fn set_enabled(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    enabled: bool,
) -> Result<(), CliError> {
    let ext = client
        .extension_set_enabled(id, enabled)
        .map_err(|err| system_locked_translated(err, "启停"))?;
    render_mutated(globals, ext)
}

/// 变更类命令（enable/disable）共用渲染：服务端返回操作后的 Extension 快照 ——
/// quiet 仅输出扩展 id（供 `$(...)` 捕获）；table 模式输出操作后快照 kv。
fn render_mutated(globals: &GlobalArgs, ext: Extension) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    if out.quiet {
        out.print_id(&ext.id);
        return Ok(());
    }
    out.render_kv(
        &extension_kv_pairs(&ext),
        serde_json::to_value(&ext).expect("纯数据 DTO 的序列化不会失败"),
    );
    Ok(())
}

/// `chrome-host extension remove <id> [--yes]`：确认流 → DELETE。
/// 移除仅删注册项、不删源目录文件（确认文案明示，避免用户误判数据被清）；
/// 403 `EXTENSION_SYSTEM_LOCKED` 翻译语义后上抛。
fn remove(
    globals: &GlobalArgs,
    client: &AgentClient,
    id: &str,
    sub_yes: bool,
) -> Result<(), CliError> {
    let out = OutputCtx::from(globals);
    // 子命令级 --yes 与全局 --yes 等效：任一放行即跳过确认流
    let yes = sub_yes || globals.yes;
    // 确认在请求之前（同 env/instance delete 的顺序决策：授权先于请求发出）
    if !yes {
        confirm_or_yes(globals, "移除扩展注册项（不删除源目录文件），继续?")?;
    }
    client
        .extension_delete(id)
        .map_err(|err| system_locked_translated(err, "移除"))?;
    if out.quiet {
        out.print_id(id);
    }
    Ok(())
}

/// 403 `EXTENSION_SYSTEM_LOCKED` 的语义翻译：服务端裸 message（「系统扩展不可禁用」）
/// 不解释「为什么被拒 / 什么能做」；这里把产品语义翻译给终端用户。**机器码原样保留**：
/// 仍构造 `CliError::Api`，--json 模式错误体携带 `EXTENSION_SYSTEM_LOCKED`，
/// exit.rs 的 (403, EXTENSION_SYSTEM_LOCKED) → 6 映射不受影响。其余错误原样上抛
/// （404 not found、网络不可达等各有自己的 exit 语义，不越权改写）。
fn system_locked_translated(err: CliError, verb: &str) -> CliError {
    match err {
        CliError::Api {
            status: 403,
            code,
            message,
        } if code == "EXTENSION_SYSTEM_LOCKED" => CliError::Api {
            status: 403,
            code,
            message: format!("内置扩展锁定，仅用户注册的扩展可{verb}（服务端: {message}）"),
        },
        other => other,
    }
}

/// 枚举 → 线上小写字符串（type/status「小写原样」）。经 serde 序列化取值：与 DTO 的
/// `#[serde(rename_all = "lowercase")]` 同一来源，服务端加变体时本函数零改动不漂移；
/// `Unknown` 兜底变体同样序列化为 "unknown"。
fn enum_label<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

/// Option 字段的 table 占位：None → `-`（仅 table 模式；json 原样保留 null，两模式无信息差）。
fn option_cell<T: ToString>(value: Option<T>) -> String {
    value.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExtensionStatus, ExtensionType};

    fn fixture(status: ExtensionStatus, ext_type: ExtensionType) -> Extension {
        Extension {
            id: "ext_1".into(),
            name: "测试扩展".into(),
            description: None,
            version: None,
            manifest_version: None,
            extension_type: ext_type,
            source_path: "/tmp/ext".into(),
            enabled: true,
            status,
            created_at: 1,
            updated_at: 2,
        }
    }

    #[test]
    fn kv_pairs_frozen_fields_lowercase_and_dash_placeholders() {
        let pairs = extension_kv_pairs(&fixture(ExtensionStatus::Ready, ExtensionType::User));
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "id",
                "name",
                "type",
                "status",
                "version",
                "manifestVersion",
                "sourcePath",
                "enabled",
                "createdAt",
                "updatedAt"
            ],
            "kv 字段集冻结，顺序即输出顺序"
        );
        let values: Vec<&str> = pairs.iter().map(|(_, v)| v.as_str()).collect();
        assert!(values.contains(&"user"), "type 小写原样: {values:?}");
        assert!(values.contains(&"ready"), "status 小写原样: {values:?}");
        // version/manifestVersion 为 null → table 占位 `-`（json 模式不受影响）
        assert_eq!(values.iter().filter(|v| **v == "-").count(), 2);
    }

    #[test]
    fn unknown_variants_render_lowercase_unknown() {
        let pairs = extension_kv_pairs(&fixture(ExtensionStatus::Unknown, ExtensionType::Unknown));
        assert!(pairs.iter().any(|(k, v)| k == "type" && v == "unknown"));
        assert!(pairs.iter().any(|(k, v)| k == "status" && v == "unknown"));
    }

    #[test]
    fn system_locked_translated_keeps_machine_code_and_status() {
        let err = system_locked_translated(
            CliError::Api {
                status: 403,
                code: "EXTENSION_SYSTEM_LOCKED".into(),
                message: "系统扩展不可禁用".into(),
            },
            "启停",
        );
        match err {
            // Api 变体保留：--json 透传机器码、exit 6 映射不变
            CliError::Api {
                status,
                code,
                message,
            } => {
                assert_eq!(status, 403);
                assert_eq!(code, "EXTENSION_SYSTEM_LOCKED");
                assert!(message.contains("内置扩展锁定"), "人话文案: {message}");
                assert!(message.contains("仅用户注册的扩展可启停"), "能力边界: {message}");
            }
            other => panic!("应保持 Api 变体: {other:?}"),
        }
    }

    #[test]
    fn other_errors_pass_through_untouched() {
        // 403 但不同 code（未来新码）：不越权翻译
        let err = system_locked_translated(
            CliError::Api {
                status: 403,
                code: "SOME_FUTURE_CODE".into(),
                message: "m".into(),
            },
            "启停",
        );
        assert!(matches!(
            &err,
            CliError::Api { code, message, .. } if code == "SOME_FUTURE_CODE" && message == "m"
        ));
        // 404 not found：原样（exit 3 语义不动）
        let err = system_locked_translated(
            CliError::Api {
                status: 404,
                code: "EXTENSION_NOT_FOUND".into(),
                message: "m".into(),
            },
            "移除",
        );
        assert!(matches!(err, CliError::Api { status: 404, .. }));
    }
}
