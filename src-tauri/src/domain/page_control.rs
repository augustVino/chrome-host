//! page 控制域纯逻辑层（P2-A）：@eN ref 表、点击几何、press 键表、wait 状态机、
//! 页面 eval 表达式生成与 JS 对话框模型。
//!
//! 行为规格 = Phase 1 的 TS 实现（`skills/chrome-host/scripts/lib/`，已由 183 单测
//! 验证），本模块是其逐语义移植；IO（CDP 收发、轮询、补偿重发）归 application 层
//! （PAGE-CONTROL-PLAN Phase 2），本文件只做可单测的纯逻辑：
//!
//! - [`RefStore`]：per-tab 的 @eN → backendNodeId 表（TS lib/ref-store.mjs）；
//! - [`largest_quad_centroid`]：DOM.getContentQuads → 点击点（TS lib/tools/interact.mjs）；
//! - [`parse_key`]/[`KeyPress`]：键名 → CDP Input.dispatchKeyEvent 三段序列
//!   （TS lib/tools/press.mjs，SPEC §4.2）；
//! - [`parse_duration`]/[`observed_matches_state`]：wait 轮询的参数与状态机
//!   （TS lib/tools/wait.mjs）；
//! - `*_expr` 表达式生成器：locate/hit-test/fill 检查/wait 观测的内嵌 JS
//!   （TS lib/tools/click-native.mjs、fill.mjs、wait.mjs）——selector 一律经
//!   [`json_escape`]（JSON.stringify 等价）转义后嵌入，防注入规则与 TS 一致；
//! - [`parse_dialog_opening`]/[`DialogBuffer`]：对话框防御解析 + 有界缓冲
//!   （TS lib/dialogs.mjs，BEHAVIOR-SPEC §1）。
//!
//! 与 TS 的已知偏差（均为语义无关的形态差异，详见各条注释）：
//! - 错误码统一加 `PAGE_` 前缀并 SCREAMING_SNAKE（TS 为 snake_case 消息前缀）；
//!   message 去掉了 TS 里冗余的 `code:` 前缀（code 已是独立字段）；
//! - `KeyPress.vk` 为 `Option<u16>`（TS 可 undefined：标点键无 vk，dispatch 时
//!   必须整体省略该字段而非置 0）；
//! - `PAGE_WAIT_TIMEOUT` 用 409 而非 504（HTTP 语义：客户端可重试的冲突态）；
//! - key_unknown 提示语去掉了 CLI 专属尾巴 `(see "cdp help")`。

// P2-B（application 层 L2 工具接线）尚未落地，本模块当前唯一消费者是
// 文件内测试；bin crate 下 pub 不豁免 dead_code，测试引用在非 --tests
// 编译中不可见。接线后删除本 allow。
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::AppError;

// ============================================================================
// 1. 错误码与 HTTP 映射
// ============================================================================

/// page 控制域错误：code 为对外稳定契约（AGENT-API 披露），status 为 HTTP 映射。
/// `effect_state` 仅 [`PageControlError::dispatch_failed`] 携带（TS click-native.mjs
/// §2.7 语义：press 前失败 = "none"——无任何输入已发出；press 后失败 = "unknown"——
/// mousePressed 已发出、效果不可判定，其余错误均为 None）。
#[derive(Debug, Clone, PartialEq)]
pub struct PageControlError {
    pub code: &'static str,
    pub status: u16,
    pub message: String,
    /// none | unknown | None（见类型注释）
    pub effect_state: Option<&'static str>,
}

impl PageControlError {
    fn business(status: u16, code: &'static str, message: String) -> Self {
        Self {
            code,
            status,
            message,
            effect_state: None,
        }
    }

    /// 404：locate / focus 未命中（TS classifyLocate / focusPressTarget）
    pub fn element_not_found(selector: &str) -> Self {
        Self::business(
            404,
            "PAGE_ELEMENT_NOT_FOUND",
            format!("Element not found: {selector}"),
        )
    }

    /// 422：locate 可见性四检未过（TS locateExpr 的 not_visible 分支）
    pub fn element_not_visible(selector: &str, why: &str) -> Self {
        let why = if why.is_empty() {
            "unknown reason"
        } else {
            why
        };
        Self::business(
            422,
            "PAGE_ELEMENT_NOT_VISIBLE",
            format!("Element not visible ({why}): {selector}"),
        )
    }

    /// 409：点击点被遮挡（TS classifyHit 的 covered 分支）
    pub fn element_blocked_covered(selector: &str, by: &str) -> Self {
        Self::business(
            409,
            "PAGE_ELEMENT_BLOCKED",
            format!("Click point on {selector} is covered by <{by}>"),
        )
    }

    /// 409：点击点无元素（视口外，TS classifyHit 的 nothing-at-point 分支）
    pub fn element_blocked_nothing(selector: &str) -> Self {
        Self::business(
            409,
            "PAGE_ELEMENT_BLOCKED",
            format!("Click point on {selector} hits nothing (outside viewport?)"),
        )
    }

    /// 404：ref 未命中且表未失效（TS refMissError 非 stale 分支）
    pub fn ref_not_found(raw_ref: &str) -> Self {
        Self::business(
            404,
            "PAGE_REF_NOT_FOUND",
            format!("{raw_ref} is not a ref from the last snap — run \"snap <target>\" and use the fresh @eN refs"),
        )
    }

    /// 409：ref 表因导航/文档替换整体失效（TS refMissError stale 分支；
    /// reason 缺省 "document changed"，与 TS invalidate 默认值一致）
    pub fn ref_stale(reason: Option<&str>) -> Self {
        Self::business(
            409,
            "PAGE_REF_STALE",
            format!(
                "page changed since the last snap ({}) — run \"snap <target>\" and use the fresh @eN refs",
                reason.unwrap_or("document changed")
            ),
        )
    }

    /// 409：ref 命中但几何/节点解析失败（TS resolveRefPoint / resolveFillTarget）
    pub fn target_moved(detail: &str) -> Self {
        Self::business(
            409,
            "PAGE_TARGET_MOVED",
            format!("ref no longer resolves to a live node ({detail}) — run \"snap <target>\" again and use fresh @eN refs"),
        )
    }

    /// 422：fill 目标不可编辑（TS requireFillable）
    pub fn target_not_fillable(target_desc: &str, why: &str) -> Self {
        Self::business(
            422,
            "PAGE_TARGET_NOT_FILLABLE",
            format!("fill target {target_desc} is not fillable: {why} — choose a text input, textarea, or the contenteditable editor root"),
        )
    }

    /// 409：fill 写入后读回校验失败（写入可能已部分生效，观察后再决定重试）
    pub fn fill_value_mismatch(message: impl Into<String>) -> Self {
        Self::business(409, "PAGE_FILL_VALUE_MISMATCH", message.into())
    }

    /// 409：wait 超时（504 → 409，任务约定；消息逐字对齐 TS waitStr 超时分支）
    pub fn wait_timeout(
        selector: &str,
        state: &str,
        elapsed_ms: i64,
        last: &str,
        polls: usize,
    ) -> Self {
        Self::business(
            409,
            "PAGE_WAIT_TIMEOUT",
            format!("Timed out after {elapsed_ms}ms waiting for \"{selector}\" to be {state} (last observed: {last}, {polls} polls)"),
        )
    }

    /// 500：Input.dispatch* 失败。`press_sent`：mousePressed 是否已发出——决定
    /// effect_state 取 unknown（已按下，效果不明）还是 none（未发出任何输入）。
    pub fn dispatch_failed(message: &str, press_sent: bool) -> Self {
        let effect = if press_sent {
            ("unknown", "mousePressed was already dispatched")
        } else {
            ("none", "no input was dispatched")
        };
        Self {
            code: "PAGE_DISPATCH_FAILED",
            status: 500,
            message: format!("{message} (effect_state: {} — {})", effect.0, effect.1),
            effect_state: Some(effect.0),
        }
    }

    /// 400：键名不识别（TS UNKNOWN_KEY_HINT，去掉了 CLI 专属尾巴 `(see "cdp help")`）
    pub fn key_unknown(input: &str) -> Self {
        Self::business(
            400,
            "PAGE_KEY_UNKNOWN",
            format!(
                "press: unknown key \"{input}\" — supported: named keys (Enter, Tab, Escape, Backspace, Delete, Home, End, PageUp, PageDown, ArrowUp/Down/Left/Right, Space, F1-F12), single characters, and modifier combos like Control+A / Meta+V / Shift+ArrowDown"
            ),
        )
    }

    /// 404：page 命令族的目标 tab 不存在（应用层路由用，TS 无对应——per-tab daemon
    /// 架构下由连接失败隐式覆盖）
    pub fn tab_not_found(tab: &str) -> Self {
        Self::business(404, "PAGE_TAB_NOT_FOUND", format!("tab not found: {tab}"))
    }

    /// ref 未命中时按表状态选 stale / not_found（TS interact.mjs refMissError）
    pub fn ref_miss(store: &RefStore, raw_ref: &str) -> Self {
        if store.stale {
            Self::ref_stale(store.last_invalid_reason())
        } else {
            Self::ref_not_found(raw_ref)
        }
    }
}

impl From<PageControlError> for AppError {
    fn from(e: PageControlError) -> Self {
        AppError::business(e.status, e.code, e.message)
    }
}

// ============================================================================
// 2. RefStore（TS lib/ref-store.mjs + tests/ref-store.test.mjs）
// ============================================================================

/// §3.7 归一化：只剥**恰好一个**前导 '@'（'@e3' → 'e3'，'@@e3' → '@e3'）。
/// 大小写敏感是刻意的：'E3'/'@E3' 原样保留、查表必失配 → ref_not_found。
pub fn normalize_ref(raw: &str) -> &str {
    raw.strip_prefix('@').unwrap_or(raw)
}

