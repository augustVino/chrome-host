use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
// ============================================================================
// 5. AX 快照（TS tools/snapshot.mjs 移植；P2-A 未含，本层实现为纯函数）
// ============================================================================

/// SPEC §3.2 INTERACTIVE_ROLES（小写匹配，吸收 Chrome 的 camelCase internalRole）
const INTERACTIVE_ROLES: [&str; 17] = [
    "button",
    "checkbox",
    "combobox",
    "listbox",
    "link",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "radio",
    "scrollbar",
    "searchbox",
    "slider",
    "spinbutton",
    "switch",
    "tab",
    "textbox",
];

/// Chrome internalRole → ARIA 规范名折叠（原生控件也有 ref）
const INTERACTIVE_ROLE_ALIASES: [(&str, &str); 5] = [
    ("textfield", "textbox"),
    ("searchfield", "searchbox"),
    ("radiobutton", "radio"),
    ("popupbutton", "combobox"),
    ("menulistoption", "option"),
];

/// 结构性行列只有 name 是 URL 时才可引用（“整行可点”的表格约定，§3.2）
const URL_BEARING_ROLES: [&str; 5] = ["row", "cell", "gridcell", "listitem", "rowheader"];

pub(crate) fn ax_field<'a>(node: &'a Value, field: &str) -> Option<&'a str> {
    node.get(field)?.get("value").and_then(Value::as_str)
}

fn normalized_role(node: &Value) -> String {
    let role = ax_field(node, "role").unwrap_or("").to_lowercase();
    for (alias, canonical) in INTERACTIVE_ROLE_ALIASES {
        if role == alias {
            return canonical.to_string();
        }
    }
    role
}

/// 该 AX 节点是否获得 @eN（§3.2：交互角色，或 URL-bearing 行列 + http(s) 名）。
fn is_ref_worthy(node: &Value) -> bool {
    let role = normalized_role(node);
    if INTERACTIVE_ROLES.contains(&role.as_str()) {
        return true;
    }
    if URL_BEARING_ROLES.contains(&role.as_str()) {
        return ax_field(node, "name")
            .is_some_and(|n| n.starts_with("http://") || n.starts_with("https://"));
    }
    false
}

/// 是否出现在快照文本里。compact 恒开（daemon.mjs 的 snap 固定
/// collectSnapshot(…, true)）：InlineTextBox 隐藏；none/generic 隐藏；
/// 无名无值的节点仅当可引用（ref 必须可被发现）才保留。
fn should_show(node: &Value) -> bool {
    let role = ax_field(node, "role").unwrap_or("");
    if role == "InlineTextBox" {
        return false;
    }
    if role == "none" || role == "generic" {
        return false;
    }
    let name_empty = ax_field(node, "name").unwrap_or("").is_empty();
    let value = node.get("value").and_then(|v| v.get("value"));
    let value_empty = value.is_none_or(|v| v.is_null() || v.as_str() == Some(""));
    if name_empty && value_empty {
        return is_ref_worthy(node);
    }
    true
}

fn format_ax_line(node: &Value, depth: usize, r#ref: Option<&str>, hover: Option<&str>) -> String {
    let role = ax_field(node, "role").unwrap_or("");
    let name = ax_field(node, "name").unwrap_or("");
    let value = node.get("value").and_then(|v| v.get("value"));
    let mut line = "  ".repeat(depth.min(10));
    if let Some(r) = r#ref {
        line.push_str(&format!("@{r} "));
    }
    line.push_str(&format!("[{role}]"));
    // hover 感知标记（紧随 role，grep "[hover]" 直接命中）：[hover] = hover
    // 它会有效果；[hover-reveal] = hover 后才显示/变化的目标
    if let Some(m) = hover {
        line.push_str(&format!(" [{m}]"));
    }
    if !name.is_empty() {
        line.push_str(&format!(" {name}"));
    }
    if let Some(v) = value {
        if !(v.is_null() || v.as_str() == Some("")) {
            line.push_str(&format!(
                " = {}",
                serde_json::to_string(v).unwrap_or_else(|_| "null".into())
            ));
        }
    }
    line
}

/// AX nodeId 归一化键：Chrome 发字符串，此处对数字形态也收敛成字符串
/// （TS 的 Map 可混型，Rust 统一 by-string）。
fn ax_key(v: &Value) -> String {
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}

