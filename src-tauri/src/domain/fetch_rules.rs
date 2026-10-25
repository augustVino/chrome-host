//! CDP Fetch 会话层拦截规则（domain 纯函数层）。
//!
//! 与 net-rules 扩展（DNR 持久层）的分工（TODO.md 第 6 项分层决策）：
//! net-rules 管**跨任务持久**的 block/redirect/headers；本模块管 **agent
//! 任务内动态**规则——mock 响应、修改请求头、按需 block。规则一次性下发
//! （`POST /instances/{id}/page/intercept` 整表替换），应用层本地决策
//! （requestPaused 事件流内匹配 + 执行，不逐请求往返调用方），生命周期
//! 绑定 page 控制连接（重建即失效）。
//!
//! 本文件只做可单测的纯逻辑：规则编译/校验、glob 匹配、命中决策、CDP
//! pattern 展开。IO（Fetch.enable/continueRequest、mock 上游）归
//! application 层（page_service + fetch_mock）。
//!
//! glob 语义（对外契约，AGENT-API.md 同步）：与 net-rules 同源的 `*`
//! 通配（`*` = 任意字符序列含空），但独立实现（两层的匹配目标不同：
//! net-rules 生成 DNR urlFilter，这里直接字符串匹配 + 透传 CDP
//! Fetch urlPattern——spike 真机实测 CDP pattern 同为 `*` 通配）。

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::error::AppError;

/// 规则数上限（pattern 总量 = Σ 规则 × resourceTypes，同受此约束——
/// 32 条 × 多类型展开后截断在 [`MAX_CDP_PATTERNS`]）。
pub const MAX_RULES: usize = 32;
/// Fetch.enable patterns 数量上限（规则 × resourceTypes 展开后；超限拒绝
/// ——拦截面过大等于把页面请求全部堵在决策路径上）。
pub const MAX_CDP_PATTERNS: usize = 64;
/// 干预记录环上限（per-tab，GET intercept 返回最近的 N 条）。
pub const JOURNAL_CAPACITY: usize = 200;

/// CDP resourceType 白名单（ResourceType 枚举，小写形态对齐协议）。
pub const RESOURCE_TYPES: [&str; 12] = [
    "document",
    "stylesheet",
    "image",
    "media",
    "font",
    "script",
    "websocket",
    "xhr",
    "fetch",
    "ping",
    "cspreport",
    "other",
];

/// 单个头的黑名单判定：精确名（cookie 等）或前缀（sec-/proxy-）。
pub fn is_forbidden_header(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    // 常见变体（cookie2/proxy-authorization 等）也拦：前缀式收紧
    lower == "cookie" || lower == "cookie2" || lower == "authorization"
        || lower == "origin" || lower == "content-length" || lower == "host"
        || lower.starts_with("sec-") || lower.starts_with("proxy-")
}

/// 拦截动作。
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// mock 响应：不发出真实请求，由本地 mock 上游按 spec 回应。
    Mock {
        /// 200-599（不允许 3xx 重定向/304——mock 语义是终态响应）
        status: u16,
        /// 响应头（黑名单外任意；CORS 头由上游注入，同名覆盖）
        headers: Vec<(String, String)>,
        /// 响应体（原始文本；wire 另支持 bodyBase64，编译期统一解码为字节）
        body: Vec<u8>,
        /// 0-10000ms（mock 上游侧延迟）
        delay_ms: u64,
    },
    /// 改写请求头（continueRequest 替换语义下的合并：原头 ∪ 规则头，规则优先）。
    SetHeaders { headers: Vec<(String, String)> },
    /// 拦截并失败（failRequest BlockedByClient）。
    Block,
}

/// 编译后的拦截规则（times 耗尽态由应用层 used 计数判定，规则本体不可变）。
#[derive(Debug, Clone, PartialEq)]
pub struct InterceptRule {
    pub id: String,
    /// URL glob（`*` 通配；须带 scheme 或 `*://` 开头——透传 CDP urlPattern
    /// 并在应用层做同 pattern 的二次匹配）
    pub url_pattern: String,
    /// 资源类型过滤（None = 全部类型）
    pub resource_types: Option<Vec<String>>,
    pub action: Action,
    /// 命中次数上限：0 = 不限；默认 1
    pub times: u32,
}

