//! page diff / diff-shot：快照文本 diff 与像素 diff。
//!
//! - diff：当前 snap（snap_tool 全管线，refs 正常演进）↔ 基线的行级 unified
//!   diff（kit diff.rs）。基线两来源：滚动基线（per-tab `last_snap`，比较后
//!   更新）或显式 baseline 文本（API `baseline` 参数，只比较不更新）。
//!   diff 文本**不剥 @eN 记号**（durable refs 下同文档 ref 稳定，
//!   记号漂移即真实变化信号——kit 的 snapshot_comparison_text 剥记号路径
//!   是跨文档比较用的，这里不用）。
//! - diff-shot：显式 baseline png（base64）↔ 服务端当前截图（screenshot_tool
//!   全管线，三形态契约不变）→ kit 像素 diff（差异图红标 + 灰阶底）。

use super::capture::screenshot_tool;
use super::snap::snap_tool;
use super::state::Watched;
use super::{PageService, SnapParams};
use crate::application::browser_kit::diff::{diff_screenshot, diff_snapshots};
use crate::error::AppError;
use base64::Engine as _;
use serde_json::{json, Value};

/// last_snap 单条上限（内存边界）：当前快照超限时**不存**且清掉旧
/// 基线（防陈旧基线与后续快照错位比较），diff 响应披露 baselineCleared——
/// 下次无显式 baseline 的 diff 将与空基线比较（全部计为 added）。
const LAST_SNAP_MAX_BYTES: usize = 2 * 1024 * 1024;

/// 像素 diff 的颜色距离阈值（0-1，kit diff_screenshot 的 threshold）：
/// 每像素 RGB 欧氏距离 > threshold×255×√3 才计差异。0.1 容忍抗锯齿/
/// 亚像素渲染噪声（同页面两次截图不报假差异），明显的内容变化（文字、
/// 色块）远超此阈值。
const PIXEL_DIFF_THRESHOLD: f64 = 0.1;

impl PageService {
    /// page diff <tab> [--selector] [--compact] [--depth]：快照文本 diff。
    /// 无 baseline = 与 per-tab 滚动基线比较并更新之；有 baseline（显式
    /// 文本）= 只比较不更新。首次无基线 → 与空串比较（一切皆 added）。
    pub async fn diff(
        &self,
        ins_id: &str,
        tab: &str,
        params: SnapParams,
        baseline: Option<String>,
    ) -> Result<Value, AppError> {
        self.command(
            super::TOOL_TIMEOUT,
            "diff",
            ins_id,
            tab,
            move |w, tid, sid| async move {
                diff_tool(&w, &tid, &sid, &params, baseline.as_deref()).await
            },
        )
        .await
    }

    /// page diff-shot <tab> [--full] --baseline <b64 png>：像素 diff。
    /// baseline 为必填的 base64 png（CLI --baseline-file 读文件后编码；
    /// API 调用方自行编码）；当前截图走 screenshot_tool 三形态契约
    ///（full = 全页，缺省视口）。
    pub async fn diff_shot(
        &self,
        ins_id: &str,
        tab: &str,
        full: bool,
        baseline_b64: &str,
    ) -> Result<Value, AppError> {
        if baseline_b64.is_empty() {
            return Err(AppError::invalid_request(
                "diff-shot 需要基线截图（CLI --baseline-file / API baseline，base64 png）",
            ));
        }
        self.command(
            super::TOOL_TIMEOUT,
            "diff-shot",
            ins_id,
            tab,
            move |w, tid, sid| async move {
                diff_shot_tool(&w, &tid, &sid, full, baseline_b64.to_string()).await
            },
        )
        .await
    }
}