fn is_root(node: &Value, by_id: &HashMap<String, &Value>) -> bool {
    match node.get("parentId") {
        None | Some(Value::Null) => true,
        // TS 的 falsy 判定：空串 parentId 也是根
        Some(Value::String(s)) if s.is_empty() => true,
        Some(p) => !by_id.contains_key(&ax_key(p)),
    }
}

struct AxVisit {
    lines: Vec<String>,
    refs: Vec<Value>,
    visited: HashSet<String>,
    next_ref: u32,
}

/// 一棵 AX 树（主文档或某 OOPIF 帧）的挂接描述（W-oopif）：owner 元素的
/// backendNodeId 处展开该帧子树。arena 下标互相引用支持嵌套 iframe。
pub(crate) struct FrameSubtree {
    pub(crate) frame_tid: String,
    pub(crate) frame_sid: String,
    /// [frame] 行的可读标注（帧 url/id）
    pub(crate) label: String,
    pub(crate) nodes: Vec<Value>,
    /// 本树内的挂接点：owner backendNodeId → arena 下标（该帧内的更深帧）
    pub(crate) attaches: HashMap<i64, usize>,
}

/// 前序 DFS：只有 ref-worthy 节点消耗编号（从 1 起）；无 backendDOMNodeId
/// 的节点不发 ref（无法解析到 CDP 节点的 ref 必是死点击）；childIds 优先、
/// parent 反查表补位（TS orderedAxChildren）；游离节点兜底遍历（visited 去重）。
/// W-oopif：`frame` 非空 = 当前遍历的是某帧子树——ref 条目携带帧维度、
/// hover 标记不适用（backendNodeId 跨进程可碰撞，帧树一律无标记）；命中
/// 挂接点时输出 `[frame]` 头行并就地展开帧子树（@eN 连续编号）。
#[allow(clippy::too_many_arguments)]
fn visit_ax(
    node: &Value,
    depth: usize,
    v: &mut AxVisit,
    by_id: &HashMap<String, &Value>,
    by_parent: &HashMap<String, Vec<&Value>>,
    attach: &HashMap<i64, usize>,
    arena: &[FrameSubtree],
    hover: &HoverMarks,
    frame: Option<(&str, &str)>,
) {
    // visited 键按树隔离：主树裸 nodeId；帧树加帧前缀（帧树与主树的 nodeId
    // 是两套独立计数——都从 1 起，裸键共享会把帧节点误判重复直接剪掉，
    // 帧内容间歇性整体缺席；owner 复测的稳定空帧正是全碰撞形态）
    let raw_key = ax_key(node.get("nodeId").unwrap_or(&Value::Null));
    let key = match frame {
        Some((ftid, _)) => format!("{ftid}:{raw_key}"),
        None => raw_key,
    };
    if !v.visited.insert(key) {
        return;
    }
    let backend = node.get("backendDOMNodeId").and_then(Value::as_i64);
    let mut assigned: Option<String> = None;
    if is_ref_worthy(node) && backend.is_some() {
        let r = format!("e{}", v.next_ref);
        v.next_ref += 1;
        let mut entry = json!({
            "ref": r,
            "backendNodeId": node.get("backendDOMNodeId").cloned().unwrap_or(Value::Null),
        });
        if let Some(name) = ax_field(node, "name").filter(|n| !n.is_empty()) {
            entry["name"] = json!(name);
        }
        if let Some((ftid, fsid)) = frame {
            entry["frameTargetId"] = json!(ftid);
            entry["frameSessionId"] = json!(fsid);
        }
        v.refs.push(entry);
        assigned = Some(r);
    }
    if should_show(node) {
        // 帧子树不参与 hover 标记（backendNodeId 跨进程可碰撞，主树标记
        // 误落到帧节点 = 假阳性；帧内 hover 扫描为后续迭代边界）
        let mark = if frame.is_none() {
            backend.and_then(|b| hover.get(&b).copied())
        } else {
            None
        };
        v.lines
            .push(format_ax_line(node, depth, assigned.as_deref(), mark));
    }
    // W-oopif 挂接：owner 元素节点行后展开帧子树（同深度+1；头行带 [frame]）
    if let Some(&idx) = backend.and_then(|b| attach.get(&b)) {
        let sub = &arena[idx];
        v.lines.push(format!(
            "{}[frame] {}",
            "  ".repeat((depth + 1).min(10)),
            sub.label
        ));
        let (fid_by, fid_bp) = forest_indexes(&sub.nodes);
        let roots: Vec<&Value> = sub.nodes.iter().filter(|n| is_root(n, &fid_by)).collect();
        for root in roots {
            visit_ax(
                root,
                depth + 1,
                v,
                &fid_by,
                &fid_bp,
                &sub.attaches,
                arena,
                &HoverMarks::default(),
                Some((sub.frame_tid.as_str(), sub.frame_sid.as_str())),
            );
        }
    }
    let own_key = ax_key(node.get("nodeId").unwrap_or(&Value::Null));
    let mut seen: HashSet<String> = HashSet::new();
    let mut children: Vec<&Value> = Vec::new();
    if let Some(ids) = node.get("childIds").and_then(Value::as_array) {
        for cid in ids {
            let k = ax_key(cid);
            if let Some(child) = by_id.get(&k) {
                let ck = ax_key(child.get("nodeId").unwrap_or(&Value::Null));
                if seen.insert(ck) {
                    children.push(child);
                }
            }
        }
    }
    if let Some(extra) = by_parent.get(&own_key) {
        for child in extra {
            let ck = ax_key(child.get("nodeId").unwrap_or(&Value::Null));
            if seen.insert(ck) {
                children.push(child);
            }
        }
    }
    for child in children {
        visit_ax(
            child,
            depth + 1,
            v,
            by_id,
            by_parent,
            attach,
            arena,
            hover,
            frame,
        );
    }
}