/// ^e\d+$（区分大小写；TS 用正则，此处手写避免引 regex 依赖）
fn is_ref_key(key: &str) -> bool {
    let mut chars = key.chars();
    if chars.next() != Some('e') {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    !rest.is_empty() && rest.iter().all(|c| c.is_ascii_digit())
}

/// 表条目：normalized 'eN' → CDP 节点句柄（name 为 snap 时抓到的可读名，可缺省）
#[derive(Debug, Clone, PartialEq)]
pub struct RefEntry {
    pub backend_node_id: i64,
    pub name: Option<String>,
}

/// per-tab 的 @eN ref 表。refs 只描述**最后一次 snap**：
/// - [`RefStore::replace`]：每次 snap 整表换 + generation+1 + 清 stale（§3.5）；
/// - [`RefStore::invalidate`]：导航/文档替换时整表清空并置 stale——旧 ref 此后
///   失败是特性（强制重新观察），不是缺陷（§3.6）；
/// - per-tab 由构造保证：每个 tab 一份表，跨 tab 用 ref 在本表必失配 →
///   ref_not_found（§3.7）。
#[derive(Default)]
pub struct RefStore {
    entries: HashMap<String, RefEntry>,
    generation: u64,
    stale: bool,
    last_invalid_reason: Option<String>,
}

impl RefStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 整表替换（snap 调用）。entries 为 JSON 数组，元素形状 = TS
    /// collectSnapshot 的 `[{ ref: 'eN', backendNodeId, name? }]`；null/非数组
    /// 按空表处理（TS `entries || []`）。逐条防御：坏条目跳过、绝不抛错——
    /// 半解析的 AX 树不能弄坏整表交换。
    pub fn replace(&mut self, entries: &Value) {
        let mut map = HashMap::new();
        for entry in entries.as_array().into_iter().flatten() {
            let Some(backend_node_id) = entry.get("backendNodeId").and_then(Value::as_i64) else {
                continue;
            };
            let raw = entry.get("ref").and_then(Value::as_str).unwrap_or_default();
            let key = normalize_ref(raw);
            if !is_ref_key(key) {
                continue;
            }
            // TS `entry.name ? …`：空串名等价于无名
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .map(str::to_string);
            map.insert(
                key.to_string(),
                RefEntry {
                    backend_node_id,
                    name,
                },
            );
        }
        self.entries = map;
        self.generation += 1;
        self.stale = false;
        self.last_invalid_reason = None;
    }

    /// 整表失效（§3.6）。有删才 generation+1 并置 stale；置位后粘滞（空表重复
    /// invalidate 是 no-op，stale 不解除），直到下一次成功 replace。
    /// 返回是否有条目被删除。
    pub fn invalidate(&mut self, reason: &str) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        self.entries.clear();
        self.generation += 1;
        self.stale = true;
        self.last_invalid_reason = Some(reason.to_string());
        true
    }

    /// raw ref（'@e3' 或 'e3'）→ 条目。内部归一化；查表保持大小写敏感。
    pub fn resolve(&self, raw_ref: &str) -> Option<&RefEntry> {
        self.entries.get(normalize_ref(raw_ref))
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn stale(&self) -> bool {
        self.stale
    }

    pub fn last_invalid_reason(&self) -> Option<&str> {
        self.last_invalid_reason.as_deref()
    }
}

// ============================================================================
// 3. 点击几何（TS lib/tools/interact.mjs largestQuadCentroid）
// ============================================================================

/// 视口 CSS 像素坐标
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// 最大四边形的质心：元素盒近似矩形，四顶点均值 ≈ 中心（旋转也成立）。
/// 面积用鞋带公式（叉积和的绝对值一半）；面积 **严格大于** 当前最优才替换
/// （零面积四边形永远不会成为 best → 全退化时返回 None）。
/// 非法 quad（长度 ≠ 8 / 含非有限数）跳过——transform 残渣与遮挡碎片不得
/// 抢占真实盒。
pub fn largest_quad_centroid(quads: &[Vec<f64>]) -> Option<Point> {
    let mut best: Option<Point> = None;
    let mut best_area = 0.0f64;
    for q in quads {
        if q.len() != 8 || q.iter().any(|n| !n.is_finite()) {
            continue;
        }
        let mut area2 = 0.0;
        let (mut cx, mut cy) = (0.0, 0.0);
        for i in 0..4 {
            let (x1, y1) = (q[i * 2], q[i * 2 + 1]);
            let j = ((i + 1) % 4) * 2;
            area2 += x1 * q[j + 1] - q[j] * y1;
            cx += x1;
            cy += y1;
        }
        let area = area2.abs() / 2.0;
        if area > best_area {
            best_area = area;
            best = Some(Point {
                x: cx / 4.0,
                y: cy / 4.0,
            });
        }
    }
    best
}

// ============================================================================
// 4. press 键系统（TS lib/tools/press.mjs + tests/press.test.mjs，SPEC §4.2）
// ============================================================================

/// 修饰键位域（SPEC §2.4/§4.2：alt=1 ctrl=2 meta=4 shift=8）。
/// 数组顺序即规范显示序（alt→ctrl→meta→shift）。
pub const MODIFIER_BITS: [(&str, u8); 4] = [("alt", 1), ("ctrl", 2), ("meta", 4), ("shift", 8)];

/// 修饰键位（便于组合判断，如文本抑制）
pub const MOD_ALT: u8 = 1;
pub const MOD_CTRL: u8 = 2;
pub const MOD_META: u8 = 4;
pub const MOD_SHIFT: u8 = 8;

/// 别名折叠（TS MODIFIER_ALIASES：control/option/opt/cmd/command/super）
fn canonical_modifier(word: &str) -> Option<&'static str> {
    match word {
        "alt" | "option" | "opt" => Some("alt"),
        "ctrl" | "control" => Some("ctrl"),
        "meta" | "cmd" | "command" | "super" => Some("meta"),
        "shift" => Some("shift"),
        _ => None,
    }
}

fn modifier_bit(name: &str) -> u8 {
    MODIFIER_BITS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, b)| *b)
        .unwrap_or(0)
}

/// 位域 → 规范序修饰键显示名（TS modifierNames，首字母大写）
pub fn modifier_names(modifiers: u8) -> Vec<&'static str> {
    MODIFIER_BITS
        .iter()
        .filter(|(_, bit)| modifiers & bit != 0)
        .map(|(name, _)| match *name {
            "alt" => "Alt",
            "ctrl" => "Ctrl",
            "meta" => "Meta",
            _ => "Shift",
        })
        .collect()
}

/// 命名键表（SPEC §4.2 SPECIAL_KEYMAP 全表）。text = 该键产生的字符（仅
/// Enter/Tab/Space）；vk = windowsVirtualKeyCode——读取 e.keyCode 的旧页面依赖它。
/// 小写名匹配（'ENTER'/'pageUp' → 规范名）。
fn named_key(lower: &str) -> Option<(&'static str, &'static str, Option<&'static str>, u16)> {
    Some(match lower {
        "enter" => ("Enter", "Enter", Some("\r"), 13),
        "escape" => ("Escape", "Escape", None, 27),
        "tab" => ("Tab", "Tab", Some("\t"), 9),
        "backspace" => ("Backspace", "Backspace", None, 8),
        "delete" => ("Delete", "Delete", None, 46),
        "home" => ("Home", "Home", None, 36),
        "end" => ("End", "End", None, 35),
        "pageup" => ("PageUp", "PageUp", None, 33),
        "pagedown" => ("PageDown", "PageDown", None, 34),
        "arrowup" => ("ArrowUp", "ArrowUp", None, 38),
        "arrowdown" => ("ArrowDown", "ArrowDown", None, 40),
        "arrowleft" => ("ArrowLeft", "ArrowLeft", None, 37),
        "arrowright" => ("ArrowRight", "ArrowRight", None, 39),
        "space" => (" ", "Space", Some(" "), 32),
        "f1" => ("F1", "F1", None, 112),
        "f2" => ("F2", "F2", None, 113),
        "f3" => ("F3", "F3", None, 114),
        "f4" => ("F4", "F4", None, 115),
        "f5" => ("F5", "F5", None, 116),
        "f6" => ("F6", "F6", None, 117),
        "f7" => ("F7", "F7", None, 118),
        "f8" => ("F8", "F8", None, 119),
        "f9" => ("F9", "F9", None, 120),
        "f10" => ("F10", "F10", None, 121),
        "f11" => ("F11", "F11", None, 122),
        "f12" => ("F12", "F12", None, 123),
        _ => return None,
    })
}

/// 单字符 → (key, code, text, vk)。标点无标准 code/vk——浏览器从 text 合成
/// （TS singleCharDescriptor；vk 按 TS：小写取大写码、其余取自身码、标点无）。
fn single_char_descriptor(ch: char) -> (String, String, Option<String>, Option<u16>) {
    match ch {
        'a'..='z' => (
            ch.to_string(),
            format!("Key{}", ch.to_ascii_uppercase()),
            Some(ch.to_string()),
            Some(ch.to_ascii_uppercase() as u16),
        ),
        'A'..='Z' => (
            ch.to_string(),
            format!("Key{ch}"),
            Some(ch.to_string()),
            Some(ch as u16),
        ),
        '0'..='9' => (
            ch.to_string(),
            format!("Digit{ch}"),
            Some(ch.to_string()),
            Some(ch as u16),
        ),
        _ => (ch.to_string(), String::new(), Some(ch.to_string()), None),
    }
}