/// diff 工具编排：snap（refs 演进）→ 取基线 → kit 行级 diff → 基线滚动
/// 更新（无显式 baseline 时）。
pub(crate) async fn diff_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    params: &SnapParams,
    baseline: Option<&str>,
) -> Result<Value, AppError> {
    // 当前快照：snap_tool 全管线（refs 演进 / hover / generation，与 page
    // snap 命令完全同源——diff 不另造快照语义）
    let snap_resp = snap_tool(watched, tid, sid, params).await?;
    let current = snap_resp
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // 基线：显式文本（只比较）> 滚动基线（无条目 = 空串，首次全 added）
    let base_text = match baseline {
        Some(b) => b.to_string(),
        None => watched
            .last_snap
            .lock()
            .unwrap()
            .get(tid)
            .cloned()
            .unwrap_or_default(),
    };
    let result = diff_snapshots(&base_text, &current);
    let (added, removed) = parse_unified_lines(&result.diff);
    // 基线滚动更新：无显式 baseline 时才写；2MiB 上限（超限清旧基线 +
    // baselineCleared 披露，见常量注释）
    let mut baseline_cleared = false;
    if baseline.is_none() {
        let mut store = watched.last_snap.lock().unwrap();
        if current.len() <= LAST_SNAP_MAX_BYTES {
            store.insert(tid.to_string(), current);
        } else {
            store.remove(tid);
            baseline_cleared = true;
        }
    }
    let mut resp = json!({
        "changed": result.changed,
        "added": added,
        "removed": removed,
        "counts": {
            "added": result.additions,
            "removed": result.removals,
            "unchanged": result.unchanged,
        },
        "diff": result.diff,
    });
    if baseline_cleared {
        resp["baselineCleared"] = json!(true);
    }
    Ok(resp)
}

/// diff-shot 工具编排：当前截图（screenshot_tool 全管线）→ 双方解码 →
/// kit 像素 diff → { differing, ratio, image? }。ratio = 差异像素百分比
///（0-100，kit mismatch_percentage 原值）；尺寸不一致 → differing=true +
/// ratio=100 + dimensionMismatch 披露（不产差异图）。
pub(crate) async fn diff_shot_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    full: bool,
    baseline_b64: String,
) -> Result<Value, AppError> {
    let shot = screenshot_tool(watched, tid, sid, None, full).await?;
    let current_b64 = shot
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::internal("截图响应缺少 data 字段"))?;
    let engine = base64::engine::general_purpose::STANDARD;
    let baseline_bytes = engine
        .decode(baseline_b64.as_bytes())
        .map_err(|e| AppError::invalid_request(format!("baseline base64 解码失败: {e}")))?;
    let current_bytes = engine
        .decode(current_b64.as_bytes())
        .map_err(|e| AppError::internal(format!("当前截图 base64 解码失败: {e}")))?;
    let result =
        diff_screenshot(&baseline_bytes, &current_bytes, PIXEL_DIFF_THRESHOLD).map_err(|e| {
            AppError::invalid_request(format!("像素 diff 失败（多为 baseline 非 png 数据）: {e}"))
        })?;
    let mut resp = json!({
        "differing": !result.matched,
        "ratio": result.mismatch_percentage,
    });
    if let Some(dm) = result.dimension_mismatch {
        resp["dimensionMismatch"] = dm;
    }
    if let Some(img) = result.diff_image {
        resp["image"] = json!(engine.encode(img));
    }
    Ok(resp)
}

/// unified diff 文本 → (added 行, removed 行)。行内容剥去 diff 前缀符号
///（快照行本身以 "- " 开头：removed 行在 diff 文本中形如 "-- button …"，
/// 剥一个前缀还原原行）。跳过 `---`/`+++` 文件头与 `@@` hunk 头；上下文
/// 行（前导空格）不进列表。纯函数，单测覆盖。
pub(crate) fn parse_unified_lines(diff: &str) -> (Vec<String>, Vec<String>) {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    for line in diff.lines() {
        if line.starts_with("---") || line.starts_with("+++") || line.starts_with("@@") {
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            added.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix('-') {
            removed.push(rest.to_string());
        }
    }
    (added, removed)
}

// ---------------- 纯函数单测 ----------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 行提取：+/- 前缀剥除、文件头与 hunk 头跳过、快照行首 "- " 不干扰
    #[test]
    fn parse_unified_lines_extracts_added_and_removed() {
        let diff = "--- before\n+++ after\n@@ -1,3 +1,4 @@\n - button \"Keep\" @e1\n-- button \"Old\" @e2\n+- button \"New\" @e3\n+- textbox \"Q\" @e4\n";
        let (added, removed) = parse_unified_lines(diff);
        assert_eq!(
            added,
            vec![
                "- button \"New\" @e3".to_string(),
                "- textbox \"Q\" @e4".to_string()
            ]
        );
        assert_eq!(removed, vec!["- button \"Old\" @e2".to_string()]);
    }

    /// identical 快路径（kit 返回空 diff 文本）→ 双列表空
    #[test]
    fn parse_unified_lines_empty_diff_yields_empty_lists() {
        let (added, removed) = parse_unified_lines("");
        assert!(added.is_empty() && removed.is_empty());
    }
}
