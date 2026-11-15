//! page 控制域纯逻辑层：@eN 键归一化、press 键表、wait 状态机、
//! 页面 eval 表达式生成与 JS 对话框模型。行为规格由文件内单测锁定；
//! IO（CDP 收发、轮询、补偿重发）归 application 层，本文件只做可单测的
//! 纯逻辑：
//!
//! - [`normalize_ref`]：@eN 键归一化（ref 表本体为 browser_kit
//!   element::RefMap，durable refs 由 kit 自管）；
//! - [`parse_key`]/[`KeyPress`]：键名 → CDP Input.dispatchKeyEvent 三段序列；
//! - [`parse_duration`]/[`observed_matches_state`]：wait 轮询的参数与状态机；
//! - `*_expr` 表达式生成器：locate/hit-test/fill 检查/wait 观测的内嵌 JS
//!   ——selector 一律经 [`json_escape`]（JSON.stringify 等价）转义后嵌入，
//!   防注入；
//! - [`parse_dialog_opening`]/[`DialogBuffer`]：对话框防御解析 + 有界缓冲。
//!
//! 形态约定：错误码统一加 `PAGE_` 前缀并 SCREAMING_SNAKE；message 不含
//! 冗余的 `code:` 前缀（code 已是独立字段）；`KeyPress.vk` 为 `Option<u16>`
//! （标点键无 vk，dispatch 时必须整体省略该字段而非置 0）；
//! `PAGE_WAIT_TIMEOUT` 用 409 而非 504（HTTP 语义：客户端可重试的冲突态）。
#![allow(dead_code)]

use std::collections::VecDeque;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::AppError;

// ============================================================================
// 1. 错误码与 HTTP 映射
// ============================================================================

/// page 控制域错误：code 为对外稳定契约（AGENT-API 披露），status 为 HTTP 映射。
/// `effect_state` 仅 [`PageControlError::dispatch_failed`] 携带（语义：press
/// 前失败 = "none"——无任何输入已发出；press 后失败 = "unknown"——
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

    /// 404：locate / focus 未命中
    pub fn element_not_found(selector: &str) -> Self {
        Self::business(
            404,
            "PAGE_ELEMENT_NOT_FOUND",
            format!("Element not found: {selector}"),
        )
    }

    /// 422：locate 可见性四检未过
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

    /// 409：点击点被遮挡
    pub fn element_blocked_covered(selector: &str, by: &str) -> Self {
        Self::business(
            409,
            "PAGE_ELEMENT_BLOCKED",
            format!("Click point on {selector} is covered by <{by}>"),
        )
    }

    /// 409：点击点无元素（视口外）
    pub fn element_blocked_nothing(selector: &str) -> Self {
        Self::business(
            409,
            "PAGE_ELEMENT_BLOCKED",
            format!("Click point on {selector} hits nothing (outside viewport?)"),
        )
    }

    /// 404：ref 未命中且表未失效
    pub fn ref_not_found(raw_ref: &str) -> Self {
        Self::business(
            404,
            "PAGE_REF_NOT_FOUND",
            format!("{raw_ref} is not a ref from the last snap — run \"snap <target>\" and use the fresh @eN refs"),
        )
    }

    /// 409：ref 表因导航/文档替换整体失效（reason 缺省 "document changed"）
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

    /// 409：ref 命中但几何/节点解析失败
    pub fn target_moved(detail: &str) -> Self {
        Self::business(
            409,
            "PAGE_TARGET_MOVED",
            format!("ref no longer resolves to a live node ({detail}) — run \"snap <target>\" again and use fresh @eN refs"),
        )
    }

    /// 422：extract 目标不是表格（table / [role=table|grid|treegrid]）
    pub fn extract_not_a_table(tag: &str, role: Option<&str>) -> Self {
        let what = match role {
            Some(r) => format!("<{tag} role=\"{r}\">"),
            None => format!("<{tag}>"),
        };
        Self::business(
            422,
            "PAGE_EXTRACT_NOT_A_TABLE",
            format!(
                "{what} is not a table — extract targets must be <table> or [role=table|grid|treegrid]"
            ),
        )
    }

    /// 422：fill 目标不可编辑
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

    /// 422：元素被禁用（browser_kit 解析链的 actionability 报告
    /// disabled——kit 原文透传，不吞信息）
    pub fn element_disabled(message: impl Into<String>) -> Self {
        Self::business(422, "PAGE_ELEMENT_DISABLED", message.into())
    }

    /// 500：page 工具链未分类失败（browser_kit String 错误的兜底
    /// 归宿——原文透传，保证不吞任何证据）
    pub fn action_failed(message: impl Into<String>) -> Self {
        Self::business(500, "PAGE_ACTION_FAILED", message.into())
    }

    /// 409：record stop 时无运行中录制（从未 start / 已 stop /
    /// 连接重建或锚点 tab 关闭致运行态废弃——reason 披露具体路径）
    pub fn record_not_running(reason: &str) -> Self {
        Self::business(
            409,
            "PAGE_RECORD_NOT_RUNNING",
            format!("无运行中的录制（{reason}）"),
        )
    }

    /// 409：同实例重复 record start（Chrome 每实例同时只允许一路
    /// 录制——capture session/编码器均为单实例资源）
    pub fn record_already_active(message: impl Into<String>) -> Self {
        Self::business(409, "PAGE_RECORD_ALREADY_ACTIVE", message.into())
    }

    /// 422：ffmpeg 探测失败（录制编码的前置环境检查，失败先于
    /// 浏览器 attach 暴露；detail = kit check_ffmpeg_available 原文，
    /// 追加具体安装命令）
    pub fn record_no_ffmpeg(detail: impl Into<String>) -> Self {
        Self::business(
            422,
            "PAGE_RECORD_NO_FFMPEG",
            format!(
                "{} — 安装后重试: brew install ffmpeg (macOS) / apt install ffmpeg (Debian/Ubuntu)",
                detail.into()
            ),
        )
    }

    /// 409：wait 超时（504 → 409，超时文案逐字钉住）
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

    /// 400：键名不识别（提示语不带 CLI 专属尾巴 `(see "cdp help")`）
    pub fn key_unknown(input: &str) -> Self {
        Self::business(
            400,
            "PAGE_KEY_UNKNOWN",
            format!(
                "press: unknown key \"{input}\" — supported: named keys (Enter, Tab, Escape, Backspace, Delete, Home, End, PageUp, PageDown, ArrowUp/Down/Left/Right, Space, F1-F12), single characters, and modifier combos like Control+A / Meta+V / Shift+ArrowDown"
            ),
        )
    }

    /// 404：page 命令族的目标 tab 不存在（应用层路由用；连接失败由
    /// 各命令的 CDP 错误路径覆盖，本码只管路由层）
    pub fn tab_not_found(tab: &str) -> Self {
        Self::business(404, "PAGE_TAB_NOT_FOUND", format!("tab not found: {tab}"))
    }
}

