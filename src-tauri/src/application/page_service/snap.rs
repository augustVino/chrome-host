use super::{cdp_send, require_conn, HOVER_SETTLE_POLL, HOVER_SETTLE_WINDOW, MAX_HOVER_QUERY_HITS};
use crate::application::page_service::ax::{
    ax_field, collect_ax_tree_multi, FrameSubtree, HoverMarks,
};
use crate::application::page_service::events::enable_session_domains;
use crate::application::page_service::state::Watched;
use crate::domain::page_control::{scan_hover_rules, HoverRules, MAX_HOVER_SHEETS};
use crate::error::AppError;
use crate::infrastructure::cdp::page_cdp::SharedPageConn;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
// ============================================================================
// 6. 工具编排（每个 = 一个内部 async fn，公开入口在 PageService 上）
// ============================================================================

pub(crate) async fn snap_tool(watched: &Watched, _tid: &str, sid: &str) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let resp = cdp_send(&conn, "Accessibility.getFullAXTree", json!({}), Some(sid)).await?;
    let nodes = resp
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // W-oopif：已 attach 的 OOPIF 帧逐个拉 AX 树 + 解析 owner 挂接点；
    // 同源 iframe 不是独立 target（无 session），内容已在主树中——天然跳过。
    // 主树 nodes 传入 sync：owner 探测命中后校验宿主文档中该 bni 确为 iframe
    // 元素（防已销毁文档的残留 target 污染——getFrameOwner 对死帧返回的
    // 节点不可信，真机实测撞上 button 的 bni）
    let (arena, main_attaches) = collect_frame_subtrees(watched, &conn, _tid, sid, &nodes).await;
    // 两遍渲染：第一遍只取 refs（backendNodeId 全集，hover 桥接的输入——
    // 仅主树条目参与，帧内 backendNodeId 属另一 DOM agent 无法在主会话桥接），
    // hover 扫描后第二遍带标记渲染文本树（纯内存重入，代价可忽略）
    let (_, refs) = collect_ax_tree_multi(&nodes, &HoverMarks::default(), &main_attaches, &arena);
    let main_refs: Vec<Value> = refs
        .iter()
        .filter(|r| r.get("frameTargetId").is_none())
        .cloned()
        .collect();
    // hover 感知（增强项：任何失败不阻断 snap 主体，状态如实上报）
    let hover = hover_scan(watched, &conn, _tid, sid, &main_refs).await;
    let (text, refs) = collect_ax_tree_multi(&nodes, &hover.marks, &main_attaches, &arena);
    let refs_count = refs.len();
    // 整表替换 + generation+1 + 清 stale（§3.5）：refs 只描述最后一次 snap
    let generation = {
        let mut tabs = watched.refs.lock().unwrap();
        let store = tabs.entry(_tid.to_string()).or_default();
        store.replace(&Value::Array(refs));
        store.generation()
    };
    Ok(
        json!({ "text": text, "refs_count": refs_count, "generation": generation, "hoverScan": hover.status }),
    )
}