fn invalid(msg: impl Into<String>) -> AppError {
    AppError::invalid_request(format!("拦截规则不合法: {}", msg.into()))
}

/// 编译规则集（wire body 的 `rules` 数组 → 校验后的规则表）。
/// 空 rules 是合法输入（= 清空，应用层据此 Fetch.disable）。
pub fn compile_rules(rules: &[Value]) -> Result<Vec<InterceptRule>, AppError> {
    if rules.len() > MAX_RULES {
        return Err(invalid(format!("规则数 {} 超上限 {MAX_RULES}", rules.len())));
    }
    let mut out = Vec::with_capacity(rules.len());
    let mut seen_ids: HashSet<String> = HashSet::new();
    for (i, r) in rules.iter().enumerate() {
        let at = format!("rules[{i}]");
        let id = r
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| invalid(format!("{at}: id 必须是非空字符串")))?;
        if !seen_ids.insert(id.to_string()) {
            return Err(invalid(format!("{at}: id 重复（{id}）")));
        }
        let pattern = r
            .get("match")
            .and_then(|m| m.get("urlPattern"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| invalid(format!("{at}.match.urlPattern 必须是非空字符串")))?;
        validate_url_pattern(pattern)
            .map_err(|e| invalid(format!("{at}.match.urlPattern: {e}")))?;
        let resource_types = match r.pointer("/match/resourceTypes") {
            None | Some(Value::Null) => None,
            Some(Value::Array(types)) if types.is_empty() => None,
            Some(Value::Array(types)) => {
                let mut ts = Vec::with_capacity(types.len());
                for t in types {
                    let s = t
                        .as_str()
                        .ok_or_else(|| invalid(format!("{at}.match.resourceTypes 须为字符串数组")))?;
                    if !RESOURCE_TYPES.iter().any(|t| *t == s.to_lowercase()) {
                        return Err(invalid(format!(
                            "{at}.match.resourceTypes: 未知类型 {s:?}（白名单 {:?}）",
                            RESOURCE_TYPES
                        )));
                    }
                    ts.push(s.to_lowercase());
                }
                Some(ts)
            }
            Some(_) => return Err(invalid(format!("{at}.match.resourceTypes 须为数组"))),
        };
        let times = match r.get("times") {
            None | Some(Value::Null) => 1,
            Some(Value::Number(n)) => {
                let t = n
                    .as_u64()
                    .filter(|t| *t <= u32::MAX as u64)
                    .ok_or_else(|| invalid(format!("{at}.times 须为 0-{} 的整数", u32::MAX)))?
                    as u32;
                t
            }
            Some(_) => return Err(invalid(format!("{at}.times 须为非负整数"))),
        };
        let action = compile_action(&r.get("action").cloned().unwrap_or(Value::Null), &at)?;
        out.push(InterceptRule {
            id: id.to_string(),
            url_pattern: pattern.to_string(),
            resource_types,
            action,
            times,
        });
    }
    Ok(out)
}

fn compile_action(a: &Value, at: &str) -> Result<Action, AppError> {
    let ty = a
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{at}.action.type 必填（mock | setHeaders | block）")))?;
    match ty {
        "mock" => {
            // 有效域 = 2xx ∪ 4xx/599：3xx（重定向）与 304 不是终态响应，
            // mock 语义下会产生歧义（改写 URL 才是本层做重定向的正确姿势）
            let status = a
                .get("status")
                .and_then(Value::as_u64)
                .and_then(|s| u16::try_from(s).ok())
                .filter(|s| (200..=299).contains(s) || (400..=599).contains(s))
                .ok_or_else(|| invalid(format!("{at}.action.status 须为 2xx 或 4xx-599（不允许 3xx 重定向/304）")))?;
            let headers = compile_headers(a.get("headers"), &format!("{at}.action.headers"))?;
            // body / bodyBase64 二选一（文本优先；都缺 = 空 body）
            let body = if let Some(text) = a.get("body").and_then(Value::as_str) {
                text.as_bytes().to_vec()
            } else if let Some(b64) = a.get("bodyBase64").and_then(Value::as_str) {
                base64_decode(b64)
                    .map_err(|e| invalid(format!("{at}.action.bodyBase64: {e}")))?
            } else {
                Vec::new()
            };
            let delay_ms = match a.get("delayMs") {
                None | Some(Value::Null) => 0,
                Some(Value::Number(n)) => n
                    .as_u64()
                    .filter(|d| *d <= 10_000)
                    .ok_or_else(|| invalid(format!("{at}.action.delayMs 须为 0-10000")))?,
                Some(_) => return Err(invalid(format!("{at}.action.delayMs 须为非负整数"))),
            };
            Ok(Action::Mock {
                status,
                headers,
                body,
                delay_ms,
            })
        }
        "setHeaders" => {
            let headers = compile_headers(a.get("headers"), &format!("{at}.action.headers"))?;
            if headers.is_empty() {
                return Err(invalid(format!("{at}.action.headers 不能为空")));
            }
            Ok(Action::SetHeaders { headers })
        }
        "block" => Ok(Action::Block),
        other => Err(invalid(format!(
            "{at}.action.type 未知 {other:?}（支持 mock | setHeaders | block）"
        ))),
    }
}