impl From<PageControlError> for AppError {
    fn from(e: PageControlError) -> Self {
        AppError::business(e.status, e.code, e.message)
    }
}

// ============================================================================
// 2. ref 键归一化（ref 表本体 = browser_kit element::RefMap，
//    domain 只保留键归一化）
// ============================================================================

/// 归一化：只剥**恰好一个**前导 '@'（'@e3' → 'e3'，'@@e3' → '@e3'）。
/// 大小写敏感是刻意的：'E3'/'@E3' 原样保留、查表必失配 → ref_not_found。
pub fn normalize_ref(raw: &str) -> &str {
    raw.strip_prefix('@').unwrap_or(raw)
}

// ============================================================================
// 3a. 元素截图几何（clip 是区域不是点）
// ============================================================================
// 注：点击点几何（largest_quad_centroid / OOPIF 投影 project_point）在
// browser_kit element::resolve_element_center（帧链换算 + scrollIntoView
// + settle 均在 kit 解析链内），本层不保留。

/// 视口 CSS 像素矩形（Page.captureScreenshot 的 clip 源）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// 全部有效 quad 的联合包围盒（getBoundingClientRect 语义）。与
/// [`largest_quad_centroid`] 取「最大 quad」不同：截图 clip 要覆盖元素**全部**
/// 可见部分——跨行 inline 元素有多个 quad，点击点必须落在实体上（取最大），
/// 而 clip 取并集才不截丢内容。零面积包围盒视为无几何（None，走 rect 回退），
/// 与点击链对退化元素的处理一致。
pub fn quads_bbox(quads: &[Vec<f64>]) -> Option<Rect> {
    let (mut min_x, mut min_y) = (f64::INFINITY, f64::INFINITY);
    let (mut max_x, mut max_y) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for q in quads {
        if q.len() != 8 || q.iter().any(|n| !n.is_finite()) {
            continue;
        }
        for i in 0..4 {
            let (x, y) = (q[i * 2], q[i * 2 + 1]);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if !min_x.is_finite() || !min_y.is_finite() {
        return None;
    }
    let (width, height) = (max_x - min_x, max_y - min_y);
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some(Rect {
        x: min_x,
        y: min_y,
        width,
        height,
    })
}

/// bbox → 整数化 clip：外扩取整（floor 左上 / ceil 右下）保证完全覆盖元素，
/// 最小 1×1。CDP clip 接受浮点，但亚像素 clip 在缩放链路上产生边缘半截像素；
/// Playwright 同样取整。宽度/高度不允许为 0（空 clip 会报错，不如给 1px
/// 让元素「不可截图」的语义由定位链错误负责）。
pub fn rounded_clip(r: &Rect) -> Rect {
    let x = r.x.floor();
    let y = r.y.floor();
    let width = ((r.x + r.width).ceil() - x).max(1.0);
    let height = ((r.y + r.height).ceil() - y).max(1.0);
    Rect {
        x,
        y,
        width,
        height,
    }
}

// ============================================================================
// 4. press 键系统
// ============================================================================

/// 修饰键位域（alt=1 ctrl=2 meta=4 shift=8）。
/// 数组顺序即规范显示序（alt→ctrl→meta→shift）。
pub const MODIFIER_BITS: [(&str, u8); 4] = [("alt", 1), ("ctrl", 2), ("meta", 4), ("shift", 8)];

/// 修饰键位（便于组合判断，如文本抑制）
pub const MOD_ALT: u8 = 1;
pub const MOD_CTRL: u8 = 2;
pub const MOD_META: u8 = 4;
pub const MOD_SHIFT: u8 = 8;

/// 别名折叠：control/option/opt/cmd/command/super → 规范名
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

/// 位域 → 规范序修饰键显示名（首字母大写）
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

/// 命名键表全表。text = 该键产生的字符（仅
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
/// （vk：小写取大写码、其余取自身码、标点无）。
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
    /// Space 的 key 字面是 " "
    pub key: String,
    pub code: String,
    pub text: Option<String>,
    pub vk: Option<u16>,
    pub modifiers: u8,
}

impl KeyPress {
    /// 显示名：规范序修饰键 + 主键，' ' 显示为 Space。
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

    /// rawKeyDown →（可选 char）→ keyUp 三段序列（绝不发
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

/// Input.dispatchKeyEvent 的参数形态（char 不带 vk、其余不带 text，
/// 空 code 整体省略——字段有无即语义）
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

/// '<key>' | 'Mod+Mod+Key' → [`KeyPress`]。
/// 解析规则：按 '+' 切分、trim、丢空 token；≤1 个剩余 token 时整个
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
// 5. wait 系统
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

/// 观测是否满足期望。hidden = 已挂载但不可见（detached ≠
/// hidden）；attached 接受任何**非** detached 观测（判定即
/// `observed !== 'detached'`），故 Unknown 也满足 attached。
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
/// 荒谬量级（>Duration 上限，try_from_secs_f64 Err）按非法处理——只影响
/// 天文数字输入，换来不 panic。
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
// 5c. hover 规则扫描（snap hover 感知，domain 纯函数）
// ============================================================================

/// hover 扫描预算：样式表数 / 单表字节 / 提取规则数。任一
/// 超限即停止采集并把 truncated 置位（诚实证据：宁可不完整不静默）。
pub const MAX_HOVER_SHEETS: usize = 32;
pub const MAX_HOVER_SHEET_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_HOVER_RULES: usize = 256;

/// CSS 文本扫描产物：可直接交给 DOM.querySelectorAll 的查询选择器集。
/// - triggers：hover 触发器（规则里含 :hover 的复合段，去 :hover、保留
///   前缀上下文）——命中元素标 [hover]（hover 它会有效果）；
/// - reveals：hover-reveal 目标（:hover 段之后还有复合段的后代/子代目标，
///   完整选择器去 :hover）——命中元素标 [hover-reveal]（hover 后才显示/
///   变化，常为 display:none，DOM 查询不受可见性影响仍可命中）。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct HoverRules {
    pub triggers: Vec<String>,
    pub reveals: Vec<String>,
    /// 预算内是否完整（false = 有截断，快照 meta 报 truncated）
    pub truncated: bool,
}

/// 扫描一批样式表文本（app 层已按 origin=regular 过滤并取文本）。
/// sheet_total = 实际 regular 表总数（可能大于 texts 长度：app 只取前
/// MAX_HOVER_SHEETS 张，超出部分在这里如实计入截断）。
pub fn scan_hover_rules(texts: &[&str], sheet_total: usize) -> HoverRules {
    let mut out = HoverRules::default();
    if sheet_total > MAX_HOVER_SHEETS {
        out.truncated = true;
    }
    let mut rules_seen = 0usize;
    'sheets: for (idx, text) in texts.iter().enumerate() {
        if idx >= MAX_HOVER_SHEETS {
            break;
        }
        if text.len() > MAX_HOVER_SHEET_BYTES {
            out.truncated = true; // 单表超限：整表跳过（不载入不解析）
            continue;
        }
        let cleaned = strip_css_comments(text);
        for sel in style_rule_selectors(&cleaned) {
            if rules_seen >= MAX_HOVER_RULES {
                out.truncated = true;
                break 'sheets;
            }
            rules_seen += 1;
            absorb_hover_selector(&sel, &mut out);
        }
    }
    out
}

