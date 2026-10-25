//! 敏感信息脱敏（纯函数、无 IO）——URL 与响应体落环前执行。
//!
//! 契约（AGENT-API.md「证据边界契约」同源披露）：
//! - **环内与 API 输出一律存脱敏后的内容**：所有入环 URL（请求 URL、attach
//!   页面 URL、Log 条目 URL——含 query 与 fragment）与 body 落环前单向脱敏
//!   （诊断环是长生命周期状态，密钥一旦入环会随查询/HAR/SSE 反复外泄；
//!   落环前脱敏让「查得到的一定安全」，无需信任每个消费面）；
//! - JSON 走**源码区间替换**而非 parse + reserialize：未命中敏感键的字节
//!   原样保留——64 位数字 ID（>2^53）的十进制文本不经任何数值转换，精度
//!   字节级不变；空白/键序/转义形态全部保持证据原貌。值域凭证（Bearer/
//!   k=v 形态出现在非敏感键的字符串值里）与文本模式共用同一扫描集；
//! - 拒绝语义优先于部分保留：结构化 body 不可解析 / 深于 24 层 / 非 JSON
//!   路径进来却是结构化形态 → 整体 `redact-failed`（宁可少证据，不可在
//!   无法定界的内容里留一个可能截断在密钥中间的前缀）。
//!
//! 无正则依赖：Bearer / k=v（含 k:v）/ XML 文本节点三类文本模式手写扫描
//! （依赖清单冻结，不为脱敏破例）。

/// URL 最大保留长度（字符数）；超长截断并追加可见标记（截断必须显式）
pub const URL_MAX_CHARS: usize = 2048;
/// JSON 脱敏的最大嵌套深度；更深整体拒绝（防御恶意深嵌套 DoS + 定界失败面）
pub const MAX_JSON_DEPTH: usize = 24;
/// 敏感值占位符（冻结契约值，API 消费方按此识别已脱敏）
pub const REDACTED: &str = "[redacted]";
/// URL 截断标记（追加在保留前缀之后）
pub const URL_TRUNCATED_MARK: &str = "[url-truncated]";

/// 敏感类别词（全小写；匹配按词whole-word，见 [`split_words`]）。
/// 取舍：`key` 单词会误伤 {"key": "...} 这类 KV 元数据（保守可接受）；
/// 不收录 `code`（业务状态码高频，误伤面过大）、`refresh`（refreshInterval
/// 等非密钥高频）。漏报即漏报——整体拒绝路径只兜 Unparseable/TooDeep/
/// Uncertain 三种定界失败，格式合法 JSON 里的词表漏报不会被拒绝兜住，
/// 因此词表按实测漏网补齐（passphrase/passcode/privatekey 连写形态）。
const SENSITIVE_WORDS: &[&str] = &[
    "token", "secret", "password", "passwd", "pwd", "auth", "authorization", "signature", "sign",
    "session", "credential", "credentials", "cookie", "bearer", "apikey", "key", "jwt", "ticket",
    "passphrase", "passcode", "privatekey",
];

/// 字段名是否敏感：HTML 实体归一 → 分词（camelCase/kebab/snake/点号）→
/// 类别词整词命中（含复数形态：tokens/keys/credentials 均命中其单数）。
/// `session*`（sessionId 等）按设计命中——会话标识即准凭证，保守可接受。
pub fn is_sensitive_key(key: &str) -> bool {
    let normalized = decode_entities(key);
    split_words(&normalized).iter().any(|w| {
        SENSITIVE_WORDS.contains(&w.as_str())
            || (w.ends_with('s')
                && w.len() > 1
                && SENSITIVE_WORDS.contains(&&w[..w.len() - 1]))
    })
}

/// 常见混淆实体归一（JS 侧拼接密钥名时用实体绕过静态扫描的手法）。
/// 只做无歧义子串替换——发生在分词**之前**，&period; 还原成 . 后即被当作
/// 分隔符，实体混淆即失效。
fn decode_entities(s: &str) -> String {
    // 顺序无关：这些实体名互不为前缀（&amp; 除外，但 amp 还原出 & 不参与匹配，
    // 还原它反而可能拼出新实体，不做）
    s.replace("&period;", ".")
        .replace("&colon;", ":")
        .replace("&semi;", ";")
        .replace("&comma;", ",")
        .replace("&lpar;", "(")
        .replace("&rpar;", ")")
        .replace("&num;", "#")
        .replace("&hyphen;", "-")
        .replace("&lowbar;", "_")
        .replace("&lsqb;", "[")
        .replace("&rsqb;", "]")
}

