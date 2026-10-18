//! net-rules 扩展的配置编译：用户形状（block/redirect/headers）→ Chrome DNR 规则数组。
//! 纯函数无 IO，全部可单测；编译发生在两个入口：
//! - `ExtensionService::set_config` 写入前校验（net-rules 配置立即反馈编译错误）
//! - `resolve_for_runtime` 物化时编译（config.js 内容为 `{"rules":[...]}`，background.js 哑应用）
//!
//! 选 declarativeNetRequest 而非 webRequest：规则在浏览器层生效，MV3 service worker
//! 休眠不影响拦截；规则经 updateDynamicRules 注册后跨页面会话持久于 profile。
//!
//! glob 语义（对外契约，AGENT-API.md 同步记载）：
//! - block/headers 的 url：可选 scheme 前缀（剥离后按域锚定）；`*.` 前缀或裸域名
//!   → `||域名`（域及全部子域）；可带路径，路径内 `*` 为通配
//! - redirect 的 from：全量 glob，每个 `*` 依序成为捕获组（`$1`、`$2`…），
//!   映射为 regexFilter + regexSubstitution；to 中的 `$N` 展开为第 N 个捕获组
//!   （实测 CfT 131：DNR 替换语法为 RE2 反引用 `\1` 风格，`$1` 会按字面量输出，
//!   故编译期把用户契约的 `$N` 翻译为 `\N`）

use serde_json::{Value, json};

use crate::error::AppError;

/// DNR condition 必填的请求类型全集（Chrome 长期稳定的 13 类；
/// webtransport/webbundle 未列入——DNR ResourceType 支持集随版本演进，宁缺勿错）
const RESOURCE_TYPES: [&str; 13] = [
    "csp_report",
    "font",
    "image",
    "main_frame",
    "media",
    "object",
    "other",
    "ping",
    "script",
    "stylesheet",
    "sub_frame",
    "websocket",
    "xmlhttprequest",
];

fn invalid(message: impl Into<String>) -> AppError {
    AppError::business(400, "EXTENSION_CONFIG_INVALID", message)
}