/// 解析后的按键描述：三段 dispatch 序列的原料。
/// `code` 空串 = 标点（无标准 code，dispatch 时整体省略）；`vk` None = 标点
/// （同上）；`text` None = 不产生字符的键（Escape 等，无 char 事件）。
#[derive(Debug, Clone, PartialEq)]
pub struct KeyPress {
    /// Space 的 key 字面是 " "（TS 同）
    pub key: String,
    pub code: String,
    pub text: Option<String>,
    pub vk: Option<u16>,
    pub modifiers: u8,
}

impl KeyPress {
    /// 显示名（TS displayKey）：规范序修饰键 + 主键，' ' 显示为 Space。
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = modifier_names(self.modifiers)
            .into_iter()
            .map(String::from)
            .collect();
        parts.push(if self.key == " " {
            "Space".to_string()
        } else {
            self.key.clone()
        });
        parts.join("+")
    }

    /// rawKeyDown →（可选 char）→ keyUp 三段序列（SPEC §4.2 已验证序列，绝不发
    /// 裸 keyDown：char 单独发且不带 vk，文本插入显式且可抑制）。
    /// 文本抑制：ctrl/meta/alt 组合不发 char（Ctrl+A 不能打出 "a"）；shift 不抑制。
    pub fn dispatch_sequence(&self) -> Vec<KeyDispatch> {
        let suppress_text = self.modifiers & (MOD_ALT | MOD_CTRL | MOD_META) != 0;
        let mut seq = Vec::with_capacity(3);
        seq.push(self.event("rawKeyDown"));
        if let Some(text) = &self.text {
            if !text.is_empty() && !suppress_text {
                seq.push(self.event("char"));
            }
        }
        seq.push(self.event("keyUp"));
        seq
    }

    /// 单个 Input.dispatchKeyEvent 参数。char 事件带 text 不带 vk；rawKeyDown/keyUp
    /// 带 vk 不带 text；code 为空串时省略（skip_serializing_if）——字段有无即语义。
    fn event(&self, ty: &'static str) -> KeyDispatch {
        let is_char = ty == "char";
        KeyDispatch {
            r#type: ty,
            key: self.key.clone(),
            code: self.code.clone(),
            text: if is_char { self.text.clone() } else { None },
            windows_virtual_key_code: if is_char { None } else { self.vk },
            modifiers: self.modifiers,
        }
    }
}

/// Input.dispatchKeyEvent 的参数形态（序列化后字段集与 TS 逐字段一致）
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct KeyDispatch {
    pub r#type: &'static str,
    pub key: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows_virtual_key_code: Option<u16>,
    pub modifiers: u8,
}

/// '<key>' | 'Mod+Mod+Key' → [`KeyPress`]（TS keyDescriptor，SPEC parseKeySpec）。
/// 解析规则逐字对齐 TS：按 '+' 切分、trim、丢空 token；≤1 个剩余 token 时整个
/// 串（trim 后）作主键（"+"/" +" 即主键 '+'）；多 token 时首个非修饰 token 作主键，
/// 全修饰输入取最后一个 token（随后必败于命名键查找——裸按修饰键不支持）。
/// 主键为单字符（UTF-16 单元，与 JS length===1 一致）或命名键（大小写不敏感）。
pub fn parse_key(spec: &str) -> Result<KeyPress, PageControlError> {
    let tokens: Vec<&str> = spec
        .split('+')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    let mut modifiers: u8 = 0;
    let base: String = if tokens.len() <= 1 {
        spec.trim().to_string()
    } else {
        let mut non_mods: Vec<&str> = Vec::new();
        for t in &tokens {
            match canonical_modifier(&t.to_lowercase()) {
                Some(name) => modifiers |= modifier_bit(name),
                None => non_mods.push(t),
            }
        }
        match non_mods.len() {
            0 => tokens.last().copied().unwrap_or_default().to_string(),
            1 => non_mods[0].to_string(),
            _ => return Err(PageControlError::key_unknown(spec)),
        }
    };
    if base.is_empty() {
        return Err(PageControlError::key_unknown(spec));
    }

    let (key, code, text, vk) = if base.encode_utf16().count() == 1 {
        let ch = base.chars().next().expect("utf16 单元为 1 必有首字符");
        let (k, c, t, v) = single_char_descriptor(ch);
        (k, c, t, v)
    } else {
        match named_key(&base.to_lowercase()) {
            Some((k, c, t, v)) => (k.to_string(), c.to_string(), t.map(String::from), Some(v)),
            None => return Err(PageControlError::key_unknown(spec)),
        }
    };
    Ok(KeyPress {
        key,
        code,
        text,
        vk,
        modifiers,
    })
}

// ============================================================================
// 5. wait 系统（TS lib/tools/wait.mjs + tests/wait.test.mjs）
// ============================================================================

/// --state 白名单（顺序即报错文案顺序：visible|hidden|attached|detached）
pub const WAIT_STATES: [&str; 4] = ["visible", "hidden", "attached", "detached"];

/// 期望状态（--state）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitState {
    Visible,
    Hidden,
    Attached,
    Detached,
}

impl WaitState {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "visible" => Self::Visible,
            "hidden" => Self::Hidden,
            "attached" => Self::Attached,
            "detached" => Self::Detached,
            _ => return None,
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Hidden => "hidden",
            Self::Attached => "attached",
            Self::Detached => "detached",
        }
    }
}

/// 一次 wait 观测的结果（wait_observe_expr 返回值 / eval 失败 ≙ Detached /
/// 返回垃圾 ≙ Unknown——"unknown" 永不满足任何状态，除 attached 外，见下）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedState {
    Visible,
    Hidden,
    Detached,
    Unknown,
}

impl ObservedState {
    pub fn parse(s: &str) -> Self {
        match s {
            "visible" => Self::Visible,
            "hidden" => Self::Hidden,
            "detached" => Self::Detached,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Hidden => "hidden",
            Self::Detached => "detached",
            Self::Unknown => "unknown",
        }
    }
}

/// 观测是否满足期望（TS satisfied()）。hidden = 已挂载但不可见（detached ≠
/// hidden）；attached 接受任何**非** detached 观测——TS 逐字是
/// `observed !== 'detached'`，故 Unknown 也满足 attached（原样保留）。
pub fn observed_matches_state(observed: ObservedState, wanted: WaitState) -> bool {
    match wanted {
        WaitState::Visible => observed == ObservedState::Visible,
        WaitState::Detached => observed == ObservedState::Detached,
        WaitState::Attached => observed != ObservedState::Detached,
        WaitState::Hidden => observed == ObservedState::Hidden,
    }
}

/// '10s' | '250ms' | '500' → Duration（裸数字 = 毫秒；小数允许）。
/// 非法输入报 400，文案带 flag 名（`--timeout: invalid duration "abc" — …`）。
/// TS 对天文数字不做上限；Duration 装不下时（try_from_secs_f64 Err）按非法处理
/// ——与 TS 的差异仅出现在荒谬量级（>Duration 上限），换来不 panic。
pub fn parse_duration(input: &str, what: &str) -> Result<Duration, AppError> {
    let invalid = || {
        AppError::invalid_request(format!(
            "{what}: invalid duration \"{input}\" — use a number of ms or e.g. 10s / 250ms / 2m"
        ))
    };
    let s = input.trim();
    // 数字前缀 + 单位后缀切分：'^(\d+(\.\d+)?)(ms|s|m)?$'
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    if !is_plain_number(num) {
        return Err(invalid());
    }
    let n: f64 = num.parse().map_err(|_| invalid())?;
    let ms = match unit {
        "" | "ms" => n,
        "s" => n * 1000.0,
        "m" => n * 60_000.0,
        _ => return Err(invalid()),
    };
    if !ms.is_finite() || ms < 0.0 {
        return Err(invalid());
    }
    Duration::try_from_secs_f64(ms / 1000.0).map_err(|_| invalid())
}

/// ^\d+(\.\d+)?$（f64 会接受 "1."/"1e5" 等形态，须先做结构校验）
fn is_plain_number(num: &str) -> bool {
    let mut chars = num.chars().peekable();
    if !chars.peek().is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }
    while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
        chars.next();
    }
    if chars.peek() == Some(&'.') {
        chars.next();
        if !chars.peek().is_some_and(|c| c.is_ascii_digit()) {
            return false;
        }
        while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
            chars.next();
        }
    }
    chars.peek().is_none()
}

// ============================================================================
// 5b. 导航 URL 白名单（page/navigate 与 instance 级 tabs/navigate 共用）
// ============================================================================