/// OOPIF 帧子树采集（W-oopif）：对 tab 已 attach 的每个帧 target 拉
/// getFullAXTree，解析 owner 元素 backendNodeId（DOM.getFrameOwner，父文档
/// session），组装 arena（支持嵌套：帧内 iframe 挂接在其父帧树下）。
/// 单帧失败（导航中/会话死）跳过不阻断——该帧内容缺席于快照，主树完整。
async fn collect_frame_subtrees(
    watched: &Watched,
    conn: &SharedPageConn,
    tab_tid: &str,
    main_sid: &str,
    main_nodes: &[Value],
) -> (Vec<FrameSubtree>, HashMap<i64, usize>) {
    // 帧树主动同步（W-oopif）：frameAttached/attachedToTarget 只覆盖**增量**——
    // 页面早于命令连接存在的帧拿不到事件（Page 域不回放历史帧树）。snap 前
    // getFrameTree 全量同步结构 + attachToTarget 主动认领会话（同源帧非
    // target，attach 报错即天然过滤）；与事件路径合流（幂等）。
    sync_frame_tree(watched, conn, tab_tid, main_sid, main_nodes).await;
    // 帧快照（锁内一次性取，避免跨 await 持锁）
    let snapshot: Vec<(String, String, Option<String>)> = {
        let tables = watched.frames.lock().unwrap();
        let Some(table) = tables.get(tab_tid) else {
            return (Vec::new(), HashMap::new());
        };
        table
            .frames
            .iter()
            .filter_map(|(fid, info)| {
                info.session_id
                    .as_ref()
                    .map(|sid| (fid.clone(), sid.clone(), info.parent_frame_id.clone()))
            })
            .collect()
    };
    if snapshot.is_empty() {
        return (Vec::new(), HashMap::new());
    }
    let root = {
        let tables = watched.frames.lock().unwrap();
        tables.get(tab_tid).and_then(|t| t.root.clone())
    };
    // 逐帧拉树（失败跳过）+ owner 解析
    struct Raw {
        tid: String,
        sid: String,
        parent: Option<String>,
        owner: i64,
        nodes: Vec<Value>,
        label: String,
    }
    let mut raws: Vec<Raw> = Vec::new();
    for (fid, fsid, parent) in snapshot {
        let nodes = fetch_frame_ax(conn, &fsid).await;
        // owner：getFrameOwner 在父文档 session（父帧 OOPIF 用父帧会话，其余主会话）
        let parent_sid = match parent.as_deref() {
            Some(p) if !p.is_empty() && Some(p.to_string()) != root => {
                let tables = watched.frames.lock().unwrap();
                tables
                    .get(tab_tid)
                    .and_then(|t| t.frames.get(p))
                    .and_then(|i| i.session_id.clone())
                    .unwrap_or_else(|| main_sid.to_string())
            }
            _ => main_sid.to_string(),
        };
        let owner = resolve_frame_owner(watched, conn, tab_tid, &fid, &parent, &parent_sid).await;
        let Some(owner_bni) = owner else { continue };
        let label = nodes
            .first()
            .and_then(|n| ax_field(n, "name"))
            .filter(|n| !n.is_empty())
            .map(|n| n.to_string())
            .unwrap_or_else(|| fid.clone());
        raws.push(Raw {
            tid: fid,
            sid: fsid,
            parent,
            owner: owner_bni,
            nodes,
            label,
        });
    }
    // 组 arena：按父子关系挂接（父帧是 OOPIF → 挂到父帧树；否则挂主树）
    let mut arena: Vec<FrameSubtree> = Vec::new();
    let mut main_attaches: HashMap<i64, usize> = HashMap::new();
    // 两轮：先全部入 arena（下标稳定），再连挂接点（引用后入的帧）
    for r in &raws {
        arena.push(FrameSubtree {
            frame_tid: r.tid.clone(),
            frame_sid: r.sid.clone(),
            label: r.label.clone(),
            nodes: r.nodes.clone(),
            attaches: HashMap::new(),
        });
    }
    for (i, r) in raws.iter().enumerate() {
        let host = match r.parent.as_deref() {
            Some(p) if !p.is_empty() && Some(p.to_string()) != root => {
                arena.iter().position(|s| s.frame_tid == p)
            }
            _ => None,
        };
        match host {
            Some(h) => {
                arena[h].attaches.insert(r.owner, i);
            }
            None => {
                main_attaches.insert(r.owner, i);
            }
        };
    }
    (arena, main_attaches)
}

