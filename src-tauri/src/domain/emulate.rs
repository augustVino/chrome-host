//! 设备仿真预设表与请求解析（纯函数，无 IO；W-cap）。
//!
//! [`PRESETS`] 为名称 → 宽高/UA/mobile/touch 的静态表（常见设备 6 款，
//! `iphone-14` 必有；UA 用真实 Chrome iOS/Android 模板）。[`resolve`] 把
//! API body 的三种形态（preset / viewport / off）解析成 [`EmulateAction`]，
//! 应用层只做 CDP 翻译，不掺业务判断——预设表可离线单测。
//!
//! 错误语义：
//! - 未知预设 → 422 `PAGE_EMULATE_UNKNOWN_PRESET`（本模块唯一新错误码；
//!   message 列出全部可用预设名）；
//! - 形态冲突 / 缺必填 / 非法宽高 → 400 `INVALID_REQUEST`（复用通用校验码，
//!   对齐 wait 的 `--state` 校验风格，不另造码）。
//!
//! override 生命周期（契约文档口径）：仿真参数绑定在 tab 的 page session 上，
//! 连接重建 / 实例重启自然清除；同一 tab 重复 emulate = 覆盖（未给的字段
//! 恢复默认，非合并）；`off` 清全部（viewport / UA / touch）。

use crate::error::AppError;

/// 一款设备的仿真参数（视口 CSS 像素）。
pub struct EmulatePreset {
    pub name: &'static str,
    pub width: i64,
    pub height: i64,
    /// 真实浏览器 UA 模板（iOS 系为 Chrome iOS「CriOS」形态）
    pub user_agent: &'static str,
    pub mobile: bool,
    pub touch: bool,
}

/// 预设表（6 款常见设备；尺寸为各设备的 CSS 视口尺寸）。
pub const PRESETS: &[EmulatePreset] = &[
    EmulatePreset {
        name: "iphone-14",
        width: 390,
        height: 844,
        user_agent: "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) \
                     AppleWebKit/605.1.15 (KHTML, like Gecko) \
                     CriOS/126.0.6478.114 Mobile/15E148 Safari/604.1",
        mobile: true,
        touch: true,
    },
    EmulatePreset {
        name: "iphone-se",
        width: 375,
        height: 667,
        user_agent: "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) \
                     AppleWebKit/605.1.15 (KHTML, like Gecko) \
                     CriOS/126.0.6478.114 Mobile/15E148 Safari/604.1",
        mobile: true,
        touch: true,
    },
    EmulatePreset {
        name: "pixel-7",
        width: 412,
        height: 915,
        user_agent: "Mozilla/5.0 (Linux; Android 14; Pixel 7) \
                     AppleWebKit/537.36 (KHTML, like Gecko) \
                     Chrome/126.0.6478.122 Mobile Safari/537.36",
        mobile: true,
        touch: true,
    },
    EmulatePreset {
        name: "galaxy-s23",
        width: 360,
        height: 780,
        user_agent: "Mozilla/5.0 (Linux; Android 14; SM-S911B) \
                     AppleWebKit/537.36 (KHTML, like Gecko) \
                     Chrome/126.0.6478.122 Mobile Safari/537.36",
        mobile: true,
        touch: true,
    },
    EmulatePreset {
        name: "ipad-air",
        width: 820,
        height: 1180,
        user_agent: "Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) \
                     AppleWebKit/605.1.15 (KHTML, like Gecko) \
                     CriOS/126.0.6478.114 Mobile/15E148 Safari/604.1",
        mobile: true,
        touch: true,
    },
    EmulatePreset {
        name: "desktop-1080p",
        width: 1920,
        height: 1080,
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                     AppleWebKit/537.36 (KHTML, like Gecko) \
                     Chrome/126.0.6478.122 Safari/537.36",
        mobile: false,
        touch: false,
    },
];

/// 按名称查预设（大小写敏感，预设名本身全小写）。
pub fn lookup(name: &str) -> Option<&'static EmulatePreset> {
    PRESETS.iter().find(|p| p.name == name)
}