/// http/https 白名单校验（原 instance_service 私有逻辑上移共享：page/navigate
/// 与实例级 open_tab/navigate 必须同源校验，防两处白名单漂移）。
/// 错误形态（码/文案）与上移前逐字一致——实例级端点的对外契约不变。
pub fn validate_http_url(url: &str) -> Result<(), AppError> {
    let parsed = url::Url::parse(url)
        .map_err(|_| AppError::invalid_request(format!("URL 不合法: {url}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AppError::invalid_request("URL 必须 http/https 开头"));
    }
    Ok(())
}

// ============================================================================
// 6. eval 表达式生成器（TS 内嵌 JS → Rust 字符串生成，语义逐字移植）
// ============================================================================

/// JSON.stringify 等价（含两侧引号）。所有生成器用它转义 selector 等用户输入，
/// 转义结果是一个完整 JSON 字符串字面量，页内求值无注入面（TS 同规则）。
pub fn json_escape(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// 定位 + 滚动居中 + 可见性检查 + 中心点计算，一个 eval 内原子完成
/// （TS click-native.mjs locateExpr）。四重可见性检查：display:none、
/// visibility:hidden、空 rect、无 offsetParent（fixed/sticky 豁免——它们
/// offsetParent 为 null 但完全可见）。
pub fn locate_expr(selector: &str) -> String {
    const TEMPLATE: &str = r#"(function() {
  const el = document.querySelector(__SEL__);
  if (!el) return JSON.stringify({ error: 'not_found' });
  el.scrollIntoView({ block: 'center' });
  const rect = el.getBoundingClientRect();
  const style = getComputedStyle(el);
  if (style.display === 'none') return JSON.stringify({ error: 'not_visible', why: 'display:none' });
  if (style.visibility === 'hidden') return JSON.stringify({ error: 'not_visible', why: 'visibility:hidden' });
  if (rect.width <= 0 || rect.height <= 0) return JSON.stringify({ error: 'not_visible', why: 'empty rect ' + Math.round(rect.width) + 'x' + Math.round(rect.height) });
  // offsetParent is null for unrendered elements — but also for position:fixed,
  // which is perfectly visible, so exempt fixed/sticky.
  if (!el.offsetParent && style.position !== 'fixed' && style.position !== 'sticky') return JSON.stringify({ error: 'not_visible', why: 'not rendered (no offsetParent)' });
  return JSON.stringify({
    x: rect.left + rect.width / 2,
    y: rect.top + rect.height / 2,
    tag: el.tagName,
    text: (el.textContent || '').trim().substring(0, 80),
  });
})()"#;
    TEMPLATE.replace("__SEL__", &json_escape(selector))
}

/// hitTestWalk 源码（TS 同名导出函数的 toString() 形态）：从命中元素沿
/// parentNode 与 shadow-DOM host 边界上行（§2.5.B），目标自身或其后代算命中。
const HIT_TEST_WALK: &str = r#"function hitTestWalk(hit, target) {
  for (let n = hit; n; ) {
    if (n === target || target.contains(n)) return true;
    const root = n.getRootNode();
    n = n.parentNode || (root && root !== n ? root.host : null);
  }
  return false;
}"#;

/// 点击点命中测试：elementFromPoint + 祖先链/shadow 穿透（TS hitTestExpr）。
/// 每次 eval 之间无对象同一性，故重新按 selector 查目标。`x`/`y` 必须是有限数
/// （调用方先行 classifyLocate 校验，与 TS 同契约）。
pub fn hit_test_expr(selector: &str, x: f64, y: f64) -> String {
    const TEMPLATE: &str = r#"(function() {
  const target = document.querySelector(__SEL__);
  if (!target) return JSON.stringify({ hit: false, reason: 'target-gone' });
  const el = document.elementFromPoint(__X__, __Y__);
  if (!el) return JSON.stringify({ hit: false, reason: 'nothing-at-point' });
  if ((__WALK__)(el, target)) return JSON.stringify({ hit: true });
  return JSON.stringify({ hit: false, reason: 'covered', by: el.tagName + (el.className ? '.' + String(el.className) : '') });
})()"#;
    TEMPLATE
        .replace("__SEL__", &json_escape(selector))
        .replace("__X__", &format!("{x}"))
        .replace("__Y__", &format!("{y}"))
        .replace("__WALK__", HIT_TEST_WALK)
}

/// fill 可填性检查脚本（TS fill.mjs 的 checkFn + fillEditable 内联）。
/// 以 `this` = 目标元素运行（Runtime.callFunctionOn on objectId）：
/// 白名单 text 型 input / textarea / contenteditable **编辑器根**；
/// readonly/disabled 拒绝；编辑宿内的后代拒绝（要填根）。
pub fn fill_fillable_check_expr() -> String {
    const TEMPLATE: &str = r#"function() {
  const check = (function fillEditable(el) {
  if (!el || !el.isConnected) return { editable: false, why: 'element is not in the document' };
  const disabled = el.matches(':disabled'); // covers ancestor <fieldset disabled> too
  if (el.tagName === 'TEXTAREA') {
    if (disabled) return { editable: false, why: 'disabled' };
    if (el.readOnly) return { editable: false, why: 'readonly' };
    return { editable: true, kind: 'native' };
  }
  if (el.tagName === 'INPUT') {
    const type = (el.getAttribute('type') || 'text').toLowerCase();
    const TEXTUAL = ['text', 'search', 'tel', 'url', 'email', 'password', 'number'];
    if (!TEXTUAL.includes(type)) {
      return { editable: false, why: `input type "${type}" is not fillable (use a text-like input)` };
    }
    if (disabled) return { editable: false, why: 'disabled' };
    if (el.readOnly) return { editable: false, why: 'readonly' };
    return { editable: true, kind: 'native' };
  }
  if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) {
    return { editable: true, kind: 'editable' };
  }
  return {
    editable: false,
    why: el.isContentEditable
      ? 'not the contenteditable editor root (fill the editing host itself instead)'
      : 'not a text input, textarea, or contenteditable editor root',
  };
})(this);
  if (!check.editable) return { not_fillable: check.why };
  const label = this.getAttribute('aria-label') || this.getAttribute('name')
    || this.getAttribute('placeholder') || this.id
    || (this.textContent || '').trim().substring(0, 40);
  return { tag: this.tagName, kind: check.kind, label: label || '' };
}"#;
    TEMPLATE.to_string()
}

/// wait 单次观测脚本：detached / visible / hidden 三态判定（TS wait.mjs waitExpr）。
/// 可见性语义与 locate_expr 的检查同源：非零 rect 且非 display:none /
/// visibility:hidden（display:none 祖先也会把 rect 归零，查元素自身即够）。
pub fn wait_observe_expr(selector: &str) -> String {
    const TEMPLATE: &str = r#"(function() {
  const el = document.querySelector(__SEL__);
  if (!el) return JSON.stringify({ observed: 'detached' });
  const rect = el.getBoundingClientRect();
  const style = getComputedStyle(el);
  const visible = rect.width > 0 && rect.height > 0
    && style.display !== 'none' && style.visibility !== 'hidden';
  return JSON.stringify({ observed: visible ? 'visible' : 'hidden' });
})()"#;
    TEMPLATE.replace("__SEL__", &json_escape(selector))
}

// ============================================================================
// 7. 对话框模型（TS lib/dialogs.mjs + tests/dialogs.test.mjs，BEHAVIOR-SPEC §1）
// ============================================================================

const MAX_FIELD_LEN: usize = 4096;
/// 缓冲上限（§1.5：32 条 FIFO，丢最旧、无丢失标记——比游标老的 cursor 只见幸存者）
pub const MAX_DIALOG_BUFFER: usize = 32;

/// Page.javascriptDialogOpening 参数的防御解析结果（§1.3：类型错误降级，不抛）
#[derive(Debug, Clone, PartialEq)]
pub struct DialogInfo {
    /// alert | confirm | prompt | beforeunload（未知/缺失 → alert）
    pub r#type: String,
    /// 字符串才保留；非字符串（如 beforeunload 的 message:false）→ ""
    pub message: String,
    pub url: Option<String>,
    pub default_prompt: Option<String>,
    pub has_browser_handler: Option<bool>,
}

/// stringField：非字符串 → None（undefined）；超 4096 → 截断加标记。
/// 按 chars 截断（TS 按 UTF-16 单元；对有效 str 语义一致，多字节安全）。
fn string_field(value: Option<&Value>) -> Option<String> {
    let s = value.and_then(Value::as_str)?;
    if s.chars().count() > MAX_FIELD_LEN {
        Some(s.chars().take(MAX_FIELD_LEN).collect::<String>() + "... [truncated]")
    } else {
        Some(s.to_string())
    }
}

/// §1.3 防御解析：类型校验降级不抛错。beforeunload 在部分 Chrome 上
/// message:false ——"仅字符串"规则把它变成 ""（不得进缓冲变成 "false"）。
pub fn parse_dialog_opening(params: &Value) -> DialogInfo {
    let ty = params
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    DialogInfo {
        r#type: if matches!(ty, "alert" | "confirm" | "prompt" | "beforeunload") {
            ty.to_string()
        } else {
            "alert".to_string()
        },
        message: string_field(params.get("message")).unwrap_or_default(),
        url: string_field(params.get("url")),
        default_prompt: string_field(params.get("defaultPrompt")),
        has_browser_handler: params.get("hasBrowserHandler").and_then(Value::as_bool),
    }
}

/// 缓冲条目（wire 形态：蛇形字段与 TS 完全一致，可缺省字段省略）
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DialogEntry {
    pub tab_id: String,
    pub r#type: String,
    pub message: String,
    /// 固定 "accepted"（本 CLI 无合作模式，§1.4 无条件 accept）
    pub handled: &'static str,
    pub sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_browser_handler: Option<bool>,
}

/// per-tab 对话框缓冲（TS DialogTracker）。**协议**：`record` 只允许在
/// Page.handleJavaScriptDialog **成功后**调用（§1.5）——sequence 只随成功前移，
/// 失败路径不得调用本方法，对话框保持打开、CDP 命令继续阻塞直至各自超时
/// （与上游一致）。游标语义见 [`DialogBuffer::since`]（§1.6）。
pub struct DialogBuffer {
    tab_id: String,
    entries: VecDeque<DialogEntry>,
    sequence: u64,
    /// FIFO 逐出累计数（dialogs 响应 meta 透出——游标早于缓冲起点时调用方
    /// 至少能知道丢了多少，而非静默只见幸存者）
    dropped: u64,
}

impl DialogBuffer {
    pub fn new(tab_id: impl Into<String>) -> Self {
        Self {
            tab_id: tab_id.into(),
            entries: VecDeque::new(),
            sequence: 0,
            dropped: 0,
        }
    }