/// 帧树主动同步（W-oopif，幂等，每次 snap 前跑）：事件路径
/// （frameAttached/attachedToTarget）只覆盖**增量**——页面早于命令连接存在
/// 的帧拿不到事件（Page 域不回放，getFrameTree 快照亦不含跨进程子帧，
/// CfT 131 实测）。故走 target 面探测：
/// 1. browser 级 Target.getTargets → type=iframe 列表（= 本实例全部 OOPIF；
///    同源 iframe 不是 target，天然排除）；
/// 2. 逐帧归属探测：DOM.getFrameOwner 发到各候选 session（page 会话优先、
///    已知帧会话次之），成功者即宿主（owner 元素所在文档），响应的
///    backendNodeId 顺手缓存（挂接点与投影共用）；
/// 3. 无会话帧 → orphan 认领或 browser 级 attachToTarget 主动建会话 + 补域
///    enable。任何失败不阻断 snap（该帧缺席，主树完整）。
async fn sync_frame_tree(
    watched: &Watched,
    conn: &SharedPageConn,
    tab_tid: &str,
    main_sid: &str,
    main_nodes: &[Value],
) {
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
        // owner 校验（顶层帧）：宿主文档（主树）AX 中该 bni 的节点必须是
        // iframe/容器类——死帧（旧文档残留 target）的 getFrameOwner 返回值
        // 不可信（真机实测指向无关元素）。嵌套帧的宿主树本轮未拉，跳过校验。
        if host_frame.is_none() {
            let is_iframe_node = main_nodes.iter().any(|n| {
                n.get("backendDOMNodeId").and_then(Value::as_i64) == Some(owner_bni)
                    && ax_field(n, "role")
                        .map(|r| {
                            let r = r.to_lowercase();
                            r == "iframe" || r == "document" || r.contains("frame")
                        })
                        .unwrap_or(false)
            });
            if !is_iframe_node {
                tracing::debug!(
                    "[page] OOPIF {fid} owner bni {owner_bni} 非当前文档 iframe 元素（死帧残留？），拒绝登记"
                );
                continue;
            }
        }
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

/// 帧内 AX 树拉取（带就绪重试）：OOPIF session 刚 attach 时 AX agent 可能
/// 尚未构建（返回仅根节点/空）——短退避重试至出现内容或预算（~1s）耗尽。
/// 这是真机实测的时序窗口：首轮 snap 恰逢 attach 即空，事件无从感知。
async fn fetch_frame_ax(conn: &SharedPageConn, fsid: &str) -> Vec<Value> {
    for attempt in 0..7 {
        if let Ok(resp) = cdp_send(conn, "Accessibility.getFullAXTree", json!({}), Some(fsid)).await
        {
            let nodes = resp
                .get("nodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            // 仅根（或空）= 未就绪；有 2+ 节点即认为内容可见
            if nodes.len() >= 2 {
                return nodes;
            }
            if attempt == 6 {
                return nodes; // 末轮：有多少给多少（诚实缺席好过假等待）
            }
        } else {
            return Vec::new(); // 会话已死：立即放弃
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    Vec::new()
}

/// 帧 owner 元素解析（缓存优先；miss 时在指定父文档 session 发
/// DOM.getFrameOwner 并回写登记表）。None = 解析失败（帧正死/导航中）。
pub(crate) async fn resolve_frame_owner(
    watched: &Watched,
    conn: &SharedPageConn,
    tab_tid: &str,
    frame_tid: &str,
    parent_frame_id: &Option<String>,
    parent_sid: &str,
) -> Option<i64> {
    {
        let tables = watched.frames.lock().unwrap();
        if let Some(bni) = tables
            .get(tab_tid)
            .and_then(|t| t.frames.get(frame_tid))
            .and_then(|i| i.owner_backend_node_id)
        {
            return Some(bni);
        }
    }
    let resp = cdp_send(
        conn,
        "DOM.getFrameOwner",
        json!({ "frameId": frame_tid }),
        Some(parent_sid),
    )
    .await;
    let bni = resp
        .ok()
        .and_then(|r| r.get("backendNodeId").and_then(Value::as_i64));
    if let Some(bni) = bni {
        let mut tables = watched.frames.lock().unwrap();
        if let Some(info) = tables
            .get_mut(tab_tid)
            .and_then(|t| t.frames.get_mut(frame_tid))
        {
            info.owner_backend_node_id = Some(bni);
        }
    }
    let _ = parent_frame_id; // 仅文档语义提示：owner 查询本就发往父文档 session
    bni
}

/// snap hover 感知的扫描结果：ref 行标记 + meta 状态。
struct HoverOutcome {
    /// backendNodeId → "hover" | "hover-reveal"（collect_ax_tree 第二遍入参）
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
async fn hover_scan(
    watched: &Watched,
    conn: &SharedPageConn,
    tid: &str,
    sid: &str,
    refs: &[Value],
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
    // 桥接：ref 的 backendNodeId 全集 → nodeId（一次批量，spike 验证逐位对齐）
    let backends: Vec<i64> = refs
        .iter()
        .filter_map(|e| e.get("backendNodeId").and_then(Value::as_i64))
        .collect();
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
async fn wait_sheets(watched: &Watched, tid: &str) {
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