fn compile_headers(v: Option<&Value>, at: &str) -> Result<Vec<(String, String)>, AppError> {
    let Some(Value::Object(map)) = v else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(map.len());
    for (k, val) in map {
        let value = val
            .as_str()
            .ok_or_else(|| invalid(format!("{at}[{k:?}] 值须为字符串")))?;
        let name = k.trim();
        if name.is_empty() {
            return Err(invalid(format!("{at} 头名不能为空")));
        }
        if is_forbidden_header(name) {
            return Err(invalid(format!(
                "{at}[{k:?}] 在黑名单内（凭据/连接语义头不可由规则设置）"
            )));
        }
        out.push((name.to_string(), value.to_string()));
    }
    Ok(out)
}

/// URL pattern 校验（非空、长度上限、须以 `scheme://` 或 `*://` 开头；
/// 语法细节交由 CDP urlPattern 与本地 glob 二次匹配兜底）。
fn validate_url_pattern(p: &str) -> Result<(), String> {
    if p.len() > 512 {
        return Err(format!("长度 {} 超上限 512", p.len()));
    }
    let looks_like_url = p.contains("://");
    if !looks_like_url {
        return Err("须包含 scheme（如 https://… 或 *://…）".to_string());
    }
    Ok(())
}

/// `*` 通配匹配（`*` = 任意字符序列含空；**其余字符全部字面匹配——包括
/// `?`**：URL pattern 里 `?` 是 query 分隔符不是单字符通配，CDP urlPattern
/// 的通配符也只有 `*`）。贪婪双指针 + 回溯（经典 glob 算法，URL 场景 `*`
/// 个数个位数量级）。
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// 首个未耗尽的命中规则：按规则表顺序取第一条（glob + resourceType 匹配
/// 且 times 未耗尽）。used = 应用层维护的 per-id 已命中计数。
pub fn first_live_match<'a>(
    rules: &'a [InterceptRule],
    used: &HashMap<String, u64>,
    url: &str,
    resource_type: &str,
) -> Option<&'a InterceptRule> {
    let rt = resource_type.to_lowercase();
    rules.iter().find(|r| {
        let times_ok = r.times == 0 || used.get(&r.id).copied().unwrap_or(0) < u64::from(r.times);
        let type_ok = r
            .resource_types
            .as_ref()
            .is_none_or(|ts| ts.contains(&rt));
        times_ok && type_ok && glob_match(&r.url_pattern, url)
    })
}

/// 展开为 Fetch.enable 的 patterns（urlPattern + 单数 resourceType；缺省
/// 类型不设 resourceType = 全类型）。展开超上限报错（拒绝下发而非静默截断
/// ——静默截断会让「以为拦了」的规则漏拦，诚实拒绝）。
pub fn cdp_patterns(rules: &[InterceptRule]) -> Result<Vec<Value>, AppError> {
    let mut out = Vec::new();
    for r in rules {
        match &r.resource_types {
            None => out.push(serde_json::json!({
                "urlPattern": r.url_pattern, "requestStage": "Request"
            })),
            Some(types) => {
                for t in types {
                    out.push(serde_json::json!({
                        "urlPattern": r.url_pattern, "requestStage": "Request", "resourceType": t
                    }));
                }
            }
        }
    }
    if out.len() > MAX_CDP_PATTERNS {
        return Err(invalid(format!(
            "展开后 pattern 数 {} 超上限 {MAX_CDP_PATTERNS}",
            out.len()
        )));
    }
    Ok(out)
}

