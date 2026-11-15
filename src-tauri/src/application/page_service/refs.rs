use crate::application::browser_kit::element::RefEntry;
use crate::application::page_service::state::Watched;
use crate::domain::page_control::{normalize_ref, PageControlError};
use crate::error::AppError;

/// TS interact.mjs isRefTarget：'@…' 宽松（CSS 选择器不可能以 @ 开头）、
/// 裸 'e<digits>' 严格；'E3'/'@E3' 走 ref 路径后大小写敏感失配（§3.7），
/// 而不是被误读成选择器。
pub(crate) fn is_ref_target(target: &str) -> bool {
    if target.starts_with('@') {
        return true;
    }
    let rest = target.strip_prefix('e').unwrap_or("");
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// ref 路径的前置查表（I1b：RefMap 语义）。ref 不在表 → 404 PAGE_REF_NOT_FOUND
/// （无表 = 从未 snap 同码；I1b 起导航不再主动失效，stale 不在此产生——
/// 由 kit 解析链的 is_detached / AX 唯一匹配恢复判定产生）。查中返回条目
/// clone（name 供结果文案用）。'@E3' 等大小写形态在这里自然失配 → 404，
/// 不会被 kit 误读成 CSS 选择器。
pub(crate) fn require_ref(
    watched: &Watched,
    tid: &str,
    raw_ref: &str,
) -> Result<RefEntry, AppError> {
    let key = normalize_ref(raw_ref);
    let tabs = watched.refs.lock().unwrap();
    let entry = tabs.get(tid).and_then(|m| m.get(key)).cloned();
    match entry {
        Some(e) => Ok(e),
        // 无表 = 从未 snap：等价 TS 的 throwaway store → ref_not_found
        None => Err(PageControlError::ref_not_found(raw_ref).into()),
    }
}

/// browser_kit 的 String 错误 → chrome-host 错误码体系（集成层契约，kit/mod.rs
/// 头注释约定的映射责任落点）。规则按任务书 S4（含单测全模式）；kit 原文
/// 透传不吞信息。两条任务书未列、但为保全既有契约所必需的规则（I1b 提交
/// 信息有登记）：
/// - "unknown ref"（kit map miss 文案）→ PAGE_REF_NOT_FOUND——红线 1 要求
///   保留 404 ref 未命中语义（否则沦为 500）；
/// - "could not locate element"（kit 唯一匹配恢复无果的文案，仅出现在
///   stale 恢复路径）→ PAGE_REF_STALE——S5 要求导航后旧 ref 报 stale。
pub(crate) fn map_kit_error(raw: &str, tool: &str) -> AppError {
    let m = raw.to_lowercase();
    if m.contains("is stale")
        || m.contains("cannot be recovered")
        || m.contains("could not locate element")
    {
        PageControlError::ref_stale(Some(raw)).into()
    } else if m.contains("is covered by") {
        AppError::business(409, "PAGE_ELEMENT_BLOCKED", raw.to_string())
    } else if m.contains("not visible") || m.contains("outside the viewport") {
        AppError::business(422, "PAGE_ELEMENT_NOT_VISIBLE", raw.to_string())
    } else if m.contains("disabled") {
        PageControlError::element_disabled(raw).into()
    } else if m.contains("not fillable") || m.contains("not editable") {
        AppError::business(422, "PAGE_TARGET_NOT_FILLABLE", raw.to_string())
    } else if m.contains("unknown ref") {
        PageControlError::ref_not_found(raw).into()
    } else if m.contains("not found") {
        AppError::business(404, "PAGE_ELEMENT_NOT_FOUND", raw.to_string())
    } else {
        PageControlError::action_failed(format!("{tool}: {raw}")).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(e: AppError) -> (String, u16) {
        (e.code().to_string(), e.status())
    }

    fn code_ref(e: &AppError) -> (String, u16) {
        (e.code().to_string(), e.status())
    }

    /// map_kit_error 全模式矩阵（任务书 S4：含单测全模式）——每条规则取 kit
    /// element.rs 的真实文案形态，防启发式自嗨
    #[test]
    fn map_kit_error_covers_all_kit_messages() {
        // stale 族：ref 精确失效 + AX 恢复不可判定 + 恢复无果
        for raw in [
            "Element ref e3 is stale",
            "Stale ref for role=button name=Buy cannot be recovered: the snapshot had several matching elements. Run snapshot again and use a fresh ref.",
            "Could not locate element with role=button name=Buy",
        ] {
            assert_eq!(code_ref(&map_kit_error(raw, "click")), ("PAGE_REF_STALE".to_string(), 409), "{raw}");
        }
        // 遮挡（kit intercepted_error 原文）
        assert_eq!(
            code_ref(&map_kit_error(
                "Element '@e1' is covered by <div.banner> at its click point, so the input would land on that element instead.",
                "click"
            )),
            ("PAGE_ELEMENT_BLOCKED".to_string(), 409)
        );
        // 可见性族（ACTIONABILITY_JS 的 reason 两个值）
        for raw in [
            "Element '@e1' is not visible, so the click would not reach it",
            "Element '@e1' is outside the viewport, so the click would not reach it",
        ] {
            assert_eq!(
                code_ref(&map_kit_error(raw, "click")),
                ("PAGE_ELEMENT_NOT_VISIBLE".to_string(), 422),
                "{raw}"
            );
        }
        // 禁用（I1b 新码）
        assert_eq!(
            code_ref(&map_kit_error(
                "Element '@e1' is disabled, so the click would not reach it",
                "click"
            )),
            ("PAGE_ELEMENT_DISABLED".to_string(), 422)
        );
        // fill 链不可编辑（I2 接入 kit fill 时生效；规则先备齐）
        assert_eq!(
            code_ref(&map_kit_error("Element '@e1' is not editable", "fill")),
            ("PAGE_TARGET_NOT_FILLABLE".to_string(), 422)
        );
        assert_eq!(
            code_ref(&map_kit_error(
                "target is not fillable: input type \"checkbox\"",
                "fill"
            )),
            ("PAGE_TARGET_NOT_FILLABLE".to_string(), 422)
        );
        // 未知 ref（kit map miss 文案；契约保全扩展）
        assert_eq!(
            code_ref(&map_kit_error("Unknown ref: e5", "click")),
            ("PAGE_REF_NOT_FOUND".to_string(), 404)
        );
        // not found 族（resolve 链的元素未命中）
        for raw in [
            "Element not found: @e1",
            "Element not found in the selected frame: #x",
        ] {
            assert_eq!(
                code_ref(&map_kit_error(raw, "click")),
                ("PAGE_ELEMENT_NOT_FOUND".to_string(), 404),
                "{raw}"
            );
        }
        // 兜底：500 PAGE_ACTION_FAILED（新码），tool 前缀 + 原文透传
        let e = map_kit_error("CDP error (DOM.enable): boom", "snap");
        assert_eq!(code_ref(&e), ("PAGE_ACTION_FAILED".to_string(), 500));
        assert!(e.to_string().contains("snap"), "tool 上下文在前缀");
        assert!(e.to_string().contains("boom"), "原文不吞");
    }

    /// 规则次序不误伤：stale 优先于 not found（恢复不可判定文案里同时含两类关键词）
    #[test]
    fn map_kit_error_order_stale_beats_not_found() {
        let e = map_kit_error(
            "Stale ref for role=x cannot be recovered: several elements now match. Run snapshot again and use a fresh ref.",
            "click",
        );
        assert_eq!(code(e), ("PAGE_REF_STALE".to_string(), 409));
    }
}