/// 422 PAGE_EMULATE_UNKNOWN_PRESET（message 附可用预设清单）
pub fn unknown_preset(name: &str) -> AppError {
    AppError::business(
        422,
        "PAGE_EMULATE_UNKNOWN_PRESET",
        format!(
            "unknown emulate preset \"{name}\" — available: {}",
            PRESETS
                .iter()
                .map(|p| p.name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
}

/// 自定义视口（宽高为 CSS 像素；构造走 [`EmulateRequest::viewport`]，
/// 校验在 [`resolve`]）。
pub struct Viewport {
    pub width: i64,
    pub height: i64,
}

/// emulate 请求的领域形态（API / MCP / CLI 共用；Option 语义与 wire 一致）。
pub struct EmulateRequest {
    pub preset: Option<String>,
    pub viewport: Option<Viewport>,
    pub user_agent: Option<String>,
    pub mobile: Option<bool>,
    pub touch: Option<bool>,
    pub off: bool,
}

/// 解析后的动作：应用层直接翻译成 CDP 命令序列，不再做判断。
#[derive(Debug)]
pub enum EmulateAction {
    /// 应用仿真（覆盖语义：user_agent=None 表示本次**不**覆盖 UA——
    /// 应用层需先清掉旧 override，未给 ≠ 保持旧的）
    Apply(EmulateSpec),
    /// 清除全部 override（viewport / UA / touch）
    Off,
}

/// 一次应用的完整参数。
#[derive(Debug)]
pub struct EmulateSpec {
    pub width: i64,
    pub height: i64,
    pub mobile: bool,
    pub touch: bool,
    /// None = 不覆盖 UA
    pub user_agent: Option<String>,
    /// 结果文案标注来源（preset 名 / 自定义）
    pub origin: EmulateOrigin,
}

#[derive(Debug)]
pub enum EmulateOrigin {
    Preset(&'static str),
    Custom,
}

impl EmulateRequest {
    /// 是否携带了除 off 之外的任何字段（形态冲突检测用）
    fn has_fields(&self) -> bool {
        self.preset.is_some()
            || self.viewport.is_some()
            || self.user_agent.is_some()
            || self.mobile.is_some()
            || self.touch.is_some()
    }
}

/// 请求 → 动作。三种合法形态（preset / viewport / off）互斥：
/// - off 与其他字段同给 → 400（矛盾输入不静默取舍）；
/// - preset 自带全部参数，与 viewport/userAgent/mobile/touch 同给 → 400；
/// - viewport 形态：宽高必填且 > 0；mobile 缺省 false、touch 缺省 false；
/// - 三者皆缺 → 400。
pub fn resolve(req: &EmulateRequest) -> Result<EmulateAction, AppError> {
    if req.off {
        if req.has_fields() {
            return Err(AppError::invalid_request(
                "emulate: \"off\" 不能与 preset/viewport/userAgent/mobile/touch 同用（off = 清除全部仿真）",
            ));
        }
        return Ok(EmulateAction::Off);
    }
    if let Some(name) = &req.preset {
        if req.viewport.is_some()
            || req.user_agent.is_some()
            || req.mobile.is_some()
            || req.touch.is_some()
        {
            return Err(AppError::invalid_request(
                "emulate: \"preset\" 自带全部参数，不能与 viewport/userAgent/mobile/touch 同用",
            ));
        }
        let p = lookup(name).ok_or_else(|| unknown_preset(name))?;
        return Ok(EmulateAction::Apply(EmulateSpec {
            width: p.width,
            height: p.height,
            mobile: p.mobile,
            touch: p.touch,
            user_agent: Some(p.user_agent.to_string()),
            origin: EmulateOrigin::Preset(p.name),
        }));
    }
    if let Some(v) = &req.viewport {
        if v.width <= 0 || v.height <= 0 {
            return Err(AppError::invalid_request(format!(
                "emulate: viewport 宽高必须为正整数（got {}×{}）",
                v.width, v.height
            )));
        }
        return Ok(EmulateAction::Apply(EmulateSpec {
            width: v.width,
            height: v.height,
            mobile: req.mobile.unwrap_or(false),
            touch: req.touch.unwrap_or(false),
            user_agent: req.user_agent.clone(),
            origin: EmulateOrigin::Custom,
        }));
    }
    Err(AppError::invalid_request(
        "emulate: 需要 preset、viewport 或 off=true 之一",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 预设表完整性：规模在 5-8、名称唯一、iphone-14 必有、iOS 系预设的
    /// UA 是真实 Chrome iOS 模板（CriOS 标记）、桌面预设不带 mobile 标记。
    #[test]
    fn preset_table_is_sane() {
        assert!(
            (5..=8).contains(&PRESETS.len()),
            "预设数应在 5-8，实际 {}",
            PRESETS.len()
        );
        let mut names: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "预设名必须唯一");
        let iphone14 = lookup("iphone-14").expect("iphone-14 预设必须存在");
        assert_eq!((iphone14.width, iphone14.height), (390, 844));
        assert!(iphone14.mobile && iphone14.touch);
        for p in PRESETS {
            assert!(p.user_agent.starts_with("Mozilla/5.0"), "{} UA 模板", p.name);
            assert!(p.width > 0 && p.height > 0, "{} 尺寸", p.name);
            // iOS 系（iPhone/iPad）必须是 Chrome iOS 的 CriOS 形态，Android 系
            // 是 Linux; Android 的移动 Chrome——UA 真实性是本表的核心价值
            let ua = p.user_agent;
            if p.name.starts_with("iphone") || p.name.starts_with("ipad") {
                assert!(ua.contains("CriOS/") && ua.contains("AppleWebKit/605.1.15"),
                    "{} 应为 Chrome iOS UA", p.name);
            } else if p.name.starts_with("pixel") || p.name.starts_with("galaxy") {
                assert!(ua.contains("(Linux; Android") && ua.contains("Mobile Safari"),
                    "{} 应为 Android Chrome UA", p.name);
            }
        }
        assert!(lookup("iphone-99").is_none(), "未知预设应查不到");
    }

    fn req(
        preset: Option<&str>,
        viewport: Option<(i64, i64)>,
        ua: Option<&str>,
        mobile: Option<bool>,
        touch: Option<bool>,
        off: bool,
    ) -> EmulateRequest {
        EmulateRequest {
            preset: preset.map(str::to_string),
            viewport: viewport.map(|(width, height)| Viewport { width, height }),
            user_agent: ua.map(str::to_string),
            mobile,
            touch,
            off,
        }
    }

    /// preset 形态：字段整体来自表，不接受任何覆盖字段。
    #[test]
    fn resolve_preset_form_uses_table_values() {
        let action = resolve(&req(Some("iphone-14"), None, None, None, None, false)).unwrap();
        match action {
            EmulateAction::Apply(spec) => {
                assert_eq!((spec.width, spec.height), (390, 844));
                assert!(spec.mobile && spec.touch);
                assert!(spec.user_agent.as_deref().unwrap().contains("CriOS/"));
                assert!(matches!(spec.origin, EmulateOrigin::Preset("iphone-14")));
            }
            _ => panic!("应为 Apply"),
        }
        // preset + 任何覆盖字段 → 400
        for r in [
            req(Some("iphone-14"), Some((1, 1)), None, None, None, false),
            req(Some("iphone-14"), None, Some("UA"), None, None, false),
            req(Some("iphone-14"), None, None, Some(true), None, false),
            req(Some("iphone-14"), None, None, None, Some(true), false),
        ] {
            let e = resolve(&r).unwrap_err();
            assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400));
        }
    }

    /// viewport 形态：宽高必填正整数；mobile/touch 缺省 false；UA 可选。
    #[test]
    fn resolve_viewport_form_defaults_mobile_touch_false() {
        let action = resolve(&req(None, Some((390, 844)), None, None, None, false)).unwrap();
        match action {
            EmulateAction::Apply(spec) => {
                assert_eq!((spec.width, spec.height), (390, 844));
                assert!(!spec.mobile, "自定义视口 mobile 缺省 false");
                assert!(!spec.touch, "自定义视口 touch 缺省 false");
                assert!(spec.user_agent.is_none());
                assert!(matches!(spec.origin, EmulateOrigin::Custom));
            }
            _ => panic!("应为 Apply"),
        }
        let with_all = resolve(&req(None, Some((800, 600)), Some("UA"), Some(true), Some(true), false))
            .unwrap();
        match with_all {
            EmulateAction::Apply(spec) => {
                assert!(spec.mobile && spec.touch);
                assert_eq!(spec.user_agent.as_deref(), Some("UA"));
            }
            _ => panic!("应为 Apply"),
        }
        // 非法宽高 → 400（零/负数）
        for vp in [(0, 844), (390, 0), (-1, 844)] {
            let e = resolve(&req(None, Some(vp), None, None, None, false)).unwrap_err();
            assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400), "{vp:?}");
        }
    }

    /// off 形态：单独 off → Off；off 混其他字段 → 400；三形态皆缺 → 400。
    #[test]
    fn resolve_off_and_missing_form() {
        assert!(matches!(
            resolve(&req(None, None, None, None, None, true)).unwrap(),
            EmulateAction::Off
        ));
        let mixed = resolve(&req(Some("iphone-14"), None, None, None, None, true)).unwrap_err();
        assert_eq!((mixed.code(), mixed.status()), ("INVALID_REQUEST", 400));
        let none = resolve(&req(None, None, None, None, None, false)).unwrap_err();
        assert_eq!((none.code(), none.status()), ("INVALID_REQUEST", 400));
    }

    /// 未知预设 → 422 PAGE_EMULATE_UNKNOWN_PRESET，message 附可用清单。
    #[test]
    fn unknown_preset_is_422_with_available_list() {
        let e = resolve(&req(Some("nokia-3310"), None, None, None, None, false)).unwrap_err();
        assert_eq!((e.code(), e.status()), ("PAGE_EMULATE_UNKNOWN_PRESET", 422));
        assert!(e.to_string().contains("iphone-14"), "应列出可用预设");
    }
}
