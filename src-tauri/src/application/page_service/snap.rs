use super::{
    cdp_send, kit_client, require_conn, HoverMarks, SnapParams, HOVER_SETTLE_POLL,
    HOVER_SETTLE_WINDOW, MAX_HOVER_QUERY_HITS,
};
use crate::application::browser_kit::element::RefMap;
use crate::application::browser_kit::snapshot::{take_snapshot, SnapshotOptions};
use crate::application::page_service::events::enable_session_domains;
use crate::application::page_service::frames::iframe_sessions_view;
use crate::application::page_service::refs::map_kit_error;
use crate::application::page_service::state::Watched;
use crate::domain::page_control::{scan_hover_rules, HoverRules, MAX_HOVER_SHEETS};
use crate::error::AppError;
use crate::infrastructure::cdp::page_cdp::SharedPageConn;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::time::Instant;
// ============================================================================
// 6. 工具编排（每个 = 一个内部 async fn，公开入口在 PageService 上）
// ============================================================================

/// I1b：snap 管线 = kit take_snapshot（durable refs + 上游快照引擎）+ 我方
/// hover_scan 与 meta 契约。锁纪律（任务书附录决策 1）：refs 的 Mutex 锁内
/// 禁止 await——RefMap 先 clone 出、take_snapshot 完成后整表写回；resolve
/// 路径只读 clone。begin_snapshot 是 kit 的调用方契约（清 map、保留
/// documents 桶与编号计数：同文档 @eN 稳定复用，消失的 ref 不僵尸残留）。
pub(crate) async fn snap_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    params: &SnapParams,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    // 帧登记同步（W-oopif 保留路径）：事件流只覆盖增量，早于命令连接存在的
    // OOPIF 靠 target 面探测补登记——iframe_sessions 的完备性前提
    sync_frame_tree(watched, &conn, tid, sid).await;
    // 锁窗口内取 clone（不跨 await）
    let mut ref_map: RefMap = watched
        .refs
        .lock()
        .unwrap()
        .entry(tid.to_string())
        .or_insert_with(RefMap::new)
        .clone();
    let iframe_sessions = iframe_sessions_view(watched, tid);
    let pre_ids = ref_map.ref_ids();
    ref_map.begin_snapshot();
    let client = kit_client(watched)?;
    let kit_opts = SnapshotOptions {
        selector: params.selector.clone(),
        interactive: params.interactive,
        compact: params.compact,
        depth: params.depth,
        urls: params.urls,
    };
    // frame_id 恒 None（主文档；任务书附录决策 2）：OOPIF 由 kit 经
    // iframe_sessions 自递归，同源 iframe 由 kit 内部 frameId 参数处理
    let mut text = take_snapshot(
        &client,
        sid,
        &kit_opts,
        &mut ref_map,
        None,
        &iframe_sessions,
    )
    .await
    .map_err(|e| map_kit_error(&e, "snap"))?;
    // hover 感知（增强项：任何失败不阻断 snap 主体，状态如实上报）。主树
    // bni 集合从 RefMap 主桶取（frame=None 条目；帧内行不加标记——语义保持）
    let main_bnis: Vec<i64> = ref_map
        .entries_sorted()
        .iter()
        .filter(|(_, e)| e.frame_id.is_none())
        .filter_map(|(_, e)| e.backend_node_id)
        .collect();
    let hover = hover_scan(watched, &conn, tid, sid, &main_bnis).await;
    // 文本后处理（纯函数）：[ref=eN] → @eN + hover 标记追加行尾
    let marked = marked_ref_ids(&ref_map, &hover.marks);
    text = postprocess_snapshot_text(&text, &marked);
    // 整表写回 + snap 计数 +1（锁窗口内，无 await）
    let refs_count = {
        let mut tabs = watched.refs.lock().unwrap();
        let slot = tabs.entry(tid.to_string()).or_insert_with(RefMap::new);
        *slot = ref_map;
        slot.ref_ids().len()
    };
    let generation = {
        let mut gens = watched.snap_gen.lock().unwrap();
        let g = gens.entry(tid.to_string()).or_insert(0);
        *g += 1;
        *g
    };
    // 消失的 ref（pre - post，@eN 形态；非空才进响应——任务书红线 1）
    let removed: Vec<String> = {
        let post = watched.refs.lock().unwrap();
        let post = post.get(tid).map(|m| m.ref_ids()).unwrap_or_default();
        let mut removed: Vec<String> = pre_ids
            .difference(&post)
            .map(|id| format!("@{id}"))
            .collect();
        removed.sort_by_key(|r| {
            r.trim_start_matches('@')
                .trim_start_matches('e')
                .parse::<u64>()
                .unwrap_or(u64::MAX)
        });
        removed
    };
    let mut resp = json!({
        "text": text,
        "refs_count": refs_count,
        "generation": generation,
        "hoverScan": hover.status,
    });
    if !removed.is_empty() {
        resp["removedRefs"] = json!(removed);
    }
    Ok(resp)
}