    /// 当前游标（命令入口打标用，§1.6）
    pub fn cursor(&self) -> u64 {
        self.sequence
    }

    /// 成功应答后归档（协议见类型注释）。返回入档条目。FIFO 超 32 丢最旧。
    pub fn record(&mut self, info: DialogInfo) -> DialogEntry {
        self.sequence += 1;
        let entry = DialogEntry {
            tab_id: self.tab_id.clone(),
            r#type: info.r#type,
            message: info.message,
            handled: "accepted",
            sequence: self.sequence,
            url: info.url,
            default_prompt: info.default_prompt,
            has_browser_handler: info.has_browser_handler,
        };
        self.entries.push_back(entry.clone());
        while self.entries.len() > MAX_DIALOG_BUFFER {
            self.entries.pop_front();
            self.dropped += 1;
        }
        entry
    }

    /// 累计逐出条数（缓冲创建起算，只增不清零；响应 meta 证据标记）
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// sequence 严格大于 cursor 的条目（§1.6）。游标早于缓冲起点时只见幸存者；
    /// 被逐出的总数由 meta.dropped 累计透出，逐出与查询间的关系可由
    /// entries[0].sequence 与 cursor 的差值推断。
    pub fn since(&self, cursor: u64) -> Vec<&DialogEntry> {
        self.entries
            .iter()
            .filter(|e| e.sequence > cursor)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Page.handleJavaScriptDialog 应答参数（§1.4 无条件 accept；beforeunload 的
/// accept 即"允许离开"）。prompt 必带 promptText = defaultPrompt ?? ""。
pub fn dialog_accept_params(info: &DialogInfo) -> Value {
    if info.r#type == "prompt" {
        json!({ "accept": true, "promptText": info.default_prompt.clone().unwrap_or_default() })
    } else {
        json!({ "accept": true })
    }
}

// ============================================================================
// 测试：Phase 1 TS 用例全量移植（每条标注来源文件与用例名）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------------- validate_http_url（导航 URL 白名单）----------------------