/// 剔除 /* ... */ 注释（CSS 注释不可嵌套；替换为空串——`a/**/:hover` 在
/// 合法 CSS 里等价 `a:hover`，替换为空格会变成后代选择器失真）。
fn strip_css_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let chars: Vec<char> = css.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() {
                if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    i += 2;
                    break;
                }
                i += 1;
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// 括号/引号感知的顶层切分：按 seps 中任一字符切，深度非 0 处不切（()
/// [] 内、引号内的分隔符不是分隔符）。返回 (前置分隔符, 段)——保留 > + ~
/// 等组合子原样，拼接回去时不失真。首段的分隔符为 '\0'。
fn split_top_level(s: &str, seps: &[char]) -> Vec<(char, String)> {
    let mut parts: Vec<(char, String)> = Vec::new();
    let mut cur = String::new();
    let mut sep = '\0';
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut prev = '\0';
    for c in s.chars() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q && prev != '\\' {
                quote = None;
            }
        } else {
            match c {
                '"' | '\'' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '(' | '[' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' | ']' => {
                    depth -= 1;
                    cur.push(c);
                }
                _ if depth == 0 && seps.contains(&c) => {
                    let trimmed = cur.trim().to_string();
                    if !trimmed.is_empty() {
                        parts.push((sep, trimmed));
                        cur.clear();
                        sep = c;
                    } else if !c.is_whitespace() {
                        // cur 为空：组合子（> + ~）覆盖前导空白；后随空白不
                        // 覆盖已记录的组合子（`a > b` 的 '>' 不被其后的空格冲掉）
                        sep = c;
                    }
                }
                _ => cur.push(c),
            }
        }
        prev = c;
    }
    let trimmed = cur.trim().to_string();
    if !trimmed.is_empty() {
        parts.push((sep, trimmed));
    }
    parts
}

/// 花括号走读：提取所有样式规则的选择器文本（@media/@supports/@layer/
/// @container/@scope 组块内层展开；@keyframes/@font-face/@page 等非条件
/// 组块整体跳过；@import/@charset 等顶层语句按 ';' 忽略）。不追求完美
/// CSS 解析（CSS 嵌套的内层规则以「最近 ';' 后的段」近似提取，丢失外层
/// & 上下文——扫描是启发式增强，不是规则引擎；误报由 querySelectorAll
/// 求值兑底：查不到元素的选择器自然无命中）。
fn style_rule_selectors(css: &str) -> Vec<String> {
    #[derive(PartialEq)]
    enum Block {
        Style,
        AtGroup,
        AtSkip,
    }
    let mut selectors = Vec::new();
    let mut stack: Vec<Block> = Vec::new();
    let mut buf = String::new();
    for c in css.chars() {
        match c {
            '{' => {
                // 取最后一个顶层 ';' 之后的段（@import 等语句丢弃）
                let head = split_top_level(&buf, &[';'])
                    .last()
                    .map(|(_, s)| s.trim().to_string())
                    .unwrap_or_default();
                let head_lower = head.to_lowercase();
                let kind = if head_lower.starts_with('@') {
                    let name = head_lower
                        .split(|ch: char| ch == '(' || ch.is_whitespace())
                        .next()
                        .unwrap_or("");
                    match name {
                        "@media" | "@supports" | "@layer" | "@container" | "@scope" => {
                            Block::AtGroup
                        }
                        _ => Block::AtSkip, // @keyframes/@font-face/@page/@property…
                    }
                } else if head.is_empty() {
                    Block::Style // 畸形 CSS：空选择器，内容忽略
                } else {
                    selectors.push(head);
                    Block::Style
                };
                stack.push(kind);
                buf.clear();
            }
            '}' => {
                stack.pop();
                buf.clear();
            }
            ';' => buf.clear(),
            _ if stack.last().is_some_and(|b| *b == Block::AtSkip) => {}
            _ => buf.push(c),
        }
    }
    selectors
}

/// 单条选择器（已逗号拆分前的原始段）→ 触发器/reveal 查询串，吸收进 out。
/// 先逗号拆分（每组独立），再复合段拆分（空格/>/+/~）；首个含裸 :hover
/// 的段 i：trigger = 段 0..=i（段 i 去 :hover）；i 后还有段 → reveal =
/// 全段去 :hover（保留组合子）。`:not(:hover)` 语义反转：:hover 在括号
/// 内不是裸 :hover，该段不触发提取（整条若无其他裸 :hover 段则跳过）。
fn absorb_hover_selector(sel: &str, out: &mut HoverRules) {
    for (_, group) in split_top_level(sel, &[',']) {
        let segments = split_top_level(&group, &[' ', '>', '+', '~']);
        if segments.is_empty() {
            continue;
        }
        let Some(i) = segments.iter().position(|(_, seg)| has_bare_hover(seg)) else {
            continue;
        };
        if let Some(t) = join_stripped(&segments[..=i]) {
            push_dedup(&mut out.triggers, t);
        }
        if i + 1 < segments.len() {
            if let Some(r) = join_stripped(&segments) {
                push_dedup(&mut out.reveals, r);
            }
        }
    }
}