/// 分词：非字母数字处断词 + camelCase 驼峰断词（小写/数字后跟大写），
/// 全部小写化。`sessionToken` → [session, token]；`access_token`、
/// `x-api-key`、`access&period;token`（归一后）同构。
fn split_words(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev_lower_or_digit = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() && prev_lower_or_digit && !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            cur.push(ch.to_ascii_lowercase());
            prev_lower_or_digit = !ch.is_ascii_uppercase();
        } else {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower_or_digit = false;
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// URL 脱敏：query 与 fragment 中敏感 key 的值 → `[redacted]`；'&' 与 ';'
/// 均作参数分隔符；超 2048 字符截断 + 标记。手工字符串处理，不经 URL
/// 解析器（parse + 重组会重排编码/规范化，破坏原始证据形态）。
pub fn redact_url(url: &str) -> String {
    let redacted = redact_query(url);
    if redacted.chars().count() > URL_MAX_CHARS {
        let head: String = redacted.chars().take(URL_MAX_CHARS).collect();
        format!("{head}{URL_TRUNCATED_MARK}")
    } else {
        redacted
    }
}

/// query/fragment 段脱敏（保留 base、参数序、分隔符与非敏感参数原样）。
/// fragment 与 query 同构处理：OAuth implicit flow 的 access_token 恰走
/// fragment（#access_token=eyJ...），只保 query 会留整段明文。
fn redact_query(url: &str) -> String {
    let qpos = url.find('?');
    let hpos = url.find('#');
    if qpos.is_none() && hpos.is_none() {
        return url.to_string();
    }
    let mut out = String::with_capacity(url.len());
    match (qpos, hpos) {
        (Some(q), Some(h)) if q < h => {
            out.push_str(&url[..=q]);
            out.push_str(&redact_pairs(&url[q + 1..h]));
            out.push('#');
            out.push_str(&redact_pairs(&url[h + 1..]));
        }
        (Some(q), None) => {
            out.push_str(&url[..=q]);
            out.push_str(&redact_pairs(&url[q + 1..]));
        }
        // '#' 在 '?' 之前（或无 '?'）：'?' 属 fragment 内容，'#' 后整段按
        // fragment 处理（redact_pairs 对无 '=' 的段原样）
        (_, Some(h)) => {
            out.push_str(&url[..=h]);
            out.push_str(&redact_pairs(&url[h + 1..]));
        }
        (None, None) => unreachable!("入口已排除"),
    }
    out
}

/// '&' / ';' 分隔的 pair 序列逐段脱敏，分隔符形态原样保留（'&' 与 ';' 混用
/// 时各归各）。';' 也作分隔符：部分框架/网关按分号解析 query，
/// `?file=x;access_token=abc` 若只按 '&' 切分会整段漏判。
fn redact_pairs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for chunk in s.split_inclusive(|c: char| c == '&' || c == ';') {
        let (pair, sep) = match chunk.as_bytes().last() {
            Some(b'&') => (&chunk[..chunk.len() - 1], '&'),
            Some(b';') => (&chunk[..chunk.len() - 1], ';'),
            _ => (chunk, '\0'),
        };
        out.push_str(&redact_pair(pair));
        if sep != '\0' {
            out.push(sep);
        }
    }
    out
}

/// 单个 pair：键（首个 '=' 前）命中敏感词 → 值替换；非敏感 pair 原样
fn redact_pair(pair: &str) -> String {
    match pair.find('=') {
        Some(epos) => {
            let (k, _v) = pair.split_at(epos);
            if is_sensitive_key(k) {
                format!("{k}={REDACTED}")
            } else {
                pair.to_string()
            }
        }
        None => pair.to_string(),
    }
}

/// 脱敏拒绝原因（统一映射为 bodyReason: "redact-failed"）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactReject {
    /// 结构化 body（JSON mime）语法不合法——无法安全定界字段
    Unparseable,
    /// 嵌套深于 [`MAX_JSON_DEPTH`]
    TooDeep,
    /// 非 JSON 路径进来却呈结构化形态（`{`/`[` 开头）——定界不可信
    Uncertain,
}

// ---------------------------------------------------------------------------
// JSON：源码区间替换 tokenizer
// ---------------------------------------------------------------------------

/// JSON body 脱敏。敏感字段名（[`is_sensitive_key`]）的值替换为 `"[redacted]"`
/// ——字符串/数字/true/false/null 一视同仁（数字形态的 PIN、验证码、遗留
/// 数字密钥与字符串同权；占位带引号，输出保持合法 JSON）；未命中的字符串
/// 值再过一遍文本模式凭证扫描（Bearer/k=v，见 [`redact_text_patterns`]）。
/// 其余字节原样（含 64 位大数、空白、键序）。数组元素继承外层敏感字名的
/// 脱敏（`{"tokens": ["a","b"]}`）；嵌套对象回到按自身键判定。
/// 语法不合法 / 深度超限 → 整体拒绝。
pub fn redact_json_body(src: &str) -> Result<String, RedactReject> {
    let mut r = JsonRedactor {
        chars: src.chars().collect(),
        pos: 0,
        out: String::with_capacity(src.len()),
    };
    match r.value(0, false) {
        Ok(()) => {
            r.ws();
            if r.pos == r.chars.len() {
                Ok(r.out)
            } else {
                Err(RedactReject::Unparseable) // 尾随垃圾
            }
        }
        Err(e) => Err(e),
    }
}

struct JsonRedactor {
    chars: Vec<char>,
    pos: usize,
    out: String,
}

impl JsonRedactor {
    /// 深度超限错误（区别于语法错误）
    fn too_deep() -> RedactReject {
        RedactReject::TooDeep
    }

