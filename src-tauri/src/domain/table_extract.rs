//! 表格结构化提取——归一化纯函数层（W-ext，TODO.md 第 11 项）。
//!
//! 两段式设计（参考 BrowserSkill extract）：收集层（注入 JS，见
//! page_control.rs 的 EXTRACT_COLLECT_FN）在页面内只读采集「DOM 事实」
//! （分区/th-td 文本/rowspan-colspan/aria 行列声明），**不解析、不解释**；
//! 本模块把事实装配成逻辑表——列名解析、合并单元格展开、跨行 header
//! headerPath、预算截断、coverage 诚实证据。纯函数无 IO，可离线单测。
//!
//! 语义契约（AGENT-API.md 同步披露）：
//! - **null ≠ 空串**：单元格值为 `null` = 该逻辑位无值（被 rowspan/colspan
//!   覆盖，或 aria-colindex 稀疏网格的空位）；`""` = 真实空单元格。
//! - **coverage.datasetComplete 永不为 true**：`unknown`（无 aria 行列声明，
//!   完整性不可判定）| `incomplete`（aria 声明 > 实际、预算截断、采集侧
//!   提前停止、无数据行）。诚实证据哲学：不编造「已取全」。
//! - 列名优先级：thead th → 首行全 th（提升为表头）→ 生成 `col_N`
//!   （来源标注 header / first-row / generated）。

use serde::Deserialize;
use serde_json::{json, Value};

/// 逻辑表单元格总数上限（预算）：超限**整行**截断（不切半行），coverage
/// 标注 truncated。收集层同值提前停止（EXTRACT_COLLECT_FN 的 BUDGET），
/// 两侧常量必须一致。
pub const CELL_BUDGET: usize = 10_000;