/// 标准 base64 解码（无第三方依赖：字母表 + padding 校验）。
/// 实现侧 body 走 UTF-8 JSON 无碍；mock 上游按原始字节回应。
fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let s: Vec<u8> = input
        .bytes()
        .filter(|b| !b" \t\r\n".contains(b))
        .collect();
    if s.is_empty() {
        return Ok(Vec::new());
    }
    if !s.len().is_multiple_of(4) {
        return Err("长度非 4 的倍数".to_string());
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for chunk in s.chunks(4) {
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && chunk.iter().rev().take(pad).any(|&c| c != b'=')) {
            return Err("padding 位置非法".to_string());
        }
        let mut acc: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            let v = if c == b'=' {
                0
            } else {
                let v = TABLE
                    .iter()
                    .position(|&t| t == c)
                    .ok_or_else(|| format!("非法字符 {:?}", c as char))?;
                if i >= 4 - pad && v != 0 {
                    // padding 前的位允许有值；只有 padding 位本身要求 '='
                }
                v as u32
            };
            acc = (acc << 6) | v;
        }
        let bytes = 3 - pad;
        out.push(((acc >> 16) & 0xff) as u8);
        if bytes > 1 {
            out.push(((acc >> 8) & 0xff) as u8);
        }
        if bytes > 2 {
            out.push((acc & 0xff) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------------- glob_match ----------------

    /// glob 语义矩阵：`*` 跨段通配、`?` 单字符、字面前缀/后缀锚定、多星。
    #[test]
    fn glob_match_matrix() {
        assert!(glob_match("https://api.foo.com/v1/*", "https://api.foo.com/v1/users?a=1"));
        assert!(glob_match("*://127.0.0.1:18799/api/*", "http://127.0.0.1:18799/api/data?n=1"));
        assert!(!glob_match("https://api.foo.com/v1/*", "https://api.foo.com/v2/x"));
        assert!(!glob_match("*://api.foo.com/*", "https://evil.com/x"));
        assert!(glob_match("a*c*e", "abcde"));
        assert!(glob_match("*", "anything at all"));
        // `?` 字面匹配（URL query 分隔符不是通配符）
        assert!(glob_match("a?c", "a?c"));
        assert!(!glob_match("a?c", "axc"));
        assert!(glob_match("https://a.com/x?n=*", "https://a.com/x?n=42"));
        // 末日难场景：星号回溯（贪婪失败后回退）
        assert!(glob_match("*b*c", "abbc"));
        assert!(!glob_match("*b*c", "abd"));
        // 空串与边界
        assert!(glob_match("*", ""));
        assert!(!glob_match("a*", ""));
        assert!(glob_match("a*", "a"));
    }

    // ---------------- compile_rules 校验矩阵 ----------------

    fn ok_body() -> Value {
        json!({ "rules": [
            { "id": "r1", "match": { "urlPattern": "https://api.foo.com/v1/*", "resourceTypes": ["xhr","fetch"] },
              "action": { "type": "mock", "status": 200, "headers": {"Content-Type":"application/json"}, "body": "{\"ok\":true}", "delayMs": 0, "times": 1 } },
            { "id": "r2", "match": { "urlPattern": "*://*.foo.com/*" },
              "action": { "type": "setHeaders", "headers": {"X-Debug-Uid":"42"} } },
            { "id": "r3", "match": { "urlPattern": "https://ads.example.com/*" },
              "action": { "type": "block" } }
        ]})
    }

    #[test]
    fn compile_rules_accepts_wire_form() {
        let rules = compile_rules(ok_body()["rules"].as_array().unwrap()).unwrap();
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].id, "r1");
        assert_eq!(rules[0].times, 1);
        assert_eq!(
            rules[0].resource_types,
            Some(vec!["xhr".to_string(), "fetch".to_string()])
        );
        match &rules[0].action {
            Action::Mock { status, headers, body, delay_ms } => {
                assert_eq!((*status, *delay_ms), (200, 0));
                assert_eq!(headers, &[("Content-Type".to_string(), "application/json".to_string())]);
                assert_eq!(body, b"{\"ok\":true}");
            }
            other => panic!("r1 应为 Mock: {other:?}"),
        }
        match &rules[2].action {
            Action::Block => {}
            other => panic!("r3 应为 Block: {other:?}"),
        }
        // 空表 = 清空（合法）
        assert!(compile_rules(&[]).unwrap().is_empty());
    }

    /// 拒绝面：黑名单头（大小写/变体）、status 越界、delay 越界、坏 id/
    /// pattern、重复 id、未知类型、bodyBase64 坏码、规则数超限。
    #[test]
    fn compile_rules_rejects_invalid() {
        let base = ok_body();
        let mut bad = base.clone();
        bad["rules"][1]["action"]["headers"] = json!({"Cookie": "session=abc"});
        let e = compile_rules(bad["rules"].as_array().unwrap()).unwrap_err();
        assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400));
        assert!(e.to_string().contains("黑名单"));

        let mut bad = base.clone();
        for reject in [302u16, 304, 199, 600] {
            bad["rules"][0]["action"]["status"] = json!(reject);
            assert!(
                compile_rules(bad["rules"].as_array().unwrap()).is_err(),
                "status {reject} 应被拒绝"
            );
        }
        for accept in [200u16, 201, 400, 404, 500, 599] {
            bad["rules"][0]["action"]["status"] = json!(accept);
            assert!(
                compile_rules(bad["rules"].as_array().unwrap()).is_ok(),
                "status {accept} 应被接受"
            );
        }

        let mut bad = base.clone();
        bad["rules"][0]["action"]["delayMs"] = json!(10001);
        assert!(compile_rules(bad["rules"].as_array().unwrap())
            .unwrap_err()
            .to_string()
            .contains("delayMs"));

        let mut bad = base.clone();
        bad["rules"][0]["action"]["headers"] = json!({"SEC-CH-UA": "\"Chrome\""});
        assert!(compile_rules(bad["rules"].as_array().unwrap())
            .unwrap_err()
            .to_string()
            .contains("黑名单"));

        let mut bad = base.clone();
        bad["rules"][2]["id"] = json!("r1");
        assert!(compile_rules(bad["rules"].as_array().unwrap())
            .unwrap_err()
            .to_string()
            .contains("重复"));

        let mut bad = base.clone();
        bad["rules"][0]["match"]["urlPattern"] = json!("api.foo.com/v1/*");
        assert!(compile_rules(bad["rules"].as_array().unwrap())
            .unwrap_err()
            .to_string()
            .contains("scheme"));

        let mut bad = base.clone();
        bad["rules"][0]["match"]["resourceTypes"] = json!(["XHR"]);
        // 大小写不敏感：XHR → xhr 合法
        assert!(compile_rules(bad["rules"].as_array().unwrap()).is_ok());
        bad["rules"][0]["match"]["resourceTypes"] = json!(["banana"]);
        assert!(compile_rules(bad["rules"].as_array().unwrap())
            .unwrap_err()
            .to_string()
            .contains("banana"));

        let mut bad = base.clone();
        // body 文本优先：须先移除 body，坏 bodyBase64 才进入解码路径
        bad["rules"][0]["action"]["body"] = Value::Null;
        bad["rules"][0]["action"]["bodyBase64"] = json!("!!!not-base64!!!");
        assert!(compile_rules(bad["rules"].as_array().unwrap())
            .unwrap_err()
            .to_string()
            .contains("bodyBase64"));

        let mut bad = base.clone();
        bad["rules"][0]["action"]["type"] = json!("redirect");
        assert!(compile_rules(bad["rules"].as_array().unwrap())
            .unwrap_err()
            .to_string()
            .contains("redirect"));

        // 规则数超限
        let many: Vec<Value> = (0..MAX_RULES + 1)
            .map(|i| json!({ "id": format!("r{i}"), "match": {"urlPattern": "*://a.com/*"}, "action": {"type":"block"} }))
            .collect();
        assert!(compile_rules(&many).unwrap_err().to_string().contains("超上限"));
    }

    /// bodyBase64 二进制通道 + base64 解码矩阵（padding/空/非法）。
    #[test]
    fn body_base64_channel() {
        let rules_json = json!([
            { "id": "b1", "match": {"urlPattern": "*://x/*"},
              "action": {"type":"mock","status":200,"bodyBase64":"aGVsbG8="} }
        ]);
        let rules = compile_rules(rules_json.as_array().unwrap()).unwrap();
        match &rules[0].action {
            Action::Mock { body, .. } => assert_eq!(body, b"hello"),
            other => panic!("{other:?}"),
        }
        // 二进制字节完整往返
        let bin = vec![0u8, 1, 2, 255, 254];
        let b64: String = {
            const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut s = String::new();
            for chunk in bin.chunks(3) {
                let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
                let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
                s.push(T[(n >> 18) as usize & 63] as char);
                s.push(T[(n >> 12) as usize & 63] as char);
                s.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
                s.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
            }
            s
        };
        let rules_json = json!([
            { "id": "b2", "match": {"urlPattern": "*://x/*"},
              "action": {"type":"mock","status":200,"bodyBase64": b64} }
        ]);
        let rules = compile_rules(rules_json.as_array().unwrap()).unwrap();
        match &rules[0].action {
            Action::Mock { body, .. } => assert_eq!(body, &bin),
            other => panic!("{other:?}"),
        }
    }

    // ---------------- first_live_match / cdp_patterns ----------------

    /// 命中决策：顺序优先、类型过滤、times 耗尽跳过（0 = 不限）。
    #[test]
    fn first_live_match_order_types_and_times() {
        let rules = compile_rules(ok_body()["rules"].as_array().unwrap()).unwrap();
        let mut used = std::collections::HashMap::new();
        // r1（xhr/fetch + api.foo.com）命中
        let m = first_live_match(&rules, &used, "https://api.foo.com/v1/x", "XHR").unwrap();
        assert_eq!(m.id, "r1");
        // resourceType 不匹配 r1 → 落到 r2？r2 pattern *://*.foo.com/* 匹配
        let m = first_live_match(&rules, &used, "https://api.foo.com/v1/x", "Script").unwrap();
        assert_eq!(m.id, "r2");
        // r1 耗尽 → 同 URL 命中 r2
        used.insert("r1".to_string(), 1);
        let m = first_live_match(&rules, &used, "https://api.foo.com/v1/x", "Fetch").unwrap();
        assert_eq!(m.id, "r2");
        // 无匹配
        assert!(first_live_match(&rules, &used, "https://other.com/x", "XHR").is_none());
        // r2 times=1 也耗尽 → 无命中（r3 不匹配该 URL）
        used.insert("r2".to_string(), 1);
        assert!(first_live_match(&rules, &used, "https://api.foo.com/x", "XHR").is_none());
    }

    /// times=0 不限次耗尽。
    #[test]
    fn times_zero_means_unlimited() {
        let rules_json = json!([
            { "id": "u", "match": {"urlPattern": "*://a.com/*"}, "times": 0,
              "action": {"type":"block"} }
        ]);
        let rules = compile_rules(rules_json.as_array().unwrap()).unwrap();
        let mut used = std::collections::HashMap::new();
        used.insert("u".to_string(), 999);
        assert!(first_live_match(&rules, &used, "https://a.com/x", "XHR").is_some());
    }

    /// patterns 展开：无类型 → 单条；多类型 → 每类型一条；超限拒绝。
    #[test]
    fn cdp_patterns_expansion_and_budget() {
        let rules = compile_rules(ok_body()["rules"].as_array().unwrap()).unwrap();
        let pats = cdp_patterns(&rules).unwrap();
        assert_eq!(pats.len(), 4); // r1 两类型 + r2 一条 + r3 一条
        assert_eq!(pats[0]["urlPattern"], json!("https://api.foo.com/v1/*"));
        assert_eq!(pats[0]["resourceType"], json!("xhr"));
        assert_eq!(pats[0]["requestStage"], json!("Request"));
        assert!(pats[2].get("resourceType").is_none(), "缺省类型不带 resourceType");

        // 展开 pattern 超限（32 规则 × 2+ 类型 > 64）
        let many: Vec<Value> = (0..MAX_RULES)
            .map(|i| json!({ "id": format!("r{i}"), "match": {"urlPattern": "*://a.com/*", "resourceTypes": ["xhr","fetch","script"]},
                            "action": {"type":"block"} }))
            .collect();
        let rules = compile_rules(&many).unwrap();
        assert!(cdp_patterns(&rules).unwrap_err().to_string().contains("pattern"));
    }
}