/// 段列表去裸 :hover 后按原分隔符拼接；任一段去后为空（如裸 ":hover"）
/// → None（该角色放弃，不产出无效查询串）。伪元素段（::after 等）一并
/// 剥离——产物是 DOM.querySelectorAll 查询串，伪元素不可查询（Chrome
/// 直接报错），触发器回归宿主元素（`a:hover::after` → `a`）。
fn join_stripped(segments: &[(char, String)]) -> Option<String> {
    let mut out = String::new();
    for (sep, seg) in segments {
        let stripped = strip_pseudo_element(&strip_bare_hover(seg));
        if stripped.is_empty() {
            return None;
        }
        match sep {
            ' ' | '\0' => {
                if !out.is_empty() {
                    out.push(' ');
                }
            }
            op => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push(*op);
                out.push(' ');
            }
        }
        out.push_str(&stripped);
    }
    Some(out)
}

/// 裸 :hover 判定：剥离后与原串不同即命中（括号/引号内的 :hover 不剥）。
fn has_bare_hover(seg: &str) -> bool {
    strip_bare_hover(seg) != seg
}

/// 剥离裸 :hover token：仅深度 0（括号外）、引号外、且后随字符非标识符
/// 字符（`:hovered` 之类的伪类名不误伤）。非 ASCII 段安全（chars 边界）。
fn strip_bare_hover(seg: &str) -> String {
    const TOKEN: &str = ":hover";
    let mut out = String::with_capacity(seg.len());
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut chars = seg.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                out.push(c);
            }
            '(' | '[' => {
                depth += 1;
                out.push(c);
            }
            ')' | ']' => {
                depth -= 1;
                out.push(c);
            }
            ':' if depth == 0 => {
                // lookahead：先取 token 余部（不含开头的 ':'），再取 token 后随
                // 字符判边界（读首字符是错的——那是 token 自身）
                let mut lookahead = chars.clone();
                let token_rest: String = (&mut lookahead).take(TOKEN.len() - 1).collect();
                if token_rest == TOKEN[1..]
                    && lookahead
                        .next()
                        .is_none_or(|nx| !(nx.is_alphanumeric() || nx == '-' || nx == '_'))
                {
                    for _ in 1..TOKEN.len() {
                        chars.next(); // 消耗 token 余下字符（不输出 = 剥除）
                    }
                } else {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn push_dedup(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

/// 剥离伪元素 token：`::name`（任意名，含 `::part(...)` 括号体）与旧式
/// 单冒号白名单 :before/:after/:first-line/:first-letter（CSS 规范的四个
/// 合法旧写法）。深度 0、引号外才剥（`:not(::before)` 参数原样保留、
/// `[data-x="::after"]` 字面量不动）；名字后必须是标识符边界
/// （`:aftermath` 之类的伪类名不误伤）。无宿主的纯伪元素段
/// （如 ":hover::after"）剥后为空 → join_stripped 判 None 整条跳过——
/// 回归宿主 * 会误标全页，宁可漏标该规则。
fn strip_pseudo_element(seg: &str) -> String {
    const LEGACY: [&str; 4] = ["before", "after", "first-line", "first-letter"];
    let mut out = String::with_capacity(seg.len());
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut chars = seg.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                out.push(c);
            }
            '(' | '[' => {
                depth += 1;
                out.push(c);
            }
            ')' | ']' => {
                depth -= 1;
                out.push(c);
            }
            ':' if depth == 0 => {
                if chars.peek() == Some(&':') {
                    // `::name` / `::name(args)`：伪元素，名字与括号体一起消耗
                    chars.next();
                    while chars
                        .peek()
                        .is_some_and(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                    {
                        chars.next();
                    }
                    if chars.peek() == Some(&'(') {
                        chars.next();
                        let mut d = 1i32;
                        for c2 in chars.by_ref() {
                            match c2 {
                                '(' => d += 1,
                                ')' => {
                                    d -= 1;
                                    if d == 0 {
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    continue;
                }
                // 旧式单冒号：仅白名单四者，且名字后是非标识符边界
                let mut probe = chars.clone();
                let head: String = (&mut probe).take("first-letter".len()).collect();
                let matched = LEGACY.iter().find_map(|name| {
                    if !head.starts_with(name) {
                        return None;
                    }
                    let boundary = match head[name.len()..].chars().next() {
                        Some(nx) => !(nx.is_alphanumeric() || nx == '-' || nx == '_'),
                        None => probe
                            .clone()
                            .next()
                            .is_none_or(|nx| !(nx.is_alphanumeric() || nx == '-' || nx == '_')),
                    };
                    boundary.then_some(name.len())
                });
                match matched {
                    Some(n) => {
                        for _ in 0..n {
                            chars.next(); // 消耗名字（':' 已读，不输出 = 剥除）
                        }
                    }
                    None => out.push(c),
                }
            }
            _ => out.push(c),
        }
    }
    out
}

// ============================================================================
// 6. eval 表达式生成器（内嵌 JS 由本层生成字符串）
// ============================================================================

/// JSON.stringify 等价（含两侧引号）。所有生成器用它转义 selector 等用户输入，
/// 转义结果是一个完整 JSON 字符串字面量，页内求值无注入面。
pub fn json_escape(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// 定位 + 滚动居中 + 可见性检查 + 中心点计算，一个 eval 内原子完成。
/// 四重可见性检查：display:none、
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

/// hitTestWalk：从命中元素沿 parentNode 与 shadow-DOM host 边界上行，
/// 目标自身或其后代算命中。
const HIT_TEST_WALK: &str = r#"function hitTestWalk(hit, target) {
  for (let n = hit; n; ) {
    if (n === target || target.contains(n)) return true;
    const root = n.getRootNode();
    n = n.parentNode || (root && root !== n ? root.host : null);
  }
  return false;
}"#;

/// 点击点命中测试：elementFromPoint + 祖先链/shadow 穿透。
/// 每次 eval 之间无对象同一性，故重新按 selector 查目标。`x`/`y` 必须是有限数
/// （调用方先行 locate 校验，同一契约）。
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

/// fill 可填性检查脚本（checkFn + fillEditable 内联）。
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

/// wait 单次观测脚本：detached / visible / hidden 三态判定。
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
// 6b. 表格结构化提取——DOM 事实收集脚本（两段式的收集段）
// ============================================================================

/// 收集器主体：`function(t)`，t = 表格元素或 null（null = 主文档最大表格
/// 自动发现）。只读采集 DOM 事实（分区/th-td 文本/rowspan-colspan/aria
/// 行列声明/隐藏行计数），**不解析、不解释**——归一化在
/// domain/table_extract.rs 纯函数层。返回 JSON 字符串（camelCase 键对齐
/// TableFacts serde）。BUDGET 与 table_extract::CELL_BUDGET 必须一致
///（超限停止采集，置 partial，行数/格数照常计数——截断判定归 domain）。
pub const EXTRACT_COLLECT_FN: &str = r#"function(t) {
  var BUDGET = 10000;
  function isTableEl(el) {
    if (!el || el.nodeType !== 1) return false;
    var role = (el.getAttribute('role') || '').toLowerCase();
    return el.tagName === 'TABLE' || role === 'table' || role === 'grid' || role === 'treegrid';
  }
  var el;
  if (t === null || t === undefined) {
    // 主文档最大表格自动发现（按单元格数；外层表格的计数包含嵌套表格，
    // 因此候选天然落在外层——不单独剔嵌套）
    var best = null, bestN = -1;
    var cands = document.querySelectorAll('table, [role=table], [role=grid], [role=treegrid]');
    for (var i = 0; i < cands.length; i++) {
      var c = cands[i];
      var n = c.querySelectorAll('th, td, [role=cell], [role=columnheader], [role=rowheader], [role=gridcell]').length;
      if (n > bestN) { bestN = n; best = c; }
    }
    el = best;
  } else {
    el = t;
  }
  if (!el) return JSON.stringify({ found: false, reason: 'no-table' });
  if (!isTableEl(el)) {
    return JSON.stringify({ found: false, reason: 'not-a-table', tag: el.tagName, role: el.getAttribute('role') });
  }
  var format = el.tagName === 'TABLE' ? 'html' : 'aria';
  function posInt(s) { var n = parseInt(s, 10); return isFinite(n) && n >= 1 ? n : null; }
  var rc = el.getAttribute('aria-rowcount'), cc = el.getAttribute('aria-colcount');
  var aria = (rc !== null || cc !== null)
    ? { rowCount: rc !== null ? posInt(rc) : null, colCount: cc !== null ? posInt(cc) : null }
    : null;
  var caption = null;
  if (format === 'html') {
    var cap = el.querySelector(':scope > caption');
    if (cap) caption = (cap.textContent || '').trim() || null;
  } else {
    caption = (el.getAttribute('aria-label') || '').trim() || null;
  }
  // 本表的行（排除嵌套表格内的行）
  function ownRows(sel) {
    var out = [];
    var all = el.querySelectorAll(sel);
    for (var i = 0; i < all.length; i++) {
      var n = all[i], p = n.parentElement, ok = true;
      while (p && p !== el) {
        if (isTableEl(p)) { ok = false; break; }
        p = p.parentElement;
      }
      if (ok) out.push(n);
    }
    return out;
  }
  var rawRows = format === 'html' ? ownRows('tr') : ownRows('[role=row]');
  function sectionKind(r) {
    var p = r.parentElement;
    while (p && p !== el) {
      var tn = p.tagName;
      if (tn === 'THEAD' || tn === 'TBODY' || tn === 'TFOOT') return p;
      p = p.parentElement;
    }
    return null;
  }
  function cellRole(c) { return (c.getAttribute('role') || '').toLowerCase(); }
  function isHeaderCell(c) {
    if (c.tagName === 'TH') return true;
    var r = cellRole(c);
    return r === 'columnheader' || r === 'rowheader';
  }
  function rowCells(r) {
    if (r.tagName === 'TR' && r.cells) {
      var out = [];
      for (var i = 0; i < r.cells.length; i++) out.push(r.cells[i]);
      return out;
    }
    var out2 = [];
    for (var i = 0; i < r.children.length; i++) {
      var c = r.children[i];
      var cr = cellRole(c);
      if (cr === 'cell' || cr === 'gridcell' || cr === 'columnheader' || cr === 'rowheader'
          || c.tagName === 'TH' || c.tagName === 'TD') out2.push(c);
    }
    return out2;
  }
  var sections = [];
  var totalRows = 0, totalCells = 0, hiddenRows = 0, collectedCells = 0, partial = false;
  for (var i = 0; i < rawRows.length; i++) {
    var r = rawRows[i];
    var sec = sectionKind(r);
    var kind = sec ? sec.tagName.toLowerCase() : 'flat';
    var cs = rowCells(r);
    var hidden = getComputedStyle(r).display === 'none'
      || (sec && getComputedStyle(sec).display === 'none');
    totalRows++;
    if (hidden) { hiddenRows++; continue; }
    totalCells += cs.length;
    if (partial) continue; // 已超预算：只计数不采集
    if (collectedCells + cs.length > BUDGET) { partial = true; continue; }
    var cells = [];
    for (var j = 0; j < cs.length; j++) {
      var c = cs[j];
      var rs, cps;
      if (c.rowSpan !== undefined && c.rowSpan !== null && isFinite(c.rowSpan)) rs = c.rowSpan;
      else rs = posInt(c.getAttribute('rowspan')) || 1;
      if (c.colSpan !== undefined && c.colSpan !== null && isFinite(c.colSpan)) cps = c.colSpan;
      else cps = posInt(c.getAttribute('colspan')) || 1;
      var hs = c.getAttribute('headers');
      cells.push({
        text: (c.textContent || '').trim(),
        header: isHeaderCell(c),
        rowSpan: rs,
        colSpan: cps,
        ariaRowIndex: posInt(c.getAttribute('aria-rowindex')),
        ariaColIndex: posInt(c.getAttribute('aria-colindex')),
        headers: hs ? hs.trim().split(/\s+/).filter(Boolean) : [],
      });
    }
    collectedCells += cs.length;
    var rowFact = { cells: cells };
    var ri = posInt(r.getAttribute('aria-rowindex'));
    if (ri !== null) rowFact.ariaRowIndex = ri;
    var last = sections[sections.length - 1];
    if (last && last.kind === kind) last.rows.push(rowFact);
    else sections.push({ kind: kind, rows: [rowFact] });
  }
  return JSON.stringify({
    found: true,
    format: format,
    caption: caption,
    aria: aria,
    hiddenRows: hiddenRows,
    totalRows: totalRows,
    totalCells: totalCells,
    partial: partial,
    sections: sections,
  });
}"#;

/// 自动发现路径的求值表达式（主文档最大表格）。
pub fn extract_discovery_expr() -> String {
    format!("({EXTRACT_COLLECT_FN})(null)")
}

/// CSS 选择器路径的求值表达式。**未命中在本层拦**（null 进入收集器会被
/// 误读为自动发现），回 found:false/no-table → 404 PAGE_ELEMENT_NOT_FOUND。
pub fn extract_selector_expr(selector: &str) -> String {
    format!(
        r#"(function() {{
  var el = document.querySelector({sel});
  if (!el) return JSON.stringify({{ found: false, reason: 'no-table' }});
  return ({fn_})(el);
}})()"#,
        sel = json_escape(selector),
        fn_ = EXTRACT_COLLECT_FN
    )
}

/// @eN 引用路径的 callFunctionOn 声明（this = 目标元素）。
pub fn extract_call_on_fn() -> String {
    format!("function() {{ return ({EXTRACT_COLLECT_FN})(this); }}")
}

// ============================================================================
// 7. 对话框模型
// ============================================================================

const MAX_FIELD_LEN: usize = 4096;
/// 缓冲上限：32 条 FIFO，丢最旧、无丢失标记——比游标老的 cursor 只见幸存者
pub const MAX_DIALOG_BUFFER: usize = 32;

/// Page.javascriptDialogOpening 参数的防御解析结果（类型错误降级，不抛）
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
/// 按 chars 截断（与按 UTF-16 单元截断语义一致，多字节安全）。
fn string_field(value: Option<&Value>) -> Option<String> {
    let s = value.and_then(Value::as_str)?;
    if s.chars().count() > MAX_FIELD_LEN {
        Some(s.chars().take(MAX_FIELD_LEN).collect::<String>() + "... [truncated]")
    } else {
        Some(s.to_string())
    }
}

/// 防御解析：类型校验降级不抛错。beforeunload 在部分 Chrome 上
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

/// 缓冲条目（wire 形态：蛇形字段，可缺省字段省略）
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DialogEntry {
    pub tab_id: String,
    pub r#type: String,
    pub message: String,
    /// 固定 "accepted"（本 CLI 无合作模式，无条件 accept）
    pub handled: &'static str,
    pub sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_browser_handler: Option<bool>,
}

/// per-tab 对话框缓冲。**协议**：`record` 只允许在
/// Page.handleJavaScriptDialog **成功后**调用——sequence 只随成功前移，
/// 失败路径不得调用本方法，对话框保持打开、CDP 命令继续阻塞直至各自超时
/// （与上游一致）。游标语义见 [`DialogBuffer::since`]。
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

    /// 当前游标（命令入口打标用）
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

    /// sequence 严格大于 cursor 的条目。游标早于缓冲起点时只见幸存者；
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

/// Page.handleJavaScriptDialog 应答参数（无条件 accept；beforeunload 的
/// accept 即"允许离开"）。prompt 必带 promptText = defaultPrompt ?? ""。
pub fn dialog_accept_params(info: &DialogInfo) -> Value {
    if info.r#type == "prompt" {
        json!({ "accept": true, "promptText": info.default_prompt.clone().unwrap_or_default() })
    } else {
        json!({ "accept": true })
    }
}

// ============================================================================
// 测试：错误码映射、键系统、wait 状态机、表达式生成器、hover 扫描与
// 对话框模型的全量单测
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
            assert_eq!(
                (e.code(), e.status()),
                ("INVALID_REQUEST", 400),
                "应拒绝: {bad}"
            );
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

    // ---------------- hover 规则扫描（5c）---------------------------------------

    /// 提取矩阵：触发器/reveal 派生、注释剔除、@media 展开、@keyframes 跳过、
    /// :not(:hover) 排除、逗号分组独立、组合子保留、去重。
    #[test]
    fn hover_scan_extracts_triggers_and_reveals() {
        let css = r#"
            /* comment with .fake:hover inside */
            .menu-item:hover { background: yellow; }
            .menu-item:hover .dropdown { display: block; }
            .dropdown { display: none; }
            .plain-rule { color: red; }
            .menu-item:hover > .child-panel { opacity: 1; }
            a:hover { text-decoration: underline; }
            .deep .ctx:hover .target { color: blue; }
            @media (min-width: 100px) {
                .media-hover:hover { color: green; }
                .media-hover:hover .media-drop { display: block; }
            }
            @keyframes spin { from { transform: rotate(0); } to { transform: rotate(9deg); } }
            .input:not(:hover) { border-color: gray; }
            .combo-a:hover, .combo-b:hover { outline: 1px solid; }
            a:hovered { color: pink; }
        "#;
        let r = scan_hover_rules(&[css], 1);
        assert_eq!(
            r.triggers,
            vec![
                ".menu-item", "a", ".deep .ctx", ".media-hover", ".combo-a", ".combo-b",
            ],
            "触发器 = 首个含 :hover 段去 :hover、保留前缀；@media 内展开；注释/.plain/@keyframes/:not(:hover)/:hovered 不产生；逗号分组独立；同串去重（同选择器只查一次）"
        );
        assert_eq!(
            r.reveals,
            vec![
                ".menu-item .dropdown",
                ".menu-item > .child-panel",
                ".deep .ctx .target",
                ".media-hover .media-drop",
            ],
            "reveal = hover 段之后还有段才派生；组合子原样保留"
        );
        assert!(!r.triggers.contains(&".input:not(:hover)".to_string()));
        assert!(!r.truncated, "预算内完整");
    }

    /// 顶层语句（@import/@charset）不污染后续选择器提取。
    #[test]
    fn hover_scan_ignores_import_and_charset_statements() {
        let css = "@charset \"utf-8\";\n@import url('x.css');\n.btn:hover { color: red; }\n";
        let r = scan_hover_rules(&[css], 1);
        assert_eq!(r.triggers, vec![".btn"]);
        assert!(r.reveals.is_empty());
    }

    /// 预算三档：规则数 256（第 257 条起丢弃并置 truncated）、单表 2MiB
    /// （整表跳过置 truncated）、表数 32（sheet_total 超出置 truncated）。
    #[test]
    fn hover_scan_budgets_truncate_honestly() {
        // 规则预算：257 条 hover 规则 → 截断，且 256 条在表
        let css_many = (0..300)
            .map(|i| format!(".c{i}:hover {{ color: red; }}"))
            .collect::<Vec<_>>()
            .join("\n");
        let r = scan_hover_rules(&[&css_many], 1);
        assert!(r.truncated);
        assert_eq!(r.triggers.len(), MAX_HOVER_RULES, "恰好取满预算后停");

        // 单表超 2MiB：跳过 + 截断（一个占位符表填充超限文本）
        let huge = format!(".x:hover {{}}\n{}", "a".repeat(MAX_HOVER_SHEET_BYTES));
        let r2 = scan_hover_rules(&[huge.as_str()], 1);
        assert!(r2.truncated && r2.triggers.is_empty());

        // 表数超出 32：sheet_total 如实上报 → 截断（文本仅 1 张也可判）
        let r3 = scan_hover_rules(&[".y:hover {}"], MAX_HOVER_SHEETS + 1);
        assert!(r3.truncated);
        assert_eq!(r3.triggers, vec![".y"], "表内规则仍提取");

        // 预算内不误报
        let r4 = scan_hover_rules(&[".z:hover {}"], 1);
        assert!(!r4.truncated);
    }

    /// 属性选择器内逗号/括号与伪函数参数不干扰顶层切分。
    #[test]
    fn hover_scan_handles_nesting_inside_brackets_and_parens() {
        let css = r#"
            [data-list="a,b"]:hover { color: red; }
            .w :is(.p, .q):hover { color: blue; }
        "#;
        let r = scan_hover_rules(&[css], 1);
        assert_eq!(
            r.triggers,
            vec!["[data-list=\"a,b\"]", ".w :is(.p, .q)"],
            "括号/引号内的分隔符不切分；:is() 参数保留"
        );
    }

    /// 伪元素剥离：产物是 DOM.querySelectorAll 查询串，伪元素不可查询
    /// （Chrome 报错 → hoverScan 恒 truncated 的狼来了形态）。触发器/`reveal`
    /// 回归宿主元素（`a:hover::after` → `a`）；无宿主的纯伪元素段整条跳过
    /// （回归 * 会误标全页）；旧式单冒号四者、`::part(...)` 括号体、
    /// `:aftermath` 类伪类名边界均钉住。
    #[test]
    fn hover_scan_strips_pseudo_elements_to_queryable_selectors() {
        let css = r#"
            .tip:hover::after { content: 'x'; }
            .btn:hover { color: red; }
            a:hover:before { content: '•'; }
            .wrap:hover .panel::before { opacity: 1; }
            input:hover::placeholder { color: gray; }
            .tab:hover::part(label) { color: blue; }
            :hover::first-line { color: green; }
            .aftermath:hover { color: pink; }
            a[data-x="::before"]:hover { color: teal; }
        "#;
        let r = scan_hover_rules(&[css], 1);
        assert_eq!(
            r.triggers,
            vec![
                ".tip", ".btn", "a", ".wrap", "input", ".tab", ".aftermath",
                "a[data-x=\"::before\"]",
            ],
            "触发器回归宿主元素；:hover::first-line 无宿主整条跳过；::part 括号体一起剥；属性选择器字面量 ::before 不误伤"
        );
        assert_eq!(r.reveals, vec![".wrap .panel"], "reveal 同样剥伪元素段");
        assert!(!r.truncated);
    }

    #[test]
    fn normalize_ref_strips_exactly_one_leading_at_and_keeps_case() {
        assert_eq!(normalize_ref("@e3"), "e3");
        assert_eq!(normalize_ref("e3"), "e3");
        assert_eq!(normalize_ref("@@e3"), "@e3", "只剥一个 @——查表必失配");
        assert_eq!(normalize_ref("@E3"), "E3", "大小写敏感，从不折叠");
        assert_eq!(normalize_ref("E3"), "E3");
    }

    // ---------------- 元素截图几何（selector 路径 clip 用）----------    // ---------------- 错误码表 ------------------------------------------------

    /// quads_bbox 取全部 quad 的并集（与点击取最大 quad 的差异是刻意的）：
    /// 跨行 inline 元素两个 quad，clip 必须覆盖两行——取最大 quad 会截丢另一行。
    #[test]
    fn quads_bbox_unions_all_valid_quads() {
        let quads = vec![
            vec![10.0, 20.0, 110.0, 20.0, 110.0, 70.0, 10.0, 70.0], // 第一行
            vec![10.0, 90.0, 60.0, 90.0, 60.0, 140.0, 10.0, 140.0], // 第二行
        ];
        assert_eq!(
            quads_bbox(&quads),
            Some(Rect {
                x: 10.0,
                y: 20.0,
                width: 100.0,
                height: 120.0
            })
        );
        // 非法 quad（长度 ≠ 8 / 非有限数）不参与并集
        let junk = vec![
            vec![0.0, 0.0, 5.0],                                   // 长度 ≠ 8
            vec![f64::NAN, 0.0, 10.0, 0.0, 10.0, 10.0, 0.0, 10.0], // 非有限数
            vec![0.0, 0.0, 10.0, 0.0, 10.0, 10.0, 0.0, 10.0],      // 唯一合法
        ];
        assert_eq!(
            quads_bbox(&junk),
            Some(Rect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0
            })
        );
    }

    /// 全退化（空表 / 零面积包围盒）→ None：元素无可截图几何，由调用方走
    /// rect 回退链或报 PAGE_TARGET_MOVED（与点击链同语义）。
    #[test]
    fn quads_bbox_none_for_empty_or_degenerate() {
        assert_eq!(quads_bbox(&[]), None);
        let zero_width = vec![vec![5.0, 5.0, 5.0, 5.0, 5.0, 25.0, 5.0, 25.0]];
        assert_eq!(quads_bbox(&zero_width), None, "零宽包围盒 = 无几何");
    }

    /// rounded_clip 外扩取整：floor 左上 / ceil 右下保证覆盖，最小 1×1。
    /// 亚像素元素（如 10.2..110.8）整数化后不丢边缘。
    #[test]
    fn rounded_clip_expands_outward_to_integers() {
        let sub_pixel = Rect {
            x: 10.2,
            y: 20.7,
            width: 100.6,
            height: 50.1,
        };
        assert_eq!(
            rounded_clip(&sub_pixel),
            Rect {
                x: 10.0,
                y: 20.0,
                width: 101.0,
                height: 51.0
            }
        );
        // 已是整数的矩形不动（ceil-floor 恰好还原）
        let exact = Rect {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 50.0,
        };
        assert_eq!(rounded_clip(&exact), exact);
    }

    // ---------------- press 键系统 ---------------------------------------------

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

    #[test]
    fn named_key_lookup_is_case_insensitive() {
        assert_eq!(parse_key("enter").unwrap().key, "Enter");
        assert_eq!(parse_key("PAGEUP").unwrap().code, "PageUp");
        assert_eq!(parse_key("Escape").unwrap().key, "Escape");
    }

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

    /// 标点：无标准 code——text 承载字符；裸 '+' 仍是单键
    #[test]
    fn punctuation_carries_text_without_code_or_vk() {
        let comma = parse_key(",").unwrap();
        assert_eq!(comma.code, "");
        assert_eq!(comma.text, Some(",".into()));
        assert_eq!(comma.vk, None);
        // '+' 切分后 token 全空 → 整串为主键
        assert_eq!(parse_key("+").unwrap().text, Some("+".into()));
    }

    #[test]
    fn modifier_combos_set_bitfield() {
        assert_eq!(parse_key("Control+A").unwrap().modifiers, MOD_CTRL);
        assert_eq!(parse_key("Meta+V").unwrap().modifiers, MOD_META);
        assert_eq!(parse_key("Shift+ArrowDown").unwrap().modifiers, MOD_SHIFT);
        let alt_f4 = parse_key("Alt+F4").unwrap();
        assert_eq!(alt_f4.modifiers, MOD_ALT);
        assert_eq!(alt_f4.key, "F4");
    }

    /// 别名：折叠到规范名、重复塌缩、规范序输出
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

    /// dispatch 序列的 JSON 形态
    fn dispatch_json(kp: &KeyPress) -> Vec<Value> {
        kp.dispatch_sequence()
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect()
    }

    #[test]
    fn dispatch_enter_is_rawkeydown_char_keyup() {
        let events = dispatch_json(&parse_key("Enter").unwrap());
        assert_eq!(
            events[0],
            json!({ "type": "rawKeyDown", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "modifiers": 0 })
        );
        // char 事件带 text、不带 vk
        assert_eq!(
            events[1],
            json!({ "type": "char", "key": "Enter", "code": "Enter", "text": "\r", "modifiers": 0 })
        );
        assert_eq!(
            events[2],
            json!({ "type": "keyUp", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "modifiers": 0 })
        );
    }

    #[test]
    fn dispatch_keys_without_text_skip_char() {
        let events = dispatch_json(&parse_key("Escape").unwrap());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "rawKeyDown");
        assert_eq!(events[1]["type"], "keyUp");
    }

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

    /// 标点的 dispatch 形态：无 code/vk——字段整体省略
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

    /// 显示名：规范序修饰键 + 主键，' ' 显示为 Space
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

    // ---------------- wait 系统 -------------------------------------------------

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

    #[test]
    fn wait_observe_expr_embeds_selector_and_visibility_semantics() {
        let expr = wait_observe_expr(".x");
        assert!(expr.contains(r#"document.querySelector(".x")"#));
        assert!(expr.contains("getBoundingClientRect"));
        assert!(expr.contains("style.display !== 'none'"));
        assert!(expr.contains("style.visibility !== 'hidden'"));
        assert!(expr.contains("detached"));
    }

    /// 状态判定矩阵：visible 只认可见；detached 只认缺席；attached 认一切已挂载；
    /// hidden 认挂载但不可见
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

    /// unknown 边界：eval 返回垃圾 → unknown；对 attached 依然满足
    /// （判定即 `observed !== 'detached'`）
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

    /// --state 白名单：visible|hidden|attached|detached
    #[test]
    fn wait_state_parse_is_whitelisted() {
        for s in WAIT_STATES {
            assert_eq!(WaitState::parse(s).map(|w| w.as_str()), Some(s));
        }
        assert_eq!(WaitState::parse("bogus"), None);
    }

    // ---------------- 表达式生成器（locate / hit-test / fill / 防注入）--------

    /// json_escape = JSON.stringify 等价（防注入基线：引号/反斜杠/控制字符全转义）
    #[test]
    fn json_escape_is_json_stringify_equivalent() {
        assert_eq!(json_escape(".x"), "\".x\"");
        assert_eq!(json_escape("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_escape("a\\b"), "\"a\\\\b\"");
        assert_eq!(json_escape("a\nb"), "\"a\\nb\"");
        assert_eq!(json_escape(""), "\"\"");
    }

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

    /// 防注入：恶意 selector 只能成为转义后的
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

    // ---------------- 对话框 ------------------------------------------------------

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

    #[test]
    fn dialog_buffer_record_builds_wire_entry_with_sequence() {
        let mut buf = DialogBuffer::new("AAAABBBBCCCCDDDD0000");
        let e1 = buf.record(parse_dialog_opening(&json!({
            "type": "prompt", "message": "name?", "url": "https://x/", "defaultPrompt": "Bob"
        })));
        let e2 = buf.record(parse_dialog_opening(
            &json!({ "type": "alert", "message": "hey" }),
        ));
        // wire 形态：可缺省字段在 JSON 中省略
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

    /// 缓冲至多 32 条，FIFO 丢最旧；
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
        buf.record(parse_dialog_opening(
            &json!({ "type": "alert", "message": "m41" }),
        ));
        buf.record(parse_dialog_opening(
            &json!({ "type": "alert", "message": "m42" }),
        ));
        assert_eq!(buf.dropped(), 10);
        assert_eq!(buf.since(0)[0].sequence, 11, "游标推断：幸存者起点后移");
    }

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

    /// 失败路径不触碰 record（域层协议：record 仅在 handle 成功后调用），
    /// 游标纹丝不动
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

    /// code/status/effect_state 映射全表 + 消息抽查
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
                PageControlError::element_disabled("Element 'x' is disabled"),
                "PAGE_ELEMENT_DISABLED",
                422,
            ),
            (
                PageControlError::action_failed("CDP error (DOM.enable): boom"),
                "PAGE_ACTION_FAILED",
                500,
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
        // 消息形态抽查：文案逐字钉住
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

    /// press 前失败 effect_state=none，press 后=unknown
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