// ============================================================================
// 事实层类型（收集器 wire 格式，serde 宽松反序列化：未知字段忽略、缺省容错）
// ============================================================================

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TableFacts {
    /// 目标解析为表格元素（自动发现命中 / 显式目标是表格）
    pub found: bool,
    /// found=false 的原因：`no-table`（自动发现无表格）/ `not-a-table`（目标非表格）
    pub reason: Option<String>,
    /// 目标元素标签（not-a-table 时供错误信息）
    pub tag: Option<String>,
    /// 目标元素 role 属性（同上）
    pub role: Option<String>,
    /// html（<table>）| aria（role=table/grid/treegrid）
    pub format: String,
    pub caption: Option<String>,
    pub aria: Option<AriaDecl>,
    /// display:none 被跳过的行数（coverage.hiddenRowsOmitted）
    pub hidden_rows: u64,
    /// 可见行总数（含表头行、含超出采集预算只计数未采集的行）
    pub total_rows: u64,
    /// 可见行的单元格总数（含超出预算只计数的）
    pub total_cells: u64,
    /// 收集侧预算提前停止（后面还有行未采集）
    pub partial: bool,
    pub sections: Vec<SectionFacts>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AriaDecl {
    /// aria-rowcount（正值才有意义；0/-1 为「未知」语义，收集侧已滤）
    pub row_count: Option<i64>,
    pub col_count: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SectionFacts {
    /// thead / tbody / tfoot / flat（无分区的散行、role 表格的全部行）
    pub kind: String,
    pub rows: Vec<RowFacts>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RowFacts {
    /// aria-rowindex（1 基逻辑行号；虚拟化网格 DOM 顺序可能乱序）
    pub aria_row_index: Option<i64>,
    pub cells: Vec<CellFacts>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CellFacts {
    /// textContent trim 后的文本
    pub text: String,
    /// th 或 role=columnheader/rowheader
    pub header: bool,
    /// 原始 rowspan/colspan（≥1；收集侧 0/非法已归 1）
    pub row_span: u64,
    pub col_span: u64,
    /// aria-colindex（1 基逻辑列号）
    pub aria_col_index: Option<i64>,
    /// headers 属性引用的 id 列表（事实收集，本版归一化不消费）
    pub headers: Vec<String>,
}

// ============================================================================
// 归一化
// ============================================================================

/// 归一化入口：事实 → `{ columns, rows, coverage, meta }`。
/// meta.target 由 application 层补充（归一化不知道目标形态）。
pub fn normalize(f: &TableFacts) -> Value {
    // ---- 1. 行分流：thead 行恒为表头；无 thead 时首个 body/flat 分区的
    // 连续全-th 行提升为表头（first-row）；tfoot 永不提升 ----
    let mut header_rows: Vec<&RowFacts> = Vec::new();
    let mut data_rows: Vec<&RowFacts> = Vec::new();
    let mut promoted = false; // 出现过提升的表头行（source=first-row）
    let mut seen_data = false;
    for sec in &f.sections {
        for row in &sec.rows {
            let all_header = !row.cells.is_empty() && row.cells.iter().all(|c| c.header);
            let is_header = match sec.kind.as_str() {
                "thead" => true,
                "tfoot" => false,
                _ => header_rows.is_empty() && !seen_data && all_header,
            };
            if is_header {
                promoted |= sec.kind != "thead";
                header_rows.push(row);
            } else {
                seen_data = true;
                data_rows.push(row);
            }
        }
    }
    let header_source: &str = if header_rows.is_empty() {
        "generated"
    } else if promoted {
        "first-row"
    } else {
        "header"
    };

    // ---- 2. 网格展开（表头/数据共用；跨行跨列锚点 + 占位 null）。
    // 表头网格只采 header 单元格标签；数据网格全采 ----
    let header_grid = expand_grid(&header_rows, true);
    let mut data_grid = expand_grid(&data_rows, false);

    let width = header_grid.width.max(data_grid.width);

    // ---- 3. 列名：表头网格自上而下取非空标签为 path，末段为 name。
    // colspan 组标签继承：被同 rowspan 锚点覆盖的位置继承**同行**锚点的标签
    //（「基本信息」跨两列→两列 path 都含它）；rowspan 续行不重复
    //（「操作」rowspan=2 → path 只一次）。无标签列生成 col_N
    //（aria-colindex 放置下位置即逻辑列号）----
    let anchor_label = |r: usize, c: usize| -> Option<String> {
        header_grid
            .rows
            .get(r)
            .and_then(|row| row.cells.get(c))
            .and_then(|v| v.as_ref().map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
    };
    let columns: Vec<Value> = (0..width)
        .map(|c| {
            let mut labels: Vec<String> = Vec::new();
            for r in 0..header_rows.len() {
                let label = match anchor_label(r, c) {
                    Some(l) => Some(l),
                    // 覆盖位：同行锚点（colspan 组）继承；跨行锚点（rowspan
                    // 续行）不重复——组语义在首行已表达
                    None => header_grid
                        .covered_by
                        .get(&(r, c))
                        .filter(|(ar, _)| *ar == r)
                        .and_then(|(ar, ac)| anchor_label(*ar, *ac)),
                };
                if let Some(l) = label {
                    labels.push(l);
                }
            }
            match labels.last() {
                Some(name) => json!({
                    "name": name,
                    "source": header_source,
                    "path": labels,
                }),
                None => json!({ "name": format!("col_{}", c + 1), "source": "generated" }),
            }
        })
        .collect();

    // ---- 4. 行排序：全部数据行都声明 aria-rowindex 才按逻辑行号升序
    // （虚拟化网格 DOM 顺序不可信；部分声明时排序反而引入臆测）。
    // 排序后锚点行号按新坐标重映射（spans 记录的是输出坐标）----
    let sortable = !data_grid.rows.is_empty()
        && data_grid.rows.iter().all(|r| r.aria_row_index.is_some());
    if sortable {
        data_grid.rows.sort_by_key(|r| r.aria_row_index.unwrap_or(i64::MAX));
    }
    // 旧行序 → 输出行序（未排序时为恒等映射）
    let mut row_remap = vec![0usize; data_grid.rows.len()];
    for (new_idx, r) in data_grid.rows.iter().enumerate() {
        row_remap[r.orig_idx] = new_idx;
    }

    // ---- 5. 预算：整行截断（不切半行）。收集侧 partial 且确有未采集行
    // 时同样判 truncated——两边口径合流到同一披露位 ----
    let mut kept_rows = 0usize;
    let mut used_cells = 0usize;
    if width > 0 {
        for _ in &data_grid.rows {
            if used_cells + width > CELL_BUDGET {
                break;
            }
            used_cells += width;
            kept_rows += 1;
        }
    }
    let collected = header_rows.len() + data_grid.rows.len();
    // 丢弃数取两口径最大值：网格内丢弃（收集到但超预算）与总量口径
    //（visible_total - 表头 - 保留；收集侧 partial 提前停止时用它补足真实丢弃数）
    let dropped_by_grid = data_grid.rows.len().saturating_sub(kept_rows);
    let visible_total = (f.total_rows.saturating_sub(f.hidden_rows)) as usize;
    let dropped = visible_total
        .saturating_sub(header_rows.len() + kept_rows)
        .max(dropped_by_grid);
    let truncated = dropped > 0 || (f.partial && (f.total_rows as usize) > collected);

    // rows：pad 到 width（缺位 = null），丢弃超预算尾行
    let rows: Vec<Vec<Value>> = data_grid.rows[..kept_rows]
        .iter()
        .map(|r| {
            (0..width)
                .map(|c| match r.cells.get(c) {
                    Some(Some(s)) => Value::String(s.clone()),
                    _ => Value::Null,
                })
                .collect()
        })
        .collect();
    // spans：锚点行号重映射到排序后坐标，截断行内的锚点随之丢弃
    let spans: Vec<Value> = data_grid
        .spans
        .iter()
        .filter_map(|s| {
            let row = *row_remap.get(s.row)?;
            (row < kept_rows).then(|| {
                json!({ "row": row, "col": s.col, "rowspan": s.rowspan, "colspan": s.colspan })
            })
        })
        .collect();

    // ---- 6. coverage（诚实证据）----
    let mut reasons: Vec<String> = Vec::new();
    let visible_rows = f.total_rows;
    if let Some(rc) = f.aria.as_ref().and_then(|a| a.row_count) {
        if rc > visible_rows as i64 {
            reasons.push(format!(
                "aria-rowcount {rc} > 可见行 {visible_rows}（虚拟化未加载完？）"
            ));
        }
    }
    if let Some(cc) = f.aria.as_ref().and_then(|a| a.col_count) {
        if cc > width as i64 {
            reasons.push(format!(
                "aria-colcount {cc} > 观测列 {width}（列未全部渲染？缺失列不生成，只披露）"
            ));
        }
    }
    if truncated {
        reasons.push(format!(
            "单元格预算 {CELL_BUDGET} 超限，整行截断（丢弃 {dropped} 行）"
        ));
    }
    let empty = rows.is_empty();
    if empty {
        // 不装作空表：无数据行 = 虚拟化未渲染或真实空结果，调用方需自行分辨
        reasons.push(
            "无数据行：tbody 为空（虚拟化表格常见——行尚未渲染）或真实空结果；可 page wait 行选择器出现后重试".into(),
        );
    }
    let dataset_complete = if reasons.is_empty() { "unknown" } else { "incomplete" };
    let mut coverage = json!({
        "datasetComplete": dataset_complete,
        "hiddenRowsOmitted": f.hidden_rows,
        "truncated": truncated,
    });
    if !reasons.is_empty() {
        coverage["reasons"] = json!(reasons);
    }

    // ---- 7. meta ----
    let cell_count = kept_rows * width;
    let mut meta = json!({
        "format": f.format,
        "caption": f.caption,
        "headerRows": header_rows.len(),
        "rowCount": kept_rows,
        "columnCount": width,
        "cellCount": cell_count,
        "spans": spans,
    });
    if let Some(a) = &f.aria {
        meta["aria"] = json!({ "rowCount": a.row_count, "colCount": a.col_count });
    }

    json!({ "columns": columns, "rows": rows, "coverage": coverage, "meta": meta })
}

/// 网格展开产物。
struct Grid {
    rows: Vec<GridRow>,
    width: usize,
    /// 展开过程中登记的合并单元格锚点（row 为展开时行序，未含排序重映射）
    spans: Vec<SpanAnchor>,
    /// 覆盖位 → 覆盖它的锚点坐标（表头 path 组标签继承用；数据网格不消费）
    covered_by: std::collections::HashMap<(usize, usize), (usize, usize)>,
}

struct GridRow {
    /// 展开时的原始行序（排序后重映射 spans 行号用）
    orig_idx: usize,
    aria_row_index: Option<i64>,
    /// 每列：Some(text) = 有值；None = 被合并覆盖 / 逻辑空位
    cells: Vec<Option<String>>,
}

#[derive(Clone, Copy)]
struct SpanAnchor {
    row: usize,
    col: usize,
    rowspan: u64,
    colspan: u64,
}

/// rowspan/colspan 展开为稠密网格：
/// - 无 aria-colindex 的单元格顺序占位（跳过已被上方 rowspan 覆盖的列）；
/// - 声明 aria-colindex 的单元格放至逻辑列（colindex-1，1 基→0 基）；
/// - 被覆盖位 = None（与空串严格区分）；锚点记入 spans。
/// `keep_header_only_label`：表头网格只采 header 单元格的文本为标签
/// （thead 里的 td 占宽但不贡献列名）；数据网格采全部文本。
fn expand_grid(rows: &[&RowFacts], keep_header_only_label: bool) -> Grid {
    // 占位表：覆盖到 (r, c) 的锚点跨度（供 rowspan 下探占位）
    let mut covered: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    let mut covered_by: std::collections::HashMap<(usize, usize), (usize, usize)> =
        std::collections::HashMap::new();
    let mut grid = Grid { rows: Vec::new(), width: 0, spans: Vec::new(), covered_by };

    for (r, row) in rows.iter().enumerate() {
        let mut cells: Vec<Option<String>> = Vec::new();
        let ensure = |cells: &mut Vec<Option<String>>, idx: usize| {
            while cells.len() <= idx {
                cells.push(None);
            }
        };
        let mut cursor = 0usize;
        for cell in &row.cells {
            // 逻辑列：声明 colindex 用声明值，否则顺序占位（跳过覆盖位）
            let mut c = match cell.aria_col_index {
                Some(ci) if ci >= 1 => (ci - 1) as usize,
                _ => {
                    while covered.contains(&(r, cursor)) {
                        cursor += 1;
                    }
                    cursor
                }
            };
            while covered.contains(&(r, c)) {
                c += 1; // 声明位被覆盖（稀疏声明 + rowspan 交叠）→ 顺延
            }
            let rowspan = cell.row_span.max(1) as usize;
            let colspan = cell.col_span.max(1) as usize;
            let label = if keep_header_only_label && !cell.header {
                None // thead 里的 td：占宽不贡献列名（数据网格不筛，全采）
            } else {
                Some(cell.text.clone())
            };
            ensure(&mut cells, c + colspan - 1);
            cells[c] = label;
            if rowspan > 1 || colspan > 1 {
                grid.spans.push(SpanAnchor {
                    row: r,
                    col: c,
                    rowspan: cell.row_span.max(1),
                    colspan: cell.col_span.max(1),
                });
                for dr in 0..rowspan {
                    for dc in 0..colspan {
                        if dr > 0 || dc > 0 {
                            covered.insert((r + dr, c + dc));
                            grid.covered_by.insert((r + dr, c + dc), (r, c));
                        }
                    }
                }
            }
            cursor = c + colspan;
        }
        grid.width = grid.width.max(cells.len());
        grid.rows.push(GridRow {
            orig_idx: r,
            aria_row_index: row.aria_row_index,
            cells,
        });
    }
    grid
}

// ============================================================================
// 测试：归一化规则全离线覆盖（W-ext 需求第 5 条）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn facts(sections: Value) -> TableFacts {
        let mut v = json!({ "found": true, "format": "html", "sections": sections });
        // total_rows 缺省按 sections 行数补齐（测试便利；生产侧收集器给真值）
        let rows: usize = v["sections"]
            .as_array()
            .map(|s| {
                s.iter()
                    .map(|sec| sec["rows"].as_array().map(|r| r.len()).unwrap_or(0))
                    .sum()
            })
            .unwrap_or(0);
        v["total_rows"] = json!(rows);
        v["total_cells"] = json!(
            v["sections"]
                .as_array()
                .map(|s| s
                    .iter()
                    .flat_map(|sec| sec["rows"].as_array().cloned().unwrap_or_default())
                    .map(|r| r["cells"].as_array().map(|c| c.len()).unwrap_or(0))
                    .sum::<usize>())
                .unwrap_or(0)
        );
        serde_json::from_value(v).unwrap()
    }

    fn cell(text: &str) -> Value {
        json!({ "text": text, "rowSpan": 1, "colSpan": 1 })
    }
    fn th(text: &str) -> Value {
        json!({ "text": text, "header": true, "rowSpan": 1, "colSpan": 1 })
    }
    fn row(cells: impl Into<Vec<Value>>) -> Value {
        json!({ "cells": cells.into() })
    }

    #[test]
    fn single_row_thead_header_names() {
        let f = facts(json!([
            { "kind": "thead", "rows": [ row([th("姓名"), th("年龄")]) ] },
            { "kind": "tbody", "rows": [ row([cell("张三"), cell("30")]) ] },
        ]));
        let out = normalize(&f);
        assert_eq!(
            out["columns"],
            json!([
                { "name": "姓名", "source": "header", "path": ["姓名"] },
                { "name": "年龄", "source": "header", "path": ["年龄"] },
            ])
        );
        assert_eq!(out["rows"], json!([["张三", "30"]]));
        assert_eq!(out["meta"]["headerRows"], 1);
        // 无 aria 声明 + 数据齐全 → unknown（永不为 true）
        assert_eq!(out["coverage"]["datasetComplete"], "unknown");
    }

    #[test]
    fn first_row_th_promoted_with_source() {
        // 无 thead，首行全 th → 提升为表头，source=first-row
        let f = facts(json!([
            { "kind": "flat", "rows": [
                row([th("订单号"), th("金额")]),
                row([cell("A1"), cell("12.5")]),
            ]},
        ]));
        let out = normalize(&f);
        assert_eq!(out["columns"][0]["source"], "first-row");
        assert_eq!(out["columns"][0]["name"], "订单号");
        assert_eq!(out["rows"], json!([["A1", "12.5"]]));
        assert_eq!(out["meta"]["headerRows"], 1);
    }

    #[test]
    fn mixed_first_row_not_promoted() {
        // 首行混 th/td → 不提升，全列生成名
        let f = facts(json!([
            { "kind": "tbody", "rows": [
                row([th("订单号"), cell("X")]),
                row([cell("A1"), cell("12.5")]),
            ]},
        ]));
        let out = normalize(&f);
        assert_eq!(
            out["columns"],
            json!([
                { "name": "col_1", "source": "generated" },
                { "name": "col_2", "source": "generated" },
            ])
        );
        assert_eq!(out["rows"].as_array().unwrap().len(), 2, "两行都是数据行");
    }

    #[test]
    fn generated_columns_when_no_header() {
        let f = facts(json!([
            { "kind": "tbody", "rows": [
                row([cell("a"), cell("b"), cell("")]),
            ]},
        ]));
        let out = normalize(&f);
        assert_eq!(out["columns"][2], json!({"name": "col_3", "source": "generated"}));
        // 真实空单元格 = ""（与 null 严格区分）
        assert_eq!(out["rows"][0][2], json!(""));
    }

    #[test]
    fn multi_row_thead_header_path() {
        // 跨行 header：["基本信息","姓名"]；colspan 展开对齐叶子列
        let f = facts(json!([
            { "kind": "thead", "rows": [
                row([
                    json!({ "text": "基本信息", "header": true, "rowSpan": 1, "colSpan": 2 }),
                    json!({ "text": "操作", "header": true, "rowSpan": 2, "colSpan": 1 }),
                ]),
                row([th("姓名"), th("年龄")]),
            ]},
            { "kind": "tbody", "rows": [ row([cell("张三"), cell("30"), cell("删除")]) ] },
        ]));
        let out = normalize(&f);
        assert_eq!(
            out["columns"],
            json!([
                { "name": "姓名", "source": "header", "path": ["基本信息", "姓名"] },
                { "name": "年龄", "source": "header", "path": ["基本信息", "年龄"] },
                { "name": "操作", "source": "header", "path": ["操作"] }, // rowspan 覆盖行不重复
            ])
        );
        assert_eq!(out["rows"], json!([["张三", "30", "删除"]]));
    }

    #[test]
    fn rowspan_expansion_null_semantics_and_anchor() {
        // 数据行 rowspan=2：被覆盖位 = null（不是 ""），锚点记 spans
        let f = facts(json!([
            { "kind": "thead", "rows": [ row([th("A"), th("B")]) ] },
            { "kind": "tbody", "rows": [
                row([
                    json!({ "text": "锚", "rowSpan": 2, "colSpan": 1 }),
                    cell("r1-b"),
                ]),
                row([cell("r2-b")]), // 首列被覆盖
            ]},
        ]));
        let out = normalize(&f);
        assert_eq!(out["rows"], json!([["锚", "r1-b"], [null, "r2-b"]]));
        assert_eq!(
            out["meta"]["spans"],
            json!([{ "row": 0, "col": 0, "rowspan": 2, "colspan": 1 }])
        );
    }

    #[test]
    fn colspan_expansion_null_fill() {
        let f = facts(json!([
            { "kind": "tbody", "rows": [
                row([
                    json!({ "text": "宽", "rowSpan": 1, "colSpan": 3 }),
                    cell("尾"),
                ]),
            ]},
        ]));
        let out = normalize(&f);
        assert_eq!(out["rows"], json!([["宽", null, null, "尾"]]));
        assert_eq!(
            out["meta"]["spans"],
            json!([{ "row": 0, "col": 0, "rowspan": 1, "colspan": 3 }])
        );
        assert_eq!(out["meta"]["columnCount"], 4);
    }

    #[test]
    fn aria_rowcount_exceeds_visible_is_incomplete() {
        let mut f = facts(json!([
            { "kind": "flat", "rows": [ row([cell("a")]) ] },
        ]));
        f.aria = Some(AriaDecl { row_count: Some(1000), col_count: None });
        let out = normalize(&f);
        assert_eq!(out["coverage"]["datasetComplete"], "incomplete");
        let reasons = out["coverage"]["reasons"].as_array().unwrap();
        assert!(reasons.iter().any(|r| r.as_str().unwrap().contains("aria-rowcount 1000")));
    }

    #[test]
    fn aria_colcount_exceeds_observed_discloses_not_fabricates() {
        let mut f = facts(json!([ { "kind": "flat", "rows": [ row([cell("a")]) ] } ]));
        f.aria = Some(AriaDecl { row_count: None, col_count: Some(8) });
        let out = normalize(&f);
        // 只产出观测到的 1 列，不编造缺失列
        assert_eq!(out["meta"]["columnCount"], 1);
        assert_eq!(out["coverage"]["datasetComplete"], "incomplete");
        assert!(out["coverage"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().contains("aria-colcount 8")));
    }

    #[test]
    fn aria_rowindex_sorts_out_of_order_dom() {
        // 虚拟化网格：DOM 顺序乱，aria-rowindex 全声明 → 按逻辑行号排
        let mut f = facts(json!([
            { "kind": "flat", "rows": [
                json!({ "ariaRowIndex": 41, "cells": [cell("r41")] }),
                json!({ "ariaRowIndex": 7,  "cells": [cell("r7")] }),
                json!({ "ariaRowIndex": 12, "cells": [cell("r12")] }),
            ]},
        ]));
        f.format = "aria".into();
        let out = normalize(&f);
        assert_eq!(
            out["rows"],
            json!([["r7"], ["r12"], ["r41"]])
        );
    }

    #[test]
    fn partial_aria_rowindex_keeps_dom_order() {
        // 部分行声明 rowindex → 不排序（排序会引入臆测的相对位置）
        let f = facts(json!([
            { "kind": "flat", "rows": [
                json!({ "ariaRowIndex": 41, "cells": [cell("first")] }),
                row([cell("second")]),
            ]},
        ]));
        let out = normalize(&f);
        assert_eq!(out["rows"], json!([["first"], ["second"]]));
    }

    #[test]
    fn aria_colindex_sparse_placement() {
        // 声明 colindex 的稀疏网格：空位 = null
        let mut f = facts(json!([
            { "kind": "flat", "rows": [
                json!({ "cells": [
                    json!({ "text": "甲", "ariaColIndex": 1, "rowSpan": 1, "colSpan": 1 }),
                    json!({ "text": "丙", "ariaColIndex": 3, "rowSpan": 1, "colSpan": 1 }),
                ]}),
            ]},
        ]));
        f.format = "aria".into();
        let out = normalize(&f);
        assert_eq!(out["rows"], json!([["甲", null, "丙"]]));
        assert_eq!(out["meta"]["columnCount"], 3);
    }

    #[test]
    fn budget_truncates_whole_rows() {
        // 宽 3 列 × 5000 行 = 15000 格 > 10000 预算 → 保留 3333 行整行截断
        let rows: Vec<Value> = (0..5000)
            .map(|_| row([cell("a"), cell("b"), cell("c")]))
            .collect();
        let f = facts(json!([{ "kind": "tbody", "rows": rows }]));
        let out = normalize(&f);
        assert_eq!(out["rows"].as_array().unwrap().len(), 3333);
        assert_eq!(out["meta"]["rowCount"], 3333);
        assert_eq!(out["meta"]["cellCount"], 9999);
        assert_eq!(out["coverage"]["truncated"], true);
        assert_eq!(out["coverage"]["datasetComplete"], "incomplete");
        assert!(out["coverage"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().contains("整行截断")));
    }

    #[test]
    fn collector_partial_with_uncollected_rows_marks_truncated() {
        // 收集侧预算停：行事实只到 10 行，但 total_rows=500 → truncated
        let rows: Vec<Value> = (0..10).map(|i| row([cell(&i.to_string())])).collect();
        let mut f = facts(json!([{ "kind": "tbody", "rows": rows }]));
        f.partial = true;
        f.total_rows = 500;
        f.total_cells = 500;
        let out = normalize(&f);
        assert_eq!(out["rows"].as_array().unwrap().len(), 10);
        assert_eq!(out["coverage"]["truncated"], true);
        // 丢弃数按总量口径披露（500 - 0 表头 - 10 保留 = 490），不因收集侧
        // 提前停止而少报
        assert!(
            out["coverage"]["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r.as_str().unwrap().contains("丢弃 490 行")),
            "{}",
            out["coverage"]["reasons"]
        );
    }

    #[test]
    fn empty_tbody_is_incomplete_with_hint_not_fake_empty_table() {
        // 空 tbody（虚拟化常见）：rows 空 + incomplete + 提示，不装作空表
        let f = facts(json!([
            { "kind": "thead", "rows": [ row([th("姓名")]) ] },
            { "kind": "tbody", "rows": [] },
        ]));
        let out = normalize(&f);
        assert_eq!(out["rows"], json!([]));
        assert_eq!(out["coverage"]["datasetComplete"], "incomplete");
        assert!(out["coverage"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().contains("虚拟化表格常见")));
        assert_eq!(out["columns"][0]["name"], "姓名");
    }

    #[test]
    fn hidden_rows_counted_not_inlined() {
        let f = facts(json!([
            { "kind": "tbody", "rows": [ row([cell("a")]) ] },
        ]));
        let mut f = f;
        f.hidden_rows = 3;
        let out = normalize(&f);
        assert_eq!(out["coverage"]["hiddenRowsOmitted"], 3);
        // hidden 行本身不改变完整性判定（collapse 的明细行是常见合法形态）
        assert_eq!(out["coverage"]["datasetComplete"], "unknown");
    }

    #[test]
    fn rowspan_over_declared_colindex_defers_to_next_free() {
        // 声明 colindex 撞上覆盖位：顺延到下一空位（事实优先，不丢单元格）
        let f = facts(json!([
            { "kind": "flat", "rows": [
                row([
                    json!({ "text": "锚", "rowSpan": 2, "colSpan": 1 }),
                ]),
                row([
                    json!({ "text": "撞", "ariaColIndex": 1, "rowSpan": 1, "colSpan": 1 }),
                ]),
            ]},
        ]));
        let out = normalize(&f);
        // 行是稠密数组（对齐 columnCount）：行 0 补位 null
        assert_eq!(out["rows"], json!([["锚", null], [null, "撞"]]));
    }

    #[test]
    fn thead_td_occupies_width_but_names_nothing() {
        // thead 内的 td：占列宽（colspan 生效）但不贡献列名
        let f = facts(json!([
            { "kind": "thead", "rows": [ row([th("姓名"), cell("说明")]) ] },
            { "kind": "tbody", "rows": [ row([cell("张三"), cell("备注")]) ] },
        ]));
        let out = normalize(&f);
        assert_eq!(
            out["columns"],
            json!([
                { "name": "姓名", "source": "header", "path": ["姓名"] },
                { "name": "col_2", "source": "generated" },
            ])
        );
    }

    #[test]
    fn tfoot_rows_stay_data_and_never_promote() {
        let f = facts(json!([
            { "kind": "tfoot", "rows": [ row([th("合计"), th("999")]) ] },
            { "kind": "tbody", "rows": [ row([cell("a"), cell("1")]) ] },
        ]));
        let out = normalize(&f);
        // tfoot 全 th 也不提升表头（页脚合计不是列名）
        assert_eq!(out["meta"]["headerRows"], 0);
        assert_eq!(out["rows"].as_array().unwrap().len(), 2);
        assert_eq!(out["columns"][0]["name"], "col_1");
    }

    #[test]
    fn zero_width_table_is_empty_incomplete() {
        // 完全无单元格（空 table 无行）：rows 空 + incomplete，不 panic
        let f = facts(json!([{ "kind": "tbody", "rows": [] }]));
        let out = normalize(&f);
        assert_eq!(out["rows"], json!([]));
        assert_eq!(out["columns"], json!([]));
        assert_eq!(out["coverage"]["datasetComplete"], "incomplete");
        assert_eq!(out["meta"]["columnCount"], 0);
    }

    #[test]
    fn budget_boundary_keeps_exact_fit() {
        // 10000 格恰好 = 预算内：不截断（边界含）
        let rows: Vec<Value> = (0..2500)
            .map(|_| row([cell("a"), cell("b"), cell("c"), cell("d")]))
            .collect();
        let f = facts(json!([{ "kind": "tbody", "rows": rows }]));
        let out = normalize(&f);
        assert_eq!(out["rows"].as_array().unwrap().len(), 2500);
        assert_eq!(out["coverage"]["truncated"], false);
        assert_eq!(out["coverage"]["datasetComplete"], "unknown");
    }
}