    fn ws(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            // 空白是证据原貌的一部分：跳过即写入（未命中敏感键的字符全部原样）
            self.out.push(self.chars[self.pos]);
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// 解析一个值并写出到 out。`sensitive` = 外层字段名敏感（叶子值一律
    /// 脱敏：字符串/数字/布尔/null；容器回到自身键判定）。
    fn value(&mut self, depth: usize, sensitive: bool) -> Result<(), RedactReject> {
        if depth > MAX_JSON_DEPTH {
            return Err(Self::too_deep());
        }
        self.ws();
        match self.peek() {
            Some('{') => self.object(depth + 1),
            Some('[') => self.array(depth + 1, sensitive),
            Some('"') => {
                let (raw, decoded) = self.string()?;
                if sensitive {
                    self.out.push('"');
                    self.out.push_str(REDACTED);
                    self.out.push('"');
                } else {
                    // 值域内嵌凭证扫描（与文本模式共用模式集）：同一份
                    // Bearer/JWT 内容不应因载体是 JSON 字符串值而非 text/
                    // plain 就漏网。替换只发生在原始区间内——[redacted] 不含
                    // 引号/反斜杠，非命中字节原样，输出仍是合法 JSON 字符串
                    let inner = &raw[1..raw.len() - 1];
                    let scanned = redact_text_patterns(inner);
                    if scanned == inner {
                        self.out.push_str(&raw);
                    } else {
                        self.out.push('"');
                        self.out.push_str(&scanned);
                        self.out.push('"');
                    }
                }
                // decoded 仅用于键敏感性判定（object() 消费）；此处消歧
                let _ = decoded;
                Ok(())
            }
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(sensitive),
            Some('t') => self.lit("true", sensitive),
            Some('f') => self.lit("false", sensitive),
            Some('n') => self.lit("null", sensitive),
            _ => Err(RedactReject::Unparseable),
        }
    }

    fn object(&mut self, depth: usize) -> Result<(), RedactReject> {
        if depth > MAX_JSON_DEPTH {
            return Err(Self::too_deep());
        }
        self.out.push('{');
        self.pos += 1;
        self.ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            self.out.push('}');
            return Ok(());
        }
        loop {
            self.ws();
            if self.peek() != Some('"') {
                return Err(RedactReject::Unparseable);
            }
            let (raw_key, decoded) = self.string()?;
            self.out.push_str(&raw_key);
            self.ws();
            if self.peek() != Some(':') {
                return Err(RedactReject::Unparseable);
            }
            self.pos += 1;
            self.out.push(':');
            // 值的脱敏由键敏感性决定（字符串叶子；容器回到自身键判定）
            self.value(depth, is_sensitive_key(&decoded))?;
            self.ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                    self.out.push(',');
                }
                Some('}') => {
                    self.pos += 1;
                    self.out.push('}');
                    return Ok(());
                }
                _ => return Err(RedactReject::Unparseable),
            }
        }
    }

    fn array(&mut self, depth: usize, sensitive: bool) -> Result<(), RedactReject> {
        if depth > MAX_JSON_DEPTH {
            return Err(Self::too_deep());
        }
        self.out.push('[');
        self.pos += 1;
        self.ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            self.out.push(']');
            return Ok(());
        }
        loop {
            // 数组元素继承 sensitive：{"tokens": ["a"]} 的字符串即字段值本体；
            // 元素若是对象，value() 内部回到按自身键判定
            self.value(depth, sensitive)?;
            self.ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                    self.out.push(',');
                }
                Some(']') => {
                    self.pos += 1;
                    self.out.push(']');
                    return Ok(());
                }
                _ => return Err(RedactReject::Unparseable),
            }
        }
    }

    /// 解析一个字符串字面量：返回 (原始切片含引号, 转义解码后内容)。
    /// 解码仅用于键敏感性判定，值内容永远不做解码改写。
    fn string(&mut self) -> Result<(String, String), RedactReject> {
        let start = self.pos;
        self.pos += 1; // 开引号
        let mut decoded = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(RedactReject::Unparseable);
            };
            match c {
                '"' => {
                    self.pos += 1;
                    let raw: String = self.chars[start..self.pos].iter().collect();
                    return Ok((raw, decoded));
                }
                '\\' => {
                    self.pos += 1;
                    let Some(esc) = self.peek() else {
                        return Err(RedactReject::Unparseable);
                    };
                    match esc {
                        'u' => {
                            let hex: String = self
                                .chars
                                .get(self.pos + 1..self.pos + 5)
                                .map(|cs| cs.iter().collect())
                                .unwrap_or_default();
                            if hex.len() != 4 || !hex.chars().all(|h| h.is_ascii_hexdigit()) {
                                return Err(RedactReject::Unparseable);
                            }
                            let cp = u32::from_str_radix(&hex, 16).unwrap_or(0xFFFD);
                            decoded.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                            self.pos += 5;
                        }
                        'n' => {
                            decoded.push('\n');
                            self.pos += 1;
                        }
                        't' => {
                            decoded.push('\t');
                            self.pos += 1;
                        }
                        'r' => {
                            decoded.push('\r');
                            self.pos += 1;
                        }
                        'b' | 'f' => {
                            decoded.push('\u{FFFD}');
                            self.pos += 1;
                        }
                        other => {
                            decoded.push(other);
                            self.pos += 1;
                        }
                    }
                }
                '\n' => return Err(RedactReject::Unparseable), // 字符串内裸换行非法
                _ => {
                    decoded.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// 数字：源码区间原样转写（64 位精度保持的根），语法按 JSON 数字集；
    /// 敏感键下的数字叶子替换为带引号占位（裸 `[redacted]` 会把 body 变成
    /// 非法 JSON——消费方按 JSON 解析 HAR/body 时不能碎）
    fn number(&mut self, sensitive: bool) -> Result<(), RedactReject> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        let mut digits = 0;
        while let Some(c) = self.peek() {
            match c {
                '0'..='9' => {
                    self.pos += 1;
                    digits += 1;
                }
                '.' | 'e' | 'E' | '+' | '-' => self.pos += 1,
                _ => break,
            }
        }
        if digits == 0 {
            return Err(RedactReject::Unparseable);
        }
        if sensitive {
            self.out.push('"');
            self.out.push_str(REDACTED);
            self.out.push('"');
        } else {
            let raw: String = self.chars[start..self.pos].iter().collect();
            self.out.push_str(&raw);
        }
        Ok(())
    }

    fn lit(&mut self, word: &str, sensitive: bool) -> Result<(), RedactReject> {
        let got: String = self
            .chars
            .get(self.pos..self.pos + word.len())
            .map(|cs| cs.iter().collect())
            .unwrap_or_default();
        if got != word {
            return Err(RedactReject::Unparseable);
        }
        self.pos += word.len();
        if sensitive {
            self.out.push('"');
            self.out.push_str(REDACTED);
            self.out.push('"');
        } else {
            self.out.push_str(word);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 文本（非 JSON）：类密钥模式保守扫描
// ---------------------------------------------------------------------------

/// 文本 body 脱敏：结构化形态（`{`/`[` 开头）→ 整体拒绝（定界不可信，
/// 宁可少证据）；其余过文本模式集（Bearer / k=v 与 k:v / XML 文本节点）。
pub fn redact_text_body(src: &str) -> Result<String, RedactReject> {
    let trimmed = src.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Err(RedactReject::Uncertain);
    }
    Ok(redact_text_patterns(src))
}

/// 文本模式集（Bearer → k=v / k:v → XML 文本节点，逐层收敛）：JSON 路径的
/// 非敏感字符串值也复用同一模式集——同一份凭证不因载体 mime 不同而漏网
fn redact_text_patterns(src: &str) -> String {
    let pass1 = redact_bearer_tokens(src);
    let pass2 = redact_kv_patterns(&pass1);
    redact_xml_text_nodes(&pass2)
}

/// `Bearer xxx`（词首大小写不敏感）→ `Bearer [redacted]`
fn redact_bearer_tokens(src: &str) -> String {
    const WORD: &str = "bearer";
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < chars.len() {
        let is_start = i == 0 || !chars[i - 1].is_ascii_alphanumeric();
        if is_start
            && chars.len() >= i + WORD.len()
            && chars[i..i + WORD.len()]
                .iter()
                .zip(WORD.chars())
                .all(|(a, b)| a.to_ascii_lowercase() == b)
        {
            let mut j = i + WORD.len();
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let token_start = j;
            while j < chars.len() && !chars[j].is_whitespace() {
                j += 1;
            }
            if j > token_start {
                // 命中词与空白原样保留（证据原貌，大小写不变），仅凭证替换
                let kept: String = chars[i..token_start].iter().collect();
                out.push_str(&kept);
                out.push_str(REDACTED);
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// k=v 与 k:v 模式（含 `key="value"` 引号形态与 query 风格 `&` 终止）：
/// 标识符命中敏感类别词时值 → `[redacted]`（闭合引号原样保留）。
/// ':' 形态覆盖 YAML/properties/日志行（`password: hunter2`）——值域取到
/// 行尾（YAML 值可含空格，按词截断会把密钥后半留在环里）。
fn redact_kv_patterns(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < chars.len() {
        if is_ident_char(chars[i]) {
            let start = i;
            while i < chars.len() && is_ident_char(chars[i]) {
                i += 1;
            }
            let key: String = chars[start..i].iter().collect();
            // 向后看：可选空白 + '=' 或 ':'
            let mut j = i;
            while j < chars.len() && (chars[j] == ' ' || chars[j] == '\t') {
                j += 1;
            }
            let sep = match chars.get(j) {
                Some('=') => '=',
                Some(':') => ':',
                _ => '\0',
            };
            if sep != '\0' && is_sensitive_key(&key) {
                j += 1;
                while j < chars.len() && (chars[j] == ' ' || chars[j] == '\t') {
                    j += 1;
                }
                let mut quote = '\0';
                if j < chars.len() && (chars[j] == '"' || chars[j] == '\'') {
                    quote = chars[j];
                    j += 1;
                }
                let vstart = j;
                while j < chars.len()
                    && !(if sep == '=' {
                        is_value_end(chars[j], quote)
                    } else {
                        is_colon_value_end(chars[j], quote)
                    })
                {
                    j += 1;
                }
                if j > vstart {
                    out.push_str(&key);
                    out.push(sep);
                    if quote != '\0' {
                        out.push(quote);
                    }
                    out.push_str(REDACTED);
                    // i 停在值后（闭合引号/终止符由主循环原样输出）
                    i = j;
                    continue;
                }
            }
            out.push_str(&key);
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'
}

/// '=' 形态的值终止：任意空白 / '&' / '<' / ',' / ';' / 闭合引号
fn is_value_end(c: char, quote: char) -> bool {
    c.is_whitespace()
        || c == '&'
        || c == '<'
        || c == ','
        || c == ';'
        || (quote != '\0' && c == quote)
        || (quote == '\0' && (c == '"' || c == '\''))
}

/// ':' 形态的值终止：行尾（YAML/properties 值 = 余下整行）/ '<'（XML 标签
/// 回开）/ 闭合引号。不以任意空白截断——多词值截断会泄漏密钥后半
fn is_colon_value_end(c: char, quote: char) -> bool {
    c == '\n'
        || c == '\r'
        || c == '<'
        || (quote != '\0' && c == quote)
        || (quote == '\0' && (c == '"' || c == '\''))
}

/// XML 文本节点：`<password>凭证</password>` / `<s:apiKey>sk-123</s:apiKey>`
/// 形态——元素名（含命名空间前缀，归一后整词判定）命中敏感词时，开闭标签
/// 之间的文本内容整体 → `[redacted]`（标签与属性原样保留，SAML/SOAP 常见
/// 载体）。自闭合、无同名片闭合标签时不动（定界失败不臆造）。
fn redact_xml_text_nodes(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '<' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // 闭合标签 / 注释 / 声明：交回主循环原样输出
        let Some(&c0) = chars.get(i + 1) else {
            out.push(chars[i]);
            i += 1;
            continue;
        };
        if c0 == '/' || c0 == '!' || c0 == '?' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // 元素名（含 ':' 命名空间前缀）
        let mut j = i + 1;
        let name_start = j;
        while j < chars.len() && (is_ident_char(chars[j]) || chars[j] == ':') {
            j += 1;
        }
        if j == name_start {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let name: String = chars[name_start..j].iter().collect();
        // 属性区：扫到标签结束 '>'（含引号内的 '>' 时可能提前，可接受——
        // 只影响定界起点，内容仍受前序 pass 扫描）
        let mut k = j;
        while k < chars.len() && chars[k] != '>' {
            k += 1;
        }
        if k >= chars.len() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let open_tag: String = chars[i..=k].iter().collect();
        // 自闭合 <name/>：无文本内容
        if chars[k - 1] == '/' {
            out.push_str(&open_tag);
            i = k + 1;
            continue;
        }
        if is_sensitive_key(&name) {
            // 找同名片闭合标签 </name>（名字串全等，含命名空间）
            let close = format!("</{name}");
            if let Some(m) = find_substring(&chars, k + 1, &close) {
                let mut e = m + close.chars().count();
                while e < chars.len() && chars[e] != '>' {
                    e += 1;
                }
                if e < chars.len() {
                    out.push_str(&open_tag);
                    out.push_str(REDACTED);
                    let close_tag: String = chars[m..=e].iter().collect();
                    out.push_str(&close_tag);
                    i = e + 1;
                    continue;
                }
            }
            // 无同名闭合：定界失败，内容交给已跑过的 Bearer/k=v 扫描兜底
        }
        out.push_str(&open_tag);
        i = k + 1;
    }
    out
}

/// 字符数组内找子串（从 `from` 起），返回起始下标
fn find_substring(haystack: &[char], from: usize, needle: &str) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() || haystack.len() < n.len() {
        return None;
    }
    (from..=haystack.len() - n.len()).find(|&s| haystack[s..s + n.len()] == n[..])
}

// ---------------------------------------------------------------------------
// mime 判定与 body 脱敏入口
// ---------------------------------------------------------------------------

/// 文本类 mime 白名单（决定是否拉 body）。参数（`; charset=`）先剥离。
pub fn is_text_mime(mime: &str) -> bool {
    let base = mime.split(';').next().unwrap_or("").trim().to_lowercase();
    if base.is_empty() {
        return false;
    }
    base.starts_with("text/")
        || base == "application/json"
        || base == "application/xml"
        || base == "application/javascript"
        || base == "application/x-www+xml"
        || base == "application/yaml"
        || base.ends_with("+json")
        || base.ends_with("+xml")
}

/// JSON 形态 mime（决定脱敏走 tokenizer 还是文本模式）
pub fn is_json_mime(mime: &str) -> bool {
    let base = mime.split(';').next().unwrap_or("").trim().to_lowercase();
    base == "application/json" || base.ends_with("+json")
}

/// body 落环前的脱敏入口：JSON mime → tokenizer；其余（text/*、xml 等）→
/// 文本模式。拒绝（Unparseable/TooDeep/Uncertain）由调用方映射
/// bodyState: "unavailable" + reason: "redact-failed"。
pub fn redact_body(mime: Option<&str>, raw: &str) -> Result<String, RedactReject> {
    match mime {
        Some(m) if is_json_mime(m) => redact_json_body(raw),
        _ => redact_text_body(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- 字段名敏感性 ----------

    #[test]
    fn sensitive_keys_match_categories_camelcase_and_entities() {
        for key in [
            "token",
            "accessToken",
            "access_token",
            "x-api-key",
            "sessionToken",
            "userPassword",
            "authSignature",
            "Authorization",
            "refresh-token",
            "jwt",
            "apiKey",
            "access&period;token",
            "pwd",
        ] {
            assert!(is_sensitive_key(key), "应判定敏感: {key}");
        }
        // 误伤面检查：整词匹配，不命中子串；sessionId 按设计命中（session 即准凭证）
        for key in ["author", "keyword", "monkey", "design", "signet", "value", "id", "page", "status"] {
            assert!(!is_sensitive_key(key), "不应误伤: {key}");
        }
    }

    // ---------- URL ----------

    #[test]
    fn redact_url_replaces_sensitive_query_values_only() {
        let out = redact_url("https://api.example.com/v1/u?token=abc123&foo=1&page=2");
        assert_eq!(
            out,
            "https://api.example.com/v1/u?token=[redacted]&foo=1&page=2"
        );
        // camelCase / 大小写不敏感
        let out = redact_url("https://a/x?sessionToken=s3cret&Signature=zz");
        assert_eq!(out, "https://a/x?sessionToken=[redacted]&Signature=[redacted]");
        // 非敏感参数、裸参数原样；fragment 同构脱敏（OAuth implicit 的
        // access_token 走 fragment，与 query 同一套判定）
        let out = redact_url("https://a/x?flag&q=kw#frag?token=1");
        assert_eq!(out, "https://a/x?flag&q=kw#frag?token=[redacted]");
        // 无 query 原样
        assert_eq!(redact_url("https://a/x"), "https://a/x");
    }

    /// 分号分隔参数与纯 fragment URL：部分网关按 ';' 解析 query（只按 '&'
    /// 切分时 `?file=x;access_token=abc` 整段漏判）；无 '?' 的 fragment
    /// 承载 URL（OAuth callback）同样要过脱敏
    #[test]
    fn redact_url_handles_semicolon_separators_and_fragment() {
        // 分号后携带敏感参数：逐段判定
        assert_eq!(
            redact_url("https://api.example.com/dl?file=x;access_token=abc123;foo=1"),
            "https://api.example.com/dl?file=x;access_token=[redacted];foo=1"
        );
        // 纯 fragment（无 query）：k=v 同构脱敏
        assert_eq!(
            redact_url("https://app.example.com/cb#access_token=eyJ.a.b&token_type=bearer"),
            "https://app.example.com/cb#access_token=[redacted]&token_type=[redacted]"
        );
        // '&' 与 ';' 混用、非敏感段原样（分隔符形态不变）
        assert_eq!(
            redact_url("https://a/x?a=1;b=2&c=3"),
            "https://a/x?a=1;b=2&c=3"
        );
        // 值内合法分号（非 k=v 段）不受影响
        assert_eq!(redact_url("https://a/x?q=a;b"), "https://a/x?q=a;b");
        // 空 query / 空 fragment 边界
        assert_eq!(redact_url("https://a/x?"), "https://a/x?");
        assert_eq!(redact_url("https://a/x#"), "https://a/x#");
    }

    #[test]
    fn redact_url_truncates_overlength_with_marker() {
        let long = format!("https://a/x?p={}", "v".repeat(URL_MAX_CHARS));
        let out = redact_url(&long);
        assert!(out.contains(URL_TRUNCATED_MARK), "截断必须显式标记");
        assert_eq!(
            out.chars().count(),
            URL_MAX_CHARS + URL_TRUNCATED_MARK.len()
        );
        // 未超长不标记
        let short = redact_url("https://a/x?token=1");
        assert!(!short.contains(URL_TRUNCATED_MARK));
    }

    // ---------- JSON tokenizer ----------

    #[test]
    fn json_redacts_sensitive_string_values_only() {
        let src = r#"{"token":"abc.def","user":{"name":"vino","password":"p@ss"},"n":42}"#;
        let out = redact_json_body(src).unwrap();
        assert_eq!(
            out,
            r#"{"token":"[redacted]","user":{"name":"vino","password":"[redacted]"},"n":42}"#
        );
    }

    /// 64 位数字 ID 精度：未命中敏感键的数字**字节级原样**（不 parse 不转写），
    /// 2^53+1 与超 u64 大数都不损失。
    #[test]
    fn json_preserves_64bit_numbers_byte_exact() {
        let src = r#"{"id":9007199254740993,"big":123456789012345678901234567890,"f":1.5}"#;
        let out = redact_json_body(src).unwrap();
        assert!(out.contains("\"id\":9007199254740993"), "{out}");
        assert!(
            out.contains("\"big\":123456789012345678901234567890"),
            "{out}"
        );
        assert!(out.contains("\"f\":1.5"));
    }

    /// 空白/键序/转义形态原样保留（证据原貌，仅替换敏感值区间）
    #[test]
    fn json_preserves_whitespace_and_layout() {
        let src = "{\n  \"b\" : 1,\n  \"a\": \"x\"\n}";
        let out = redact_json_body(src).unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn json_handles_escapes_in_keys_and_values() {
        // 转义混淆的键（\u0065 = 'e'）命中类别词
        let src = r#"{"tok\u0065n":"abc"}"#;
        let out = redact_json_body(src).unwrap();
        assert!(out.contains("[redacted]"), "{out}");
        // 值内转义不受影响（非敏感键原样）
        let src = r#"{"path":"a\"b\\c"}"#;
        assert_eq!(redact_json_body(src).unwrap(), src);
    }

    /// 敏感键下的非字符串值（数字 PIN/验证码、布尔、null）同权脱敏；
    /// 占位带引号——裸 [redacted] 会把 body 变成非法 JSON
    #[test]
    fn json_redacts_sensitive_non_string_values() {
        let src = r#"{"password":123456,"secret":987654321,"ok":true,"flag":false,"token":null,"apiKey":true,"n":42}"#;
        let out = redact_json_body(src).unwrap();
        assert_eq!(
            out,
            r#"{"password":"[redacted]","secret":"[redacted]","ok":true,"flag":false,"token":"[redacted]","apiKey":"[redacted]","n":42}"#
        );
        // 数组元素继承：数字叶子同样替换
        let out = redact_json_body(r#"{"tokens":[1,2],"ids":[3,4]}"#).unwrap();
        assert_eq!(out, r#"{"tokens":["[redacted]","[redacted]"],"ids":[3,4]}"#);
    }

    /// 词表实测漏网补齐：passphrase/passcode/privatekey 连写形态
    #[test]
    fn sensitive_keys_cover_passphrase_variants() {
        // 连写形态（分词后无 password/key 词根可命中）
        for key in ["passphrase", "passcode", "privatekey", "PASSCODE"] {
            assert!(is_sensitive_key(key), "应判定敏感: {key}");
        }
        // 分词形态（private_key）本就命中 key 词根；passPhrase 分词为
        // pass+phrase，pass 不入词表（passRate/passCount 误伤面）——边界如注释
        assert!(is_sensitive_key("private_key"));
        assert!(!is_sensitive_key("passPhrase"));
        // 误伤面不变
        assert!(!is_sensitive_key("passenger"));
        assert!(!is_sensitive_key("keyword"));
    }

    /// 值域内嵌凭证：非敏感键的字符串值也过文本模式集——同一份 JWT 不因
    /// 载体是 application/json（错误响应回显、日志型接口）而非 text/plain 漏网
    #[test]
    fn json_scans_string_values_for_embedded_credentials() {
        let out = redact_json_body(
            r#"{"error":"upstream rejected Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig"}"#,
        )
        .unwrap();
        assert_eq!(out, r#"{"error":"upstream rejected Bearer [redacted]"}"#);
        // k=v 形态
        let out = redact_json_body(r#"{"log":"ok token=abc123 done"}"#).unwrap();
        assert_eq!(out, r#"{"log":"ok token=[redacted] done"}"#);
        // XML 文本节点形态
        let out = redact_json_body(r#"{"payload":"<password>hunter2</password>"}"#).unwrap();
        assert_eq!(out, r#"{"payload":"<password>[redacted]</password>"}"#);
        // 无命中的值字节原样（含转义形态）
        let src = r#"{"path":"a\"b\\c"}"#;
        assert_eq!(redact_json_body(src).unwrap(), src);
    }

    #[test]
    fn json_array_elements_inherit_sensitive_key() {
        let src = r#"{"tokens":["a","b"],"ids":[1,2],"mixed":{"token":"x"}}"#;
        let out = redact_json_body(src).unwrap();
        assert_eq!(
            out,
            r#"{"tokens":["[redacted]","[redacted]"],"ids":[1,2],"mixed":{"token":"[redacted]"}}"#
        );
    }

    #[test]
    fn json_rejects_malformed_and_too_deep() {
        assert_eq!(
            redact_json_body(r#"{"ab""#),
            Err(RedactReject::Unparseable)
        );
        assert_eq!(redact_json_body("{"), Err(RedactReject::Unparseable));
        assert_eq!(
            redact_json_body("{} trailing"),
            Err(RedactReject::Unparseable)
        );
        // 深度边界：MAX_JSON_DEPTH 层合法，+1 拒绝（顶层数组记第 1 层）
        let ok_depth = format!("{}{}", "[".repeat(MAX_JSON_DEPTH - 1), "]".repeat(MAX_JSON_DEPTH - 1));
        assert!(redact_json_body(&ok_depth).is_ok(), "24 层内合法");
        let too_deep = format!("{}{}", "[".repeat(MAX_JSON_DEPTH + 1), "]".repeat(MAX_JSON_DEPTH + 1));
        assert_eq!(redact_json_body(&too_deep), Err(RedactReject::TooDeep));
    }

    // ---------- 文本模式 ----------

    #[test]
    fn text_redacts_bearer_and_kv_patterns() {
        // 冒号形态在 kv pass 命中 Authorization（值域到行尾），先跑的 Bearer
        // 扫描产物一并被值域覆盖——两层叠加后整值归 [redacted]
        assert_eq!(
            redact_text_body("Authorization: Bearer abc.def.ghi").unwrap(),
            "Authorization:[redacted]"
        );
        assert_eq!(
            redact_text_body("bearer XYZ123 ok").unwrap(),
            "bearer [redacted] ok",
            "命中词大小写/空白原样保留"
        );
        assert_eq!(
            redact_text_body("token=abc123&foo=1").unwrap(),
            "token=[redacted]&foo=1"
        );
        // 引号形态（HTML 属性）
        assert_eq!(
            redact_text_body("<a data-token=\"sec\">x</a>").unwrap(),
            "<a data-token=\"[redacted]\">x</a>"
        );
        // 非敏感 k=v 不动
        assert_eq!(
            redact_text_body("page=2&size=10").unwrap(),
            "page=2&size=10"
        );
    }

    /// 冒号形态（YAML/properties/日志行）与 XML 文本节点：所在 mime
    /// （application/yaml、xml 系）均在拉取白名单内，字段型凭证的常见载体
    #[test]
    fn text_redacts_colon_form_and_xml_text_nodes() {
        // 冒号形态：值域到行尾（多词密钥不留后半）
        assert_eq!(
            redact_text_body("password: hunter2\nclient_secret: abc").unwrap(),
            "password:[redacted]\nclient_secret:[redacted]"
        );
        assert_eq!(
            redact_text_body("password: my secret phrase").unwrap(),
            "password:[redacted]"
        );
        // 引号值：闭合引号由主循环原样回接，行内后续内容保留
        assert_eq!(
            redact_text_body("password: \"hunter2\" next").unwrap(),
            "password:\"[redacted]\" next"
        );
        // 非敏感冒号行（时间戳、URL scheme）不受影响
        assert_eq!(
            redact_text_body("12:30:01 https://a/x ok").unwrap(),
            "12:30:01 https://a/x ok"
        );
        // XML 元素内容（含命名空间前缀）；非敏感元素与其内容原样
        assert_eq!(
            redact_text_body(
                "<s:Envelope><password>hunter2</password><apiKey>sk-123</apiKey><data id=\"1\">x</data></s:Envelope>"
            )
            .unwrap(),
            "<s:Envelope><password>[redacted]</password><apiKey>[redacted]</apiKey><data id=\"1\">x</data></s:Envelope>"
        );
        // 敏感元素嵌套内容整体替换（内容里的子元素不逐个漏判）
        assert_eq!(
            redact_text_body("<credentials><user>a</user></credentials>").unwrap(),
            "<credentials>[redacted]</credentials>"
        );
        // 自闭合与无闭合标签：定界失败不动（内容仍受 Bearer/k=v 扫描）
        assert_eq!(
            redact_text_body("<token/>ok").unwrap(),
            "<token/>ok"
        );
    }

    /// 非 JSON 路径进来却呈结构化形态 → 整体拒绝（Uncertain）：
    /// 定界不可信时保留前缀可能恰好截断在密钥中间
    #[test]
    fn text_rejects_structured_looking_bodies() {
        assert_eq!(
            redact_text_body(r#"{"token": "abc""#),
            Err(RedactReject::Uncertain)
        );
        assert_eq!(redact_text_body(" [1,2]"), Err(RedactReject::Uncertain));
        // 普通文本不受影响
        assert!(redact_text_body("hello world").is_ok());
    }

    // ---------- mime ----------

    #[test]
    fn text_mime_whitelist_and_json_classification() {
        for mime in [
            "text/html",
            "text/plain; charset=utf-8",
            "application/json",
            "application/vnd.api+json",
            "application/xml",
            "application/atom+xml",
            "TEXT/CSS",
        ] {
            assert!(is_text_mime(mime), "应拉 body: {mime}");
        }
        for mime in ["image/png", "application/octet-stream", "", "font/woff2"] {
            assert!(!is_text_mime(mime), "不应拉 body: {mime}");
        }
        assert!(is_json_mime("application/json"));
        assert!(is_json_mime("application/ld+json; charset=utf8"));
        assert!(!is_json_mime("text/plain"));
        assert!(!is_json_mime("application/xml"));
    }

    /// 落环入口分流：JSON mime → tokenizer；text → 模式扫描 + 结构化拒绝
    #[test]
    fn redact_body_dispatches_by_mime() {
        assert_eq!(
            redact_body(Some("application/json"), r#"{"token":"x"}"#).unwrap(),
            r#"{"token":"[redacted]"}"#
        );
        assert_eq!(
            redact_body(Some("text/plain"), "Bearer xyz").unwrap(),
            "Bearer [redacted]"
        );
        // text mime + JSON 形态 → 拒绝
        assert_eq!(
            redact_body(Some("text/plain"), r#"{"token":"x"}"#),
            Err(RedactReject::Uncertain)
        );
        // mime 缺失按文本模式
        assert_eq!(redact_body(None, "a=1").unwrap(), "a=1");
    }
}