    /// http/https 白名单与拒绝面：实例级 open_tab/navigate 与 page/navigate
    /// 共用同一校验，这里钉住“哪些 scheme 被拒”——特别是 javascript:/file:/data:
    /// （可执行/本地文件面），防未来某处单独放宽。
    #[test]
    fn validate_http_url_whitelists_schemes() {
        for ok in [
            "http://example.com",
            "https://example.com/path?q=1",
            "http://127.0.0.1:8080/",
        ] {
            assert!(validate_http_url(ok).is_ok(), "应放行: {ok}");
        }
        for bad in [
            "",
            "not a url",
            "ftp://example.com",
            "file:///etc/hosts",
            "javascript:alert(1)",
            "data:text/html,<script>",
            "chrome://settings",
            "about:blank",
            "example.com/no-scheme",
        ] {
            let e = validate_http_url(bad).unwrap_err();
            assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400), "应拒绝: {bad}");
        }
        // 拒绝面双方文案：非法解析 vs scheme 白名单（对外契约逐字保持）
        assert!(validate_http_url("javascript:x")
            .unwrap_err()
            .to_string()
            .contains("必须 http/https"));
        assert!(validate_http_url(":::")
            .unwrap_err()
            .to_string()
            .contains("URL 不合法"));
    }

    // ---------------- RefStore / normalize_ref（tests/ref-store.test.mjs）------

    /// TS "normalizeRef strips exactly one leading @, keeps case"
    #[test]
    fn normalize_ref_strips_exactly_one_leading_at_and_keeps_case() {
        assert_eq!(normalize_ref("@e3"), "e3");
        assert_eq!(normalize_ref("e3"), "e3");
        assert_eq!(normalize_ref("@@e3"), "@e3", "只剥一个 @——查表必失配");
        assert_eq!(normalize_ref("@E3"), "E3", "大小写敏感，从不折叠");
        assert_eq!(normalize_ref("E3"), "E3");
    }

    /// TS "replace swaps the whole table and bumps generation; resolve takes raw or @-prefixed refs"
    #[test]
    fn replace_swaps_whole_table_and_bumps_generation() {
        let mut store = RefStore::new();
        assert_eq!(store.generation(), 0);
        store.replace(&json!([
            { "ref": "e1", "backendNodeId": 101, "name": "OK" },
            { "ref": "e2", "backendNodeId": 102 },
        ]));
        assert_eq!(store.generation(), 1);
        assert_eq!(store.len(), 2);

        assert_eq!(
            store.resolve("e1"),
            Some(&RefEntry {
                backend_node_id: 101,
                name: Some("OK".into())
            })
        );
        assert_eq!(
            store.resolve("@e2"),
            Some(&RefEntry {
                backend_node_id: 102,
                name: None
            }),
            "resolve 内部归一化"
        );

        // 整表换：新 snap 更少节点时，旧 ref 也必须死
        store.replace(&json!([{ "ref": "e1", "backendNodeId": 201 }]));
        assert_eq!(store.generation(), 2);
        assert_eq!(store.resolve("e2"), None);
        assert_eq!(store.resolve("e1").map(|e| e.backend_node_id), Some(201));
    }

    /// TS "resolve is case-sensitive per §3.7: E3/@E3 never match e3"
    #[test]
    fn resolve_is_case_sensitive() {
        let mut store = RefStore::new();
        store.replace(&json!([{ "ref": "e3", "backendNodeId": 33 }]));
        assert_eq!(store.resolve("E3"), None);
        assert_eq!(store.resolve("@E3"), None);
    }

    /// TS "invalidate drops all entries, flags stale with reason; replace clears stale"
    #[test]
    fn invalidate_drops_all_entries_and_flags_stale_until_replace() {
        let mut store = RefStore::new();
        store.replace(&json!([{ "ref": "e1", "backendNodeId": 101 }]));

        assert!(store.invalidate("navigation"));
        assert_eq!(store.len(), 0);
        assert_eq!(store.generation(), 2);
        assert!(store.stale());
        assert_eq!(store.last_invalid_reason(), Some("navigation"));
        assert_eq!(store.resolve("e1"), None);

        // 空表重复 invalidate：no-op，stale 粘滞
        assert!(!store.invalidate("document updated"));
        assert!(store.stale());
        assert_eq!(
            store.last_invalid_reason(),
            Some("navigation"),
            "reason 保持首次失效的"
        );

        store.replace(&json!([{ "ref": "e1", "backendNodeId": 301 }]));
        assert!(!store.stale());
        assert_eq!(store.last_invalid_reason(), None);
        assert_eq!(store.resolve("e1").map(|e| e.backend_node_id), Some(301));
    }

    /// TS "invalidate on a never-populated store neither bumps generation nor flags stale"
    #[test]
    fn invalidate_on_never_populated_store_changes_nothing() {
        let mut store = RefStore::new();
        assert!(!store.invalidate("document changed"));
        assert_eq!(store.generation(), 0);
        assert!(!store.stale());
    }

    /// TS "replace skips malformed entries defensively instead of throwing"
    #[test]
    fn replace_skips_malformed_entries_defensively() {
        let mut store = RefStore::new();
        store.replace(&json!([
            null,
            { "ref": "e1" },                         // 无 backendNodeId
            { "ref": "e2", "backendNodeId": "x" },   // 非整数 backendNodeId
            { "ref": "button", "backendNodeId": 5 },  // 键非 eN 形
            { "ref": "E5", "backendNodeId": 6 },      // 大写——正则区分大小写
            { "ref": "e4", "backendNodeId": 7, "name": "" }, // falsy 名等价无名
        ]));
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.resolve("e4"),
            Some(&RefEntry {
                backend_node_id: 7,
                name: None
            })
        );
    }

    /// TS interact.mjs refMissError（press/fill 的 ref 未命中路径共用）
    #[test]
    fn ref_miss_error_picks_stale_or_not_found_by_table_state() {
        let mut store = RefStore::new();
        store.replace(&json!([{ "ref": "e1", "backendNodeId": 1 }]));
        let miss = PageControlError::ref_miss(&store, "@e9");
        assert_eq!((miss.code, miss.status), ("PAGE_REF_NOT_FOUND", 404));
        assert!(miss.message.contains("@e9"));

        store.invalidate("navigation");
        let stale = PageControlError::ref_miss(&store, "@e9");
        assert_eq!((stale.code, stale.status), ("PAGE_REF_STALE", 409));
        assert!(stale.message.contains("navigation"));
    }

    // ---------------- 点击几何（tests/interact.test.mjs）----------------------

    /// TS "ref click picks the LARGEST quad (occlusion/transform leftovers discarded)"
    #[test]
    fn largest_quad_centroid_picks_largest_over_junk() {
        let quads = vec![
            vec![0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0], // 1x1 残渣
            vec![100.0, 100.0, 200.0, 100.0, 200.0, 160.0, 100.0, 160.0], // 100x60 真实盒
        ];
        // 最大 = 100x60 @ (100,100) → 质心 (150,130)；1x1 垃圾（零可见残渣）必须输
        assert_eq!(
            largest_quad_centroid(&quads),
            Some(Point { x: 150.0, y: 130.0 })
        );
    }

    /// TS "clickStr routes @eN through the ref chain and clicks the quad centroid"（(5,5)）
    #[test]
    fn largest_quad_centroid_takes_vertex_mean() {
        let quads = vec![vec![0.0, 0.0, 10.0, 0.0, 10.0, 10.0, 0.0, 10.0]];
        assert_eq!(
            largest_quad_centroid(&quads),
            Some(Point { x: 5.0, y: 5.0 })
        );
    }

    /// TS largestQuadCentroid 的 continue 守卫：长度 ≠ 8 / 非有限数跳过
    #[test]
    fn largest_quad_centroid_skips_invalid_quads() {
        let quads = vec![
            vec![0.0, 0.0, 10.0, 0.0],                             // 长度 ≠ 8
            vec![0.0, f64::NAN, 10.0, 0.0, 10.0, 10.0, 0.0, 10.0], // 非有限数
            vec![0.0, 0.0, 10.0, 0.0, 10.0, 10.0, 0.0, 10.0],      // 合法
        ];
        assert_eq!(
            largest_quad_centroid(&quads),
            Some(Point { x: 5.0, y: 5.0 })
        );
    }

    /// TS resolveRefPoint：quads 空 / 全退化 → None → 走 rect 回退链
    #[test]
    fn largest_quad_centroid_none_when_empty_or_degenerate() {
        assert_eq!(largest_quad_centroid(&[]), None);
        let flat = vec![vec![0.0, 0.0, 10.0, 0.0, 10.0, 0.0, 0.0, 0.0]]; // 零面积
        assert_eq!(
            largest_quad_centroid(&flat),
            None,
            "面积须严格大于 0 才能成为 best"
        );
    }

    // ---------------- press 键系统（tests/press.test.mjs）--------------------

    /// TS "keyDescriptor: named keys carry key/code/text/vk from the SPEC table"
    #[test]
    fn named_keys_carry_key_code_text_vk() {
        assert_eq!(
            parse_key("Enter").unwrap(),
            KeyPress {
                key: "Enter".into(),
                code: "Enter".into(),
                text: Some("\r".into()),
                vk: Some(13),
                modifiers: 0,
            }
        );
        assert_eq!(parse_key("Tab").unwrap().text, Some("\t".into()));
        let space = parse_key("Space").unwrap();
        assert_eq!(space.key, " ");
        assert_eq!(space.code, "Space");
        assert_eq!(space.text, Some(" ".into()));
        assert_eq!(parse_key("Escape").unwrap().text, None, "Escape 不产生字符");
        assert_eq!(parse_key("F1").unwrap().vk, Some(112));
        assert_eq!(parse_key("F12").unwrap().vk, Some(123));
        assert_eq!(parse_key("ArrowDown").unwrap().vk, Some(40));
    }

    /// TS "keyDescriptor: named-key lookup is case-insensitive"
    #[test]
    fn named_key_lookup_is_case_insensitive() {
        assert_eq!(parse_key("enter").unwrap().key, "Enter");
        assert_eq!(parse_key("PAGEUP").unwrap().code, "PageUp");
        assert_eq!(parse_key("Escape").unwrap().key, "Escape");
    }

    /// TS "keyDescriptor: single characters map to Key*/Digit* codes with text"
    #[test]
    fn single_chars_map_to_key_digit_codes() {
        assert_eq!(
            parse_key("a").unwrap(),
            KeyPress {
                key: "a".into(),
                code: "KeyA".into(),
                text: Some("a".into()),
                vk: Some(65),
                modifiers: 0,
            }
        );
        assert_eq!(
            parse_key("A").unwrap(),
            KeyPress {
                key: "A".into(),
                code: "KeyA".into(),
                text: Some("A".into()),
                vk: Some(65),
                modifiers: 0,
            }
        );
        let seven = parse_key("7").unwrap();
        assert_eq!(seven.code, "Digit7");
        assert_eq!(seven.vk, Some(55));
    }

    /// TS 同上（标点部分）：无标准 code——text 承载字符；裸 '+' 仍是单键
    #[test]
    fn punctuation_carries_text_without_code_or_vk() {
        let comma = parse_key(",").unwrap();
        assert_eq!(comma.code, "");
        assert_eq!(comma.text, Some(",".into()));
        assert_eq!(comma.vk, None);
        // '+' 切分后 token 全空 → 整串为主键
        assert_eq!(parse_key("+").unwrap().text, Some("+".into()));
    }

    /// TS "keyDescriptor: modifier combos set the bit field (alt=1 ctrl=2 meta=4 shift=8)"
    #[test]
    fn modifier_combos_set_bitfield() {
        assert_eq!(parse_key("Control+A").unwrap().modifiers, MOD_CTRL);
        assert_eq!(parse_key("Meta+V").unwrap().modifiers, MOD_META);
        assert_eq!(parse_key("Shift+ArrowDown").unwrap().modifiers, MOD_SHIFT);
        let alt_f4 = parse_key("Alt+F4").unwrap();
        assert_eq!(alt_f4.modifiers, MOD_ALT);
        assert_eq!(alt_f4.key, "F4");
    }

    /// TS 同上（别名部分）：折叠到规范名、重复塌缩、规范序输出
    #[test]
    fn modifier_aliases_fold_canonically_and_duplicates_collapse() {
        assert_eq!(
            modifier_names(parse_key("ctrl+a").unwrap().modifiers),
            vec!["Ctrl"]
        );
        assert_eq!(
            modifier_names(parse_key("CMD+a").unwrap().modifiers),
            vec!["Meta"]
        );
        assert_eq!(
            modifier_names(parse_key("option+enter").unwrap().modifiers),
            vec!["Alt"]
        );
        assert_eq!(
            modifier_names(parse_key("super+a").unwrap().modifiers),
            vec!["Meta"]
        );
        assert_eq!(
            parse_key("Control+Ctrl+A").unwrap().modifiers,
            2,
            "重复修饰塌缩为一位"
        );
        // 规范序 alt→ctrl→meta→shift，与输入顺序无关
        let all = parse_key("Shift+Ctrl+Alt+Meta+a").unwrap().modifiers;
        assert_eq!(modifier_names(all), vec!["Alt", "Ctrl", "Meta", "Shift"]);
    }

    /// TS "keyDescriptor: unknown keys → key_unknown with the supported-key hint"
    #[test]
    fn unknown_keys_reject_with_hint() {
        for bad in ["Foo", "Control", "Control+Shift", "A+B", ""] {
            let err = parse_key(bad).unwrap_err();
            assert_eq!(
                (err.code, err.status),
                ("PAGE_KEY_UNKNOWN", 400),
                "bad={bad}"
            );
            assert!(
                err.message.contains("Enter"),
                "提示语列出命名键: {}",
                err.message
            );
        }
    }

    /// dispatch 序列的 JSON 形态（= TS pressStr 里 FakeCdp.callsOf 断言的参数）
    fn dispatch_json(kp: &KeyPress) -> Vec<Value> {
        kp.dispatch_sequence()
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect()
    }

    /// TS "pressStr: Enter = rawKeyDown(vk) → char(text, no vk) → keyUp(vk)"
    #[test]
    fn dispatch_enter_is_rawkeydown_char_keyup() {
        let events = dispatch_json(&parse_key("Enter").unwrap());
        assert_eq!(
            events[0],
            json!({ "type": "rawKeyDown", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "modifiers": 0 })
        );
        // char 事件带 text、按 SPEC 不带 vk
        assert_eq!(
            events[1],
            json!({ "type": "char", "key": "Enter", "code": "Enter", "text": "\r", "modifiers": 0 })
        );
        assert_eq!(
            events[2],
            json!({ "type": "keyUp", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "modifiers": 0 })
        );
    }

    /// TS "pressStr: keys without text (Escape) skip the char event"
    #[test]
    fn dispatch_keys_without_text_skip_char() {
        let events = dispatch_json(&parse_key("Escape").unwrap());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "rawKeyDown");
        assert_eq!(events[1]["type"], "keyUp");
    }

    /// TS "pressStr: ctrl/meta/alt suppress the char; shift does not"
    #[test]
    fn dispatch_ctrl_meta_alt_suppress_char_but_shift_does_not() {
        let ctrl = dispatch_json(&parse_key("Control+A").unwrap());
        assert_eq!(ctrl.len(), 2, "Ctrl+A 无 char——不得打出 \"a\"");
        for e in &ctrl {
            assert_eq!(e["modifiers"], 2);
        }

        assert_eq!(dispatch_json(&parse_key("Alt+F4").unwrap()).len(), 2);

        let shift = dispatch_json(&parse_key("Shift+Tab").unwrap());
        assert_eq!(shift.len(), 3, "shift 不抑制 char");
        assert_eq!(
            shift[1],
            json!({ "type": "char", "key": "Tab", "code": "Tab", "text": "\t", "modifiers": 8 })
        );
    }

    /// TS singleCharDescriptor 的 dispatch 形态：标点无 code/vk——字段整体省略
    #[test]
    fn dispatch_punctuation_omits_code_and_vk_fields() {
        let events = dispatch_json(&parse_key(",").unwrap());
        assert_eq!(
            events[0],
            json!({ "type": "rawKeyDown", "key": ",", "modifiers": 0 })
        );
        assert_eq!(
            events[1],
            json!({ "type": "char", "key": ",", "text": ",", "modifiers": 0 })
        );
        assert_eq!(
            events[2],
            json!({ "type": "keyUp", "key": ",", "modifiers": 0 })
        );
    }

    /// TS displayKey：规范序修饰键 + 主键，' ' 显示为 Space
    #[test]
    fn display_uses_canonical_order_and_space_label() {
        assert_eq!(parse_key("Enter").unwrap().display(), "Enter");
        assert_eq!(parse_key("Control+A").unwrap().display(), "Ctrl+A");
        assert_eq!(parse_key("Space").unwrap().display(), "Space");
        assert_eq!(
            parse_key("shift+ctrl+a").unwrap().display(),
            "Ctrl+Shift+a",
            "主键保留原大小写"
        );
    }

    // ---------------- wait 系统（tests/wait.test.mjs）------------------------

    /// TS "parseDuration: bare numbers are ms; units ms/s/m scale; decimals ok"
    #[test]
    fn parse_duration_units_decimals_and_whitespace() {
        assert_eq!(
            parse_duration("10s", "--timeout").unwrap(),
            Duration::from_millis(10_000)
        );
        assert_eq!(
            parse_duration("250ms", "--interval").unwrap(),
            Duration::from_millis(250)
        );
        assert_eq!(
            parse_duration("500", "--timeout").unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(
            parse_duration("1.5s", "--timeout").unwrap(),
            Duration::from_millis(1500)
        );
        assert_eq!(
            parse_duration("2m", "--timeout").unwrap(),
            Duration::from_millis(120_000)
        );
        assert_eq!(
            parse_duration(" 10s ", "--timeout").unwrap(),
            Duration::from_millis(10_000)
        );
    }

    /// TS "parseDuration: garbage and negatives throw with the flag name"
    #[test]
    fn parse_duration_rejects_garbage_and_negatives_with_flag_name() {
        for bad in ["abc", "-5", "10x", "", "s", "NaN", "1.", ".5", "1.5.5"] {
            let err = parse_duration(bad, "--timeout").unwrap_err();
            assert_eq!(err.code(), "INVALID_REQUEST", "bad={bad}");
            let msg = err.to_string();
            assert!(msg.contains("--timeout"), "报错带 flag 名: {msg}");
            assert!(msg.contains("invalid duration"));
        }
    }

    /// TS "waitExpr observes attachment and the locateExpr visibility semantics"
    #[test]
    fn wait_observe_expr_embeds_selector_and_visibility_semantics() {
        let expr = wait_observe_expr(".x");
        assert!(expr.contains(r#"document.querySelector(".x")"#));
        assert!(expr.contains("getBoundingClientRect"));
        assert!(expr.contains("style.display !== 'none'"));
        assert!(expr.contains("style.visibility !== 'hidden'"));
        assert!(expr.contains("detached"));
    }

    /// TS satisfied()（waitStr 各状态用例的判定核心）：
    /// visible 只认可见；detached 只认缺席；attached 认一切已挂载；hidden 认挂载但不可见
    #[test]
    fn observed_matches_state_matrix() {
        use ObservedState as O;
        use WaitState as W;

        assert!(observed_matches_state(O::Visible, W::Visible));
        assert!(!observed_matches_state(O::Hidden, W::Visible));

        assert!(observed_matches_state(O::Detached, W::Detached));
        assert!(!observed_matches_state(O::Visible, W::Detached));

        assert!(
            observed_matches_state(O::Hidden, W::Attached),
            "attached 接受任何已挂载"
        );
        assert!(!observed_matches_state(O::Detached, W::Attached));

        // hidden 要求挂载但不可见——detached ≠ hidden，必须继续轮询到超时
        assert!(observed_matches_state(O::Hidden, W::Hidden));
        assert!(!observed_matches_state(O::Detached, W::Hidden));
    }

    /// TS satisfied 的 unknown 边界：eval 返回垃圾 → unknown；对 attached 依然满足
    /// （TS 逐字 `observed !== 'detached'`，原样保留）
    #[test]
    fn observed_unknown_satisfies_only_attached() {
        use ObservedState as O;
        use WaitState as W;

        assert!(!observed_matches_state(O::Unknown, W::Visible));
        assert!(!observed_matches_state(O::Unknown, W::Hidden));
        assert!(!observed_matches_state(O::Unknown, W::Detached));
        assert!(observed_matches_state(O::Unknown, W::Attached));
        assert_eq!(ObservedState::parse("garbage"), O::Unknown);
    }

    /// TS waitStr 校验 "--state must be one of visible|hidden|attached|detached"
    #[test]
    fn wait_state_parse_is_whitelisted() {
        for s in WAIT_STATES {
            assert_eq!(WaitState::parse(s).map(|w| w.as_str()), Some(s));
        }
        assert_eq!(WaitState::parse("bogus"), None);
    }

    // ---------------- 表达式生成器（click-native / fill / 防注入）------------

    /// json_escape = JSON.stringify 等价（防注入基线：引号/反斜杠/控制字符全转义）
    #[test]
    fn json_escape_is_json_stringify_equivalent() {
        assert_eq!(json_escape(".x"), "\".x\"");
        assert_eq!(json_escape("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_escape("a\\b"), "\"a\\\\b\"");
        assert_eq!(json_escape("a\nb"), "\"a\\nb\"");
        assert_eq!(json_escape(""), "\"\"");
    }

    /// TS click-native.mjs locateExpr：四重可见性检查 + fixed/sticky 豁免 + 中心点
    #[test]
    fn locate_expr_contains_visibility_checks_and_exemptions() {
        let expr = locate_expr("#submit");
        assert!(expr.contains(r##"document.querySelector("#submit")"##));
        assert!(expr.contains("error: 'not_found'"));
        assert!(expr.contains("style.display === 'none'"));
        assert!(expr.contains("style.visibility === 'hidden'"));
        assert!(expr.contains("rect.width <= 0 || rect.height <= 0"));
        assert!(expr.contains("offsetParent"), "未渲染检查");
        assert!(expr.contains("position !== 'fixed'"), "fixed 豁免");
        assert!(expr.contains("position !== 'sticky'"), "sticky 豁免");
        assert!(expr.contains("rect.left + rect.width / 2"), "视口中心点");
    }

    /// 防注入（TS 用 JSON.stringify，此处等价）：恶意 selector 只能成为转义后的
    /// 字符串字面量——selector 内引号一律以 \" 形态出现，无裸引号注入面
    #[test]
    fn locate_expr_escapes_hostile_selectors() {
        let hostile = "a[x=\"); steal()=\"]";
        let expr = locate_expr(hostile);
        assert!(expr.contains(&format!("document.querySelector({})", json_escape(hostile))));
        assert!(
            !expr.contains("steal()=\""),
            "selector 的闭合引号必须被转义"
        );
    }

    /// TS hitTestExpr + hitTestWalk 内联：elementFromPoint + 祖先链/shadow 穿透
    #[test]
    fn hit_test_expr_embeds_point_and_walk() {
        let expr = hit_test_expr(".btn", 150.0, 130.5);
        assert!(expr.contains(r#"document.querySelector(".btn")"#));
        assert!(expr.contains("elementFromPoint(150, 130.5)"));
        assert!(expr.contains("'target-gone'"));
        assert!(expr.contains("'nothing-at-point'"));
        assert!(expr.contains("'covered'"));
        // walk 源码内联：祖先链 + shadow host 边界
        assert!(expr.contains("function hitTestWalk(hit, target)"));
        assert!(expr.contains("target.contains(n)"));
        assert!(expr.contains("root.host"));
    }

    /// TS fill.mjs fillEditable + checkFn：白名单/readonly/disabled/编辑器根语义
    #[test]
    fn fill_check_expr_contains_fillability_gate() {
        let expr = fill_fillable_check_expr();
        assert!(expr.contains("'TEXTAREA'"));
        // text 型 input 白名单
        assert!(expr.contains("'text', 'search', 'tel', 'url', 'email', 'password', 'number'"));
        // disabled（含祖先 fieldset）与 readonly 拒绝
        assert!(expr.contains("':disabled'"));
        assert!(expr.contains("el.readOnly"));
        // contenteditable 编辑器根：自身可编辑且父不可编辑
        assert!(expr.contains("el.isContentEditable"));
        assert!(expr.contains("el.parentElement.isContentEditable"));
        assert!(expr.contains("not the contenteditable editor root"));
        // checkFn 骨架：不可填 → not_fillable；可填 → tag/kind/label
        assert!(expr.contains("{ not_fillable: check.why }"));
        assert!(expr.contains("kind: check.kind"));
    }

    // ---------------- 对话框（tests/dialogs.test.mjs）------------------------

    /// TS "parse: known types pass through, unknown types normalize to alert"
    #[test]
    fn parse_dialog_known_types_pass_through() {
        for ty in ["alert", "confirm", "prompt", "beforeunload"] {
            assert_eq!(parse_dialog_opening(&json!({ "type": ty })).r#type, ty);
        }
        // 未来 CDP 新类型不得 crash 守护，也不得漏进缓冲
        assert_eq!(
            parse_dialog_opening(&json!({ "type": "foo" })).r#type,
            "alert"
        );
        assert_eq!(parse_dialog_opening(&json!({})).r#type, "alert");
    }

    /// TS "parse: message keeps strings only (beforeunload boolean false becomes "")"
    #[test]
    fn parse_dialog_message_keeps_strings_only() {
        // 部分 Chrome 对 beforeunload 发 message:false——不得进缓冲变成 "false"
        assert_eq!(
            parse_dialog_opening(&json!({ "message": false })).message,
            ""
        );
        assert_eq!(
            parse_dialog_opening(&json!({ "message": null })).message,
            ""
        );
        assert_eq!(
            parse_dialog_opening(&json!({ "message": "Leave?" })).message,
            "Leave?"
        );
    }

    /// TS "parse: fields longer than 4096 chars are truncated with a marker"
    #[test]
    fn parse_dialog_truncates_long_fields_with_marker() {
        let exact = "a".repeat(4096);
        let over = "a".repeat(4097);
        assert_eq!(
            parse_dialog_opening(&json!({ "message": exact })).message,
            exact
        );
        let truncated = parse_dialog_opening(&json!({ "message": over })).message;
        assert_eq!(truncated, format!("{exact}... [truncated]"));
        // url / defaultPrompt 同规则
        assert_eq!(
            parse_dialog_opening(&json!({ "url": over })).url,
            Some(format!("{exact}... [truncated]"))
        );
        assert_eq!(
            parse_dialog_opening(&json!({ "defaultPrompt": over })).default_prompt,
            Some(format!("{exact}... [truncated]"))
        );
    }

    /// TS "parse: url/defaultPrompt non-strings drop to undefined, hasBrowserHandler keeps booleans only"
    #[test]
    fn parse_dialog_optional_fields_are_type_guarded() {
        let p = parse_dialog_opening(
            &json!({ "url": 42, "defaultPrompt": true, "hasBrowserHandler": "yes" }),
        );
        assert_eq!(p.url, None);
        assert_eq!(p.default_prompt, None);
        assert_eq!(p.has_browser_handler, None);

        let q = parse_dialog_opening(
            &json!({ "url": "https://x/", "defaultPrompt": "hi", "hasBrowserHandler": false }),
        );
        assert_eq!(q.url.as_deref(), Some("https://x/"));
        assert_eq!(q.default_prompt.as_deref(), Some("hi"));
        assert_eq!(q.has_browser_handler, Some(false));
    }

    /// TS "tracker: record assigns monotonically increasing sequence and builds the wire entry"
    #[test]
    fn dialog_buffer_record_builds_wire_entry_with_sequence() {
        let mut buf = DialogBuffer::new("AAAABBBBCCCCDDDD0000");
        let e1 = buf.record(parse_dialog_opening(&json!({
            "type": "prompt", "message": "name?", "url": "https://x/", "defaultPrompt": "Bob"
        })));
        let e2 = buf.record(parse_dialog_opening(
            &json!({ "type": "alert", "message": "hey" }),
        ));
        // wire 形态与 TS 深比较一致：可缺省字段在 JSON 中省略
        assert_eq!(
            serde_json::to_value(&e1).unwrap(),
            json!({
                "tab_id": "AAAABBBBCCCCDDDD0000", "type": "prompt", "message": "name?",
                "url": "https://x/", "default_prompt": "Bob", "handled": "accepted", "sequence": 1
            })
        );
        assert_eq!(
            serde_json::to_value(&e2).unwrap(),
            json!({
                "tab_id": "AAAABBBBCCCCDDDD0000", "type": "alert", "message": "hey",
                "handled": "accepted", "sequence": 2
            })
        );
        assert_eq!(buf.cursor(), 2);
    }

    /// TS "tracker: buffer holds at most 32 entries, FIFO drops the oldest"；
    /// 逐出不再静默：dropped 累计计数随 dialogs 响应 meta 透出（证据边界契约）
    #[test]
    fn dialog_buffer_fifo_cap_counts_dropped() {
        let mut buf = DialogBuffer::new("T");
        assert_eq!(buf.dropped(), 0);
        for i in 0..MAX_DIALOG_BUFFER + 8 {
            buf.record(parse_dialog_opening(
                &json!({ "type": "alert", "message": format!("m{}", i + 1) }),
            ));
        }
        assert_eq!(buf.len(), MAX_DIALOG_BUFFER);
        // 幸存者是最新 32 条；since(0) 错过被逐出的 8 条——但 dropped 声明了丢失量
        let seen = buf.since(0);
        assert_eq!(seen.len(), MAX_DIALOG_BUFFER);
        assert_eq!(seen[0].sequence, 9);
        assert_eq!(seen[0].message, "m9");
        assert_eq!(buf.dropped(), 8, "40 条入 32 环，累计逐出 8");
        // 追加两条再逐出两条：计数只增不清零（调用方拿差值可推知区间丢失）
        buf.record(parse_dialog_opening(&json!({ "type": "alert", "message": "m41" })));
        buf.record(parse_dialog_opening(&json!({ "type": "alert", "message": "m42" })));
        assert_eq!(buf.dropped(), 10);
        assert_eq!(buf.since(0)[0].sequence, 11, "游标推断：幸存者起点后移");
    }

    /// TS "tracker: since() is strictly greater-than"
    #[test]
    fn dialog_buffer_since_is_strictly_greater() {
        let mut buf = DialogBuffer::new("T");
        for _ in 0..3 {
            buf.record(parse_dialog_opening(&json!({ "type": "alert" })));
        }
        assert_eq!(
            buf.since(1).iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(buf.since(3).is_empty());
        assert_eq!(buf.since(0).len(), 3);
    }

    /// TS "handle: alert/confirm/beforeunload send {accept:true} without promptText"
    #[test]
    fn dialog_accept_params_non_prompt_is_accept_only() {
        for ty in ["alert", "confirm", "beforeunload"] {
            let info = parse_dialog_opening(&json!({ "type": ty, "message": "m" }));
            assert_eq!(
                dialog_accept_params(&info),
                json!({ "accept": true }),
                "ty={ty}"
            );
        }
    }

    /// TS "handle: prompt sends promptText = defaultPrompt ?? \"\""
    #[test]
    fn dialog_accept_params_prompt_uses_default_prompt_or_empty() {
        let with = parse_dialog_opening(
            &json!({ "type": "prompt", "message": "name?", "defaultPrompt": "Bob" }),
        );
        assert_eq!(
            dialog_accept_params(&with),
            json!({ "accept": true, "promptText": "Bob" })
        );

        let without = parse_dialog_opening(&json!({ "type": "prompt", "message": "name?" }));
        assert_eq!(
            dialog_accept_params(&without),
            json!({ "accept": true, "promptText": "" })
        );
    }

    /// TS "handle: failed handle … does not advance the cursor"：失败路径不
    /// 触碰 record（域层协议：record 仅在 handle 成功后调用），游标纹丝不动
    #[test]
    fn dialog_sequence_moves_only_on_successful_record() {
        let mut buf = DialogBuffer::new("T");
        assert_eq!(buf.cursor(), 0);
        assert!(buf.is_empty());
        // （失败的 handleJavaScriptDialog 在应用层直接返回，不调 record）
        let first = buf.record(parse_dialog_opening(
            &json!({ "type": "alert", "message": "n" }),
        ));
        assert_eq!(first.sequence, 1, "失败没有移动任何东西——下次成功仍从 1 起");
    }

    // ---------------- 错误码表 ------------------------------------------------

    /// 任务 §1：code/status/effect_state 映射全表 + 消息抽查（文案对齐 TS）
    #[test]
    fn page_error_codes_map_to_status_without_effect_state() {
        let cases: Vec<(PageControlError, &str, u16)> = vec![
            (
                PageControlError::element_not_found("#a"),
                "PAGE_ELEMENT_NOT_FOUND",
                404,
            ),
            (
                PageControlError::element_not_visible("#a", "display:none"),
                "PAGE_ELEMENT_NOT_VISIBLE",
                422,
            ),
            (
                PageControlError::element_blocked_covered("#a", "DIV.overlay"),
                "PAGE_ELEMENT_BLOCKED",
                409,
            ),
            (
                PageControlError::element_blocked_nothing("#a"),
                "PAGE_ELEMENT_BLOCKED",
                409,
            ),
            (
                PageControlError::ref_not_found("@e9"),
                "PAGE_REF_NOT_FOUND",
                404,
            ),
            (PageControlError::ref_stale(None), "PAGE_REF_STALE", 409),
            (
                PageControlError::target_moved("no objectId"),
                "PAGE_TARGET_MOVED",
                409,
            ),
            (
                PageControlError::target_not_fillable("@e1", "disabled"),
                "PAGE_TARGET_NOT_FILLABLE",
                422,
            ),
            (
                PageControlError::fill_value_mismatch("gone"),
                "PAGE_FILL_VALUE_MISMATCH",
                409,
            ),
            (
                PageControlError::wait_timeout(".x", "visible", 1000, "hidden", 4),
                "PAGE_WAIT_TIMEOUT",
                409,
            ),
            (
                PageControlError::key_unknown("Foo"),
                "PAGE_KEY_UNKNOWN",
                400,
            ),
            (
                PageControlError::tab_not_found("tab-1"),
                "PAGE_TAB_NOT_FOUND",
                404,
            ),
        ];
        for (e, code, status) in cases {
            assert_eq!(e.code, code);
            assert_eq!(e.status, status);
            assert_eq!(e.effect_state, None, "仅 dispatch_failed 携带 effect_state");
        }
        // 消息形态抽查：TS 文案逐字保留
        assert!(PageControlError::element_not_visible("#a", "")
            .message
            .contains("unknown reason"));
        assert_eq!(
            PageControlError::wait_timeout(".loading", "visible", 30, "hidden", 7).message,
            "Timed out after 30ms waiting for \".loading\" to be visible (last observed: hidden, 7 polls)"
        );
        assert!(PageControlError::ref_stale(None)
            .message
            .contains("document changed"));
    }

    /// TS click-native.mjs §2.7：press 前失败 effect_state=none，press 后=unknown
    #[test]
    fn dispatch_failed_carries_effect_state_wording() {
        let post = PageControlError::dispatch_failed("boom", true);
        assert_eq!((post.code, post.status), ("PAGE_DISPATCH_FAILED", 500));
        assert_eq!(post.effect_state, Some("unknown"));
        assert!(post.message.contains("mousePressed was already dispatched"));

        let pre = PageControlError::dispatch_failed("boom", false);
        assert_eq!(pre.effect_state, Some("none"));
        assert!(pre.message.contains("no input was dispatched"));
    }

    /// 域错误 → AppError：code/status 无损上抛（应用层统一错误出口）
    #[test]
    fn page_error_converts_to_app_error() {
        let app: AppError = PageControlError::ref_stale(Some("navigation")).into();
        assert_eq!(app.code(), "PAGE_REF_STALE");
        assert_eq!(app.status(), 409);
        assert!(app.to_string().contains("navigation"));
    }
}