/// 一棵树的 (by_id, by_parent) 索引（visit_ax 与帧子树共用）
fn forest_indexes(nodes: &[Value]) -> (HashMap<String, &Value>, HashMap<String, Vec<&Value>>) {
    let by_id: HashMap<String, &Value> = nodes
        .iter()
        .map(|n| (ax_key(n.get("nodeId").unwrap_or(&Value::Null)), n))
        .collect();
    let mut by_parent: HashMap<String, Vec<&Value>> = HashMap::new();
    for n in nodes {
        if let Some(p) = n.get("parentId") {
            if !p.is_null() {
                by_parent.entry(ax_key(p)).or_default().push(n);
            }
        }
    }
    (by_id, by_parent)
}

/// snap hover 感知：backendNodeId → 标记（"hover" = hover 触发器，
/// "hover-reveal" = hover 后才显示/变化的目标）。
pub(crate) type HoverMarks = HashMap<i64, &'static str>;
/// per-tab 样式表登记：targetId → { styleSheetId → (origin, disabled) }。
pub(crate) type SheetRegistry = HashMap<String, HashMap<String, (String, bool)>>;

/// 主树 + 帧子树的整树收集（W-oopif）。帧条目在挂接点处展开，@eN 连续编号；
/// refs 条目携带 frameTargetId/frameSessionId（主树条目无此二字段）。
pub(crate) fn collect_ax_tree_multi(
    main_nodes: &[Value],
    hover: &HoverMarks,
    main_attaches: &HashMap<i64, usize>,
    arena: &[FrameSubtree],
) -> (String, Vec<Value>) {
    let (by_id, by_parent) = forest_indexes(main_nodes);
    let mut v = AxVisit {
        lines: Vec::new(),
        refs: Vec::new(),
        visited: HashSet::new(),
        next_ref: 1,
    };
    for n in main_nodes {
        if is_root(n, &by_id) {
            visit_ax(
                n,
                0,
                &mut v,
                &by_id,
                &by_parent,
                main_attaches,
                arena,
                hover,
                None,
            );
        }
    }
    for n in main_nodes {
        visit_ax(
            n,
            0,
            &mut v,
            &by_id,
            &by_parent,
            main_attaches,
            arena,
            hover,
            None,
        ); // 兜底：游离节点
    }
    (v.lines.join("\n"), v.refs)
}

/// 单树收集（无帧；旧入口，测试与 hover 首遍共用）
#[allow(dead_code)]
pub(crate) fn collect_ax_tree(nodes: &[Value], hover: &HoverMarks) -> (String, Vec<Value>) {
    collect_ax_tree_multi(nodes, hover, &HashMap::new(), &[])
}