/// hover 命中 bni → 该 ref 的标记（仅主桶条目；帧内 bni 属另一 DOM agent，
/// 不参与主会话桥接，误标即假阳性——帧行无标记语义保持）
fn marked_ref_ids(ref_map: &RefMap, marks: &HoverMarks) -> HashMap<String, &'static str> {
    let mut out = HashMap::new();
    for (ref_id, entry) in ref_map.entries_sorted() {
        if entry.frame_id.is_some() {
            continue;
        }
        if let Some(mark) = entry
            .backend_node_id
            .and_then(|bni| marks.get(&bni))
            .copied()
        {
            out.insert(ref_id, mark);
        }
    }
    out
}

/// 快照文本行内替换：`ref=eN` 记号 → `@eN`（含组合属性形态
/// `[checked=false, ref=e2]` → `[checked=false, @e2]`）。记号边界严格判定
///（前导 '['/空格/逗号 + 尾随 ']'/逗号/空格/行尾），防止 name/url 属性里
/// 恰含 `ref=eN` 字面量被误改。hover 标记（" [hover]" / " [hover-reveal]"）
/// 追加行尾。纯函数，单测覆盖。
pub(crate) fn postprocess_snapshot_text(
    text: &str,
    marked: &HashMap<String, &'static str>,
) -> String {
    text.lines()
        .map(|line| {
            let (rewritten, refs) = rewrite_ref_tokens(line);
            match refs.iter().find_map(|r| marked.get(r.as_str()).copied()) {
                Some(mark) => format!("{rewritten} [{mark}]"),
                None => rewritten,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 单行后处理：`[ref=eN]`（唯一属性）→ `@eN`（去括号）；组合属性形态
/// `[checked=false, ref=e2]` → `[checked=false, @e2]`（保留括号）。记号边界
/// 严格判定（前导 '['/空格/逗号 + 尾随 ']'/逗号/空格/行尾），防止
/// name/url 属性里恰含 `ref=eN` 字面量被误改。返回命中的 ref id（“eN”）。
fn rewrite_ref_tokens(line: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(line.len());
    let mut refs = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let ascii_at = |chars: &[char], i: usize| -> u8 {
        // 仅用于 ASCII 边界判定：非 ASCII 位置返回 b'.'（永不匹配边界集）
        let c = chars[i];
        if c.is_ascii() {
            c as u8
        } else {
            b'.'
        }
    };
    let mut i = 0usize;
    'outer: while i < chars.len() {
        if chars[i] == 'r'
            && i + 4 < chars.len()
            && chars[i + 1] == 'e'
            && chars[i + 2] == 'f'
            && chars[i + 3] == '='
            && chars[i + 4] == 'e'
        {
            let before_ok = i == 0 || matches!(ascii_at(&chars, i - 1), b'[' | b' ' | b',');
            let mut j = i + 5;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            let after_ok = j >= chars.len() || matches!(ascii_at(&chars, j), b']' | b',' | b' ');
            if before_ok && j > i + 5 && after_ok {
                let id: String = chars[i + 4..j].iter().collect();
                // 唯一属性：前导 '[' 紧邻 + 尾随 ']' 紧邻 → 连括号一起换成 @eN
                let lone = i > 0 && chars[i - 1] == '[' && j < chars.len() && chars[j] == ']';
                if lone {
                    // 回退掉已写入的 '['
                    out.pop();
                    out.push('@');
                    out.push_str(&id);
                    refs.push(id);
                    i = j + 1;
                    continue 'outer;
                }
                out.push('@');
                out.push_str(&id);
                refs.push(id);
                i = j;
                continue 'outer;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    (out, refs)
}

/// 帧树主动同步（W-oopif，幂等，每次 snap 前跑）：事件路径
/// （frameAttached/attachedToTarget）只覆盖**增量**——页面早于命令连接存在
/// 的帧拿不到事件（Page 域不回放，getFrameTree 快照亦不含跨进程子帧，
/// CfT 131 实测）。故走 target 面探测：
/// 1. browser 级 Target.getTargets → type=iframe 列表（= 本实例全部 OOPIF；
///    同源 iframe 不是 target，天然排除）；
/// 2. 逐帧归属探测：DOM.getFrameOwner 发到各候选 session（page 会话优先、
///    已知帧会话次之），成功者即宿主（owner 元素所在文档），响应的
///    backendNodeId 顺手缓存；
/// 3. 无会话帧 → orphan 认领或 browser 级 attachToTarget 主动建会话 + 补域
///    enable。任何失败不阻断 snap（该帧缺席，主树完整）。
/// I1b：原「宿主主树 AX 校验（owner bni 必须是 iframe 类节点，防死帧残留）」
/// 随 AX 中间树移交 kit 而失去数据源，删除——kit 管线对死帧天然容忍（快照
/// 递归按 AX 树中的 Iframe 节点发起，死帧不在树中即不会被请求；解析链的
/// frame_id 只来自快照条目），脏登记表现为惰性残留（targetDestroyed 摘除）。
async fn sync_frame_tree(watched: &Watched, conn: &SharedPageConn, tab_tid: &str, main_sid: &str) {
    let Ok(resp) = cdp_send(conn, "Target.getTargets", json!({}), None).await else {
        return;
    };
    let iframes: Vec<(String, String)> = resp
        .get("targetInfos")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|t| t.get("type").and_then(Value::as_str) == Some("iframe"))
                .filter_map(|t| {
                    Some((
                        t.get("targetId")?.as_str()?.to_string(),
                        t.get("url")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    if iframes.is_empty() {
        return;
    }
    let orphans: Vec<(String, String)> = {
        let o = watched.orphan_frame_sessions.lock().unwrap();
        o.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    };
    for (fid, _url) in iframes {
        // 已有会话且 owner 已缓存的帧跳过（幂等；owner 缓存 miss 时仍探测补齐）
        let (has_session, owner_cached) = {
            let tables = watched.frames.lock().unwrap();
            let info = tables.get(tab_tid).and_then(|t| t.frames.get(&fid));
            (
                info.and_then(|i| i.session_id.clone()).is_some(),
                info.and_then(|i| i.owner_backend_node_id).is_some(),
            )
        };
        if has_session && owner_cached {
            continue;
        }
        // 归属探测：候选 = **本 tab** 的 page 会话 + 本 tab 已知帧会话（嵌套
        // 宿主）。getFrameOwner 精确到宿主文档（非宿主一律 ERR，真机矩阵
        // 实测）——把别的 tab 的会话放进候选会用错主树校验/写错登记表
        // （每个 tab 的帧由该 tab 自己的 sync 登记）。
        let mut candidates: Vec<(Option<String>, String)> = vec![(None, main_sid.to_string())];
        {
            let tables = watched.frames.lock().unwrap();
            if let Some(table) = tables.get(tab_tid) {
                for (other_fid, info) in table.frames.iter() {
                    if let Some(sid) = &info.session_id {
                        candidates.push((Some(other_fid.clone()), sid.clone()));
                    }
                }
            }
        }
        let mut hit: Option<(Option<String>, i64)> = None;
        for (host_frame, sid) in &candidates {
            if let Ok(r) = cdp_send(
                conn,
                "DOM.getFrameOwner",
                json!({ "frameId": fid }),
                Some(sid),
            )
            .await
            {
                if let Some(bni) = r.get("backendNodeId").and_then(Value::as_i64) {
                    hit = Some((host_frame.clone(), bni));
                    break;
                }
            }
        }
        let Some((host_frame, owner_bni)) = hit else {
            continue; // 宿主不明（探测期页面导航等）：留待事件路径/下轮 sync
        };
        // 会话：已有 > orphan > 主动 attach
        let sid = if has_session {
            None
        } else {
            match orphans
                .iter()
                .find(|(f, _)| f == &fid)
                .map(|(_, s)| s.clone())
            {
                Some(sid) => Some(sid),
                None => match cdp_send(
                    conn,
                    "Target.attachToTarget",
                    json!({ "targetId": fid, "flatten": true }),
                    None,
                )
                .await
                {
                    Ok(r) => r
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    Err(_) => None,
                },
            }
        };
        if let Some(sid) = &sid {
            enable_session_domains(watched, conn, tab_tid, sid, false).await;
        }
        let mut tables = watched.frames.lock().unwrap();
        let table = tables.entry(tab_tid.to_string()).or_default();
        let info = table.frames.entry(fid.clone()).or_default();
        if info.parent_frame_id.is_none() {
            info.parent_frame_id = host_frame.clone();
        }
        info.owner_backend_node_id = Some(owner_bni);
        if let Some(sid) = sid {
            info.session_id = Some(sid);
            tracing::debug!(
                "[page] OOPIF {fid} 同步登记（宿主 {:?}，owner bni {owner_bni}）",
                host_frame.as_deref().unwrap_or(main_sid)
            );
        }
    }
}

/// snap hover 感知的扫描结果：ref 行标记 + meta 状态。
struct HoverOutcome {
    /// backendNodeId → "hover" | "hover-reveal"（后处理入参）
    marks: HoverMarks,
    /// "ok" | "truncated" | "skipped" | "error"——任务书三值外增加 error
    ///（桥接等基础设施故障，诚实区分「没扫出」与「扫不了」），报告披露
    status: &'static str,
}

/// hover 扫描编排（spike 定案路径，全部 CDP 域、零注入）：
/// 样式表登记（CSS.enable@attach → styleSheetAdded 事件，origin=regular 且
/// 未 disabled）→ CSS.getStyleSheetText → domain 提取（预算 MAX_HOVER_*）→
/// DOM.querySelectorAll 逐选择器求值（spike 验证：纯选择器零文本污染、
/// display:none 的 reveal 目标可命中）→ DOM.pushNodesByBackendIdsToFrontend
/// 一次批量桥 backendNodeId → nodeId → 命中映射 ref 行标记。
/// 失败语义（诚实证据）：单表/单选择器失败 → 截断标记继续（部分数据优于
/// 无数据）；桥接失败 → status error（标记无法落到 ref，snap 主体不受影响）。
/// I1b：入参从 AX refs JSON 改为主树 bni 集合（RefMap 主桶）——kit 快照不
/// 再产出 AX 中间树，桥接语义不变。
async fn hover_scan(
    watched: &Watched,
    conn: &SharedPageConn,
    tid: &str,
    sid: &str,
    main_bnis: &[i64],
) -> HoverOutcome {
    // 登记为空时短等回放窗口（attach 后 CSS.enable 的 styleSheetAdded 在途）
    wait_sheets(watched, tid).await;
    let regular: Vec<String> = {
        let sheets = watched.sheets.lock().unwrap();
        let Some(m) = sheets.get(tid) else {
            return HoverOutcome {
                marks: HoverMarks::default(),
                status: "skipped",
            };
        };
        let mut ids: Vec<String> = m
            .iter()
            .filter(|(_, (origin, disabled))| origin == "regular" && !disabled)
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort(); // HashMap 迭代序随机，排序保证扫描/截断可复现
        ids
    };
    let total = regular.len();
    if total == 0 {
        return HoverOutcome {
            marks: HoverMarks::default(),
            status: "skipped",
        };
    }
    let mut truncated = total > MAX_HOVER_SHEETS;
    // 取文本（≤32 张；单表失败跳过 + 截断标记）
    let mut texts: Vec<String> = Vec::new();
    for id in regular.iter().take(MAX_HOVER_SHEETS) {
        match cdp_send(
            conn,
            "CSS.getStyleSheetText",
            json!({ "styleSheetId": id }),
            Some(sid),
        )
        .await
        {
            Ok(r) => {
                if let Some(t) = r.get("text").and_then(Value::as_str) {
                    texts.push(t.to_string());
                }
            }
            Err(_) => truncated = true,
        }
    }
    // domain 纯函数提取（预算：单表 2MiB / 总规则 256，内部如实置 truncated）
    let slices: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
    let rules: HoverRules = scan_hover_rules(&slices, total);
    truncated |= rules.truncated;
    if rules.triggers.is_empty() && rules.reveals.is_empty() {
        // 无 hover 规则（或仅 :not(:hover) 等）→ skipped；有截断则 truncated 优先
        //（可能漏看了未扫到的规则，不装作「确认无」）
        let status = if truncated { "truncated" } else { "skipped" };
        return HoverOutcome {
            marks: HoverMarks::default(),
            status,
        };
    }
    // DOM 求值：根 nodeId（fill 定位链同款 getDocument；退化值 1 = 主文档恒为
    // 首个 getDocument 的 nodeId，spike 实测）
    let root = match cdp_send(conn, "DOM.getDocument", json!({}), Some(sid)).await {
        Ok(r) => r
            .pointer("/root/nodeId")
            .and_then(Value::as_i64)
            .unwrap_or(1),
        Err(_) => 1,
    };
    let mut trigger_hits: HashSet<i64> = HashSet::new();
    let mut reveal_hits: HashSet<i64> = HashSet::new();
    for sel in &rules.triggers {
        hover_query_hits(conn, sid, root, sel, &mut trigger_hits, &mut truncated).await;
    }
    for sel in &rules.reveals {
        hover_query_hits(conn, sid, root, sel, &mut reveal_hits, &mut truncated).await;
    }
    // 桥接：主树 backendNodeId 集合 → nodeId（一次批量，spike 验证逐位对齐）
    let backends: Vec<i64> = main_bnis.to_vec();
    let node_ids: Vec<i64> = match cdp_send(
        conn,
        "DOM.pushNodesByBackendIdsToFrontend",
        json!({ "backendNodeIds": backends }),
        Some(sid),
    )
    .await
    {
        Ok(r) => r
            .get("nodeIds")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default(),
        Err(_) => {
            return HoverOutcome {
                marks: HoverMarks::default(),
                status: "error",
            };
        }
    };
    if node_ids.len() != backends.len() {
        // 形状防御（逐位映射的前提）：宁可放弃标记也不错位错标
        return HoverOutcome {
            marks: HoverMarks::default(),
            status: "error",
        };
    }
    // 映射：hover 优先于 hover-reveal（既是触发器又是目标时，hover 语义对
    // agent 更可操作：hover 它一定有效果）
    let mut marks = HoverMarks::new();
    for (backend, nid) in backends.iter().zip(node_ids) {
        if trigger_hits.contains(&nid) {
            marks.insert(*backend, "hover");
        } else if reveal_hits.contains(&nid) {
            marks.insert(*backend, "hover-reveal");
        }
    }
    let status = if truncated { "truncated" } else { "ok" };
    HoverOutcome { marks, status }
}

/// 单选择器求值：命中 nodeId 集合（上限 MAX_HOVER_QUERY_HITS，超出截断）。
/// 选择器非法（真机报文：CfT 131 为 "DOM Error while querying"，旧变体
/// "... is not a valid selector"——启发式提取产物仍可能的形态，如预处理器的
/// /deep/、>>> 组合子）→ 跳过该选择器（该规则无标记，不装作预算截断：
/// truncated 语义 = 预算内不完整，狼来了效应会让真截断失去信号）；其他
/// 错误（会话/超时/节点失放）→ 截断标记。
async fn hover_query_hits(
    conn: &SharedPageConn,
    sid: &str,
    root: i64,
    sel: &str,
    hits: &mut HashSet<i64>,
    truncated: &mut bool,
) {
    match cdp_send(
        conn,
        "DOM.querySelectorAll",
        json!({ "nodeId": root, "selector": sel }),
        Some(sid),
    )
    .await
    {
        Ok(r) => {
            let ids = r
                .get("nodeIds")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if ids.len() > MAX_HOVER_QUERY_HITS {
                *truncated = true;
            }
            for nid in ids.into_iter().take(MAX_HOVER_QUERY_HITS) {
                if let Some(n) = nid.as_i64() {
                    hits.insert(n);
                }
            }
        }
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            if msg.contains("dom error while querying") || msg.contains("is not a valid selector") {
                tracing::debug!("[page] hover 选择器非法，跳过: {sel}（{e}）");
                return;
            }
            *truncated = true;
        }
    }
}

/// 换文档（根导航 / document.open）后的样式表登记重建：清空登记 + 把
/// css_since 重置为当下（仅对 attach 时 CSS.enable 成功过的 tab——entry
/// 已存在才刷新；从未成功的 tab 重置只会白付结算等待）。新文档的
/// styleSheetAdded 回放从该时刻起重新享受 HOVER_SETTLE_WINDOW 结算窗口，
/// 否则窗口起点仍停在 attach 时刻，导航后首个 snap 会在回放未到时被
/// 立即放行（空登记假报 skipped，或部分登记被当完整扫描）。
pub(crate) fn reset_sheet_registry(watched: &Watched, tid: &str) {
    watched.sheets.lock().unwrap().remove(tid);
    let mut since = watched.css_since.lock().unwrap();
    if since.contains_key(tid) {
        since.insert(tid.to_string(), Instant::now());
    }
}

/// 登记为空时短等 styleSheetAdded 回放：仅当 CSS.enable 刚发出（css_since
/// 在窗口内）时生效——窗口起点是 enable 时刻而非本次 snap（无表页只有首个
/// 窗口期内的 snap 付等待，之后即时返回）。到点放行，空登记按无表处理。
pub(crate) async fn wait_sheets(watched: &Watched, tid: &str) {
    let since = watched.css_since.lock().unwrap().get(tid).copied();
    let Some(t0) = since else { return };
    let deadline = t0 + HOVER_SETTLE_WINDOW;
    loop {
        let empty = watched
            .sheets
            .lock()
            .unwrap()
            .get(tid)
            .is_none_or(|m| m.is_empty());
        if !empty || Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(HOVER_SETTLE_POLL).await;
    }
}

// ---------------- 后处理纯函数单测 ----------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 记号改写：单一属性、组合属性、多记号行；边界外（name/url 值域）不动
    #[test]
    fn rewrite_ref_tokens_rewrites_only_attr_positions() {
        assert_eq!(
            rewrite_ref_tokens("- button \"Buy\" [ref=e1]"),
            ("- button \"Buy\" @e1".to_string(), vec!["e1".to_string()])
        );
        assert_eq!(
            rewrite_ref_tokens("  - radio \"x\" [checked=false, ref=e2, url=https://a]"),
            (
                "  - radio \"x\" [checked=false, @e2, url=https://a]".to_string(),
                vec!["e2".to_string()]
            )
        );
        // name 里恰含 ref=e1（前导是引号）与 url 查询串（前导是 ?）都不动
        let (out, refs) = rewrite_ref_tokens("- link \"ref=e1\" [ref=e2] [url=https://x?ref=e3]");
        assert_eq!(out, "- link \"ref=e1\" @e2 [url=https://x?ref=e3]");
        assert_eq!(refs, vec!["e2".to_string()]);
        // e1 是 e12 的前缀：数字贪婪到边界，互不误伤；唯一属性去括号、
        // 组合属性保留括号
        let (out, refs) = rewrite_ref_tokens("- a [ref=e12] [ref=e1, selected]");
        assert_eq!(out, "- a @e12 [@e1, selected]");
        assert_eq!(refs, vec!["e12".to_string(), "e1".to_string()]);
        // 非 ASCII 前后（CJK 名）不炸、不改写
        assert_eq!(
            rewrite_ref_tokens("- button \"购买\" [ref=e5]"),
            ("- button \"购买\" @e5".to_string(), vec!["e5".to_string()])
        );
    }

    /// hover 标记：命中行尾追加、未命中行原样；帧内行（标记表不含其 ref）不动
    #[test]
    fn postprocess_appends_hover_marks_at_line_end() {
        let mut marked = HashMap::new();
        marked.insert("e1".to_string(), "hover");
        marked.insert("e2".to_string(), "hover-reveal");
        let text = "- button \"Menu\" [ref=e1]\n- link \"Item\" [ref=e2]\n- textbox \"Q\" [ref=e3]";
        assert_eq!(
            postprocess_snapshot_text(text, &marked),
            "- button \"Menu\" @e1 [hover]\n- link \"Item\" @e2 [hover-reveal]\n- textbox \"Q\" @e3"
        );
    }
}