/// RE2 元字符转义（regexFilter 需要正则形态；RE2 与 ECMAScript 元字符集在此子集上一致）
fn escape_re(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// glob → 锚定正则：`*` 依序替换为捕获组，整体 `^...$` 锚定。
/// `https://api.foo.com/*` → `^https://api\.foo\.com/(.*)$`
fn glob_to_anchored_regex(glob: &str) -> String {
    let mut out = String::from("^");
    for (i, part) in glob.split('*').enumerate() {
        if i > 0 {
            out.push_str("(.*)");
        }
        out.push_str(&escape_re(part));
    }
    out.push('$');
    out
}

/// url glob → DNR urlFilter：
/// 剥离 scheme → `*.` 前缀收敛为 `||`（域及子域）→ 裸域名/带路径一律 `||` 前缀锚定。
/// `*.google-analytics.com` / `google-analytics.com` → `||google-analytics.com`
fn to_url_filter(pattern: &str) -> Result<String, AppError> {
    let p = pattern.trim();
    if p.is_empty() {
        return Err(invalid(format!("URL 匹配模式不能为空: {pattern:?}")));
    }
    let p = p.strip_prefix("https://").or_else(|| p.strip_prefix("http://")).unwrap_or(p);
    if let Some(rest) = p.strip_prefix("*.") {
        return Ok(format!("||{rest}"));
    }
    if p.starts_with("||") {
        return Ok(p.to_string());
    }
    Ok(format!("||{p}"))
}

/// `$N`（用户契约，JS 惯用）→ DNR regexSubstitution 的 RE2 反引用 `\N`。
/// `$` 后非数字则按字面量 `$` 输出（无转义语法——替换串语义保持直观）
fn to_re2_substitution(to: &str) -> String {
    let chars: Vec<char> = to.chars().collect();
    let mut out = String::with_capacity(to.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                out.push('\\');
                out.extend(&chars[i + 1..j]);
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn resource_types() -> Value {
    json!(RESOURCE_TYPES)
}

/// 编译配置为 DNR 规则数组（updateDynamicRules 的 addRules 形态，id 从 1 连续分配）。
///
/// 输入形状（未知字段忽略）：
/// ```json
/// {"block": ["*.google-analytics.com"],
///  "redirect": [{"from": "https://api.foo.com/*", "to": "http://localhost:3000/$1"}],
///  "headers": [{"url": "https://app.foo.com/*", "set": {"X-Debug-Uid": "42"}}]}
/// ```
/// headers 走 requestHeaders（服务端可见）；单条非法 → 整体 Err（物化路径告警跳过
/// 扩展、写入路径 400 拒存，宁可不拦截也不带病生效）
pub fn compile(config: &Value) -> Result<Value, AppError> {
    let obj = match config.as_object() {
        Some(o) => o,
        None => {
            return Err(invalid("net-rules 配置必须是 JSON 对象"));
        }
    };

    let mut rules = Vec::new();
    let mut id = 0u32;

    if let Some(blocks) = obj.get("block") {
        let blocks = blocks.as_array().ok_or_else(|| invalid("block 必须是字符串数组"))?;
        for pattern in blocks {
            let pattern = pattern
                .as_str()
                .ok_or_else(|| invalid("block 条目必须是字符串"))?;
            id += 1;
            rules.push(json!({
                "id": id,
                "priority": 1,
                "action": { "type": "block" },
                "condition": {
                    "urlFilter": to_url_filter(pattern)?,
                    "resourceTypes": resource_types(),
                }
            }));
        }
    }

    if let Some(redirects) = obj.get("redirect") {
        let redirects =
            redirects.as_array().ok_or_else(|| invalid("redirect 必须是对象数组"))?;
        for r in redirects {
            let from = r
                .get("from")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| invalid("redirect 条目缺少非空 from"))?;
            let to = r
                .get("to")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| invalid("redirect 条目缺少非空 to"))?;
            id += 1;
            rules.push(json!({
                "id": id,
                "priority": 1,
                "action": {
                    "type": "redirect",
                    "redirect": { "regexSubstitution": to_re2_substitution(to) },
                },
                "condition": {
                    "regexFilter": glob_to_anchored_regex(from),
                    "resourceTypes": resource_types(),
                }
            }));
        }
    }

    if let Some(headers) = obj.get("headers") {
        let headers = headers.as_array().ok_or_else(|| invalid("headers 必须是对象数组"))?;
        for h in headers {
            let url = h
                .get("url")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| invalid("headers 条目缺少非空 url"))?;
            let set = h
                .get("set")
                .and_then(Value::as_object)
                .filter(|m| !m.is_empty())
                .ok_or_else(|| invalid("headers 条目缺少非空 set 对象"))?;
            let operations: Vec<Value> = set
                .iter()
                .map(|(name, value)| {
                    json!({
                        "header": name,
                        "operation": "set",
                        "value": value.as_str().unwrap_or(&value.to_string()),
                    })
                })
                .collect();
            id += 1;
            rules.push(json!({
                "id": id,
                "priority": 1,
                "action": {
                    "type": "modifyHeaders",
                    "requestHeaders": operations,
                },
                "condition": {
                    "urlFilter": to_url_filter(url)?,
                    "resourceTypes": resource_types(),
                }
            }));
        }
    }

    Ok(json!(rules))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 核心映射：三类规则各一条，验证 glob → urlFilter/regexFilter 的形态
    #[test]
    fn compiles_all_three_action_kinds() {
        let rules = compile(&json!({
            "block": ["*.google-analytics.com"],
            "redirect": [{"from": "https://api.foo.com/*", "to": "http://localhost:3000/$1"}],
            "headers": [{"url": "https://app.foo.com/api/*", "set": {"X-Debug-Uid": "42"}}]
        }))
        .unwrap();
        let arr = rules.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["id"], 1, "id 连续分配");
        assert_eq!(arr[0]["action"]["type"], "block");
        assert_eq!(arr[0]["condition"]["urlFilter"], "||google-analytics.com");
        assert_eq!(
            arr[0]["condition"]["resourceTypes"].as_array().unwrap().len(),
            13,
            "condition 必须带全量 resourceTypes"
        );

        assert_eq!(arr[1]["action"]["type"], "redirect");
        assert_eq!(
            arr[1]["condition"]["regexFilter"],
            "^https://api\\.foo\\.com/(.*)$",
            "from glob → 锚定正则，* 为捕获组"
        );
        assert_eq!(
            arr[1]["action"]["redirect"]["regexSubstitution"],
            "http://localhost:3000/\\1",
            "$1 翻译为 RE2 反引用（DNR 替换语法，实测 $1 会按字面量输出）"
        );

        assert_eq!(arr[2]["action"]["type"], "modifyHeaders");
        assert_eq!(arr[2]["condition"]["urlFilter"], "||app.foo.com/api/*");
        assert_eq!(arr[2]["action"]["requestHeaders"][0]["header"], "X-Debug-Uid");
        assert_eq!(arr[2]["action"]["requestHeaders"][0]["operation"], "set");
        assert_eq!(arr[2]["action"]["requestHeaders"][0]["value"], "42");
    }

    /// 空对象/空数组 → 空规则（扩展加载后清空既有 dynamic rules，配置删除即回到无拦截）
    #[test]
    fn empty_config_compiles_to_empty_rules() {
        assert_eq!(compile(&json!({})).unwrap(), json!([]));
        assert_eq!(compile(&json!({"block": []})).unwrap(), json!([]));
    }

    /// glob 变体：scheme 剥离、`*.` 与裸域名同义（域及子域）、多捕获组顺序
    #[test]
    fn url_filter_variants() {
        assert_eq!(to_url_filter("https://ads.example.com/x*").unwrap(), "||ads.example.com/x*");
        assert_eq!(to_url_filter("http://a.b").unwrap(), "||a.b");
        assert_eq!(to_url_filter("*.foo.com").unwrap(), "||foo.com");
        assert_eq!(to_url_filter("foo.com").unwrap(), "||foo.com");
        assert_eq!(to_url_filter("||explicit.com").unwrap(), "||explicit.com");
        assert!(to_url_filter("  ").is_err(), "空白模式显式报错");

        let regex = glob_to_anchored_regex("https://a.com/*/x/*");
        assert_eq!(regex, "^https://a\\.com/(.*)/x/(.*)$", "* 依序编号：$1 $2");
    }

    /// 非法输入：类型错误/缺字段 → 整体 Err（不带病生成部分规则）
    #[test]
    fn rejects_malformed_config() {
        assert!(compile(&json!({"block": "not-array"})).is_err());
        assert!(compile(&json!({"block": [42]})).is_err());
        assert!(compile(&json!({"redirect": [{"to": "x"}]})).is_err());
        assert!(compile(&json!({"redirect": [{"from": ""}]})).is_err());
        assert!(compile(&json!({"headers": [{"url": "a.com"}]})).is_err());
        assert!(compile(&json!({"headers": [{"set": {}}]})).is_err());
        assert!(compile(&json!([])).is_err(), "顶层必须是对象");
    }

    /// 正则元字符必须转义（否则用户 URL 中的 `.` 匹配任意字符，拦截面意外扩大）
    #[test]
    fn regex_metacharacters_are_escaped() {
        let regex = glob_to_anchored_regex("https://v1.api.example.com/p(a)th?q=1");
        assert_eq!(regex, "^https://v1\\.api\\.example\\.com/p\\(a\\)th\\?q=1$");
    }

    /// 替换串翻译：$N → RE2 反引用；非数字 `$` 与多组号保持字面/顺序；
    /// 无 `$` 的替换串原样（无注入路径）
    #[test]
    fn substitution_translates_dollar_refs_to_re2() {
        assert_eq!(to_re2_substitution("http://l:3000/$1"), "http://l:3000/\\1");
        assert_eq!(to_re2_substitution("$2/x/$1"), "\\2/x/\\1");
        assert_eq!(to_re2_substitution("a$b"), "a$b", "$ 后非数字 → 字面量");
        assert_eq!(to_re2_substitution("price$$"), "price$$");
        assert_eq!(to_re2_substitution("http://l/$10"), "http://l/\\10");
        assert_eq!(to_re2_substitution("http://l/path"), "http://l/path");
    }
}
