use super::*;
use crate::domain::instance::Instance;
use crate::domain::page_control::DialogInfo;
use crate::infrastructure::cdp::page_cdp::{PageCdp, PageCdpFuture};
use crate::infrastructure::db::client::init_pool;
use crate::infrastructure::db::schema::init_schema;
use futures_util::FutureExt;
use std::collections::VecDeque;

const INS: &str = "ins_page";
const PORT: u16 = 9333;

// ---------------- fixture：内存库 + Running 实例（抄 diagnostics setup）--

fn setup_running() -> Arc<InstanceRepository> {
    let dir = std::env::temp_dir().join(format!("cem-page-svc-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let pool = init_pool(&dir.join("test.db")).unwrap();
    init_schema(&pool).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    pool.get()
            .unwrap()
            .execute(
                "INSERT INTO environments (id, name, keep_alive, created_at, updated_at) VALUES ('env_x', 'x', 0, ?1, ?1)",
                rusqlite::params![now],
            )
            .unwrap();
    let repo = Arc::new(InstanceRepository::new(pool));
    repo.insert(&Instance {
        id: INS.into(),
        name: None,
        environment_id: "env_x".into(),
        login_profile_id: None,
        profile_dir: "/tmp/x".into(),
        pid: None,
        cdp_port: Some(PORT),
        status: InstanceStatus::Running,
        host_rules: None,
        browser_version: None,
        started_at: None,
        stopped_at: None,
        created_at: now,
        updated_at: now,
    })
    .unwrap();
    repo
}

// ---------------- fake CDP：按方法脚本化响应 + 全量调用记录 -------------

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    params: Value,
    session: Option<String>,
}

impl Recorded {
    fn param(&self, key: &str) -> Value {
        self.params.get(key).cloned().unwrap_or(Value::Null)
    }
}

type Calls = Arc<Mutex<Vec<Recorded>>>;
type Script = Arc<Mutex<HashMap<String, VecDeque<Result<Value, String>>>>>;
type EventTap = Arc<Mutex<Option<EventSink>>>;
/// 方法 → 完成闸门：send 记录调用后先等闸门放行再回脚本响应（模拟服务端
/// 响应延迟，构造「命令 in-flight 未完成」形态——P2-F 并发回归的靶子）。
/// 发送端 drop（测试结束）也视为放行，无死锁残留。
type Gates = Arc<Mutex<HashMap<String, VecDeque<tokio::sync::oneshot::Receiver<()>>>>>;

struct FakeCdp {
    calls: Calls,
    script: Script,
    /// 持有事件通道发送端：保活 watch_loop（通道不因 factory 返回而关闭）。
    /// 只写不读（持有的目的就是保活），dead_code 豁免。
    #[allow(dead_code)]
    events: Option<EventSink>,
    gates: Gates,
}

impl PageCdp for FakeCdp {
    fn send_value<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        session_id: Option<&'a str>,
    ) -> PageCdpFuture<'a> {
        let method = method.to_string();
        let sid = session_id.map(str::to_string);
        async move {
            self.calls.lock().unwrap().push(Recorded {
                method: method.clone(),
                params,
                session: sid,
            });
            let gate = self
                .gates
                .lock()
                .unwrap()
                .get_mut(&method)
                .and_then(VecDeque::pop_front);
            if let Some(rx) = gate {
                let _ = rx.await; // 闸门放行（或发送端 drop）才继续
            }
            let mut script = self.script.lock().unwrap();
            match script.get_mut(&method).and_then(VecDeque::pop_front) {
                Some(Ok(v)) => Ok(v),
                Some(Err(m)) => Err(AppError::business(500, "CDP_COMMAND_FAILED", m)),
                None => Ok(Value::Null),
            }
        }
        .boxed()
    }
}

fn fake_conn(
    calls: &Calls,
    script: &Script,
    events: Option<EventSink>,
    gates: &Gates,
) -> SharedPageConn {
    Arc::new(FakeCdp {
        calls: Arc::clone(calls),
        script: Arc::clone(script),
        events,
        gates: Arc::clone(gates),
    })
}

/// fake connector：记录连接次数、暴露事件通道 tap（测试注入事件）。
fn fake_service() -> (PageService, Calls, Script, EventTap) {
    let (calls, script, tap, connector) = fake_connector();
    let svc = PageService::with_connector(setup_running(), connector);
    (svc, calls, script, tap)
}

fn fake_connector() -> (Calls, Script, EventTap, Connector) {
    let calls: Calls = Arc::default();
    let script: Script = Arc::default();
    let tap: EventTap = Arc::default();
    let gates: Gates = Arc::default();
    let c2 = Arc::clone(&calls);
    let s2 = Arc::clone(&script);
    let t2 = Arc::clone(&tap);
    let g2 = Arc::clone(&gates);
    let connector: Connector = Arc::new(move |_port, events| {
        *t2.lock().unwrap() = Some(events.clone());
        let c = Arc::clone(&c2);
        let s = Arc::clone(&s2);
        let g = Arc::clone(&g2);
        async move { Ok(fake_conn(&c, &s, Some(events), &g)) }.boxed()
    });
    (calls, script, tap, connector)
}

/// 预置武装服务：Watched 直接登记（连接/映射就位），绕过 establish 的
/// spawn 路径——工具编排测试全确定性。tabs：T1→S1、TAB2→S2 两 target
///（空 target → 409；前缀 "TA" 唯一解析 TAB2）。
fn armed() -> (PageService, Arc<Watched>, Calls, Script) {
    let (calls, script, _tap, connector) = fake_connector();
    let svc = PageService::with_connector(setup_running(), connector);
    let watched = Arc::new(Watched::new());
    *watched.conn.lock().unwrap() = Some(fake_conn(&calls, &script, None, &Gates::default()));
    watched.tabs.lock().unwrap().attach("T1", "S1");
    watched.tabs.lock().unwrap().attach("TAB2", "S2");
    svc.watched
        .lock()
        .unwrap()
        .insert(INS.into(), Arc::clone(&watched));
    (svc, watched, calls, script)
}

fn given(script: &Script, method: &str, responses: Vec<Result<Value, String>>) {
    script
        .lock()
        .unwrap()
        .insert(method.to_string(), responses.into_iter().collect());
}

fn calls_of(calls: &Calls, method: &str) -> Vec<Recorded> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.method == method)
        .cloned()
        .collect()
}

/// Watched.tabs 的测试读法：targetId → sessionId（无 = None）
fn tab_session(watched: &Watched, tid: &str) -> Option<String> {
    watched
        .tabs
        .lock()
        .unwrap()
        .rows
        .iter()
        .find(|(t, _)| t.as_str() == tid)
        .map(|(_, s)| s.clone())
}

fn eval_response(payload: Value) -> Result<Value, String> {
    Ok(json!({ "result": { "type": "string", "value": payload.to_string() } }))
}

/// 轮询等待异步条件（watch_loop 跑在 tauri 全局 runtime 上）
async fn eventually(what: &str, f: impl Fn() -> bool) {
    for _ in 0..600 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("条件未满足: {what}");
}

fn attach_event(tid: &str, sid: &str, ty: &str) -> Value {
    json!({
        "method": "Target.attachedToTarget",
        "params": { "sessionId": sid, "targetInfo": { "targetId": tid, "type": ty } }
    })
}

/// Page.loadEventFired（flatten 模式：sessionId 在顶层）
fn load_event(sid: &str) -> Value {
    json!({ "method": "Page.loadEventFired", "sessionId": sid, "params": {} })
}

/// Page.getNavigationHistory 响应：currentIndex + entries（id/url）
fn hist(current: i64, ids: &[i64]) -> Result<Value, String> {
    let entries: Vec<Value> = ids
        .iter()
        .map(|i| json!({ "id": i, "url": format!("https://a/{i}") }))
        .collect();
    Ok(json!({ "currentIndex": current, "entries": entries }))
}

// ---------------- 纯逻辑：tab 路由 / ref 分流 ---------------------------

#[test]
fn resolve_target_routes_exact_and_prefix_and_errors() {
    let mut tabs = TabTable::default();
    tabs.attach("T1ABC", "s1");
    tabs.attach("TAB2", "s2");
    // 精确 / 唯一前缀（大小写不敏感）
    assert_eq!(
        resolve_target(&tabs, "T1ABC").unwrap(),
        ("T1ABC".into(), "s1".into())
    );
    assert_eq!(
        resolve_target(&tabs, "t1a").unwrap(),
        ("T1ABC".into(), "s1".into())
    );
    // 无匹配：404 PAGE_TAB_NOT_FOUND
    let miss = resolve_target(&tabs, "NOPE").unwrap_err();
    assert_eq!((miss.code(), miss.status()), ("PAGE_TAB_NOT_FOUND", 404));
    // 多匹配：409 PAGE_TAB_AMBIGUOUS（文案对齐 TS resolvePrefix）
    let amb = resolve_target(&tabs, "T").unwrap_err();
    assert_eq!((amb.code(), amb.status()), ("PAGE_TAB_AMBIGUOUS", 409));
    assert!(amb.to_string().contains("matches 2 targets"));
    // 空 target 的语义见 resolve_target_empty_input_selects_unique_or_lists_all
    //（P2-F F1：不参与前缀匹配，唯一自动选中 / 多个 409 列全表）
}

/// F1（真机复测修正）：空 target 不猜页面——唯一 page target 自动选中；
/// 多个 → 409 `PAGE_TAB_AMBIGUOUS` 且 message 列出全部 targetId 供选择
///（「最近 attach」近似真机取错页面，已放弃）；表空 → 404。
#[test]
fn resolve_target_empty_input_selects_unique_or_lists_all() {
    let mut tabs = TabTable::default();
    assert_eq!(
        resolve_target(&tabs, "").unwrap_err().code(),
        "PAGE_TAB_NOT_FOUND",
        "表空 → 404"
    );
    tabs.attach("TAB1", "S1");
    assert_eq!(
        resolve_target(&tabs, "").unwrap(),
        ("TAB1".into(), "S1".into()),
        "唯一 page target 自动选中"
    );
    tabs.attach("TAB2", "S2");
    tabs.attach("TAB3", "S3");
    let amb = resolve_target(&tabs, "").unwrap_err();
    assert_eq!((amb.code(), amb.status()), ("PAGE_TAB_AMBIGUOUS", 409));
    let msg = amb.to_string();
    for tid in ["TAB1", "TAB2", "TAB3"] {
        assert!(
            msg.contains(tid),
            "错误信息应列出全部 targetId（缺 {tid}）: {msg}"
        );
    }
    // 唯一者 detach 后回到自动选中
    tabs.detach("TAB2");
    tabs.detach("TAB3");
    assert_eq!(
        resolve_target(&tabs, "").unwrap(),
        ("TAB1".into(), "S1".into())
    );
}

/// TS interact.mjs isRefTarget：@ 宽松、裸 e<digits> 严格、其余是选择器。
/// '@E3' 也走 ref 路径（随后大小写敏感失配 → ref_not_found，而非误读为选择器）
#[test]
fn is_ref_target_matches_ts_routing() {
    for yes in ["@e3", "@x", "@E3", "e3", "e12"] {
        assert!(is_ref_target(yes), "{yes} 应走 ref 路径");
    }
    for no in ["E3", ".btn", "#a", "e", "e3x", ""] {
        assert!(!is_ref_target(no), "{no} 应走 selector 路径");
    }
}

// ---------------- 纯逻辑：AX 快照（snapshot.mjs 移植）-------------------

fn ax_node(id: &str, role: &str, extra: Value) -> Value {
    let mut n = json!({ "nodeId": id, "role": { "value": role } });
    for (k, v) in extra.as_object().unwrap() {
        n[k] = v.clone();
    }
    n
}

/// 编号 = 前序 DFS、只有 ref-worthy 消耗编号；行 = 缩进树 + @eN + 值
#[test]
fn collect_ax_tree_assigns_refs_preorder_and_formats_lines() {
    let nodes = vec![
        ax_node("1", "WebArea", json!({ "name": { "value": "Home" } })),
        ax_node(
            "2",
            "button",
            json!({
                "parentId": "1", "name": { "value": "OK" }, "backendDOMNodeId": 101
            }),
        ),
        ax_node("3", "generic", json!({ "parentId": "1" })),
        ax_node(
            "4",
            "textbox",
            json!({
                "parentId": "1", "name": { "value": "q" },
                "value": { "value": "hi" }, "backendDOMNodeId": 102
            }),
        ),
    ];
    let marks = HoverMarks::default();
    let (text, refs) = collect_ax_tree(&nodes, &marks);
    assert_eq!(
        text, "[WebArea] Home\n  @e1 [button] OK\n  @e2 [textbox] q = \"hi\"",
        "generic 隐藏；interactive 前序编号；值按 JSON.stringify 形态"
    );
    assert_eq!(
        refs,
        vec![
            json!({ "ref": "e1", "backendNodeId": 101, "name": "OK" }),
            json!({ "ref": "e2", "backendNodeId": 102, "name": "q" }),
        ]
    );
}

/// 别名折叠、URL-bearing 行列、隐藏角色、无名无值节点的可引用豁免
#[test]
fn collect_ax_tree_alias_urlbearing_and_hidden_roles() {
    let nodes = vec![
        ax_node("1", "textfield", json!({ "backendDOMNodeId": 1 })), // 别名 → textbox
        ax_node(
            "2",
            "row",
            json!({ "name": { "value": "https://x/" }, "backendDOMNodeId": 2 }),
        ), // URL-bearing
        ax_node(
            "3",
            "row",
            json!({ "name": { "value": "R2" }, "backendDOMNodeId": 3 }),
        ), // 非 URL：无 ref
        ax_node("4", "checkbox", json!({ "backendDOMNodeId": 4 })),  // 无名无值但可引用 → 显示
        ax_node(
            "5",
            "radio",
            json!({ "backendDOMNodeId": 5, "childIds": [] }),
        ), // 无名不可引用？可引用 → 显示
        ax_node("6", "InlineTextBox", json!({ "name": { "value": "xx" } })), // compact 隐藏
        ax_node("7", "heading", json!({ "backendDOMNodeId": 7 })),   // 无名无值且不可引用 → 隐藏
    ];
    let marks = HoverMarks::default();
    let (text, refs) = collect_ax_tree(&nodes, &marks);
    // 别名折叠只影响 ref 判定，显示行保留原始角色名（TS formatAxNode 同）
    assert!(text.contains("@e1 [textfield]"));
    assert!(text.contains("@e2 [row] https://x/"));
    assert!(text.contains("[row] R2"));
    assert!(
        text.contains("@e3 [checkbox]"),
        "无名无值的可引用节点必须可见"
    );
    assert!(text.contains("@e4 [radio]"));
    assert!(
        !text.contains("InlineTextBox"),
        "compact 形态隐藏 InlineTextBox"
    );
    assert!(!text.contains("[heading]"), "无名无值且不可引用 → 隐藏");
    assert_eq!(refs.len(), 4, "row(R2) 与 heading 不消耗编号");
}

/// childIds 优先 + parent 反查补位 + 游离节点兜底 + backendDOMNodeId 缺失。
/// （无名非交互节点如 list 本身不显示行——只有子项可见，TS 同规则）
#[test]
fn collect_ax_tree_children_ordering_orphans_and_missing_backend() {
    let nodes = vec![
        // 根：childIds 声明 3→2；反查表里 4 也挂在 1 下
        ax_node(
            "1",
            "list",
            json!({ "childIds": ["3", "2"], "name": { "value": "items" } }),
        ),
        ax_node(
            "2",
            "listitem",
            json!({ "backendDOMNodeId": 21, "name": { "value": "b" } }),
        ),
        ax_node(
            "3",
            "listitem",
            json!({ "backendDOMNodeId": 31, "name": { "value": "a" } }),
        ),
        ax_node(
            "4",
            "listitem",
            json!({ "backendDOMNodeId": 41, "name": { "value": "c" } }),
        ),
        // parentId 指向不存在的节点 → 游离根（兜底遍历兜住）；无 backend → 无 ref
        ax_node("5", "tab", json!({ "parentId": "999" })),
    ];
    let mut with_parent = nodes.clone();
    with_parent[3]["parentId"] = json!("1"); // 4 挂 1
    let marks = HoverMarks::default();
    let (text, refs) = collect_ax_tree(&with_parent, &marks);
    // childIds 顺序（3 在 2 前）优先于反查表（4 最后）；listitem 非 URL 名无 ref
    let items: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("[listitem]"))
        .map(|l| l.trim_start())
        .collect();
    assert_eq!(items, vec!["[listitem] a", "[listitem] b", "[listitem] c"]);
    assert!(text.contains("[tab]"), "游离根节点兜底可见");
    assert!(
        refs.is_empty(),
        "无 URL 名的 listitem 与无 backend 的 tab 都不发 ref"
    );
    // 防御：nodes 缺失时输出空表而非 panic
    let marks = HoverMarks::default();
    let (empty_text, empty_refs) = collect_ax_tree(&[], &marks);
    assert_eq!((empty_text.as_str(), empty_refs.len()), ("", 0));
}

// ---------------- 纯逻辑：locate / hit 分类 -----------------------------

#[test]
fn classify_locate_and_hit_matrices() {
    // locate：not_found / not_visible / 非有限坐标 / 正常
    let e = classify_locate(&json!({ "error": "not_found" }), "#a").unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ELEMENT_NOT_FOUND", 404));
    let e = classify_locate(
        &json!({ "error": "not_visible", "why": "display:none" }),
        "#a",
    )
    .unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ELEMENT_NOT_VISIBLE", 422));
    assert!(e.to_string().contains("display:none"));
    let e = classify_locate(&json!({ "x": "oops", "y": 3.0 }), "#a").unwrap_err();
    assert_eq!(
        e.code(),
        "INTERNAL_ERROR",
        "垃圾坐标是防御性 500（TS 同为无码错误）"
    );
    let ok = classify_locate(
        &json!({ "x": 1.0, "y": 2.5, "tag": "DIV", "text": "t" }),
        "#a",
    )
    .unwrap();
    assert_eq!(ok, (1.0, 2.5, "DIV".into(), "t".into()));

    // hit：命中 / target-gone / covered / nothing / 垃圾
    assert!(classify_hit(&json!({ "hit": true }), "#a").is_ok());
    let e = classify_hit(&json!({ "reason": "target-gone" }), "#a").unwrap_err();
    assert_eq!(e.code(), "PAGE_ELEMENT_NOT_FOUND");
    let e = classify_hit(&json!({ "reason": "covered", "by": "DIV.overlay" }), "#a").unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ELEMENT_BLOCKED", 409));
    assert!(e.to_string().contains("<DIV.overlay>"));
    let e = classify_hit(&json!({ "reason": "nothing-at-point" }), "#a").unwrap_err();
    assert_eq!(e.code(), "PAGE_ELEMENT_BLOCKED");
    assert!(
        classify_hit(&json!({ "hit": false }), "#a")
            .unwrap_err()
            .code()
            == "INTERNAL_ERROR"
    );
}

// ---------------- ensure：门卫 / 幂等 -----------------------------------

/// 实例缺失 → 404；非 Running → 409 PAGE_SERVICE_UNAVAILABLE
#[tokio::test]
async fn ensure_gates_on_running_instance() {
    let (svc, _c, _s, _t) = fake_service();
    let miss = svc.ensure("nope").await.map(|_| ()).unwrap_err();
    assert_eq!((miss.code(), miss.status()), ("INSTANCE_NOT_FOUND", 404));

    let repo = svc.repo.clone();
    repo.update_runtime(
        INS,
        crate::domain::instance::RuntimeUpdate {
            status: Some(InstanceStatus::Stopped),
            ..Default::default()
        },
    )
    .unwrap();
    let stopped = svc.ensure(INS).await.map(|_| ()).unwrap_err();
    assert_eq!(
        (stopped.code(), stopped.status()),
        ("PAGE_SERVICE_UNAVAILABLE", 409)
    );
}

/// 幂等：两次 ensure 同一 Watched、只建一次连、autoAttach 形态正确
#[tokio::test]
async fn ensure_is_idempotent_and_sends_flatten_autoattach() {
    let (svc, calls, _script, _tap) = fake_service();
    let (w1, g1) = svc.ensure(INS).await.unwrap();
    drop(g1);
    let (w2, g2) = svc.ensure(INS).await.unwrap();
    assert!(Arc::ptr_eq(&w1, &w2), "登记表复用同一 Watched");
    assert!(w2.conn.lock().unwrap().is_some());
    assert_eq!(w2.generation.load(Ordering::Relaxed), 1);
    drop(g2);

    let attaches = calls_of(&calls, "Target.setAutoAttach");
    assert_eq!(attaches.len(), 1, "第二次 ensure 不重建连接");
    assert_eq!(
        attaches[0].params,
        json!({ "autoAttach": true, "flatten": true, "waitForDebuggerOnStart": false })
    );
}

// ---------------- watch_loop 事件接线 ------------------------------------

/// attachedToTarget(page) 建映射 + 双 enable；iframe target 不入 tabs 表
/// 但得同一套 enable（W-oopif：frame session 是跨 frame 命令通道）；
/// worker 等其他 target 完全不参与
#[tokio::test]
async fn attached_registers_page_tab_and_enables_domains() {
    let (svc, calls, _script, tap) = fake_service();
    let (watched, g) = svc.ensure(INS).await.unwrap();
    drop(g);

    let tx = tap.lock().unwrap().clone().unwrap();
    tx.send(attach_event("TAB_A", "SES_A", "page")).unwrap();
    tx.send(attach_event("TAB_B", "SES_B", "iframe")).unwrap();
    tx.send(attach_event("TAB_W", "SES_W", "worker")).unwrap();

    eventually("TAB_A 入表", || tab_session(&watched, "TAB_A").is_some()).await;
    assert_eq!(tab_session(&watched, "TAB_A").as_deref(), Some("SES_A"));
    // iframe 等 target 不参与 page 命令路由（tabs 表只有 page），但
    // frame session 照发 enable（孤儿会话先登记，frameAttached 后认领）
    eventually("iframe enable 已发出", || {
        calls_of(&calls, "Page.enable").len() == 2
    })
    .await;
    let enables = calls_of(&calls, "Page.enable");
    let page_enables: Vec<&str> = enables
        .iter()
        .filter_map(|r| r.session.as_deref())
        .collect();
    assert!(page_enables.contains(&"SES_A"));
    assert!(
        page_enables.contains(&"SES_B"),
        "iframe session 同套 enable"
    );
    assert!(
        tab_session(&watched, "TAB_B").is_none(),
        "iframe 不入 tabs 表"
    );
    // worker 等其余 target 零参与
    assert!(!page_enables.contains(&"SES_W"));
    assert_eq!(calls_of(&calls, "Page.enable").len(), 2);
}

/// L2（冒烟遗留）：首条 attachedToTarget 延迟 100ms 到达（真机形态：事件
/// 异步在途）——ensure 的建连分支在握手窗口内等它入表，紧随其后的 snap
/// 不再 PAGE_TAB_NOT_FOUND；连接已在位后（已有命令在跑）不再重等窗口。
#[tokio::test]
async fn ensure_handshake_window_absorbs_late_first_attach() {
    let calls: Calls = Arc::default();
    let script: Script = Arc::default();
    // 无对话框场景：attach 后的 L3 探测被 Chrome 拒（-32602 形态，静默）
    given(
        &script,
        "Page.handleJavaScriptDialog",
        vec![Err("No dialog is showing".into())],
    );
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [] })), Ok(json!({ "nodes": [] }))],
    );
    // 自建 connector：连接建立后 100ms 才送达首条 attach（事件异步延迟）
    let c2 = Arc::clone(&calls);
    let s2 = Arc::clone(&script);
    let connector: Connector = Arc::new(move |_port, events| {
        let tx = events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = tx.send(attach_event("TAB_LATE", "SES_LATE", "page"));
        });
        let c = Arc::clone(&c2);
        let s = Arc::clone(&s2);
        async move { Ok(fake_conn(&c, &s, Some(events), &Gates::default())) }.boxed()
    });
    let svc = PageService::with_connector(setup_running(), connector);

    let t0 = Instant::now();
    let resp = svc
        .snap(INS, "")
        .await
        .expect("握手窗口等到迟到 attach，不再 NOT_FOUND");
    assert_eq!(
        resp["targetId"],
        json!("TAB_LATE"),
        "空 target 唯一自动选中"
    );
    assert!(
        t0.elapsed() >= Duration::from_millis(100),
        "确实等到了迟到的事件（{:?}）",
        t0.elapsed()
    );
    assert!(resp.get("dialogs_seen").is_none(), "探测失败不入缓冲");
    eventually("attach 探测已发出（L3 联动）", || {
        calls_of(&calls, "Page.handleJavaScriptDialog").len() == 1
    })
    .await;

    // 连接已在位（tabs 非空）→ 后续命令不重等窗口（ensure 只在建连分支等）
    let t1 = Instant::now();
    let resp2 = svc.snap(INS, "").await.unwrap();
    assert_eq!(resp2["targetId"], json!("TAB_LATE"));
    assert!(
        t1.elapsed() < ATTACH_HANDSHAKE_WINDOW,
        "连接在位不阻塞（{:?}）",
        t1.elapsed()
    );
}

/// L2 兑底：窗口内无任何 attach 到达 → 300ms 到点放行（不报错不悬挂），
/// snap 报 PAGE_TAB_NOT_FOUND，保留 retry shortly 语义。
#[tokio::test]
async fn ensure_handshake_window_times_out_into_retry_semantics() {
    let (svc, _calls, _script, _tap) = fake_service();
    let t0 = Instant::now();
    let err = svc.snap(INS, "").await.unwrap_err();
    assert_eq!((err.code(), err.status()), ("PAGE_TAB_NOT_FOUND", 404));
    assert!(
        err.to_string().contains("retry shortly"),
        "保留既有兑底文案: {err}"
    );
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= ATTACH_HANDSHAKE_WINDOW,
        "窗口确实等满（{elapsed:?}）"
    );
    assert!(elapsed < Duration::from_secs(5), "到点即返回，不悬挂");
}

/// L3（冒烟遗留）：session attach 时盲发一次 handleJavaScriptDialog——
/// 成功 = 重连前挂着的历史对话框已被接管，无事件参数可得，用保守默认
/// 构造 DialogInfo 入缓冲（Page.enable 之后、握手内顺序完成）。
#[tokio::test]
async fn attach_probe_adopts_stale_dialog_with_conservative_defaults() {
    let (_svc, watched, calls, script) = armed();
    given(&script, "Page.handleJavaScriptDialog", vec![Ok(json!({}))]);

    handle_event(&watched, &attach_event("T1", "S1", "page")).await;

    let handles = calls_of(&calls, "Page.handleJavaScriptDialog");
    assert_eq!(handles.len(), 1, "attach 后恰好盲发一次");
    assert_eq!(handles[0].params, json!({ "accept": true }));
    assert_eq!(handles[0].session.as_deref(), Some("S1"));
    // 探测必须**先于** enable（对话框悬挂时 handleJavaScriptDialog 是唯一
    // 不被 renderer 阻塞的命令；先探测 accept 再 enable 才能全部成功
    // ——孤儿对话框真机验证 2026-10-25）
    let order = |m: &str| {
        calls
            .lock()
            .unwrap()
            .iter()
            .position(|r| r.method == m)
            .unwrap()
    };
    assert!(order("Page.handleJavaScriptDialog") < order("Page.enable"));
    // 保守默认入缓冲：alert / 固定文案 / accepted / sequence 顺延起 1
    let dialogs = watched.dialogs.lock().unwrap();
    let b = dialogs.get("T1").expect("成功探测入缓冲");
    let e = b.since(0)[0];
    assert_eq!(e.r#type, "alert");
    assert_eq!(e.message, "reconnected dialog (auto-accepted)");
    assert_eq!(e.handled, "accepted");
    assert_eq!(e.sequence, 1);
}

/// L3：无对话框（Chrome 回 -32602 No dialog is showing 形态）→ 静默忽略：
/// 不建缓冲、不影响映射建立；其他错误同样吞掉（best-effort 探测）。
#[tokio::test]
async fn attach_probe_silent_when_no_dialog_showing() {
    let (_svc, watched, _calls, script) = armed();
    given(
        &script,
        "Page.handleJavaScriptDialog",
        vec![Err("No dialog is showing".into())],
    );

    handle_event(&watched, &attach_event("T9", "S9", "page")).await;

    assert!(
        watched.dialogs.lock().unwrap().get("T9").is_none(),
        "无对话框 → 不建缓冲"
    );
    assert_eq!(
        tab_session(&watched, "T9").as_deref(),
        Some("S9"),
        "映射与 enable 照常建立，探测不影响 attach 路径"
    );
}

/// L3 与常规对话框路径互不干扰：探测接管历史对话框（seq 1，保守默认）
/// 后，新对话框仍走事件驱动应答（seq 2，真实参数），sequence 顺延不断档。
#[tokio::test]
async fn attach_probe_coexists_with_event_driven_dialog_flow() {
    let (_svc, watched, calls, script) = armed();
    given(
        &script,
        "Page.handleJavaScriptDialog",
        vec![Ok(json!({})), Ok(json!({}))],
    );
    // 重连握手：探测接管重连前挂着的历史对话框
    handle_event(&watched, &attach_event("T1", "S1", "page")).await;
    // 常规路径：新对话框事件到达，带真实参数应答（F2 后事件驱动照旧）
    handle_event(
        &watched,
        &json!({
            "method": "Page.javascriptDialogOpening", "sessionId": "S1",
            "params": { "type": "confirm", "message": "leave?" }
        }),
    )
    .await;

    let dialogs = watched.dialogs.lock().unwrap();
    let b = dialogs.get("T1").unwrap();
    let entries = b.since(0);
    assert_eq!(entries.len(), 2, "探测 + 事件各入一条");
    assert_eq!(
        (
            entries[0].sequence,
            entries[0].r#type.as_str(),
            entries[0].message.as_str()
        ),
        (1, "alert", "reconnected dialog (auto-accepted)")
    );
    assert_eq!(
        (
            entries[1].sequence,
            entries[1].r#type.as_str(),
            entries[1].message.as_str()
        ),
        (2, "confirm", "leave?")
    );
    drop(dialogs);
    // 两次 handle：探测 1 次 + 事件应答 1 次，互不吞并
    assert_eq!(calls_of(&calls, "Page.handleJavaScriptDialog").len(), 2);
}

/// detachedFromTarget / targetDestroyed：映射 + RefStore + DialogBuffer 同清
#[tokio::test]
async fn detached_and_destroyed_drop_tab_state() {
    let (_svc, watched, _calls, _script) = armed();
    watched
        .refs
        .lock()
        .unwrap()
        .insert("T1".into(), RefStore::new());
    watched
        .dialogs
        .lock()
        .unwrap()
        .insert("T1".into(), DialogBuffer::new("T1"));

    handle_event(
            &watched,
            &json!({ "method": "Target.detachedFromTarget", "params": { "sessionId": "S1", "targetId": "T1" } }),
        )
        .await;
    assert!(tab_session(&watched, "T1").is_none());
    assert!(watched.refs.lock().unwrap().get("T1").is_none());
    assert!(watched.dialogs.lock().unwrap().get("T1").is_none());

    // 仅带 sessionId 的 detach：按映射反查清理
    watched.tabs.lock().unwrap().attach("T2", "S9");
    watched
        .refs
        .lock()
        .unwrap()
        .insert("T2".into(), RefStore::new());
    handle_event(
        &watched,
        &json!({ "method": "Target.detachedFromTarget", "params": { "sessionId": "S9" } }),
    )
    .await;
    assert!(tab_session(&watched, "T2").is_none());

    // targetDestroyed 同路径
    watched.tabs.lock().unwrap().attach("T3", "S3");
    watched
        .refs
        .lock()
        .unwrap()
        .insert("T3".into(), RefStore::new());
    handle_event(
        &watched,
        &json!({ "method": "Target.targetDestroyed", "params": { "targetId": "T3" } }),
    )
    .await;
    assert!(tab_session(&watched, "T3").is_none());
    assert!(watched.refs.lock().unwrap().get("T3").is_none());
}

/// 对话框：prompt 带 promptText 应答、成功入缓冲；失败只日志不入档
#[tokio::test]
async fn dialog_opening_accepts_and_records_only_on_success() {
    let (_svc, watched, calls, script) = armed();
    given(
        &script,
        "Page.handleJavaScriptDialog",
        vec![Ok(json!({})), Err("tab busy".into())],
    );

    handle_event(
        &watched,
        &json!({
            "method": "Page.javascriptDialogOpening", "sessionId": "S1",
            "params": { "type": "prompt", "message": "name?", "defaultPrompt": "Bob" }
        }),
    )
    .await;
    let handles = calls_of(&calls, "Page.handleJavaScriptDialog");
    assert_eq!(
        handles[0].params,
        json!({ "accept": true, "promptText": "Bob" }),
        "prompt 应答带 promptText = defaultPrompt ?? \"\"（§1.4）"
    );
    assert_eq!(handles[0].session.as_deref(), Some("S1"));
    let len = {
        let buf = watched.dialogs.lock().unwrap();
        buf.get("T1").expect("成功后入缓冲").len()
    };
    assert_eq!(len, 1);

    // 失败路径：不 record（游标不动，对话框保持打开——§1.5 协议）
    let before = watched.dialogs.lock().unwrap().get("T1").unwrap().cursor();
    handle_event(
        &watched,
        &json!({
            "method": "Page.javascriptDialogOpening", "sessionId": "S1",
            "params": { "type": "alert", "message": "again" }
        }),
    )
    .await;
    let after = {
        let dialogs = watched.dialogs.lock().unwrap();
        let b = dialogs.get("T1").unwrap();
        (b.cursor(), b.len())
    };
    assert_eq!(after, (before, 1), "失败不移动游标不入档");
}

/// F2（真机形态，根治回归）：工具命令 in-flight（响应被闸门挂住——真机
/// = click 卡在被 alert 阻塞的页面，命令响应迟迟不回）期间，对话框
/// 事件到达，handle 必须在同一条连接上立即发出并完成，不等该命令。
/// 连接被锁包裹（或 send 串行化）的旧实现会在等锁/等命令中错过对话框
/// 黄金窗口，迟到的应答被 Chrome -32602 拒绝、缓冲零记录。
#[tokio::test]
async fn dialog_handle_completes_while_command_in_flight() {
    let calls: Calls = Arc::default();
    let script: Script = Arc::default();
    let gates: Gates = Arc::default();
    given(&script, "Page.handleJavaScriptDialog", vec![Ok(json!({}))]);
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [] }))],
    );
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    gates
        .lock()
        .unwrap()
        .insert("Accessibility.getFullAXTree".into(), VecDeque::from([held]));

    let watched = Arc::new(Watched::new());
    *watched.conn.lock().unwrap() = Some(fake_conn(&calls, &script, None, &gates));
    watched.tabs.lock().unwrap().attach("T1", "S1");

    // in-flight 命令：后台发出并停在闸门（响应未回）
    let conn = watched.conn.lock().unwrap().clone().unwrap();
    let inflight_conn = Arc::clone(&conn);
    let inflight = tokio::spawn(async move {
        cdp_send(
            &inflight_conn,
            "Accessibility.getFullAXTree",
            json!({}),
            Some("S1"),
        )
        .await
    });
    eventually("in-flight 命令已发出并停在闸门", || {
        calls_of(&calls, "Accessibility.getFullAXTree").len() == 1
    })
    .await;

    // 对话框事件到达：handle 不等 in-flight 命令，立即完成并入缓冲
    let handled = tokio::time::timeout(
        Duration::from_secs(1),
        handle_event(
            &watched,
            &json!({
                "method": "Page.javascriptDialogOpening", "sessionId": "S1",
                "params": { "type": "alert", "message": "hi" }
            }),
        ),
    )
    .await;
    assert!(
        handled.is_ok(),
        "handle 不得等 in-flight 命令完成（send &self 并发）"
    );
    assert_eq!(
        watched.dialogs.lock().unwrap().get("T1").map(|b| b.len()),
        Some(1),
        "handle 成功 → 入缓冲（§1.5）"
    );
    assert_eq!(calls_of(&calls, "Page.handleJavaScriptDialog").len(), 1);

    // 放行闸门：in-flight 命令正常完成（未被 handle 影响）
    release.send(()).unwrap();
    inflight.await.unwrap().unwrap();
}

/// F1 端到端：空 target 唯一 page target 时自动选中；多 target → 409 且
/// message 列出全部 targetId（不再猜「最近 attach」）。
#[tokio::test]
async fn empty_tab_selects_unique_target_or_lists_all() {
    let (svc, watched, _calls, script) = armed();
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [] }))],
    );
    // armed 有 T1/TAB2 两 target → 409 列全表
    let amb = svc.snap(INS, "").await.unwrap_err();
    assert_eq!((amb.code(), amb.status()), ("PAGE_TAB_AMBIGUOUS", 409));
    let msg = amb.to_string();
    assert!(
        msg.contains("T1") && msg.contains("TAB2"),
        "附 tab 列表: {msg}"
    );

    // 剩唯一 target → 自动选中
    watched.tabs.lock().unwrap().detach("TAB2");
    let resp = svc.snap(INS, "").await.unwrap();
    assert_eq!(resp["targetId"], json!("T1"));
}

/// 根 frame 导航失效 ref；子 frame 导航不失效；文档事件按各自 reason
#[tokio::test]
async fn navigation_invalidates_refs_root_frame_only() {
    let (_svc, watched, _calls, _script) = armed();
    watched
        .refs
        .lock()
        .unwrap()
        .entry("T1".into())
        .or_default()
        .replace(&json!([{ "ref": "e1", "backendNodeId": 1 }]));

    // 子 frame 导航：parentId 非空 → 跳过
    handle_event(
        &watched,
        &json!({
            "method": "Page.frameNavigated", "sessionId": "S1",
            "params": { "frame": { "id": "f1", "parentId": "root" } }
        }),
    )
    .await;
    assert!(!watched.refs.lock().unwrap().get("T1").unwrap().stale());

    // 根 frame 导航 → invalidate("navigation")
    handle_event(
        &watched,
        &json!({
            "method": "Page.frameNavigated", "sessionId": "S1",
            "params": { "frame": { "id": "root" } }
        }),
    )
    .await;
    {
        let binding = watched.refs.lock().unwrap();
        let store = binding.get("T1").unwrap();
        assert!(store.stale());
        assert_eq!(store.last_invalid_reason(), Some("navigation"));
    }

    // documentOpened / documentUpdated 同理（replace 后再失效才有感）
    watched
        .refs
        .lock()
        .unwrap()
        .get_mut("T1")
        .unwrap()
        .replace(&json!([{ "ref": "e1", "backendNodeId": 2 }]));
    handle_event(
        &watched,
        &json!({ "method": "Page.documentOpened", "sessionId": "S1" }),
    )
    .await;
    assert_eq!(
        watched
            .refs
            .lock()
            .unwrap()
            .get("T1")
            .unwrap()
            .last_invalid_reason(),
        Some("document opened")
    );
    watched
        .refs
        .lock()
        .unwrap()
        .get_mut("T1")
        .unwrap()
        .replace(&json!([{ "ref": "e1", "backendNodeId": 3 }]));
    handle_event(
        &watched,
        &json!({ "method": "DOM.documentUpdated", "sessionId": "S1" }),
    )
    .await;
    assert_eq!(
        watched
            .refs
            .lock()
            .unwrap()
            .get("T1")
            .unwrap()
            .last_invalid_reason(),
        Some("document updated")
    );
}

/// 通道关闭 = 连接死亡 → 清连接槽 + 移除登记（代际与指针双守卫的主路径）
#[tokio::test]
async fn watch_loop_teardown_on_channel_close() {
    let (calls, script, _tap, _connector) = fake_connector();
    let registry: Arc<Mutex<HashMap<String, Arc<Watched>>>> = Arc::default();
    let watched = Arc::new(Watched::new());
    let conn = fake_conn(&calls, &script, None, &Gates::default());
    *watched.conn.lock().unwrap() = Some(Arc::clone(&conn));
    registry
        .lock()
        .unwrap()
        .insert(INS.into(), Arc::clone(&watched));
    // 对齐 establish 的代际：fetch_add 后 spawn（gen = 新值）
    let gen = watched.generation.fetch_add(1, Ordering::Relaxed) + 1;

    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(watch_loop(
        INS.into(),
        Arc::clone(&registry),
        Arc::clone(&watched),
        Arc::clone(&conn),
        rx,
        gen,
    ));
    drop(tx); // reader 结束 = 通道关闭
    task.await.unwrap();

    assert!(watched.conn.lock().unwrap().is_none(), "连接槽已清");
    assert!(registry.lock().unwrap().get(INS).is_none(), "登记表已移除");
}

/// 僵尸代守卫：新一代接管后，旧代 loop 退出时不动登记表/不清新连接
#[tokio::test]
async fn watch_loop_zombie_leaves_new_generation_alone() {
    let (calls, script, _tap, _connector) = fake_connector();
    let registry: Arc<Mutex<HashMap<String, Arc<Watched>>>> = Arc::default();
    let watched = Arc::new(Watched::new());
    let old_conn = fake_conn(&calls, &script, None, &Gates::default());
    *watched.conn.lock().unwrap() = Some(Arc::clone(&old_conn));
    registry
        .lock()
        .unwrap()
        .insert(INS.into(), Arc::clone(&watched));
    // 旧代 gen=1（对齐 establish）；新代接管 = fetch_add → generation=2
    watched.generation.fetch_add(1, Ordering::Relaxed);

    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(watch_loop(
        INS.into(),
        Arc::clone(&registry),
        Arc::clone(&watched),
        Arc::clone(&old_conn),
        rx,
        1,
    ));
    // 模拟断连后命令重建：换新连接 + 代际 +1（establish 的效果）
    let new_conn = fake_conn(&calls, &script, None, &Gates::default());
    *watched.conn.lock().unwrap() = Some(Arc::clone(&new_conn));
    watched.generation.fetch_add(1, Ordering::Relaxed);
    drop(tx);
    task.await.unwrap();

    assert!(
        Arc::ptr_eq(&watched.conn.lock().unwrap().clone().unwrap(), &new_conn),
        "旧代不得清掉新代的连接槽"
    );
    assert!(
        registry
            .lock()
            .unwrap()
            .get(INS)
            .is_some_and(|w| Arc::ptr_eq(w, &watched)),
        "旧代不得移除新代的登记"
    );
}

// ---------------- 工具：snap ---------------------------------------------

#[tokio::test]
async fn snap_builds_text_swaps_refs_and_bumps_generation() {
    let (svc, watched, _calls, script) = armed();
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![
            Ok(json!({ "nodes": [
                { "nodeId": "1", "role": { "value": "button" }, "name": { "value": "OK" }, "backendDOMNodeId": 101 },
                { "nodeId": "2", "role": { "value": "link" }, "name": { "value": "Home" }, "backendDOMNodeId": 102 },
            ]})),
            Ok(json!({ "nodes": [
                { "nodeId": "1", "role": { "value": "button" }, "name": { "value": "New" }, "backendDOMNodeId": 201 },
            ]})),
        ],
    );

    let r1 = svc.snap(INS, "T1").await.unwrap();
    assert_eq!(r1["instanceId"], json!(INS));
    assert_eq!(r1["targetId"], json!("T1"));
    assert_eq!(r1["refs_count"], json!(2));
    assert_eq!(r1["generation"], json!(1));
    assert!(r1["text"].as_str().unwrap().contains("@e1 [button] OK"));

    // 整表替换：第二次 snap 后 e2 死、e1 指向新节点、generation+1（§3.5）
    let r2 = svc.snap(INS, "T1").await.unwrap();
    assert_eq!(r2["refs_count"], json!(1));
    assert_eq!(r2["generation"], json!(2));
    let binding = watched.refs.lock().unwrap();
    let store = binding.get("T1").unwrap();
    assert_eq!(store.resolve("e2"), None, "旧 ref 必须死");
    assert_eq!(store.resolve("e1").map(|e| e.backend_node_id), Some(201));
}

// ---------------- 工具：snap hover 感知 -----------------------------------

/// styleSheetAdded 事件登记（含 origin/disabled 元数据，读取侧才过滤）
fn sheet_added_event(sid: &str, sheet_id: &str, origin: &str, disabled: bool) -> Value {
    sheet_added_event_with_url(sid, sheet_id, origin, disabled, "")
}

/// 带 sourceURL 的登记事件（外部表/扩展表的真机形态：inline 表空串、
/// 扩展表 chrome-extension://…）
fn sheet_added_event_with_url(
    sid: &str,
    sheet_id: &str,
    origin: &str,
    disabled: bool,
    source_url: &str,
) -> Value {
    json!({
        "method": "CSS.styleSheetAdded", "sessionId": sid,
        "params": { "header": { "styleSheetId": sheet_id, "origin": origin, "disabled": disabled, "sourceURL": source_url } }
    })
}

/// 扩展注入表（sourceURL = chrome-extension://…，origin 可能仍报 regular）
/// 登记即排除：真机复测它们取文本不稳定（Err → 恒触发 truncated 的狼来
/// 了效应）且对页面 hover 语义无贡献——无 hover 规则页必须回到 skipped。
#[tokio::test]
async fn extension_style_sheets_are_excluded_from_registry() {
    let (_svc, watched, _calls, _script) = armed();
    // 扩展表（带 chrome-extension:// sourceURL）不登记
    handle_event(
        &watched,
        &sheet_added_event_with_url(
            "S1",
            "ext-1",
            "regular",
            false,
            "chrome-extension://abcdef/navbar.css",
        ),
    )
    .await;
    // 普通外部表（http sourceURL）与 inline 表（空 sourceURL）照常登记
    handle_event(
        &watched,
        &sheet_added_event_with_url("S1", "site-1", "regular", false, "https://a/style.css"),
    )
    .await;
    handle_event(
        &watched,
        &sheet_added_event("S1", "inline-1", "regular", false),
    )
    .await;
    {
        let sheets = watched.sheets.lock().unwrap();
        let t1 = sheets.get("T1").unwrap();
        assert!(!t1.contains_key("ext-1"), "扩展表不得入登记表");
        assert!(t1.contains_key("site-1") && t1.contains_key("inline-1"));
        assert_eq!(t1.len(), 2);
    }
    // 排除发生在登记侧：hover_scan 的 total 不再被扩展表灌水（>32 预算
    // 误报的根因）——30 张扩展表 + 0 张页面表 → 空登记 → skipped
    let (svc2, watched2, _c2, script2) = armed();
    for i in 0..40 {
        handle_event(
            &watched2,
            &sheet_added_event_with_url(
                "S1",
                &format!("ext-{i}"),
                "regular",
                false,
                "chrome-extension://abcdef/injected.css",
            ),
        )
        .await;
    }
    given(
        &script2,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [] }))],
    );
    let resp = svc2.snap(INS, "T1").await.unwrap();
    assert_eq!(
        resp["hoverScan"],
        json!("skipped"),
        "全部为扩展表 → 登记空 → skipped（而非 truncated）"
    );
    assert_eq!(calls_of(&_c2, "CSS.getStyleSheetText").len(), 0);
}

/// 端到端：登记表 → 取文本 → domain 提取 → querySelectorAll 逐选择器 →
/// pushNodes 桥 → 第二遍渲染标 [hover] / [hover-reveal]，hoverScan=ok。
/// spike 定案路径全链（display:none 的 reveal 目标在真实浏览器可命中，
/// FakeCdp 只验证链路形状）。
#[tokio::test]
async fn snap_marks_hover_and_reveal_from_css() {
    let (svc, watched, calls, script) = armed();
    // 登记两张表：regular（可读）+ user-agent（读取侧过滤）
    handle_event(&watched, &sheet_added_event("S1", "s2", "regular", false)).await;
    handle_event(
        &watched,
        &sheet_added_event("S1", "s1", "user-agent", false),
    )
    .await;
    given(
        &script,
        "CSS.getStyleSheetText",
        vec![Ok(
            json!({ "text": ".menu-item:hover { background: yellow; }\n.menu-item:hover a.drop-link { color: blue; }" }),
        )],
    );
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [
            { "nodeId": "1", "role": { "value": "WebArea" }, "name": { "value": "Menu Page" } },
            { "nodeId": "2", "role": { "value": "button" }, "name": { "value": "Menu" }, "parentId": "1", "backendDOMNodeId": 101 },
            { "nodeId": "3", "role": { "value": "link" }, "name": { "value": "Item" }, "parentId": "1", "backendDOMNodeId": 102 },
        ]}))],
    );
    given(
        &script,
        "DOM.getDocument",
        vec![Ok(json!({ "root": { "nodeId": 1 } }))],
    );
    // 查询顺序：triggers（去重后 [".menu-item"]）→ reveals
    given(
        &script,
        "DOM.querySelectorAll",
        vec![Ok(json!({ "nodeIds": [8] })), Ok(json!({ "nodeIds": [9] }))],
    );
    given(
        &script,
        "DOM.pushNodesByBackendIdsToFrontend",
        vec![Ok(json!({ "nodeIds": [8, 9] }))],
    );

    let resp = svc.snap(INS, "T1").await.unwrap();
    assert_eq!(resp["hoverScan"], json!("ok"));
    assert_eq!(resp["refs_count"], json!(2));
    let text = resp["text"].as_str().unwrap();
    assert!(text.contains("@e1 [button] [hover] Menu"), "{text}");
    assert!(text.contains("@e2 [link] [hover-reveal] Item"), "{text}");
    // 文本取自 sheet s2（排序后 s1 在前但 origin 过滤跳过）；桥接逐位对齐
    let sheet = calls_of(&calls, "CSS.getStyleSheetText");
    assert_eq!(sheet.len(), 1);
    assert_eq!(sheet[0].param("styleSheetId"), json!("s2"));
    let push = calls_of(&calls, "DOM.pushNodesByBackendIdsToFrontend");
    assert_eq!(push[0].param("backendNodeIds"), json!([101, 102]));
}

/// querySelectorAll 报选择器非法（真机 CfT 131 报文 "DOM Error while
/// querying"；旧变体 "... is not a valid selector"——启发式提取产物仍
/// 可能的形态，如预处理器 /deep/ 组合子）→ 跳过该选择器而非 truncated：
/// truncated 语义 = 预算内不完整，语法失败只丢该条规则，不能让含非法
/// 形态的页面恒报狼来了。
#[tokio::test]
async fn snap_hover_scan_skips_invalid_selectors_instead_of_truncating() {
    let (svc, watched, _calls, script) = armed();
    handle_event(&watched, &sheet_added_event("S1", "s1", "regular", false)).await;
    given(
        &script,
        "CSS.getStyleSheetText",
        vec![Ok(
            json!({ "text": ".btn:hover { color: red; }\n.legacy:hover /deep/ .tip { color: blue; }" }),
        )],
    );
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [
            { "nodeId": "1", "role": { "value": "button" }, "name": { "value": "B" }, "backendDOMNodeId": 9 },
        ]}))],
    );
    given(
        &script,
        "DOM.getDocument",
        vec![Ok(json!({ "root": { "nodeId": 1 } }))],
    );
    // 查询序：trigger ".btn" 命中；reveal ".legacy /deep/ .tip" 被拒（非法）
    given(
        &script,
        "DOM.querySelectorAll",
        vec![
            Ok(json!({ "nodeIds": [3] })),
            Err("-32000 DOM Error while querying".to_string()),
        ],
    );
    given(
        &script,
        "DOM.pushNodesByBackendIdsToFrontend",
        vec![Ok(json!({ "nodeIds": [3] }))],
    );

    let resp = svc.snap(INS, "T1").await.unwrap();
    assert_eq!(
        resp["hoverScan"],
        json!("ok"),
        "非法选择器只跳过该条，不截断"
    );
    assert!(
        resp["text"].as_str().unwrap().contains("[hover]"),
        "合法规则照常标记"
    );
}

/// 有表但无 hover 规则 → skipped；文本树无标记，hover 链零 DOM 查询。
#[tokio::test]
async fn snap_hover_scan_skipped_when_no_hover_rules() {
    let (svc, watched, calls, script) = armed();
    handle_event(&watched, &sheet_added_event("S1", "s1", "regular", false)).await;
    given(
        &script,
        "CSS.getStyleSheetText",
        vec![Ok(json!({ "text": ".plain { color: red; }" }))],
    );
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [
            { "nodeId": "1", "role": { "value": "button" }, "name": { "value": "OK" }, "backendDOMNodeId": 7 },
        ]}))],
    );
    let resp = svc.snap(INS, "T1").await.unwrap();
    assert_eq!(resp["hoverScan"], json!("skipped"));
    let text = resp["text"].as_str().unwrap();
    assert!(
        text.contains("@e1 [button] OK") && !text.contains("[hover"),
        "{text}"
    );
    assert_eq!(calls_of(&calls, "DOM.querySelectorAll").len(), 0);
}

/// 登记表空（无表页/事件未达）→ skipped（snap 不挂）。两路径：
/// 无 css_since（未 attach 过/CSS.enable 失败）即时返回；css_since 在
/// 窗口期内（回放可能在途）等到窗口耗尽再放行。
#[tokio::test]
async fn snap_hover_scan_skipped_when_registry_empty() {
    let (svc, watched, _calls, script) = armed();
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [] })), Ok(json!({ "nodes": [] }))],
    );
    // 路径 1：armed 未走 on_attached，无 css_since → 即时 skipped
    let t0 = Instant::now();
    let resp = svc.snap(INS, "T1").await.unwrap();
    assert_eq!(resp["hoverScan"], json!("skipped"));
    assert!(
        t0.elapsed() < HOVER_SETTLE_WINDOW,
        "无 css_since 不等窗口（{:?}）",
        t0.elapsed()
    );
    // 路径 2：css_since 刚登记（模拟 CSS.enable 刚成功）→ 等满窗口仍空 → skipped
    watched
        .css_since
        .lock()
        .unwrap()
        .insert("T1".into(), Instant::now());
    let t1 = Instant::now();
    let resp2 = svc.snap(INS, "T1").await.unwrap();
    assert_eq!(resp2["hoverScan"], json!("skipped"));
    assert!(
        t1.elapsed() >= HOVER_SETTLE_WINDOW,
        "窗口期内确实等满（{:?}）",
        t1.elapsed()
    );
    // 窗口过后（人工把 css_since 回拨）→ 即时返回，不为无表页付每次等待
    watched.css_since.lock().unwrap().insert(
        "T1".into(),
        Instant::now() - HOVER_SETTLE_WINDOW - Duration::from_millis(50),
    );
    let t2 = Instant::now();
    let resp3 = svc.snap(INS, "T1").await.unwrap();
    assert_eq!(resp3["hoverScan"], json!("skipped"));
    assert!(t2.elapsed() < HOVER_SETTLE_WINDOW);
}

/// 规则预算 256 超限 → truncated（诚实证据）；snap 主体照常出树。
#[tokio::test]
async fn snap_hover_scan_truncated_by_rule_budget() {
    let (svc, watched, _calls, script) = armed();
    handle_event(&watched, &sheet_added_event("S1", "s1", "regular", false)).await;
    let css = (0..300)
        .map(|i| format!(".c{i}:hover {{ color: red; }}"))
        .collect::<Vec<_>>()
        .join("\n");
    given(
        &script,
        "CSS.getStyleSheetText",
        vec![Ok(json!({ "text": css }))],
    );
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [] }))],
    );
    // 后续 querySelectorAll/push 不脚本化 → Ok(Null)/空命中，链路不炸
    let resp = svc.snap(INS, "T1").await.unwrap();
    assert_eq!(resp["hoverScan"], json!("truncated"));
    assert_eq!(resp["refs_count"], json!(0));
}

/// 桥接失败（pushNodes 报错）→ status=error（任务书三值外的第四值：
/// 诚实区分「没扫出」与「扫不了」）；标记放弃但 snap 主体不受影响。
#[tokio::test]
async fn snap_hover_scan_error_when_bridge_fails() {
    let (svc, watched, _calls, script) = armed();
    handle_event(&watched, &sheet_added_event("S1", "s1", "regular", false)).await;
    given(
        &script,
        "CSS.getStyleSheetText",
        vec![Ok(json!({ "text": ".menu-item:hover { color: red; }" }))],
    );
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [
            { "nodeId": "1", "role": { "value": "button" }, "name": { "value": "Menu" }, "backendDOMNodeId": 101 },
        ]}))],
    );
    given(
        &script,
        "DOM.getDocument",
        vec![Ok(json!({ "root": { "nodeId": 1 } }))],
    );
    given(
        &script,
        "DOM.querySelectorAll",
        vec![Ok(json!({ "nodeIds": [8] }))],
    );
    given(
        &script,
        "DOM.pushNodesByBackendIdsToFrontend",
        vec![Err("backend node not found".into())],
    );
    let resp = svc.snap(INS, "T1").await.unwrap();
    assert_eq!(resp["hoverScan"], json!("error"));
    let text = resp["text"].as_str().unwrap();
    assert!(
        text.contains("@e1 [button] Menu") && !text.contains("[hover"),
        "{text}"
    );
}

/// 根 frame 导航 = 换文档：登记表整清（Chrome 不保证逐张 Removed，死表
/// 残留会让 getStyleSheetText 恒 Err → 假 truncated，真机实测 total=4
/// texts=1）；子 frame 导航不清。新表由 styleSheetAdded 重新登记后 snap 恢复 ok。
#[tokio::test]
async fn navigation_clears_sheet_registry_root_frame_only() {
    let (svc, watched, _calls, script) = armed();
    // 预置 ref 表（invalidate_refs 只对已存在的 store 起效，与 sheets
    // 清理同一事件分支触发——钉住两条链路不回归）
    watched
        .refs
        .lock()
        .unwrap()
        .entry("T1".into())
        .or_default()
        .replace(&json!([{ "ref": "e1", "backendNodeId": 1 }]));
    handle_event(&watched, &sheet_added_event("S1", "a-1", "regular", false)).await;
    handle_event(&watched, &sheet_added_event("S1", "a-2", "regular", false)).await;
    assert_eq!(watched.sheets.lock().unwrap()["T1"].len(), 2);

    // 子 frame 导航：不清（子 frame 不换主文档）
    handle_event(
        &watched,
        &json!({
            "method": "Page.frameNavigated", "sessionId": "S1",
            "params": { "frame": { "id": "f1", "parentId": "root" } }
        }),
    )
    .await;
    assert_eq!(
        watched.sheets.lock().unwrap()["T1"].len(),
        2,
        "子 frame 导航不清登记"
    );

    // 根 frame 导航：整清
    handle_event(
        &watched,
        &json!({
            "method": "Page.frameNavigated", "sessionId": "S1",
            "params": { "frame": { "id": "root" } }
        }),
    )
    .await;
    assert!(
        watched.sheets.lock().unwrap().get("T1").is_none(),
        "根 frame 导航整清登记"
    );
    // refs 失效链路与 sheets 清理同一事件分支触发（既有语义不回归；
    // 须在 snap 前断言——snap 的 replace 会清 stale 与 reason）
    assert_eq!(
        watched
            .refs
            .lock()
            .unwrap()
            .get("T1")
            .unwrap()
            .last_invalid_reason(),
        Some("navigation")
    );

    // 新文档的表重新登记 → snap ok（死表已不存在，无 Err 无假 truncated）
    handle_event(&watched, &sheet_added_event("S1", "b-1", "regular", false)).await;
    given(
        &script,
        "CSS.getStyleSheetText",
        vec![Ok(json!({ "text": ".btn:hover { color: red; }" }))],
    );
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [
            { "nodeId": "1", "role": { "value": "button" }, "name": { "value": "B" }, "backendDOMNodeId": 9 },
        ]}))],
    );
    given(
        &script,
        "DOM.getDocument",
        vec![Ok(json!({ "root": { "nodeId": 1 } }))],
    );
    given(
        &script,
        "DOM.querySelectorAll",
        vec![Ok(json!({ "nodeIds": [3] }))],
    );
    given(
        &script,
        "DOM.pushNodesByBackendIdsToFrontend",
        vec![Ok(json!({ "nodeIds": [3] }))],
    );
    let resp = svc.snap(INS, "T1").await.unwrap();
    assert_eq!(resp["hoverScan"], json!("ok"));
    assert!(resp["text"].as_str().unwrap().contains("[hover]"));
}

/// 根导航清 sheets 同时重置 css_since：结算窗口对文档代际重开——否则
/// 窗口起点仍停在 attach 时刻，导航后首个 snap 被立即放行，
/// styleSheetAdded 回放在途时空登记假报 skipped（或部分登记被当完整扫描）。
/// 未 attach 过 CSS（无 css_since 条目）的 tab 不人为造条目（不白付等待）。
#[tokio::test]
async fn root_navigation_reopens_css_settle_window() {
    let (_svc, watched, _calls, _script) = armed();
    handle_event(&watched, &sheet_added_event("S1", "s1", "regular", false)).await;
    // 人工老化：模拟 attach 已远（窗口早已过）
    watched.css_since.lock().unwrap().insert(
        "T1".into(),
        Instant::now() - HOVER_SETTLE_WINDOW - Duration::from_millis(50),
    );

    // 无 css_since 的 tab（TAB2 从未 attach CSS）：根导航不造条目
    handle_event(
        &watched,
        &json!({ "method": "Page.frameNavigated", "sessionId": "S2",
            "params": { "frame": { "id": "r2" } } }),
    )
    .await;
    assert!(
        watched.css_since.lock().unwrap().get("TAB2").is_none(),
        "未 attach CSS 的 tab 不重置"
    );

    // T1 根导航：登记清空 + 窗口重置为当下
    handle_event(
        &watched,
        &json!({ "method": "Page.frameNavigated", "sessionId": "S1",
            "params": { "frame": { "id": "r1" } } }),
    )
    .await;
    let since = watched.css_since.lock().unwrap().get("T1").copied();
    assert!(
        since.is_some_and(|t| Instant::now().duration_since(t) < HOVER_SETTLE_WINDOW),
        "css_since 重置为当下（首个 snap 重新进入回放等待窗口）"
    );
    assert!(
        watched.sheets.lock().unwrap().get("T1").is_none(),
        "登记同时清空"
    );
}

/// document.open() 换文档（Page.documentOpened）与根导航同形态：旧样式表
/// 全部销毁且 styleSheetRemoved 不保证补发（死表残留 → hoverScan 恒
/// truncated）——清登记 + 重开结算窗口。
#[tokio::test]
async fn document_opened_clears_sheet_registry_and_reopens_window() {
    let (_svc, watched, _calls, _script) = armed();
    handle_event(&watched, &sheet_added_event("S1", "s1", "regular", false)).await;
    assert_eq!(watched.sheets.lock().unwrap()["T1"].len(), 1);
    watched.css_since.lock().unwrap().insert(
        "T1".into(),
        Instant::now() - HOVER_SETTLE_WINDOW - Duration::from_millis(50),
    );

    handle_event(
        &watched,
        &json!({ "method": "Page.documentOpened", "sessionId": "S1" }),
    )
    .await;
    assert!(
        watched.sheets.lock().unwrap().get("T1").is_none(),
        "登记清空（旧表全死）"
    );
    let since = watched.css_since.lock().unwrap().get("T1").copied();
    assert!(
        since.is_some_and(|t| Instant::now().duration_since(t) < HOVER_SETTLE_WINDOW),
        "结算窗口重开（新文档回放可等待）"
    );
}

/// styleSheetAdded/Removed 事件维护登记表；drop_tab 随 per-tab 状态清理。
#[tokio::test]
async fn style_sheet_events_maintain_registry_per_tab() {
    let (_svc, watched, _calls, _script) = armed();
    handle_event(&watched, &sheet_added_event("S1", "s1", "regular", false)).await;
    handle_event(
        &watched,
        &sheet_added_event("S1", "s2", "user-agent", false),
    )
    .await;
    handle_event(
        &watched,
        &sheet_added_event("S2", "other-tab-sheet", "regular", false),
    )
    .await;
    {
        let sheets = watched.sheets.lock().unwrap();
        let t1 = sheets.get("T1").unwrap();
        assert_eq!(t1.len(), 2, "登记全量（origin 过滤在读取侧）");
        assert_eq!(t1.get("s1"), Some(&("regular".into(), false)));
        assert_eq!(sheets.get("TAB2").unwrap().len(), 1, "按 tab 隔离");
    }
    // Removed 按表撤销
    handle_event(
            &watched,
            &json!({ "method": "CSS.styleSheetRemoved", "sessionId": "S1", "params": { "styleSheetId": "s1" } }),
        )
        .await;
    assert!(!watched.sheets.lock().unwrap()["T1"].contains_key("s1"));
    // tab 销毁 → 登记随 per-tab 状态清理
    handle_event(
        &watched,
        &json!({ "method": "Target.targetDestroyed", "params": { "targetId": "T1" } }),
    )
    .await;
    assert!(watched.sheets.lock().unwrap().get("T1").is_none());
}

// ---------------- 工具：click（ref / selector 两路）----------------------

#[tokio::test]
async fn click_ref_path_resolves_quad_centroid_and_dispatches() {
    let (svc, watched, calls, script) = armed();
    watched
        .refs
        .lock()
        .unwrap()
        .entry("T1".into())
        .or_default()
        .replace(&json!([{ "ref": "e1", "backendNodeId": 101, "name": "OK" }]));
    given(
        &script,
        "DOM.getContentQuads",
        vec![Ok(
            json!({ "quads": [[0.0, 0.0, 200.0, 0.0, 200.0, 100.0, 0.0, 100.0]] }),
        )],
    );

    let resp = svc.click(INS, "T1", "@e1").await.unwrap();
    assert_eq!(
        resp["result"],
        json!("Clicked @e1 \"OK\" at CSS (100, 50)"),
        "坐标 = 最大四边形质心，JS 数字形态"
    );
    // 命令链：scrollIntoViewIfNeeded → getContentQuads → 三段鼠标（均带 session）
    let scroll = calls_of(&calls, "DOM.scrollIntoViewIfNeeded");
    assert_eq!(
        (scroll.len(), scroll[0].param("backendNodeId")),
        (1, json!(101))
    );
    assert_eq!(
        calls_of(&calls, "DOM.getContentQuads")[0].param("backendNodeId"),
        json!(101)
    );
    let mouse = calls_of(&calls, "Input.dispatchMouseEvent");
    assert_eq!(
        mouse.iter().map(|c| c.param("type")).collect::<Vec<_>>(),
        vec![
            json!("mouseMoved"),
            json!("mousePressed"),
            json!("mouseReleased")
        ]
    );
    for c in &mouse {
        assert_eq!(c.param("x"), json!(100.0));
        assert_eq!(c.param("y"), json!(50.0));
        assert_eq!(c.param("button"), json!("left"));
        assert_eq!(c.param("clickCount"), json!(1));
        assert_eq!(c.session.as_deref(), Some("S1"));
    }
}

/// quads 空/退化 → resolveNode + getBoundingClientRect 回退链
#[tokio::test]
async fn click_ref_falls_back_to_rect_when_quads_degenerate() {
    let (svc, watched, calls, script) = armed();
    watched
        .refs
        .lock()
        .unwrap()
        .entry("T1".into())
        .or_default()
        .replace(&json!([{ "ref": "e1", "backendNodeId": 7 }]));
    given(
        &script,
        "DOM.getContentQuads",
        vec![Ok(json!({ "quads": [] }))],
    );
    given(
        &script,
        "DOM.resolveNode",
        vec![Ok(json!({ "object": { "objectId": "obj7" } }))],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![Ok(
            json!({ "result": { "value": { "x": 10.0, "y": 20.0, "w": 100.0, "h": 60.0 } } }),
        )],
    );

    let resp = svc.click(INS, "T1", "e1").await.unwrap();
    assert_eq!(resp["result"], json!("Clicked @e1 at CSS (60, 50)"));
    let calls = calls.lock().unwrap();
    let order: Vec<&str> = calls
        .iter()
        .map(|c| c.method.as_str())
        .filter(|m| *m != "Runtime.enable")
        .collect();
    assert_eq!(
        order,
        vec![
            "DOM.scrollIntoViewIfNeeded",
            "DOM.getContentQuads",
            "DOM.resolveNode",
            "Runtime.callFunctionOn",
            "Input.dispatchMouseEvent",
            "Input.dispatchMouseEvent",
            "Input.dispatchMouseEvent",
        ]
    );
}

/// ref 未命中：无表 = not_found；失效后 = stale（P2-A ref_miss 语义）
#[tokio::test]
async fn click_ref_miss_and_stale_errors() {
    let (svc, _w, _calls, _script) = armed();
    let miss = svc.click(INS, "T1", "@e9").await.unwrap_err();
    assert_eq!((miss.code(), miss.status()), ("PAGE_REF_NOT_FOUND", 404));

    let (svc2, watched2, _c2, _s2) = armed();
    let mut store = RefStore::new();
    store.replace(&json!([{ "ref": "e1", "backendNodeId": 1 }]));
    store.invalidate("navigation");
    watched2.refs.lock().unwrap().insert("T1".into(), store);
    let stale = svc2.click(INS, "T1", "@e1").await.unwrap_err();
    assert_eq!((stale.code(), stale.status()), ("PAGE_REF_STALE", 409));
    assert!(stale.to_string().contains("navigation"));
}

#[tokio::test]
async fn click_selector_path_locates_hit_tests_and_dispatches() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![
            eval_response(json!({ "x": 150.0, "y": 130.0, "tag": "BUTTON", "text": "Save" })),
            eval_response(json!({ "hit": true })),
        ],
    );

    let resp = svc.click(INS, "TA", "#save").await.unwrap();
    // 前缀 "TA" 唯一解析到 TAB2 → 全链带 session S2
    assert_eq!(resp["targetId"], json!("TAB2"));
    assert_eq!(
        resp["result"],
        json!("Clicked <BUTTON> \"Save\" (native input @150,130)")
    );
    let evals = calls_of(&calls, "Runtime.evaluate");
    assert_eq!(evals.len(), 2);
    assert!(
        evals[0]
            .param("expression")
            .as_str()
            .unwrap()
            .contains(r##"querySelector("#save")"##),
        "第一次 eval = locate"
    );
    assert!(
        evals[1]
            .param("expression")
            .as_str()
            .unwrap()
            .contains("elementFromPoint(150, 130)"),
        "第二次 eval = hit-test（注入命中点）"
    );
    for c in &evals {
        assert_eq!(c.session.as_deref(), Some("S2"));
    }
    assert_eq!(
        calls_of(&calls, "Runtime.enable").len(),
        2,
        "evalStr 每次先 enable（TS 同）"
    );
    assert_eq!(calls_of(&calls, "Input.dispatchMouseEvent").len(), 3);
}

/// 可见性 / 遮挡分类：locate not_visible → 422；hit covered → 409，且零派发
#[tokio::test]
async fn click_selector_classifies_visibility_and_coverage() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![eval_response(
            json!({ "error": "not_visible", "why": "display:none" }),
        )],
    );
    let e = svc.click(INS, "T1", ".x").await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ELEMENT_NOT_VISIBLE", 422));
    assert_eq!(calls_of(&calls, "Input.dispatchMouseEvent").len(), 0);

    let (svc2, _w2, _c2, script2) = armed();
    given(
        &script2,
        "Runtime.evaluate",
        vec![
            eval_response(json!({ "x": 5.0, "y": 5.0, "tag": "A", "text": "t" })),
            eval_response(json!({ "hit": false, "reason": "covered", "by": "DIV.overlay" })),
        ],
    );
    let e2 = svc2.click(INS, "T1", ".x").await.unwrap_err();
    assert_eq!((e2.code(), e2.status()), ("PAGE_ELEMENT_BLOCKED", 409));
    assert!(e2.to_string().contains("<DIV.overlay>"));
}

/// press 后失败：补一发 release，effect_state=unknown（§2.7）
#[tokio::test]
async fn click_press_failure_compensates_release_with_effect_state() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![
            eval_response(json!({ "x": 1.0, "y": 2.0, "tag": "A", "text": "t" })),
            eval_response(json!({ "hit": true })),
        ],
    );
    given(
        &script,
        "Input.dispatchMouseEvent",
        vec![Ok(json!({})), Ok(json!({})), Err("boom".into())],
    );

    let e = svc.click(INS, "T1", ".x").await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_DISPATCH_FAILED", 500));
    assert!(e.to_string().contains("effect_state: unknown"));
    assert!(e
        .to_string()
        .contains("mousePressed was already dispatched"));
    let mouse = calls_of(&calls, "Input.dispatchMouseEvent");
    assert_eq!(
        mouse.iter().map(|c| c.param("type")).collect::<Vec<_>>(),
        vec![
            json!("mouseMoved"),
            json!("mousePressed"),
            json!("mouseReleased"),
            json!("mouseReleased")
        ],
        "失败后的补偿 release 也已发出"
    );
}

// ---------------- 工具：fill ---------------------------------------------

#[tokio::test]
async fn fill_pipeline_focus_clear_inserts_and_verifies() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "DOM.getDocument",
        vec![Ok(json!({ "root": { "nodeId": 5 } }))],
    );
    given(
        &script,
        "DOM.querySelector",
        vec![Ok(json!({ "nodeId": 9 }))],
    );
    given(
        &script,
        "DOM.resolveNode",
        vec![Ok(json!({ "object": { "objectId": "obj9" } }))],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![
            Ok(
                json!({ "result": { "value": { "tag": "INPUT", "kind": "native", "label": "q" } } }),
            ),
            Ok(json!({ "result": { "value": { "cleared": true } } })),
            Ok(json!({ "result": { "value": { "matches": true } } })),
        ],
    );

    let resp = svc.fill(INS, "T1", "#q", "hello").await.unwrap();
    assert_eq!(
        resp["result"],
        json!("Filled <INPUT> \"q\" with \"hello\" (5 chars)")
    );

    let focus = calls_of(&calls, "DOM.focus");
    assert_eq!(
        (focus.len(), focus[0].param("objectId")),
        (1, json!("obj9"))
    );
    let insert = calls_of(&calls, "Input.insertText");
    assert_eq!((insert.len(), insert[0].param("text")), (1, json!("hello")));
    let release = calls_of(&calls, "Runtime.releaseObject");
    assert_eq!(
        (release.len(), release[0].param("objectId")),
        (1, json!("obj9"))
    );
    let call_fns = calls_of(&calls, "Runtime.callFunctionOn");
    assert_eq!(call_fns.len(), 3, "check → clear → verify");
    // verify 参数预计算两形态（multiLine / singleLine）
    assert_eq!(
        call_fns[2].param("arguments"),
        json!([{ "value": "hello" }, { "value": "hello" }])
    );
}

#[tokio::test]
async fn fill_rejects_not_fillable_and_reports_mismatch() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "DOM.getDocument",
        vec![Ok(json!({ "root": { "nodeId": 5 } }))],
    );
    given(
        &script,
        "DOM.querySelector",
        vec![Ok(json!({ "nodeId": 9 }))],
    );
    given(
        &script,
        "DOM.resolveNode",
        vec![Ok(json!({ "object": { "objectId": "obj9" } }))],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![Ok(
            json!({ "result": { "value": { "not_fillable": "disabled" } } }),
        )],
    );
    let e = svc.fill(INS, "T1", "#q", "x").await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_TARGET_NOT_FILLABLE", 422));
    assert_eq!(calls_of(&calls, "Input.insertText").len(), 0);
    assert_eq!(
        calls_of(&calls, "Runtime.releaseObject").len(),
        1,
        "失败路径同样归还句柄（finally 语义）"
    );

    // 回读不一致：消息带期望/实际摘要（写入可能已部分生效，观察后再重试）
    let (svc2, _w2, _c2, script2) = armed();
    given(
        &script2,
        "DOM.getDocument",
        vec![Ok(json!({ "root": { "nodeId": 5 } }))],
    );
    given(
        &script2,
        "DOM.querySelector",
        vec![Ok(json!({ "nodeId": 9 }))],
    );
    given(
        &script2,
        "DOM.resolveNode",
        vec![Ok(json!({ "object": { "objectId": "obj9" } }))],
    );
    given(
        &script2,
        "Runtime.callFunctionOn",
        vec![
            Ok(json!({ "result": { "value": { "tag": "INPUT", "kind": "native", "label": "" } } })),
            Ok(json!({ "result": { "value": { "cleared": true } } })),
            Ok(
                json!({ "result": { "value": { "matches": false, "actual": "hell", "expected": "hello" } } }),
            ),
        ],
    );
    let e2 = svc2.fill(INS, "T1", "#q", "hello").await.unwrap_err();
    assert_eq!((e2.code(), e2.status()), ("PAGE_FILL_VALUE_MISMATCH", 409));
    let msg = e2.to_string();
    assert!(
        msg.contains("expected \"hello\"") && msg.contains("got \"hell\""),
        "{msg}"
    );
}

/// clear 阶段 focus_lost：页面在 focus 后抢走了焦点 → 409 fill_focus_lost
#[tokio::test]
async fn fill_reports_focus_lost_when_page_steals_focus() {
    let (svc, _w, _calls, script) = armed();
    given(
        &script,
        "DOM.getDocument",
        vec![Ok(json!({ "root": { "nodeId": 5 } }))],
    );
    given(
        &script,
        "DOM.querySelector",
        vec![Ok(json!({ "nodeId": 9 }))],
    );
    given(
        &script,
        "DOM.resolveNode",
        vec![Ok(json!({ "object": { "objectId": "obj9" } }))],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![
            Ok(json!({ "result": { "value": { "tag": "INPUT", "kind": "native", "label": "" } } })),
            Ok(json!({ "result": { "value": { "focus_lost": true } } })),
        ],
    );
    let e = svc.fill(INS, "T1", "#q", "x").await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_FILL_FOCUS_LOST", 409));
}

// ---------------- 工具：press --------------------------------------------

#[tokio::test]
async fn press_dispatches_three_part_sequence_and_suppresses_combo_text() {
    let (svc, _w, calls, _script) = armed();
    let resp = svc.press(INS, "T1", "Enter", None).await.unwrap();
    assert_eq!(resp["result"], json!("Pressed Enter"));
    let events = calls_of(&calls, "Input.dispatchKeyEvent");
    assert_eq!(
        events[0].params,
        json!({ "type": "rawKeyDown", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "modifiers": 0 })
    );
    assert_eq!(
        events[1].params,
        json!({ "type": "char", "key": "Enter", "code": "Enter", "text": "\r", "modifiers": 0 }),
        "char 带 text 不带 vk"
    );
    assert_eq!(events[2].param("type"), json!("keyUp"));

    // Ctrl+A：char 被抑制（不能打出 "a"）
    let (svc2, _w2, calls2, _s2) = armed();
    svc2.press(INS, "T1", "Control+A", None).await.unwrap();
    let events2 = calls_of(&calls2, "Input.dispatchKeyEvent");
    assert_eq!(events2.len(), 2);
    for e in &events2 {
        assert_eq!(e.param("modifiers"), json!(2));
    }
}

/// --target ref：先 DOM.focus（backendNodeId），再发键序
#[tokio::test]
async fn press_focuses_ref_target_before_dispatch() {
    let (svc, watched, calls, _script) = armed();
    watched
        .refs
        .lock()
        .unwrap()
        .entry("T1".into())
        .or_default()
        .replace(&json!([{ "ref": "e1", "backendNodeId": 42, "name": "search" }]));

    let resp = svc.press(INS, "T1", "Enter", Some("@e1")).await.unwrap();
    assert_eq!(resp["result"], json!("Pressed Enter on @e1 \"search\""));
    let focus = calls_of(&calls, "DOM.focus");
    assert_eq!(
        (focus.len(), focus[0].param("backendNodeId")),
        (1, json!(42))
    );
    assert_eq!(calls_of(&calls, "Input.dispatchKeyEvent").len(), 3);
}

// ---------------- 工具：wait ---------------------------------------------

#[tokio::test]
async fn wait_satisfied_first_poll_and_timeout_paths() {
    let (svc, _w, _calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![eval_response(json!({ "observed": "visible" }))],
    );
    let resp = svc.wait(INS, "T1", ".ready", None, None).await.unwrap();
    let result = resp["result"].as_str().unwrap();
    assert!(result.starts_with("Waited: .ready is visible (after ") && result.contains("1 polls"));

    // 超时：恒 hidden → PAGE_WAIT_TIMEOUT，附最后观察态与轮询数
    let (svc2, _w2, _c2, script2) = armed();
    let hidden: Vec<Result<Value, String>> = (0..16)
        .map(|_| eval_response(json!({ "observed": "hidden" })))
        .collect();
    given(&script2, "Runtime.evaluate", hidden);
    let e = svc2
        .wait(INS, "T1", ".x", Some("visible"), Some("80ms"))
        .await
        .unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_WAIT_TIMEOUT", 409));
    let msg = e.to_string();
    assert!(msg.contains("last observed: hidden"), "{msg}");
    assert!(msg.contains("waiting for \".x\" to be visible"), "{msg}");
    assert!(
        msg.contains("2 polls"),
        "80ms 预算 + 首轮即查：第 2 轮过截止线: {msg}"
    );

    // eval 异常 = detached：state=detached 首轮即满足
    let (svc3, _w3, _c3, script3) = armed();
    given(&script3, "Runtime.evaluate", vec![Err("navigation".into())]);
    let resp3 = svc3
        .wait(INS, "T1", ".x", Some("detached"), None)
        .await
        .unwrap();
    assert!(resp3["result"].as_str().unwrap().contains("is detached"));

    // 垃圾观察值 = unknown：满足 attached（TS `observed !== 'detached'` 原样）
    let (svc4, _w4, _c4, script4) = armed();
    given(
        &script4,
        "Runtime.evaluate",
        vec![eval_response(json!({ "observed": "garbage" }))],
    );
    let resp4 = svc4
        .wait(INS, "T1", ".x", Some("attached"), None)
        .await
        .unwrap();
    assert!(resp4["result"].as_str().unwrap().contains("is attached"));

    // 非法 state → 400（白名单）
    let bad = svc
        .wait(INS, "T1", ".x", Some("bogus"), None)
        .await
        .unwrap_err();
    assert_eq!(bad.code(), "INVALID_REQUEST");
    assert!(bad.to_string().contains("visible|hidden|attached|detached"));
}

// ---------------- 工具：dialogs + dialogs_seen 组装 ----------------------

#[tokio::test]
async fn dialogs_reads_buffer_since_cursor() {
    let (svc, watched, _calls, _script) = armed();
    {
        let mut tabs = watched.dialogs.lock().unwrap();
        let buf = tabs
            .entry("T1".to_string())
            .or_insert_with(|| DialogBuffer::new("T1"));
        buf.record(DialogInfo {
            r#type: "alert".into(),
            message: "m1".into(),
            url: None,
            default_prompt: None,
            has_browser_handler: None,
        });
        buf.record(DialogInfo {
            r#type: "alert".into(),
            message: "m2".into(),
            url: None,
            default_prompt: None,
            has_browser_handler: None,
        });
    }
    let resp = svc.dialogs(INS, "T1", Some(1)).await.unwrap();
    assert_eq!(resp["instanceId"], json!(INS));
    assert_eq!(resp["cursor"], json!(2), "游标随成功应答前移");
    let entries = resp["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "since(1) 严格大于：只见 seq 2");
    assert_eq!(entries[0]["message"], json!("m2"));

    // 无缓冲的 tab 返回空 entries 而非错误（查询投影空数据语义）
    let resp2 = svc.dialogs(INS, "TAB2", None).await.unwrap();
    assert_eq!(resp2["cursor"], json!(0));
    assert_eq!(resp2["entries"].as_array().unwrap().len(), 0);
}

/// §1.6：命令期间新应答的对话框附 dialogs_seen（仅成功响应）
#[tokio::test]
async fn command_attaches_dialogs_seen_on_success() {
    let (svc, _watched, _calls, _script) = armed();
    let resp = svc
        .command(
            Duration::from_secs(5),
            "probe",
            INS,
            "T1",
            |w, tid, _sid| async move {
                // 模拟命令执行期间对话框被应答入档
                w.dialogs
                    .lock()
                    .unwrap()
                    .entry(tid.clone())
                    .or_insert_with(|| DialogBuffer::new(tid.clone()))
                    .record(DialogInfo {
                        r#type: "prompt".into(),
                        message: "name?".into(),
                        url: None,
                        default_prompt: Some("Bob".into()),
                        has_browser_handler: None,
                    });
                Ok(json!({ "result": "done" }))
            },
        )
        .await
        .unwrap();
    assert_eq!(resp["instanceId"], json!(INS));
    assert_eq!(resp["targetId"], json!("T1"));
    let seen = resp["dialogs_seen"].as_array().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["type"], json!("prompt"));
    assert_eq!(seen[0]["prompt_text"], serde_json::Value::Null);
    assert_eq!(seen[0]["default_prompt"], json!("Bob"));
    // 无新对话框时省略字段（wire 保持向后兼容）
    let resp2 = svc
        .command(
            Duration::from_secs(5),
            "probe",
            INS,
            "T1",
            |_w, _tid, _sid| async { Ok(json!({ "result": "again" })) },
        )
        .await
        .unwrap();
    assert!(resp2.get("dialogs_seen").is_none());
}

// ---------------- 工具级预算 ---------------------------------------------

/// 预算超时 → 504 PAGE_TOOL_TIMEOUT（wait 例外口径在 wait() 内组装）
#[tokio::test]
async fn within_budget_times_out_stalled_tools() {
    let e = within_budget::<Value, _>(Duration::from_millis(20), "test", std::future::pending())
        .await
        .unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_TOOL_TIMEOUT", 504));
    assert!(e.to_string().contains("page tool test"));
}

/// 未知 tab：经工具入口的 404 路由
#[tokio::test]
async fn unknown_tab_errors_through_tool_entry() {
    let (svc, _w, _calls, _script) = armed();
    let e = svc.click(INS, "NOPE", "@e1").await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_TAB_NOT_FOUND", 404));
}

// ---------------- 工具：eval（P2-G）---------------------------------------

/// Runtime.evaluate 原始响应形状（工具层直接拼 result RemoteObject）
fn rt_result(value: Value) -> Result<Value, String> {
    Ok(json!({ "result": { "type": "stub", "value": value } }))
}

/// evalStr 序列化全分支：对象 pretty JSON / 字符串原样 / undefined 与
/// null 字面量（靠 result.type 区分）/ 数字 JS 形态 / 布尔 / 数组
#[tokio::test]
async fn eval_serializes_result_per_ts_evalstr() {
    let (svc, _w, _calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![
            Ok(json!({ "result": { "type": "object", "value": { "ok": true } } })),
            rt_result(json!("plain text")),
            Ok(json!({ "result": { "type": "undefined" } })),
            Ok(json!({ "result": { "type": "object", "subtype": "null", "value": null } })),
            rt_result(json!(150)),
            rt_result(json!(2.5)),
            rt_result(json!(false)),
            rt_result(json!([1, 2])),
        ],
    );
    let r = svc.eval(INS, "T1", "({ok:true})").await.unwrap();
    assert_eq!(
        r["result"],
        json!("{\n  \"ok\": true\n}"),
        "对象 → pretty JSON"
    );
    assert_eq!(r["instanceId"], json!(INS));
    assert_eq!(r["targetId"], json!("T1"));
    assert_eq!(
        svc.eval(INS, "T1", "'x'").await.unwrap()["result"],
        json!("plain text")
    );
    assert_eq!(
        svc.eval(INS, "T1", "u").await.unwrap()["result"],
        json!("undefined")
    );
    assert_eq!(
        svc.eval(INS, "T1", "n").await.unwrap()["result"],
        json!("null")
    );
    assert_eq!(
        svc.eval(INS, "T1", "1").await.unwrap()["result"],
        json!("150")
    );
    assert_eq!(
        svc.eval(INS, "T1", "2").await.unwrap()["result"],
        json!("2.5")
    );
    assert_eq!(
        svc.eval(INS, "T1", "3").await.unwrap()["result"],
        json!("false")
    );
    assert_eq!(
        svc.eval(INS, "T1", "4").await.unwrap()["result"],
        json!("[\n  1,\n  2\n]")
    );
}

/// 页面抛错 → 500 PAGE_EVAL_FAILED（文案取 exceptionDetails.text）
#[tokio::test]
async fn eval_exception_maps_to_page_eval_failed() {
    let (svc, _w, _calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![Ok(json!({
            "result": { "type": "undefined" },
            "exceptionDetails": { "text": "ReferenceError: x is not defined" }
        }))],
    );
    let e = svc.eval(INS, "T1", "x").await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_EVAL_FAILED", 500));
    assert!(e.to_string().contains("ReferenceError"));
}

// ---------------- 工具：html（P2-G）---------------------------------------

/// 两分支：缺省 = documentElement.outerHTML；有 selector = querySelector
/// 命中元素的 outerHTML（表达式形状断言防退化）
#[tokio::test]
async fn html_reads_full_document_or_selector_element() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![
            rt_result(json!("<html><body>full</body></html>")),
            rt_result(json!("<div id=\"main\">hit</div>")),
        ],
    );
    let r = svc.html(INS, "T1", None).await.unwrap();
    assert_eq!(r["html"], json!("<html><body>full</body></html>"));
    let exprs = calls_of(&calls, "Runtime.evaluate");
    assert!(
        exprs[0]
            .param("expression")
            .as_str()
            .unwrap()
            .contains("documentElement.outerHTML"),
        "缺省走整文档表达式"
    );

    let r2 = svc.html(INS, "T1", Some("#main")).await.unwrap();
    assert_eq!(r2["html"], json!("<div id=\"main\">hit</div>"));
    // 重新提取：exprs 是第一次调用后的快照，不含第二次的表达式
    let exprs = calls_of(&calls, "Runtime.evaluate");
    let expr = exprs[1].param("expression");
    let expr = expr.as_str().unwrap();
    assert!(
        expr.contains(r##"querySelector("#main")"##),
        "selector 注入用 JSON 转义: {expr}"
    );
    assert!(expr.contains("outerHTML"));
}

/// selector 未命中 → 404 PAGE_ELEMENT_NOT_FOUND（任务两选一取错误码，
/// 区别于 TS 的字符串哨兵 'Element not found'）
#[tokio::test]
async fn html_selector_miss_reports_element_not_found() {
    let (svc, _w, _calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![Ok(
            json!({ "result": { "type": "object", "subtype": "null", "value": null } }),
        )],
    );
    let e = svc.html(INS, "T1", Some("#gone")).await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ELEMENT_NOT_FOUND", 404));
    assert!(e.to_string().contains("#gone"));
}

// ---------------- 工具：extract（W-ext）---------------------------------

/// 收集器事实（同构 wire 样本）：thead 2 列 + tbody 1 行 + aria-rowcount
/// 超出可见行 → 归一化后 columns/rows/coverage/meta 齐备
fn facts_wire() -> Value {
    json!({
        "found": true, "format": "html", "caption": "订单",
        "aria": { "rowCount": 100, "colCount": null },
        "hiddenRows": 1, "totalRows": 2, "totalCells": 4, "partial": false,
        "sections": [
            { "kind": "thead", "rows": [ { "cells": [
                { "text": "姓名", "header": true, "rowSpan": 1, "colSpan": 1 },
                { "text": "年龄", "header": true, "rowSpan": 1, "colSpan": 1 },
            ] } ] },
            { "kind": "tbody", "rows": [ { "cells": [
                { "text": "张三", "rowSpan": 1, "colSpan": 1 },
                { "text": "", "rowSpan": 1, "colSpan": 1 },
            ] } ] },
        ]
    })
}

/// 自动发现路径：Runtime.evaluate 收集 → 归一化输出 + 命令骨架字段
#[tokio::test]
async fn extract_auto_discovers_and_normalizes() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![rt_result(json!(facts_wire().to_string()))],
    );
    let r = svc.extract(INS, "T1", None).await.unwrap();
    assert_eq!(
        r["columns"],
        json!([
            { "name": "姓名", "source": "header", "path": ["姓名"] },
            { "name": "年龄", "source": "header", "path": ["年龄"] },
        ])
    );
    // 空串保留（真实空单元格）
    assert_eq!(r["rows"], json!([["张三", ""]]));
    assert_eq!(r["coverage"]["datasetComplete"], "incomplete");
    assert_eq!(r["coverage"]["hiddenRowsOmitted"], 1);
    assert_eq!(r["meta"]["target"], "(auto-discovered)");
    assert_eq!(r["meta"]["caption"], "订单");
    // 命令骨架：instanceId/targetId 注入；evaluate 发到 tab session
    assert_eq!(r["instanceId"], INS);
    assert_eq!(r["targetId"], "T1");
    let evals = calls_of(&calls, "Runtime.evaluate");
    assert!(evals[0]
        .param("expression")
        .as_str()
        .unwrap()
        .contains("BUDGET = 10000"));
    assert_eq!(evals[0].session.as_deref(), Some("S1"));
}

/// 选择器未命中（页内拦 null → found:false/no-table）→ 404；
/// 表达式里 querySelector 注入 JSON 转义
#[tokio::test]
async fn extract_selector_miss_is_element_not_found() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![rt_result(json!(
            json!({ "found": false, "reason": "no-table" }).to_string()
        ))],
    );
    let e = svc.extract(INS, "T1", Some("#gone")).await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ELEMENT_NOT_FOUND", 404));
    let expr = calls_of(&calls, "Runtime.evaluate")[0]
        .param("expression")
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        expr.contains(r##"querySelector("#gone")"##),
        "selector 注入用 JSON 转义: {expr}"
    );
}

/// 目标命中但非表格 → 422 PAGE_EXTRACT_NOT_A_TABLE
#[tokio::test]
async fn extract_non_table_target_is_422() {
    let (svc, _w, _calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![rt_result(json!(
            json!({ "found": false, "reason": "not-a-table", "tag": "DIV", "role": null })
                .to_string()
        ))],
    );
    let e = svc.extract(INS, "T1", Some(".list")).await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_EXTRACT_NOT_A_TABLE", 422));
    assert!(e.to_string().contains("DIV"));
}

/// @eN 路径：resolveNode → callFunctionOn（returnByValue）→ 归一化；
/// 未 snap 的 ref → 404 PAGE_REF_NOT_FOUND
#[tokio::test]
async fn extract_ref_path_resolves_and_calls_on_element() {
    let (svc, watched, calls, script) = armed();
    watched
        .refs
        .lock()
        .unwrap()
        .entry("T1".into())
        .or_default()
        .replace(&json!([{ "ref": "e1", "backendNodeId": 7 }]));
    given(
        &script,
        "DOM.resolveNode",
        vec![Ok(json!({ "object": { "objectId": "OID-1" } }))],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![Ok(
            json!({ "result": { "type": "string", "value": facts_wire().to_string() } }),
        )],
    );
    let r = svc.extract(INS, "T1", Some("@e1")).await.unwrap();
    assert_eq!(r["rows"], json!([["张三", ""]]));
    assert_eq!(r["meta"]["target"], "@e1");
    let rn = calls_of(&calls, "DOM.resolveNode");
    assert_eq!(rn[0].param("backendNodeId"), json!(7));
    assert_eq!(rn[0].session.as_deref(), Some("S1"));
    let cfo = calls_of(&calls, "Runtime.callFunctionOn");
    assert_eq!(cfo[0].param("objectId"), json!("OID-1"));
    assert_eq!(cfo[0].param("returnByValue"), json!(true));
    assert!(cfo[0]
        .param("functionDeclaration")
        .as_str()
        .unwrap()
        .contains("BUDGET = 10000"));

    // 未 snap 的 ref：refs 表空（另一个 tab 无表）→ 404
    let e = svc.extract(INS, "TAB2", Some("@e9")).await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_REF_NOT_FOUND", 404));
}

// ---------------- 工具：clickxy（P2-G）------------------------------------

/// 坐标即意图：直接三段派发（带 session），零 eval（无 locate/hit-test）
#[tokio::test]
async fn clickxy_dispatches_three_part_sequence_without_hit_test() {
    let (svc, _w, calls, _script) = armed();
    let resp = svc.clickxy(INS, "T1", 150.0, 130.5).await.unwrap();
    assert_eq!(
        resp["result"],
        json!("Clicked at CSS (150, 130.5)"),
        "JS 数字形态"
    );
    let mouse = calls_of(&calls, "Input.dispatchMouseEvent");
    assert_eq!(
        mouse.iter().map(|c| c.param("type")).collect::<Vec<_>>(),
        vec![
            json!("mouseMoved"),
            json!("mousePressed"),
            json!("mouseReleased")
        ]
    );
    for c in &mouse {
        assert_eq!(c.param("x"), json!(150.0));
        assert_eq!(c.param("y"), json!(130.5));
        assert_eq!(c.session.as_deref(), Some("S1"));
    }
    assert!(
        calls_of(&calls, "Runtime.evaluate").is_empty(),
        "无 locate/hit-test（坐标即意图）"
    );
}

/// 非有限坐标（JSON 携带不了 NaN，拦 CLI/本地调用方的 inf/nan 字面量）
#[tokio::test]
async fn clickxy_rejects_non_finite_coordinates() {
    let (svc, _w, calls, _script) = armed();
    let e = svc.clickxy(INS, "T1", f64::NAN, 5.0).await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400));
    assert_eq!(calls_of(&calls, "Input.dispatchMouseEvent").len(), 0);
}

// ---------------- 工具：type（P2-G，owner 补充）---------------------------

/// 对焦点元素直接 insertText（带 session），无任何定位/校验 eval
#[tokio::test]
async fn type_inserts_text_into_focused_element() {
    let (svc, _w, calls, _script) = armed();
    let resp = svc.r#type(INS, "T1", "hello").await.unwrap();
    assert_eq!(resp["result"], json!("Typed 5 characters"));
    let insert = calls_of(&calls, "Input.insertText");
    assert_eq!(insert.len(), 1);
    assert_eq!(insert[0].param("text"), json!("hello"));
    assert_eq!(insert[0].session.as_deref(), Some("S1"));
}

/// 空文本 → 400（对齐 TS typeStr 的 'text required'）
#[tokio::test]
async fn type_rejects_empty_text() {
    let (svc, _w, calls, _script) = armed();
    let e = svc.r#type(INS, "T1", "").await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400));
    assert_eq!(calls_of(&calls, "Input.insertText").len(), 0);
}

// ---------------- 工具：loadall（P2-G）------------------------------------

fn rt_bool(b: bool) -> Result<Value, String> {
    Ok(json!({ "result": { "type": "boolean", "value": b } }))
}

fn located_ok() -> Result<Value, String> {
    eval_response(json!({ "x": 10.0, "y": 20.0, "tag": "BUTTON", "text": "More" }))
}

/// 分支一：存在 → 原生点击一次 → 下一轮探针消失 → 'until it disappeared'
///（点击链 = locate/hit/三段派发，非 TS 的页内 el.click()）
#[tokio::test]
async fn loadall_clicks_until_selector_disappears() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![
            rt_bool(true),
            located_ok(),
            eval_response(json!({ "hit": true })),
            rt_bool(false),
        ],
    );
    let resp = svc
        .loadall(INS, "T1", ".more", Some("1ms"), None)
        .await
        .unwrap();
    assert_eq!(
        resp["result"],
        json!("Clicked \".more\" 1 time(s) until it disappeared")
    );
    assert_eq!(
        calls_of(&calls, "Input.dispatchMouseEvent").len(),
        3,
        "每次存活轮次一次原生点击"
    );
}

/// 分支二：首轮探针即不存在 → 0 次点击、零派发
#[tokio::test]
async fn loadall_absent_initially_clicks_zero_times() {
    let (svc, _w, calls, script) = armed();
    given(&script, "Runtime.evaluate", vec![rt_bool(false)]);
    let resp = svc
        .loadall(INS, "T1", ".more", Some("1ms"), None)
        .await
        .unwrap();
    assert_eq!(
        resp["result"],
        json!("Clicked \".more\" 0 time(s) until it disappeared")
    );
    assert_eq!(calls_of(&calls, "Input.dispatchMouseEvent").len(), 0);
}

/// 分支三：达上限后复查仍在页面 → 诚实文案 'still present'（非 disappeared）
#[tokio::test]
async fn loadall_cap_reached_reports_still_present() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![
            rt_bool(true),
            located_ok(),
            eval_response(json!({ "hit": true })),
            rt_bool(true),
            located_ok(),
            eval_response(json!({ "hit": true })),
            rt_bool(true), // 达上限后的复查：仍在
        ],
    );
    let resp = svc
        .loadall(INS, "T1", ".more", Some("1ms"), Some(2))
        .await
        .unwrap();
    assert_eq!(
        resp["result"],
        json!("Clicked \".more\" 2 time(s) but it is still present (--max-clicks 2 reached)")
    );
    assert_eq!(calls_of(&calls, "Input.dispatchMouseEvent").len(), 6);
}

/// 达上限退出但复查已消失（末次点击后页面移除）→ 仍报 disappeared
#[tokio::test]
async fn loadall_cap_exit_rechecked_and_found_gone() {
    let (svc, _w, _calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![
            rt_bool(true),
            located_ok(),
            eval_response(json!({ "hit": true })),
            rt_bool(false), // 达上限后的复查：已消失
        ],
    );
    let resp = svc
        .loadall(INS, "T1", ".more", Some("1ms"), Some(1))
        .await
        .unwrap();
    assert_eq!(
        resp["result"],
        json!("Clicked \".more\" 1 time(s) until it disappeared")
    );
}

/// 入参门：空 selector / 非法 --interval → 400（不建连不点击）
#[tokio::test]
async fn loadall_rejects_bad_interval_and_empty_selector() {
    let (svc, _w, calls, _script) = armed();
    let e = svc
        .loadall(INS, "T1", ".more", Some("abc"), None)
        .await
        .unwrap_err();
    assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400));
    assert!(e.to_string().contains("--interval"));
    let e2 = svc.loadall(INS, "T1", "", None, None).await.unwrap_err();
    assert_eq!(e2.code(), "INVALID_REQUEST");
    assert_eq!(calls_of(&calls, "Input.dispatchMouseEvent").len(), 0);
}

// ---------------- 工具：navigate / back / forward / reload ---------------

/// URL 白名单在进串行锁/建连前拦截（400），与实例级 tabs/navigate 同源
///（domain validate_http_url）；非 http(s) scheme 零 CDP 发送。
#[tokio::test]
async fn navigate_rejects_non_http_url_before_connecting() {
    let (svc, _w, calls, _script) = armed();
    for bad in ["ftp://x", "javascript:alert(1)", "file:///etc/hosts", ""] {
        let e = svc.navigate(INS, "T1", bad).await.unwrap_err();
        assert_eq!(
            (e.code(), e.status()),
            ("INVALID_REQUEST", 400),
            "应拒绝: {bad}"
        );
    }
    assert_eq!(
        calls_of(&calls, "Page.navigate").len(),
        0,
        "校验失败不建连不发送"
    );
}

/// Page.navigate 响应携带 errorText（net::ERR_*）→ 500 PAGE_NAV_FAILED：
/// 导航未发生是失败，与 loaded:false（已开始、未证实）严格区分。
#[tokio::test]
async fn navigate_error_text_maps_to_page_nav_failed() {
    let (svc, _w, _calls, script) = armed();
    given(
        &script,
        "Page.navigate",
        vec![Ok(
            json!({ "frameId": "F1", "errorText": "net::ERR_NAME_NOT_RESOLVED" }),
        )],
    );
    let e = svc
        .navigate(INS, "T1", "https://no-such-host.invalid/")
        .await
        .unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_NAV_FAILED", 500));
    assert!(e.to_string().contains("net::ERR_NAME_NOT_RESOLVED"));
}

/// 同文档导航（sameDocumentNavigation / 无新 loader）→ 不等待 load 事件，
/// 立即 loaded:true（DOM 同步替换，无 load 可等——等下去必假 loaded:false）。
#[tokio::test]
async fn navigate_same_document_returns_without_waiting() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Page.navigate",
        vec![Ok(
            json!({ "frameId": "F1", "sameDocumentNavigation": true }),
        )],
    );
    // 无 load 事件注入：若实现误等 load，此 await 会挂到超时
    let resp = svc.navigate(INS, "T1", "https://a/#frag").await.unwrap();
    assert_eq!(resp["loaded"], json!(true));
    assert_eq!(resp["url"], json!("https://a/#frag"));
    assert!(resp["result"]
        .as_str()
        .unwrap()
        .contains("same-document navigation"));
    assert_eq!(
        calls_of(&calls, "Page.navigate")[0].session.as_deref(),
        Some("S1")
    );
}

/// 新文档导航：命令后等 Page.loadEventFired，事件经 watch_loop 同款分派链
///（handle_event）放行 → loaded:true。
#[tokio::test]
async fn navigate_waits_for_load_event() {
    let (svc, watched, calls, script) = armed();
    given(
        &script,
        "Page.navigate",
        vec![Ok(json!({ "frameId": "F1", "loaderId": "L1" }))],
    );
    let task = async {
        // Page.navigate 调用已记录 = 等待者已登记（登记先于发送）
        eventually("Page.navigate 已发出", || {
            calls_of(&calls, "Page.navigate").len() == 1
        })
        .await;
        handle_event(&watched, &load_event("S1")).await;
    };
    let (resp, ()) = tokio::join!(svc.navigate(INS, "T1", "https://example.com/"), task);
    let resp = resp.unwrap();
    assert_eq!(resp["loaded"], json!(true));
    assert!(resp["result"].as_str().unwrap().contains("load complete"));
    assert_eq!(resp["url"], json!("https://example.com/"));
    assert_eq!(resp["instanceId"], json!(INS));
    assert_eq!(resp["targetId"], json!("T1"));
}

/// 加载等待窗口耗尽 → loaded:false 诚实返回（导航已开始，非失败）。
/// 直调内部工具注入小窗口（服务层常量 25s，测试不可等）。
#[tokio::test]
async fn navigate_load_timeout_reports_loaded_false() {
    let (_svc, watched, _calls, script) = armed();
    given(
        &script,
        "Page.navigate",
        vec![Ok(json!({ "frameId": "F1", "loaderId": "L1" }))],
    );
    let resp = navigate_tool(
        &watched,
        "T1",
        "S1",
        "https://example.com/",
        Duration::from_millis(120),
    )
    .await
    .unwrap();
    assert_eq!(resp["loaded"], json!(false));
    assert!(resp["result"].as_str().unwrap().contains("not confirmed"));
}

/// back/forward 边界：无更早/更晚历史条目 → 409 PAGE_NAV_NO_HISTORY，
/// message 携带方向（back/forward 各自到头）。
#[tokio::test]
async fn back_forward_at_history_boundaries_report_409() {
    let (svc, _w, calls, script) = armed();
    // 单条历史（current=0）：back 与 forward 都在边界
    given(
        &script,
        "Page.getNavigationHistory",
        vec![hist(0, &[1]), hist(0, &[1])],
    );
    let back = svc.back(INS, "T1").await.unwrap_err();
    assert_eq!((back.code(), back.status()), ("PAGE_NAV_NO_HISTORY", 409));
    assert!(back.to_string().contains("back"), "方向入 message: {back}");
    let fwd = svc.forward(INS, "T1").await.unwrap_err();
    assert_eq!((fwd.code(), fwd.status()), ("PAGE_NAV_NO_HISTORY", 409));
    assert!(fwd.to_string().contains("forward"), "方向入 message: {fwd}");
    // 两条历史（current=0）：forward 越界同理（另一侧边界）
    let (svc2, _w2, _c2, script2) = armed();
    given(
        &script2,
        "Page.getNavigationHistory",
        vec![hist(1, &[1, 2])],
    );
    let fwd2 = svc2.forward(INS, "T1").await.unwrap_err();
    assert_eq!((fwd2.code(), fwd2.status()), ("PAGE_NAV_NO_HISTORY", 409));
    assert_eq!(calls_of(&calls, "Page.navigateToHistoryEntry").len(), 0);
}

/// 同文档历史条目（SPA pushState 产生）：不触发 load 事件，以历史索引
/// 提交 + readyState=complete 为完成信号 → 快路径立即 loaded:true，
/// 不等满窗口。
#[tokio::test]
async fn back_same_document_entry_commits_fast() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Page.getNavigationHistory",
        vec![hist(1, &[1, 2]), hist(0, &[1, 2])],
    );
    given(&script, "Page.navigateToHistoryEntry", vec![Ok(json!({}))]);
    given(
        &script,
        "Runtime.evaluate",
        vec![rt_result(json!("complete"))],
    );
    // 无 load 事件注入：若实现误等 load，此 await 会挂到超时
    let resp = svc.back(INS, "T1").await.unwrap();
    assert_eq!(resp["loaded"], json!(true));
    assert_eq!(resp["index"], json!(0));
    assert_eq!(resp["url"], json!("https://a/1"));
    let nav = calls_of(&calls, "Page.navigateToHistoryEntry");
    assert_eq!(
        (
            nav.len(),
            nav[0].param("entryId"),
            nav[0].session.as_deref()
        ),
        (1, json!(1), Some("S1"))
    );
    assert!(resp["result"].as_str().unwrap().contains("back to entry 0"));
    assert!(resp["result"].as_str().unwrap().contains("same-document"));
}

/// 新文档历史条目：提交确认后 readyState 仍在 loading → 转 load 事件
/// 等待，事件到达才 loaded:true。
#[tokio::test]
async fn back_full_document_load_waits_for_load_event() {
    let (svc, watched, calls, script) = armed();
    given(
        &script,
        "Page.getNavigationHistory",
        vec![hist(1, &[1, 2]), hist(0, &[1, 2])],
    );
    given(&script, "Page.navigateToHistoryEntry", vec![Ok(json!({}))]);
    given(
        &script,
        "Runtime.evaluate",
        vec![rt_result(json!("loading"))],
    );
    let probe = async {
        // readyState 探测已发出 = 提交已确认且等待者已登记（登记先于导航发送）
        eventually("readyState 探测已发出", || {
            calls_of(&calls, "Runtime.evaluate").len() == 1
        })
        .await;
        handle_event(&watched, &load_event("S1")).await;
    };
    let (resp, ()) = tokio::join!(svc.back(INS, "T1"), probe);
    let resp = resp.unwrap();
    assert_eq!(resp["loaded"], json!(true));
    assert!(resp["result"].as_str().unwrap().contains("load complete"));
}

/// 提交确认轮询窗口耗尽（currentIndex 一直未到目标）→ loaded:false，
/// result 明示 commit not confirmed（直调内部工具注入小窗口）。
#[tokio::test]
async fn back_commit_timeout_reports_loaded_false() {
    let (_svc, watched, _calls, script) = armed();
    given(&script, "Page.getNavigationHistory", vec![hist(1, &[1, 2])]);
    given(&script, "Page.navigateToHistoryEntry", vec![Ok(json!({}))]);
    // 后续 getNavigationHistory 轮询耗尽脚本队列 → Ok(Null)：无 currentIndex
    let resp = history_step_tool(&watched, "T1", "S1", -1, Duration::from_millis(150))
        .await
        .unwrap();
    assert_eq!(resp["loaded"], json!(false));
    assert!(resp["result"]
        .as_str()
        .unwrap()
        .contains("commit not confirmed"));
}

/// reload：等 load 事件 → loaded:true（params 携带 ignoreCache）；
/// 窗口耗尽 → loaded:false（直调内部工具注入小窗口）。
#[tokio::test]
async fn reload_waits_load_event_and_timeout_paths() {
    let (svc, watched, calls, script) = armed();
    given(&script, "Page.reload", vec![Ok(json!({}))]);
    let probe = async {
        eventually("Page.reload 已发出", || {
            calls_of(&calls, "Page.reload").len() == 1
        })
        .await;
        assert_eq!(
            calls_of(&calls, "Page.reload")[0].params,
            json!({ "ignoreCache": true })
        );
        handle_event(&watched, &load_event("S1")).await;
    };
    let (resp, ()) = tokio::join!(svc.reload(INS, "T1", true), probe);
    let resp = resp.unwrap();
    assert_eq!(resp["loaded"], json!(true));
    assert!(resp["result"].as_str().unwrap().contains("load complete"));

    let (_svc2, watched2, _c2, script2) = armed();
    given(&script2, "Page.reload", vec![Ok(json!({}))]);
    let resp2 = reload_tool(&watched2, "T1", "S1", false, Duration::from_millis(120))
        .await
        .unwrap();
    assert_eq!(resp2["loaded"], json!(false));
    assert!(resp2["result"].as_str().unwrap().contains("not confirmed"));
}

/// load 事件按 tab 定向放行：其他 tab 的 loadEventFired 不误触；
/// tab 销毁时等待者随 per-tab 状态一并清理。
#[tokio::test]
async fn load_event_resolves_waiter_by_tab_and_cleanup_on_drop() {
    let (_svc, watched, _calls, _script) = armed();
    let mut rx = register_load_waiter(&watched, "T1");
    // TAB2（session S2）的 load 事件不影响 T1 的等待者
    handle_event(&watched, &load_event("S2")).await;
    assert!(rx.try_recv().is_err(), "其他 tab 的事件不得放行");
    handle_event(&watched, &load_event("S1")).await;
    assert!(rx.try_recv().is_ok(), "本 tab 事件放行等待者");

    // tab 销毁 → 等待者登记随之清理（drop_tab）
    let _rx2 = register_load_waiter(&watched, "TAB2");
    handle_event(
        &watched,
        &json!({ "method": "Target.targetDestroyed", "params": { "targetId": "TAB2" } }),
    )
    .await;
    assert!(watched.load_waiters.lock().unwrap().get("TAB2").is_none());
}

// ---------------- 工具：screenshot（W-cap）-----------------------------

/// 无 target = 整视口：captureScreenshot 无 clip、png 格式、响应形状与
/// 诊断面 screenshot 一致（format/data + command 补的 instanceId/targetId）。
#[tokio::test]
async fn screenshot_viewport_captures_without_clip() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Page.captureScreenshot",
        vec![Ok(json!({ "data": "aGVsbG8=" }))],
    );
    let resp = svc.screenshot(INS, "T1", None, false).await.unwrap();
    assert_eq!(resp["format"], json!("png"));
    assert_eq!(resp["data"], json!("aGVsbG8="));
    assert_eq!(resp["instanceId"], json!(INS));
    assert_eq!(resp["targetId"], json!("T1"));
    let cap = calls_of(&calls, "Page.captureScreenshot");
    assert_eq!(cap.len(), 1);
    assert_eq!(cap[0].param("format"), json!("png"));
    assert!(cap[0].params.get("clip").is_none(), "无 target 不带 clip");
    assert_eq!(cap[0].session.as_deref(), Some("S1"));
}

/// ref 路径：scrollIntoView + quads（backendNodeId）→ 联合 bbox 外扩取整
/// → clip {scale:1}。两个跨行 quad 的并集必须完整进 clip（取最大 quad
/// 会截丢另一行——这是 clip 与点击质心的语义差异）。
#[tokio::test]
async fn screenshot_ref_clips_union_bbox_rounded_outward() {
    let (svc, watched, calls, script) = armed();
    watched
        .refs
        .lock()
        .unwrap()
        .entry("T1".into())
        .or_default()
        .replace(&json!([{ "ref": "e1", "backendNodeId": 101, "name": "OK" }]));
    given(
        &script,
        "DOM.getContentQuads",
        vec![Ok(json!({ "quads": [
                [10.2, 20.7, 110.2, 20.7, 110.2, 70.7, 10.2, 70.7],
                [10.2, 90.5, 60.2, 90.5, 60.2, 140.5, 10.2, 140.5]
            ] }))],
    );
    given(
        &script,
        "Page.captureScreenshot",
        vec![Ok(json!({ "data": "aGVsbG8=" }))],
    );
    let resp = svc.screenshot(INS, "T1", Some("@e1"), false).await.unwrap();
    assert_eq!(resp["data"], json!("aGVsbG8="));
    let scroll = calls_of(&calls, "DOM.scrollIntoViewIfNeeded");
    assert_eq!(scroll[0].param("backendNodeId"), json!(101));
    assert_eq!(
        calls_of(&calls, "DOM.getContentQuads")[0].param("backendNodeId"),
        json!(101)
    );
    let cap = calls_of(&calls, "Page.captureScreenshot");
    // 并集 bbox (10.2,20.7)→(110.2,140.5)，外扩取整 → (10,20) 101×121
    // （右/下界 ceil：ceil(110.2)=111 − floor(10.2)=10 → 101）
    assert_eq!(
        cap[0].params.get("clip"),
        Some(&json!({ "x": 10.0, "y": 20.0, "width": 101.0, "height": 121.0, "scale": 1 }))
    );
}

/// selector 路径：getDocument → querySelector（nodeId 链，与 fill 同源）；
/// 未命中（nodeId 0）→ 404 PAGE_ELEMENT_NOT_FOUND（与 click/fill 同码）。
#[tokio::test]
async fn screenshot_selector_uses_node_id_and_reports_not_found() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "DOM.getDocument",
        vec![Ok(json!({ "root": { "nodeId": 3 } }))],
    );
    given(
        &script,
        "DOM.querySelector",
        vec![Ok(json!({ "nodeId": 7 }))],
    );
    given(
        &script,
        "DOM.getContentQuads",
        vec![Ok(
            json!({ "quads": [[0.0, 0.0, 50.0, 0.0, 50.0, 50.0, 0.0, 50.0]] }),
        )],
    );
    given(
        &script,
        "Page.captureScreenshot",
        vec![Ok(json!({ "data": "aGVsbG8=" }))],
    );
    svc.screenshot(INS, "T1", Some("#hero"), false)
        .await
        .unwrap();
    let q = calls_of(&calls, "DOM.querySelector");
    assert_eq!(q[0].param("selector"), json!("#hero"));
    assert_eq!(q[0].param("nodeId"), json!(3));
    let scroll = calls_of(&calls, "DOM.scrollIntoViewIfNeeded");
    assert_eq!(
        scroll[0].param("nodeId"),
        json!(7),
        "selector 路径走 nodeId 句柄"
    );
    let cap = calls_of(&calls, "Page.captureScreenshot");
    assert_eq!(
        cap[0].params.get("clip"),
        Some(&json!({ "x": 0.0, "y": 0.0, "width": 50.0, "height": 50.0, "scale": 1 }))
    );

    // 未命中 → 404，且不截屏
    let (svc2, _w2, _c2, script2) = armed();
    given(
        &script2,
        "DOM.getDocument",
        vec![Ok(json!({ "root": { "nodeId": 3 } }))],
    );
    given(
        &script2,
        "DOM.querySelector",
        vec![Ok(json!({ "nodeId": 0 }))],
    );
    let e = svc2
        .screenshot(INS, "T1", Some(".missing"), false)
        .await
        .unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ELEMENT_NOT_FOUND", 404));
}

/// quads 退化 → resolveNode + getBoundingClientRect 回退（与点击链同源），
/// 回退也退化 → 409 PAGE_TARGET_MOVED（不发明新码）。
#[tokio::test]
async fn screenshot_falls_back_to_rect_and_reports_moved() {
    let (svc, watched, calls, script) = armed();
    watched
        .refs
        .lock()
        .unwrap()
        .entry("T1".into())
        .or_default()
        .replace(&json!([{ "ref": "e1", "backendNodeId": 9, "name": "OK" }]));
    given(
        &script,
        "DOM.getContentQuads",
        vec![Ok(json!({ "quads": [] }))],
    );
    given(
        &script,
        "DOM.resolveNode",
        vec![Ok(json!({ "object": { "objectId": "obj9" } }))],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![Ok(
            json!({ "result": { "value": { "x": 5.1, "y": 6.2, "w": 100.4, "h": 40.9 } } }),
        )],
    );
    given(
        &script,
        "Page.captureScreenshot",
        vec![Ok(json!({ "data": "aGVsbG8=" }))],
    );
    svc.screenshot(INS, "T1", Some("e1"), false).await.unwrap();
    let cap = calls_of(&calls, "Page.captureScreenshot");
    // 外扩取整：floor(5.1)=5 / ceil(105.5)=106 → 101；floor(6.2)=6 / ceil(47.1)=48 → 42
    assert_eq!(
        cap[0].params.get("clip"),
        Some(&json!({ "x": 5.0, "y": 6.0, "width": 101.0, "height": 42.0, "scale": 1 }))
    );

    // 回退 rect 也退化（零高）→ PAGE_TARGET_MOVED
    let (svc2, watched2, _c2, script2) = armed();
    watched2
        .refs
        .lock()
        .unwrap()
        .entry("T1".into())
        .or_default()
        .replace(&json!([{ "ref": "e1", "backendNodeId": 9, "name": "OK" }]));
    given(
        &script2,
        "DOM.getContentQuads",
        vec![Ok(json!({ "quads": [] }))],
    );
    given(
        &script2,
        "DOM.resolveNode",
        vec![Ok(json!({ "object": { "objectId": "obj9" } }))],
    );
    given(
        &script2,
        "Runtime.callFunctionOn",
        vec![Ok(
            json!({ "result": { "value": { "x": 5.0, "y": 6.0, "w": 100.0, "h": 0.0 } } }),
        )],
    );
    let e = svc2
        .screenshot(INS, "T1", Some("e1"), false)
        .await
        .unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_TARGET_MOVED", 409));
}

/// 响应缺 data 字段：显式 500 INTERNAL_ERROR，不装作成功（诊断面同款）。
#[tokio::test]
async fn screenshot_missing_data_is_explicit_internal_error() {
    let (svc, _w, _calls, script) = armed();
    given(&script, "Page.captureScreenshot", vec![Ok(json!({}))]);
    let e = svc.screenshot(INS, "T1", None, false).await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("INTERNAL_ERROR", 500));
    assert!(e.to_string().contains("data"));
}

/// 空字符串 target：与 click 的空 selector 同门（400），缺省视口应传 None。
#[tokio::test]
async fn screenshot_empty_target_is_rejected() {
    let (svc, _w, calls, _script) = armed();
    let e = svc
        .screenshot(INS, "T1", Some(""), false)
        .await
        .unwrap_err();
    assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400));
    assert_eq!(calls_of(&calls, "Page.captureScreenshot").len(), 0);
}

// ---------------- 工具：screenshot --full（W-shot）--------------------

/// full：先 Runtime.evaluate 读文档尺寸+DPR，再 clip 全文档 +
/// captureBeyondViewport=true + scale 1（DPR 换算留给 CDP 内部）；
/// 响应带尺寸证据字段（full/width/height/documentWidth/documentHeight/
/// truncated）——调用方靠它们核对 png 尺寸与截断，不靠猜。
#[tokio::test]
async fn screenshot_full_clips_whole_document_with_evidence_fields() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![rt_result(json!([1280, 2592, 2]))],
    );
    given(
        &script,
        "Page.captureScreenshot",
        vec![Ok(json!({ "data": "aGVsbG8=" }))],
    );
    let resp = svc.screenshot(INS, "T1", None, true).await.unwrap();
    assert_eq!(resp["data"], json!("aGVsbG8="));
    assert_eq!(
        (
            resp["full"].as_bool().unwrap(),
            resp["width"].as_i64().unwrap(),
            resp["height"].as_i64().unwrap(),
            resp["documentWidth"].as_i64().unwrap(),
            resp["documentHeight"].as_i64().unwrap(),
            resp["truncated"].as_bool().unwrap(),
        ),
        (true, 1280, 2592, 1280, 2592, false)
    );
    let cap = calls_of(&calls, "Page.captureScreenshot");
    assert_eq!(cap.len(), 1);
    assert_eq!(cap[0].param("captureBeyondViewport"), json!(true));
    assert_eq!(
        cap[0].params.get("clip"),
        Some(&json!({ "x": 0, "y": 0, "width": 1280, "height": 2592, "scale": 1 }))
    );
    assert_eq!(cap[0].param("format"), json!("png"));
    // 尺寸读数经 tab 的 page session（不是 browser 级）
    assert_eq!(cap[0].session.as_deref(), Some("S1"));
}

/// 超限截断（DPR 感知）：上限按设备像素 16384 换算 CSS（真机 spike：
/// 16384 device px 是 CDP 硬顶，超过即 -32000 拒绝）。DPR=2 → 上限 8192
/// CSS px：20000×9000.8 文档 → 双边截到 8192、truncated=true；
/// documentWidth/Height 保留原始读数（诚实证据：调用方能算出被截了多少）。
/// DPR=1 → 上限 16384：20000 宽截、9000 高不截，truncated 仍 true（任一边）。
#[tokio::test]
async fn screenshot_full_caps_each_side_by_dpr_and_marks_truncated() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![rt_result(json!([20000, 9000.8, 2]))],
    );
    given(
        &script,
        "Page.captureScreenshot",
        vec![
            Ok(json!({ "data": "aGVsbG8=" })),
            Ok(json!({ "data": "aGVsbG8=" })),
        ],
    );
    let resp = svc.screenshot(INS, "T1", None, true).await.unwrap();
    assert_eq!(
        (
            resp["width"].as_i64().unwrap(),
            resp["height"].as_i64().unwrap(),
            resp["documentWidth"].as_i64().unwrap(),
            resp["documentHeight"].as_i64().unwrap(),
            resp["truncated"].as_bool().unwrap(),
        ),
        (8192, 8192, 20000, 9000, true)
    );
    let cap = calls_of(&calls, "Page.captureScreenshot");
    assert_eq!(
        cap[0].params.get("clip"),
        Some(&json!({ "x": 0, "y": 0, "width": 8192, "height": 8192, "scale": 1 }))
    );

    // DPR=1：上限 16384；高 9000 未超限不截，宽 20000 截断
    let (svc2, _w2, _c2, script2) = armed();
    given(
        &script2,
        "Runtime.evaluate",
        vec![rt_result(json!([20000, 9000, 1]))],
    );
    given(
        &script2,
        "Page.captureScreenshot",
        vec![Ok(json!({ "data": "aGVsbG8=" }))],
    );
    let resp2 = svc2.screenshot(INS, "T1", None, true).await.unwrap();
    assert_eq!(
        (
            resp2["width"].as_i64().unwrap(),
            resp2["height"].as_i64().unwrap(),
            resp2["truncated"].as_bool().unwrap(),
        ),
        (16384, 9000, true)
    );
}

/// full + target 互斥：两个语义拼在一起只会掩盖调用方 bug，显式 400，
/// 且不发出任何 CDP 命令（校验在 service 入口，早于建连/尺寸读数）。
#[tokio::test]
async fn screenshot_full_and_target_are_mutually_exclusive() {
    let (svc, _w, calls, _script) = armed();
    let e = svc
        .screenshot(INS, "T1", Some("#hero"), true)
        .await
        .unwrap_err();
    assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400));
    assert!(e.to_string().contains("mutually exclusive"));
    assert_eq!(calls_of(&calls, "Page.captureScreenshot").len(), 0);
    assert_eq!(calls_of(&calls, "Runtime.evaluate").len(), 0);
}

// ---------------- 工具：pdf（W-cap）------------------------------------

/// 参数透传 + 缺省归一：landscape=false / printBackground=true /
/// 8.5×11；显式给值时原样透传；响应 format=pdf + data。
#[tokio::test]
async fn pdf_passes_params_and_applies_defaults() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Page.printToPDF",
        vec![
            Ok(json!({ "data": "JVBER" })),
            Ok(json!({ "data": "JVBER" })),
        ],
    );
    let resp = svc.pdf(INS, "T1", None, None, None, None).await.unwrap();
    assert_eq!(
        (
            resp["format"].as_str().unwrap(),
            resp["data"].as_str().unwrap()
        ),
        ("pdf", "JVBER")
    );
    let p = calls_of(&calls, "Page.printToPDF");
    assert_eq!(p[0].param("landscape"), json!(false));
    assert_eq!(p[0].param("printBackground"), json!(true));
    assert_eq!(p[0].param("paperWidth"), json!(8.5));
    assert_eq!(p[0].param("paperHeight"), json!(11.0));

    svc.pdf(INS, "T1", Some(true), Some(false), Some(8.27), Some(11.69))
        .await
        .unwrap();
    let p2 = &calls_of(&calls, "Page.printToPDF")[1];
    assert_eq!(p2.param("landscape"), json!(true));
    assert_eq!(p2.param("printBackground"), json!(false));
    assert_eq!(p2.param("paperWidth"), json!(8.27));
    assert_eq!(p2.param("paperHeight"), json!(11.69));
    // pdf 走 page session（printToPDF 的域前置）
    assert_eq!(p2.session.as_deref(), Some("S1"));
}

/// 非法纸张（0/负/NaN）→ 400，且不发 printToPDF。
#[tokio::test]
async fn pdf_rejects_non_positive_paper() {
    let (svc, _w, calls, _script) = armed();
    for (w, h) in [
        (Some(0.0), None),
        (None, Some(-1.0)),
        (Some(f64::NAN), None),
    ] {
        let e = svc.pdf(INS, "T1", None, None, w, h).await.unwrap_err();
        assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400));
    }
    assert_eq!(calls_of(&calls, "Page.printToPDF").len(), 0);
}

// ---------------- 工具：emulate（W-cap）-------------------------------

/// preset 形态：metrics + UA + touch 三命令成套（iphone-14 自带 mobile/touch
/// 与真实 UA），deviceScaleFactor 不覆盖（0 = 保持宿主缩放）。
#[tokio::test]
async fn emulate_preset_applies_metrics_ua_touch() {
    let (svc, _w, calls, _script) = armed();
    let resp = svc
        .emulate(
            INS,
            "T1",
            crate::domain::emulate::EmulateRequest {
                preset: Some("iphone-14".into()),
                viewport: None,
                user_agent: None,
                mobile: None,
                touch: None,
                off: false,
            },
        )
        .await
        .unwrap();
    assert!(resp["result"].as_str().unwrap().contains("iphone-14"));
    let m = calls_of(&calls, "Emulation.setDeviceMetricsOverride");
    assert_eq!(m.len(), 1);
    assert_eq!(
        m[0].params,
        json!({ "width": 390, "height": 844, "deviceScaleFactor": 0, "mobile": true })
    );
    let ua = calls_of(&calls, "Emulation.setUserAgentOverride");
    assert!(ua[0]
        .param("userAgent")
        .as_str()
        .unwrap()
        .contains("CriOS/"));
    let touch = calls_of(&calls, "Emulation.setTouchEmulationEnabled");
    assert_eq!(touch[0].param("enabled"), json!(true));
    for c in [m[0].clone(), ua[0].clone(), touch[0].clone()] {
        assert_eq!(
            c.session.as_deref(),
            Some("S1"),
            "仿真发 page session，非 browser 级"
        );
    }
}

/// 覆盖语义：第二次 viewport 形态未给 UA → setUserAgentOverride 空串恢复
/// 默认（CfT 131 无 clearUserAgentOverride，真机实测 -32601；未给 =
/// 恢复默认，非保持旧值）；touch 未给 → 关闭。
#[tokio::test]
async fn emulate_reapplies_overwrite_semantics() {
    let (svc, _w, calls, _script) = armed();
    let req = |w: i64, h: i64, ua: Option<&str>| crate::domain::emulate::EmulateRequest {
        preset: None,
        viewport: Some(crate::domain::emulate::Viewport {
            width: w,
            height: h,
        }),
        user_agent: ua.map(str::to_string),
        mobile: None,
        touch: None,
        off: false,
    };
    svc.emulate(INS, "T1", req(390, 844, Some("Bot/1.0")))
        .await
        .unwrap();
    svc.emulate(INS, "T1", req(200, 200, None)).await.unwrap();
    let metrics = calls_of(&calls, "Emulation.setDeviceMetricsOverride");
    assert_eq!(metrics.len(), 2);
    assert_eq!(metrics[1].param("width"), json!(200));
    let ua = calls_of(&calls, "Emulation.setUserAgentOverride");
    assert_eq!(
        ua.len(),
        2,
        "两次都发 setUserAgentOverride（第二次为空串恢复）"
    );
    assert_eq!(ua[0].param("userAgent"), json!("Bot/1.0"));
    assert_eq!(
        ua[1].param("userAgent"),
        json!(""),
        "第二次未给 UA 必须恢复默认（空串，非 clear 命令）"
    );
    assert_eq!(
        calls_of(&calls, "Emulation.clearUserAgentOverride").len(),
        0
    );
    let touch = calls_of(&calls, "Emulation.setTouchEmulationEnabled");
    assert_eq!(touch[1].param("enabled"), json!(false));
}

/// off 清全部三域：metrics / UA / touch。
#[tokio::test]
async fn emulate_off_clears_all_three() {
    let (svc, _w, calls, _script) = armed();
    svc.emulate(
        INS,
        "T1",
        crate::domain::emulate::EmulateRequest {
            preset: None,
            viewport: None,
            user_agent: None,
            mobile: None,
            touch: None,
            off: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        calls_of(&calls, "Emulation.clearDeviceMetricsOverride").len(),
        1
    );
    let ua = calls_of(&calls, "Emulation.setUserAgentOverride");
    assert_eq!(
        (ua.len(), ua[0].param("userAgent")),
        (1, json!("")),
        "UA 恢复 = 空串 setUserAgentOverride（CfT 131 无 clear 命令）"
    );
    assert_eq!(
        calls_of(&calls, "Emulation.clearUserAgentOverride").len(),
        0
    );
    let touch = calls_of(&calls, "Emulation.setTouchEmulationEnabled");
    assert_eq!((touch.len(), touch[0].param("enabled")), (1, json!(false)));
    assert_eq!(
        calls_of(&calls, "Emulation.setDeviceMetricsOverride").len(),
        0
    );
}

/// 未知预设 → 422 PAGE_EMULATE_UNKNOWN_PRESET，且**建连前**拒绝：
/// 不发任何 Emulation 命令（resolve 在 command 之前）。
#[tokio::test]
async fn emulate_unknown_preset_422_before_any_cdp() {
    let (svc, _w, calls, _script) = armed();
    let e = svc
        .emulate(
            INS,
            "T1",
            crate::domain::emulate::EmulateRequest {
                preset: Some("nokia-3310".into()),
                viewport: None,
                user_agent: None,
                mobile: None,
                touch: None,
                off: false,
            },
        )
        .await
        .unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_EMULATE_UNKNOWN_PRESET", 422));
    assert!(e.to_string().contains("iphone-14"), "message 附可用预设");
    let emulate_calls = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.method.starts_with("Emulation."))
        .count();
    assert_eq!(emulate_calls, 0);
}

// ---------------- W-oopif：帧登记 / snap 合并 / 投影 / 帧会话路由 ----------------

fn register_frame(
    watched: &Watched,
    tab: &str,
    root: &str,
    fid: &str,
    sid: Option<&str>,
    parent: &str,
    owner: Option<i64>,
) {
    let mut tables = watched.frames.lock().unwrap();
    let t = tables.entry(tab.to_string()).or_default();
    t.root = Some(root.to_string());
    t.frames.insert(
        fid.to_string(),
        FrameInfo {
            session_id: sid.map(str::to_string),
            parent_frame_id: Some(parent.to_string()),
            owner_backend_node_id: owner,
        },
    );
}

/// frameAttached → iframe attach 认领 → frameDetached 摘除 + 帧内 ref 失效；
/// attach 先于 frameAttached 的孤儿路径同样收敛
#[tokio::test]
async fn frame_registry_lifecycle_via_events() {
    let (svc, calls, _script, tap) = fake_service();
    let (watched, g) = svc.ensure(INS).await.unwrap();
    drop(g);
    // 预置 ref 表（帧条目 + 主条目），验证事件驱动的失效
    watched
        .refs
        .lock()
        .unwrap()
        .entry("TAB1".into())
        .or_default()
        .replace(&json!([
            { "ref": "e1", "backendNodeId": 101 },
            { "ref": "e2", "backendNodeId": 202, "frameTargetId": "FRM_A" },
        ]));

    let tx = tap.lock().unwrap().clone().unwrap();
    tx.send(attach_event("TAB1", "SES1", "page")).unwrap();
    eventually("TAB1 入表", || tab_session(&watched, "TAB1").is_some()).await;

    // 路径一：frameAttached 先到，attach 认领
    tx.send(json!({ "method": "Page.frameAttached", "sessionId": "SES1",
            "params": { "frameId": "FRM_A", "parentFrameId": "F_ROOT" } }))
        .unwrap();
    eventually("FRM_A 登记", || {
        watched
            .frames
            .lock()
            .unwrap()
            .get("TAB1")
            .map(|t| t.frames.contains_key("FRM_A"))
            .unwrap_or(false)
    })
    .await;
    tx.send(attach_event("FRM_A", "SES_FRM_A", "iframe"))
        .unwrap();
    eventually("FRM_A 会话认领", || {
        watched
            .frames
            .lock()
            .unwrap()
            .get("TAB1")
            .and_then(|t| t.frames.get("FRM_A"))
            .and_then(|i| i.session_id.clone())
            .as_deref()
            == Some("SES_FRM_A")
    })
    .await;

    // 路径二：孤儿 attach 先到（归属未知），frameAttached 后认领
    tx.send(attach_event("FRM_B", "SES_FRM_B", "iframe"))
        .unwrap();
    eventually("孤儿 enable 已发", || {
        calls_of(&calls, "Page.enable")
            .iter()
            .any(|r| r.session.as_deref() == Some("SES_FRM_B"))
    })
    .await;
    tx.send(json!({ "method": "Page.frameAttached", "sessionId": "SES1",
            "params": { "frameId": "FRM_B", "parentFrameId": "F_ROOT" } }))
        .unwrap();
    eventually("FRM_B 孤儿认领", || {
        watched
            .frames
            .lock()
            .unwrap()
            .get("TAB1")
            .and_then(|t| t.frames.get("FRM_B"))
            .and_then(|i| i.session_id.clone())
            .as_deref()
            == Some("SES_FRM_B")
    })
    .await;

    // frameDetached：登记摘除 + 帧内 ref 失效（主条目不动）
    tx.send(json!({ "method": "Page.frameDetached", "sessionId": "SES1",
            "params": { "frameId": "FRM_A", "reason": "replaced" } }))
        .unwrap();
    eventually("FRM_A 摘除", || {
        !watched
            .frames
            .lock()
            .unwrap()
            .get("TAB1")
            .map(|t| t.frames.contains_key("FRM_A"))
            .unwrap_or(false)
    })
    .await;
    let refs = watched.refs.lock().unwrap();
    let store = refs.get("TAB1").unwrap();
    assert!(
        store.resolve("e1").unwrap().stale_reason.is_none(),
        "主 frame 条目不受影响"
    );
    assert!(
        store.resolve("e2").unwrap().stale_reason.is_some(),
        "帧条目已随帧失效"
    );
}

/// frameNavigated：子帧 → 仅该帧条目 stale；根帧 → 整表失效 + 帧登记清空
/// （与 hover 的根导航清 sheets 逻辑共存）
#[tokio::test]
async fn frame_navigation_scopes_invalidation() {
    let (svc, _calls, _script, tap) = fake_service();
    let (watched, g) = svc.ensure(INS).await.unwrap();
    drop(g);
    watched
        .refs
        .lock()
        .unwrap()
        .entry("TAB1".into())
        .or_default()
        .replace(&json!([
            { "ref": "e1", "backendNodeId": 101 },
            { "ref": "e2", "backendNodeId": 202, "frameTargetId": "FRM_A" },
        ]));
    let tx = tap.lock().unwrap().clone().unwrap();
    tx.send(attach_event("TAB1", "SES1", "page")).unwrap();
    eventually("TAB1 入表", || tab_session(&watched, "TAB1").is_some()).await;
    tx.send(json!({ "method": "Page.frameAttached", "sessionId": "SES1",
            "params": { "frameId": "FRM_A", "parentFrameId": "F_ROOT" } }))
        .unwrap();
    eventually("FRM_A 登记", || {
        watched
            .frames
            .lock()
            .unwrap()
            .get("TAB1")
            .map(|t| t.frames.contains_key("FRM_A"))
            .unwrap_or(false)
    })
    .await;

    // 子帧导航：只失效 FRM_A 条目
    tx.send(json!({ "method": "Page.frameNavigated", "sessionId": "SES1",
            "params": { "frame": { "id": "FRM_A", "parentId": "F_ROOT", "url": "https://b/x2" } } })).unwrap();
    eventually("帧条目 stale", || {
        watched
            .refs
            .lock()
            .unwrap()
            .get("TAB1")
            .and_then(|s| s.resolve("e2"))
            .and_then(|e| e.stale_reason.clone())
            .is_some()
    })
    .await;
    {
        let refs = watched.refs.lock().unwrap();
        assert!(!refs.get("TAB1").unwrap().stale(), "整表 stale 不置位");
        assert!(refs
            .get("TAB1")
            .unwrap()
            .resolve("e1")
            .unwrap()
            .stale_reason
            .is_none());
    }

    // 根导航：整表失效 + 帧登记清空 + sheets 清空（hover 共存）
    watched
        .sheets
        .lock()
        .unwrap()
        .insert("TAB1".into(), Default::default());
    tx.send(
        json!({ "method": "Page.frameNavigated", "sessionId": "SES1",
            "params": { "frame": { "id": "F_ROOT", "url": "https://a/next" } } }),
    )
    .unwrap();
    eventually("整表 stale", || {
        watched
            .refs
            .lock()
            .unwrap()
            .get("TAB1")
            .map(|s| s.stale())
            .unwrap_or(false)
    })
    .await;
    eventually("帧登记清空", || {
        watched
            .frames
            .lock()
            .unwrap()
            .get("TAB1")
            .map(|t| t.frames.is_empty())
            .unwrap_or(true)
    })
    .await;
    assert!(
        watched.sheets.lock().unwrap().get("TAB1").is_none(),
        "根导航清 sheets（hover 共存）"
    );
}

/// 根导航重建帧表：root 必须在清表**之后**写入——旧序（先写后清）会让
/// root 随整表删除丢失（生产代码 root 恒 None：顶层 OOPIF 投影链 at_root
/// 永不命中、主帧发起的下载无 tab 归属）。真实事件序列驱动，不预设表。
#[tokio::test]
async fn root_navigation_rebuilds_frame_table_with_root_set() {
    let (svc, _calls, _script, tap) = fake_service();
    let (watched, g) = svc.ensure(INS).await.unwrap();
    drop(g);
    let tx = tap.lock().unwrap().clone().unwrap();
    tx.send(attach_event("TAB1", "SES1", "page")).unwrap();
    eventually("TAB1 入表", || tab_session(&watched, "TAB1").is_some()).await;

    // 旧表带着子帧与 root（模拟首次导航前的登记形态）
    register_frame(&watched, "TAB1", "F_OLD", "FRM_OLD", None, "F_OLD", None);

    // 连接存活期根导航：表重建，root = 新 fid
    tx.send(
        json!({ "method": "Page.frameNavigated", "sessionId": "SES1",
            "params": { "frame": { "id": "F_NEW", "url": "https://a/next" } } }),
    )
    .unwrap();
    eventually("root 写入且旧子帧清空", || {
        let tables = watched.frames.lock().unwrap();
        tables
            .get("TAB1")
            .map(|t| t.root.as_deref() == Some("F_NEW") && !t.frames.contains_key("FRM_OLD"))
            .unwrap_or(false)
    })
    .await;

    // 事件路径登记的顶层 OOPIF（parent = 主帧 id）：帧表为空但 root 已知
    tx.send(json!({ "method": "Page.frameAttached", "sessionId": "SES1",
            "params": { "frameId": "FRM_A", "parentFrameId": "F_NEW" } }))
        .unwrap();
    eventually("FRM_A 登记", || {
        watched
            .frames
            .lock()
            .unwrap()
            .get("TAB1")
            .map(|t| t.frames.contains_key("FRM_A"))
            .unwrap_or(false)
    })
    .await;

    // 主帧发起的下载归属：root == 主帧 frameId → tab 命中（旧序恒 None）
    assert_eq!(
        download_tab_of_frame(&watched, "F_NEW").as_deref(),
        Some("TAB1"),
        "root 存活 → 主帧下载可归属 tab"
    );
}

/// snap 合并：帧树渲染为挂接点下的 [frame] 段，@eN 连续编号，
/// RefStore 条目携带帧维度（主树条目无）；owner 经 DOM.getFrameOwner 解析
#[tokio::test]
async fn snap_merges_frame_subtree_with_continuous_refs() {
    let (svc, watched, _calls, script) = armed();
    register_frame(
        &watched,
        "T1",
        "F_ROOT",
        "FRM_A",
        Some("S_FRM_A"),
        "F_ROOT",
        None,
    );
    let main_tree = json!({ "nodes": [
        ax_node("m1", "WebArea", json!({ "childIds": ["m2", "m3"] })),
        ax_node("m2", "iframe", json!({ "parentId": "m1", "name": { "value": "跨域表单" }, "backendDOMNodeId": 501 })),
        ax_node("m3", "button", json!({ "parentId": "m1", "name": { "value": "OK" }, "backendDOMNodeId": 101 })),
    ]});
    let frame_tree = json!({ "nodes": [
        ax_node("f1", "WebArea", json!({ "childIds": ["f2", "f3"], "name": { "value": "frame doc" } })),
        ax_node("f2", "button", json!({ "parentId": "f1", "name": { "value": "提交" }, "backendDOMNodeId": 601 })),
        ax_node("f3", "textbox", json!({ "parentId": "f1", "name": { "value": "用户名" }, "backendDOMNodeId": 602 })),
    ]});
    // 调用序：主树（S1）先、帧树（S_FRM_A）后 —— FakeCdp 按队列序消耗
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(main_tree), Ok(frame_tree)],
    );
    given(
        &script,
        "DOM.getFrameOwner",
        vec![Ok(json!({ "backendNodeId": 501 }))],
    );

    let resp = svc.snap(INS, "T1").await.unwrap();
    let text = resp["text"].as_str().unwrap();
    assert!(
        text.contains("[iframe] 跨域表单"),
        "挂接点（owner 元素）行存在:\n{text}"
    );
    assert!(
        text.contains("[frame] frame doc"),
        "[frame] 段头行:\n{text}"
    );
    assert!(
        text.contains("@e1 [button] 提交"),
        "帧内按钮获连续编号:\n{text}"
    );
    assert!(
        text.contains("@e3 [button] OK"),
        "主树按钮编号顺延:\n{text}"
    );
    assert_eq!(resp["refs_count"], 3);

    let store = watched.refs.lock().unwrap();
    let st = store.get("T1").unwrap();
    let e1 = st.resolve("e1").unwrap();
    assert_eq!(e1.backend_node_id, 601);
    assert_eq!(e1.frame_target_id.as_deref(), Some("FRM_A"));
    assert_eq!(e1.frame_session_id.as_deref(), Some("S_FRM_A"));
    assert_eq!(
        st.resolve("e3").unwrap().frame_target_id,
        None,
        "主树条目无帧维度"
    );
    // owner 解析已缓存（后续 click 投影免查）
    drop(store);
    assert_eq!(
        watched
            .frames
            .lock()
            .unwrap()
            .get("T1")
            .unwrap()
            .frames
            .get("FRM_A")
            .unwrap()
            .owner_backend_node_id,
        Some(501)
    );
}

/// click 帧内 ref：元素 quad 在帧会话解析 → owner quad 在主会话解析 →
/// 投影坐标 → Input 三段派发发**根 target**（浏览器按命中测试路由进 OOPIF）
#[tokio::test]
async fn click_ref_in_frame_projects_and_dispatches_on_root() {
    let (svc, watched, _calls, script) = armed();
    register_frame(
        &watched,
        "T1",
        "F_ROOT",
        "FRM_A",
        Some("S_FRM_A"),
        "F_ROOT",
        Some(501),
    );
    watched.refs.lock().unwrap().entry("T1".into()).or_default().replace(&json!([
            { "ref": "e1", "backendNodeId": 601, "frameTargetId": "FRM_A", "frameSessionId": "S_FRM_A", "name": "提交" },
        ]));
    // 队列序：先帧内元素 quad（帧会话），后 owner quad（主会话）
    given(
        &script,
        "DOM.getContentQuads",
        vec![
            Ok(json!({ "quads": [[10.0, 20.0, 110.0, 20.0, 110.0, 60.0, 10.0, 60.0]] })),
            Ok(json!({ "quads": [[100.0, 200.0, 400.0, 200.0, 400.0, 350.0, 100.0, 350.0]] })),
        ],
    );

    let resp = svc.click(INS, "T1", "@e1").await.unwrap();
    assert_eq!(
        resp["result"],
        json!("Clicked @e1 \"提交\" at CSS (160, 240)"),
        "帧内 (60,40) 经 owner (100,200) 平移 = 根视口 (160,240)"
    );
    let quads = calls_of(&_calls, "DOM.getContentQuads");
    assert_eq!(quads.len(), 2);
    assert_eq!(
        quads[0].session.as_deref(),
        Some("S_FRM_A"),
        "元素 quad 在帧会话"
    );
    assert_eq!(
        quads[1].session.as_deref(),
        Some("S1"),
        "owner quad 在主会话"
    );
    let inputs = calls_of(&_calls, "Input.dispatchMouseEvent");
    assert_eq!(inputs.len(), 3, "三段鼠标事件");
    assert!(
        inputs.iter().all(|r| r.session.as_deref() == Some("S1")),
        "Input 派发全部在根 target"
    );
    assert_eq!(inputs[1].params["x"], 160.0);
    assert_eq!(inputs[1].params["y"], 240.0);
}

/// 事件路径登记的帧内点击投影（不用 register_frame 预设 root——它总把
/// root 填好，掩住了生产路径 root 丢失的缺陷）：(1) root 经 frameNavigated
/// 写入，frameAttached 登记 parent=主帧 id → pf == root 终止；(2) root
/// 未知（attach 到已加载页，连接后从未见 frameNavigated——SPA 动态注入
/// iframe 的常态）→ 兜底「parent 未登记在帧表 = 主文档直属」终止。
/// 两个形态都不得 409 owning iframe chain is not resolvable。
#[tokio::test]
async fn click_frame_ref_via_event_path_projects_without_preset_root() {
    let (svc, watched, _calls, script) = armed();
    // 形态 1：事件序建表（root 由根导航写入）
    handle_event(
        &watched,
        &json!({ "method": "Page.frameNavigated", "sessionId": "S1",
            "params": { "frame": { "id": "F_ROOT" } } }),
    )
    .await;
    handle_event(
        &watched,
        &json!({ "method": "Page.frameAttached", "sessionId": "S1",
            "params": { "frameId": "FRM_A", "parentFrameId": "F_ROOT" } }),
    )
    .await;
    // 预置两条 ref：e1 走形态 1（root 已知），e2 稍后走形态 2（root 未知兑底）
    watched.refs.lock().unwrap().entry("T1".into()).or_default().replace(&json!([
            { "ref": "e1", "backendNodeId": 601, "frameTargetId": "FRM_A", "frameSessionId": "S_FRM_A", "name": "帧内按钮" },
            { "ref": "e2", "backendNodeId": 602, "frameTargetId": "FRM_B", "frameSessionId": "S_FRM_B", "name": "动态帧按钮" },
        ]));
    // 几何链脚本（调用序）：形态 1 = 元素 quad(601@帧) → owner 解析(FRM_A@S1)
    // → owner quad(501@S1)；形态 2 同构（602 → 502）
    given(
        &script,
        "DOM.getContentQuads",
        vec![
            Ok(json!({ "quads": [[10.0, 20.0, 110.0, 20.0, 110.0, 60.0, 10.0, 60.0]] })),
            Ok(json!({ "quads": [[100.0, 200.0, 400.0, 200.0, 400.0, 350.0, 100.0, 350.0]] })),
            Ok(json!({ "quads": [[10.0, 20.0, 110.0, 20.0, 110.0, 60.0, 10.0, 60.0]] })),
            Ok(json!({ "quads": [[100.0, 200.0, 400.0, 200.0, 400.0, 350.0, 100.0, 350.0]] })),
        ],
    );
    given(
        &script,
        "DOM.getFrameOwner",
        vec![
            Ok(json!({ "backendNodeId": 501 })),
            Ok(json!({ "backendNodeId": 502 })),
        ],
    );

    let resp = svc.click(INS, "T1", "@e1").await.unwrap();
    assert_eq!(
        resp["result"],
        json!("Clicked @e1 \"帧内按钮\" at CSS (160, 240)"),
        "形态 1：parent == root（事件写入）→ 投影终止"
    );

    // 形态 2：root 未知——帧表整体重建为无 root 的纯事件登记
    watched.frames.lock().unwrap().remove("T1");
    handle_event(
        &watched,
        &json!({ "method": "Page.frameAttached", "sessionId": "S1",
            "params": { "frameId": "FRM_B", "parentFrameId": "F_ROOT_UNSEEN" } }),
    )
    .await;
    let resp2 = svc.click(INS, "T1", "@e2").await.unwrap();
    assert_eq!(
        resp2["result"],
        json!("Clicked @e2 \"动态帧按钮\" at CSS (160, 240)"),
        "形态 2：parent 未登记且 root 未知 → 主文档直属兑底，不 409"
    );
}

/// fill 帧内 ref：DOM/Runtime 命令（resolveNode/focus/callFunctionOn/
/// insertText/releaseObject）全部路由到 frame session
#[tokio::test]
async fn fill_ref_in_frame_runs_on_frame_session() {
    let (svc, watched, _calls, script) = armed();
    register_frame(
        &watched,
        "T1",
        "F_ROOT",
        "FRM_A",
        Some("S_FRM_A"),
        "F_ROOT",
        Some(501),
    );
    watched.refs.lock().unwrap().entry("T1".into()).or_default().replace(&json!([
            { "ref": "e1", "backendNodeId": 602, "frameTargetId": "FRM_A", "frameSessionId": "S_FRM_A", "name": "用户名" },
        ]));
    given(
        &script,
        "DOM.resolveNode",
        vec![Ok(json!({ "object": { "objectId": "objF" } }))],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![
            Ok(
                json!({ "result": { "value": { "tag": "INPUT", "kind": "native", "label": "用户名" } } }),
            ),
            Ok(json!({ "result": { "value": { "cleared": true } } })),
            Ok(json!({ "result": { "value": { "matches": true } } })),
        ],
    );

    let resp = svc.fill(INS, "T1", "@e1", "vincent").await.unwrap();
    assert_eq!(
        resp["result"],
        json!("Filled <INPUT> \"用户名\" with \"vincent\" (7 chars)")
    );
    let resolve = calls_of(&_calls, "DOM.resolveNode");
    assert_eq!(resolve.len(), 1);
    assert_eq!(
        resolve[0].session.as_deref(),
        Some("S_FRM_A"),
        "resolveNode 在帧会话"
    );
    for r in calls_of(&_calls, "Runtime.callFunctionOn") {
        assert_eq!(
            r.session.as_deref(),
            Some("S_FRM_A"),
            "callFunctionOn 在帧会话"
        );
    }
    let insert = calls_of(&_calls, "Input.insertText");
    assert_eq!(insert.len(), 1);
    assert_eq!(
        insert[0].session.as_deref(),
        Some("S_FRM_A"),
        "insertText 在帧会话"
    );
    assert_eq!(
        calls_of(&_calls, "Runtime.releaseObject")[0]
            .session
            .as_deref(),
        Some("S_FRM_A"),
        "对象归还到解析它的同一会话"
    );
}

/// 帧级失效后的 ref 使用 → 409 PAGE_REF_STALE（曾存在、已随帧死亡，
/// 语义区别于 NOT_FOUND）
#[tokio::test]
async fn stale_frame_ref_reports_ref_stale() {
    let (svc, watched, _calls, _script) = armed();
    {
        let mut refs = watched.refs.lock().unwrap();
        let store = refs.entry("T1".into()).or_default();
        store.replace(&json!([
            { "ref": "e1", "backendNodeId": 101 },
            { "ref": "e2", "backendNodeId": 601, "frameTargetId": "FRM_A" },
        ]));
        store.invalidate_frame("FRM_A", "frame navigation");
    }
    let err = svc.click(INS, "T1", "@e2").await.unwrap_err();
    assert_eq!((err.status(), err.code()), (409, "PAGE_REF_STALE"));
    assert!(
        err.to_string().contains("frame navigation"),
        "message 携带失效原因"
    );
    // 主 frame ref 不受影响（该路径正常走几何链，此处只需确认不报 stale）
    let err2 = svc.click(INS, "T1", "@e99").await.unwrap_err();
    assert_eq!(err2.code(), "PAGE_REF_NOT_FOUND", "未知 ref 仍 404");
}

/// sync 帧树主动路径（真机打磨核心）：未注入登记时，snap 内联完成
/// getTargets 发现 → getFrameOwner 归属（owner 校验：主树 bni 必须是
/// iframe 类节点，防死帧/异页污染）→ attachToTarget 认领会话 → 帧树
/// 拉取合并。死帧（owner 命中非 iframe 节点）拒绝登记。
#[tokio::test]
async fn snap_syncs_frame_registry_via_target_probe() {
    let (svc, watched, _calls, script) = armed();
    // 无任何帧登记（同步路径全自驱）；root 由 sync 后首次使用仍 None——
    // 投影链在 root 未知时按"parent None/主会话"收敛，功能不受阻
    let main_tree = json!({ "nodes": [
        ax_node("m1", "WebArea", json!({ "childIds": ["m2", "m3"] })),
        ax_node("m2", "iframe", json!({ "parentId": "m1", "name": { "value": "跨域" }, "backendDOMNodeId": 10 })),
        ax_node("m3", "button", json!({ "parentId": "m1", "name": { "value": "OK" }, "backendDOMNodeId": 9 })),
    ]});
    let frame_tree = json!({ "nodes": [
        ax_node("f1", "WebArea", json!({ "childIds": ["f2"], "name": { "value": "fdoc" } })),
        ax_node("f2", "button", json!({ "parentId": "f1", "name": { "value": "帧按钮" }, "backendDOMNodeId": 600 })),
    ]});
    given(
        &script,
        "Target.getTargets",
        vec![Ok(json!({ "targetInfos": [
            { "targetId": "PAGE_OTHER", "type": "page", "url": "https://other/" },
            { "targetId": "FRM_SYNC", "type": "iframe", "url": "https://frame.example/" },
            { "targetId": "FRM_DEAD", "type": "iframe", "url": "https://dead.example/" },
        ] }))],
    );
    // 归属探测：FRM_SYNC 命中主会话返回 bni=10（主树 Iframe 节点）；
    // FRM_DEAD 返回 bni=9（主树 button——死帧污染形态，必须被校验拒绝）
    given(
        &script,
        "DOM.getFrameOwner",
        vec![
            Ok(json!({ "backendNodeId": 10 })),
            Ok(json!({ "backendNodeId": 9 })),
        ],
    );
    given(
        &script,
        "Target.attachToTarget",
        vec![Ok(json!({ "sessionId": "S_FRM_SYNC" }))],
    );
    // AX 拉取序：主树（S1）→ FRM_SYNC 帧（S_FRM_SYNC）
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(main_tree), Ok(frame_tree)],
    );

    let resp = svc.snap(INS, "T1").await.unwrap();
    assert_eq!(resp["refs_count"], 2, "主按钮 + 帧按钮");
    let text = resp["text"].as_str().unwrap();
    assert!(text.contains("[frame] fdoc"), "{text}");
    // 主树序：m2(iframe) 先于 m3(button)——挂接处帧内容先编号（前序 DFS）
    assert!(text.contains("@e1 [button] 帧按钮"), "{text}");
    assert!(text.contains("@e2 [button] OK"), "{text}");

    // 登记：FRM_SYNC 会话/owner 就位；FRM_DEAD 未登记（校验拒绝）
    let tables = watched.frames.lock().unwrap();
    let table = tables.get("T1").unwrap();
    let synced = table.frames.get("FRM_SYNC").unwrap();
    assert_eq!(synced.session_id.as_deref(), Some("S_FRM_SYNC"));
    assert_eq!(synced.owner_backend_node_id, Some(10));
    assert!(!table.frames.contains_key("FRM_DEAD"), "死帧拒绝登记");
    drop(tables);
    // RefStore 条目带帧维度
    let refs = watched.refs.lock().unwrap();
    let e1 = refs.get("T1").unwrap().resolve("e1").unwrap();
    assert_eq!(e1.frame_target_id.as_deref(), Some("FRM_SYNC"));
    assert_eq!(e1.frame_session_id.as_deref(), Some("S_FRM_SYNC"));
    assert!(refs
        .get("T1")
        .unwrap()
        .resolve("e2")
        .unwrap()
        .frame_target_id
        .is_none());
}

/// nodeId 碰撞回归（owner 复测缺陷）：主树与帧树的 nodeId 是两套独立
/// 计数（都从 1 起）——裸键共享 visited 时帧节点被误判重复剪掉，
/// 帧内容**稳定**整体缺席（id 全碰撞形态）。修复 = 帧树 visited 键加
/// 帧前缀隔离。此前 armed 测试用 m*/f* 前缀 id 恰好不碰撞，未拦住。
#[tokio::test]
async fn snap_renders_frame_tree_despite_node_id_collision() {
    let (svc, watched, _calls, script) = armed();
    register_frame(
        &watched,
        "T1",
        "F_ROOT",
        "FRM_A",
        Some("S_FRM_A"),
        "F_ROOT",
        Some(9),
    );
    // 主树与帧树 nodeId **完全同名空间**（1/2/3 对 1/2/3）
    let main_tree = json!({ "nodes": [
        { "nodeId": 1, "role": { "value": "WebArea" }, "childIds": [2, 3] },
        { "nodeId": 2, "parentId": 1, "role": { "value": "button" }, "name": { "value": "main" }, "backendDOMNodeId": 8 },
        { "nodeId": 3, "parentId": 1, "role": { "value": "Iframe" }, "backendDOMNodeId": 9 },
    ]});
    let frame_tree = json!({ "nodes": [
        { "nodeId": 1, "role": { "value": "WebArea" }, "childIds": [2, 3], "name": { "value": "fdoc" } },
        { "nodeId": 2, "parentId": 1, "role": { "value": "textbox" }, "name": { "value": "uname" }, "backendDOMNodeId": 60 },
        { "nodeId": 3, "parentId": 1, "role": { "value": "button" }, "name": { "value": "fbtn" }, "backendDOMNodeId": 61 },
    ]});
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(main_tree), Ok(frame_tree)],
    );

    let resp = svc.snap(INS, "T1").await.unwrap();
    assert_eq!(
        resp["refs_count"], 3,
        "主按钮 + 帧输入框 + 帧按钮——碰撞不再吞帧"
    );
    let text = resp["text"].as_str().unwrap();
    assert!(text.contains("@e2 [textbox] uname"), "{text}");
    assert!(text.contains("@e3 [button] fbtn"), "{text}");
    let refs = watched.refs.lock().unwrap();
    assert_eq!(
        refs.get("T1")
            .unwrap()
            .resolve("e2")
            .unwrap()
            .frame_target_id
            .as_deref(),
        Some("FRM_A")
    );
}

// ---------------- W-file：下载等待 / 记录环 / 上传 ----------------------

/// 临时目录里的真实文件（上传校验用；目录唯一防并发测试互踩）。
/// 返回 canonicalize 物理路径——macOS 的 temp 在 /var 软链下，upload 的
/// 拒软链校验按契约拒别名形态（真实调用方改传物理路径，同测试）
fn temp_upload_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cem-upload-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    std::fs::canonicalize(&p).unwrap()
}

/// armed() 绕过 establish，手动设下载目录
fn set_download_dir(watched: &Watched, dir: &std::path::Path) {
    watched.downloads.lock().unwrap().dir = dir.to_path_buf();
}

/// 新临时下载目录（canonicalize 物理路径，理由同 temp_upload_file）
fn temp_download_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cem-dl-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn will_begin(guid: &str, name: &str, frame_id: &str) -> Value {
    json!({ "method": "Browser.downloadWillBegin", "params": {
            "guid": guid, "suggestedFilename": name,
            "url": "https://dl.example/f?token=sekret", "frameId": frame_id } })
}

fn progress(guid: &str, state: &str, received: u64) -> Value {
    json!({ "method": "Browser.downloadProgress", "params": {
            "guid": guid, "state": state, "receivedBytes": received, "totalBytes": received } })
}

/// establish 与域启用集同点位下发 Browser.setDownloadBehavior（browser 级，
/// 目录 = profile/downloads）——连接重建时随 establish 重发
#[tokio::test]
async fn establish_sends_download_behavior_pointing_at_profile_downloads() {
    let (svc, calls, _script, _tap) = fake_service();
    let (_w, g) = svc.ensure(INS).await.unwrap();
    drop(g);
    let set = calls_of(&calls, "Browser.setDownloadBehavior");
    assert_eq!(set.len(), 1);
    assert_eq!(set[0].session, None, "browser 级（无 session）");
    assert_eq!(set[0].param("behavior"), "allow");
    assert_eq!(set[0].param("eventsEnabled"), true);
    assert_eq!(set[0].param("downloadPath"), "/tmp/x/downloads");
}

/// 主路径：先注册等待者，WillBegin + Progress(completed) 到达 → 返回记录
///（bytes 取落盘实测值；url 脱敏；记录环同步留痕）
#[tokio::test]
async fn download_wait_returns_first_completed_record() {
    let (svc, watched, _calls, _script) = armed();
    let svc = std::sync::Arc::new(svc);
    let dir = temp_download_dir();
    std::fs::write(dir.join("hello.txt"), vec![7u8; 1024]).unwrap();
    set_download_dir(&watched, &dir);

    let task = tokio::spawn({
        let svc = std::sync::Arc::clone(&svc);
        async move { svc.download_wait(INS, "T1", Some(3000)).await }
    });
    eventually("等待者登记", || {
        !watched.downloads.lock().unwrap().waiters.is_empty()
    })
    .await;
    handle_event(&watched, &will_begin("g1", "hello.txt", "F_UNKN")).await;
    handle_event(&watched, &progress("g1", "completed", 1024)).await;

    let resp = task.await.unwrap().unwrap();
    assert_eq!(resp["fileName"], "hello.txt");
    assert_eq!(
        resp["filePath"],
        dir.join("hello.txt").display().to_string()
    );
    assert_eq!(resp["bytes"], 1024, "落盘实测大小");
    assert_eq!(resp["verified"], true);
    assert_eq!(resp["matched"], true);
    assert_eq!(resp["instanceId"], INS);
    assert!(
        !resp["url"].as_str().unwrap().contains("sekret"),
        "url 脱敏"
    );
    // 记录环同步留痕（审计面含已消费条目，matched 区分）
    let ring = svc.downloads_list(INS, "T1").await.unwrap();
    assert_eq!(ring["downloads"].as_array().unwrap().len(), 1);
}

/// 诚实证据：canceled/interrupted 如实报 PAGE_DOWNLOAD_FAILED（区别于超时）；
/// 记录环留痕失败终态
#[tokio::test]
async fn download_wait_reports_failure_terminal_states() {
    let (svc, watched, _c, _s) = armed();
    let svc = std::sync::Arc::new(svc);
    set_download_dir(&watched, &temp_download_dir());

    for (guid, name, state) in [("g1", "a.zip", "canceled"), ("g2", "b.zip", "interrupted")] {
        let task = tokio::spawn({
            let svc = std::sync::Arc::clone(&svc);
            async move { svc.download_wait(INS, "T1", Some(3000)).await }
        });
        eventually("等待者登记", || {
            !watched.downloads.lock().unwrap().waiters.is_empty()
        })
        .await;
        handle_event(&watched, &will_begin(guid, name, "")).await;
        handle_event(&watched, &progress(guid, state, 10)).await;
        let err = task.await.unwrap().unwrap_err();
        assert_eq!(
            (err.code(), err.status()),
            ("PAGE_DOWNLOAD_FAILED", 500),
            "{state}"
        );
    }
    let ring = svc.downloads_list(INS, "T1").await.unwrap();
    let list = ring["downloads"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["state"], "canceled");
    assert_eq!(list[1]["state"], "interrupted");
    assert_eq!(
        list[0]["matched"], true,
        "失败也被等待者消费（如实报错而非挂起）"
    );
}

/// 超时 = 窗口内无终态（不是失败断言）；有 in_flight 时披露「进行中未完成」
#[tokio::test]
async fn download_wait_timeout_means_no_terminal_state() {
    let (svc, watched, _c, _s) = armed();
    set_download_dir(&watched, &temp_download_dir());

    let err = svc.download_wait(INS, "T1", Some(60)).await.unwrap_err();
    assert_eq!((err.code(), err.status()), ("PAGE_DOWNLOAD_TIMEOUT", 409));
    assert!(
        !err.to_string().contains("仍在进行"),
        "无 in_flight 时不说「进行中」"
    );

    // WillBegin 已到但未完成：超时披露进行中（区分「下载了但未完成」）
    handle_event(&watched, &will_begin("g9", "slow.bin", "")).await;
    let err2 = svc.download_wait(INS, "T1", Some(60)).await.unwrap_err();
    assert_eq!(err2.code(), "PAGE_DOWNLOAD_TIMEOUT");
    assert!(err2.to_string().contains("仍在进行"));
}

/// 记录环容量 20：最旧滚出；GET 面返回存活的 20 条
#[tokio::test]
async fn download_history_ring_keeps_last_20() {
    let (svc, watched, _c, _s) = armed();
    set_download_dir(&watched, &temp_download_dir());
    for i in 0..22 {
        let guid = format!("g{i}");
        handle_event(&watched, &will_begin(&guid, &format!("f{i}.bin"), "")).await;
        handle_event(&watched, &progress(&guid, "completed", i)).await;
    }
    let ring = svc.downloads_list(INS, "T1").await.unwrap();
    let list = ring["downloads"].as_array().unwrap();
    assert_eq!(list.len(), 20);
    assert_eq!(list[0]["fileName"], "f2.bin", "最旧的滚出");
    assert_eq!(list[19]["fileName"], "f21.bin");
    assert_eq!(list[0]["matched"], false, "无等待者消费");
    assert_eq!(
        list[0]["verified"], false,
        "文件未真实落盘（FakeCdp）：bytes 用事件值"
    );
    assert_eq!(list[0]["bytes"], 2);
}

/// 安全边界：suggestedFilename 含路径形态（../）→ 拼路径拒绝，等待者
/// 收 PAGE_DOWNLOAD_FAILED；记录 note 披露拒绝原因
#[tokio::test]
async fn download_with_path_separators_in_name_is_rejected() {
    let (svc, watched, _c, _s) = armed();
    let svc = std::sync::Arc::new(svc);
    set_download_dir(&watched, &temp_download_dir());
    let task = tokio::spawn({
        let svc = std::sync::Arc::clone(&svc);
        async move { svc.download_wait(INS, "T1", Some(3000)).await }
    });
    eventually("等待者登记", || {
        !watched.downloads.lock().unwrap().waiters.is_empty()
    })
    .await;
    handle_event(&watched, &will_begin("g1", "../../evil.sh", "")).await;
    handle_event(&watched, &progress("g1", "completed", 5)).await;
    let err = task.await.unwrap().unwrap_err();
    assert_eq!((err.code(), err.status()), ("PAGE_DOWNLOAD_FAILED", 500));
    let ring = svc.downloads_list(INS, "T1").await.unwrap();
    let rec = &ring["downloads"][0];
    assert_eq!(rec["filePath"], serde_json::Value::Null, "不拼路径");
    assert!(rec["note"].as_str().unwrap().contains("拒绝"));
}

/// 并发等待：终态先到先得（最早注册的等待者拿走）；未拿到的等自己超时
#[tokio::test]
async fn concurrent_download_waits_resolve_first_terminal_only() {
    let (svc, watched, _c, _s) = armed();
    let svc = std::sync::Arc::new(svc);
    set_download_dir(&watched, &temp_download_dir());
    let t1 = tokio::spawn({
        let svc = std::sync::Arc::clone(&svc);
        async move { svc.download_wait(INS, "T1", Some(3000)).await }
    });
    eventually("首个等待者登记", || {
        watched.downloads.lock().unwrap().waiters.len() == 1
    })
    .await;
    let t2 = tokio::spawn({
        let svc = std::sync::Arc::clone(&svc);
        async move { svc.download_wait(INS, "T1", Some(80)).await }
    });
    eventually("第二个等待者登记", || {
        watched.downloads.lock().unwrap().waiters.len() == 2
    })
    .await;
    handle_event(&watched, &will_begin("g1", "a.bin", "")).await;
    handle_event(&watched, &progress("g1", "completed", 1)).await;
    let r1 = t1.await.unwrap().unwrap();
    assert_eq!(r1["fileName"], "a.bin");
    let err = t2.await.unwrap().unwrap_err();
    assert_eq!(
        err.code(),
        "PAGE_DOWNLOAD_TIMEOUT",
        "未匹配的等待者各自超时"
    );
}

/// 红线：download-wait 等待期**不占实例串行锁**——等待挂起时另一命令
///（downloads_list，command 骨架要拿串行锁）必须立即完成。若回归为
/// 持锁等待（典型用法 wait→click 会死锁到超时），本测试失败。
#[tokio::test]
async fn download_wait_does_not_block_other_commands_while_waiting() {
    let (svc, watched, _c, _s) = armed();
    let svc = std::sync::Arc::new(svc);
    set_download_dir(&watched, &temp_download_dir());
    let task = tokio::spawn({
        let svc = std::sync::Arc::clone(&svc);
        async move { svc.download_wait(INS, "T1", Some(2000)).await }
    });
    eventually("等待者登记", || {
        !watched.downloads.lock().unwrap().waiters.is_empty()
    })
    .await;
    let t0 = Instant::now();
    let ring = svc
        .downloads_list(INS, "T1")
        .await
        .expect("等待期间另一命令不应被阻塞");
    assert_eq!(ring["capacity"], 20);
    assert!(
        t0.elapsed() < Duration::from_millis(1000),
        "downloads_list 耗时 {:?}——被挂起的 download-wait 阻塞了",
        t0.elapsed()
    );
    // 收尾：完成下载，避免任务悬挂
    handle_event(&watched, &will_begin("g1", "x.bin", "")).await;
    handle_event(&watched, &progress("g1", "completed", 1)).await;
    task.await.unwrap().unwrap();
}

/// 等待期间连接重建（establish 整体重置下载态）→ 等待者诚实收
/// PAGE_SERVICE_UNAVAILABLE（可重试），而非挂到自己的超时
#[tokio::test]
async fn connection_rebuild_during_wait_fails_waiter_honestly() {
    let (svc, calls, _script, tap) = fake_service();
    let svc = std::sync::Arc::new(svc);
    let (_w, g) = svc.ensure(INS).await.unwrap();
    drop(g);
    // 建一个 page tab（空 tab 路由会在等待者登记前就 404）
    let watched = svc.watched.lock().unwrap().get(INS).unwrap().clone();
    let tx = tap.lock().unwrap().clone().unwrap();
    tx.send(attach_event("TAB_R", "SES_R", "page")).unwrap();
    eventually("TAB_R 入表", || tab_session(&watched, "TAB_R").is_some()).await;
    let task = tokio::spawn({
        let svc = std::sync::Arc::clone(&svc);
        async move { svc.download_wait(INS, "TAB_R", Some(3000)).await }
    });
    eventually("等待者登记", || {
        !watched.downloads.lock().unwrap().waiters.is_empty()
    })
    .await;
    // 模拟断连：清连接槽 → 下一条命令触发重建（establish 清空等待登记）
    *watched.conn.lock().unwrap() = None;
    let _ = svc.downloads_list(INS, "TAB_R").await; // 错误无所谓，重建已发生
    let err = task.await.unwrap().unwrap_err();
    assert_eq!(
        (err.code(), err.status()),
        ("PAGE_SERVICE_UNAVAILABLE", 409)
    );
    assert!(err.to_string().contains("download-wait"));
    // 重建即重发 setDownloadBehavior
    assert_eq!(calls_of(&calls, "Browser.setDownloadBehavior").len(), 2);
}

/// tab 归属：WillBegin 的 frameId 命中帧登记表（root/子帧）→ 记录带 tab
#[tokio::test]
async fn download_records_tab_attribution_from_frame_registry() {
    let (svc, watched, _c, _s) = armed();
    set_download_dir(&watched, &temp_download_dir());
    register_frame(
        &watched,
        "T1",
        "F_ROOT",
        "FRM_A",
        Some("S_FRM_A"),
        "F_ROOT",
        Some(9),
    );
    handle_event(&watched, &will_begin("g1", "x.bin", "F_ROOT")).await;
    handle_event(&watched, &progress("g1", "completed", 1)).await;
    let ring = svc.downloads_list(INS, "T1").await.unwrap();
    assert_eq!(ring["downloads"][0]["tab"], "T1");
}

/// upload 主路径（selector）：定位链 → describeNode 校验 → setFileInputFiles
///（nodeId + 主会话；files = 主机绝对路径原样——不拷贝）
#[tokio::test]
async fn upload_selector_path_validates_and_sets_files() {
    let (svc, _w, calls, script) = armed();
    let f = temp_upload_file("a.png", b"abc");
    given(
        &script,
        "DOM.getDocument",
        vec![Ok(json!({ "root": { "nodeId": 1 } }))],
    );
    given(
        &script,
        "DOM.querySelector",
        vec![Ok(json!({ "nodeId": 5 }))],
    );
    given(
        &script,
        "DOM.describeNode",
        vec![Ok(
            json!({ "node": { "nodeName": "INPUT", "attributes": ["type", "file", "accept", ".png"] } }),
        )],
    );
    let resp = svc
        .upload(INS, "T1", "input[type=file]", &f.display().to_string())
        .await
        .unwrap();
    assert_eq!(resp["fileName"], "a.png");
    assert_eq!(resp["bytes"], 3);
    assert!(resp["result"].as_str().unwrap().contains("a.png"));
    let set = calls_of(&calls, "DOM.setFileInputFiles");
    assert_eq!(set.len(), 1);
    assert_eq!(set[0].param("files"), json!([f.display().to_string()]));
    assert_eq!(set[0].param("nodeId"), 5);
    assert_eq!(set[0].session.as_deref(), Some("S1"));
}

/// 校验红线：非 file input（input[type=text] / BUTTON）→ 422
/// PAGE_TARGET_NOT_FILE_INPUT，且**不发送** setFileInputFiles
#[tokio::test]
async fn upload_rejects_non_file_input_targets() {
    let (svc, _w, calls, script) = armed();
    let f = temp_upload_file("a.png", b"abc");
    given(
        &script,
        "DOM.getDocument",
        vec![
            Ok(json!({ "root": { "nodeId": 1 } })),
            Ok(json!({ "root": { "nodeId": 1 } })),
        ],
    );
    given(
        &script,
        "DOM.querySelector",
        vec![Ok(json!({ "nodeId": 5 })), Ok(json!({ "nodeId": 5 }))],
    );
    given(
        &script,
        "DOM.describeNode",
        vec![
            Ok(json!({ "node": { "nodeName": "INPUT", "attributes": ["type", "text"] } })),
            Ok(json!({ "node": { "nodeName": "BUTTON" } })),
        ],
    );
    for target in ["#q", "#b"] {
        let err = svc
            .upload(INS, "T1", target, &f.display().to_string())
            .await
            .unwrap_err();
        assert_eq!(
            (err.code(), err.status()),
            ("PAGE_TARGET_NOT_FILE_INPUT", 422),
            "{target}"
        );
    }
    assert!(
        calls_of(&calls, "DOM.setFileInputFiles").is_empty(),
        "校验失败不发送"
    );
}

/// ref 路径：@eN → backendNodeId 直用（免前端化），帧内 ref 路由帧会话
#[tokio::test]
async fn upload_ref_path_uses_backend_node_id() {
    let (svc, watched, calls, script) = armed();
    let f = temp_upload_file("b.bin", b"12345");
    watched
        .refs
        .lock()
        .unwrap()
        .entry("T1".into())
        .or_default()
        .replace(&json!([{ "ref": "e1", "backendNodeId": 42 }]));
    given(
        &script,
        "DOM.describeNode",
        vec![Ok(
            json!({ "node": { "nodeName": "INPUT", "attributes": ["type", "file"] } }),
        )],
    );
    let resp = svc
        .upload(INS, "T1", "@e1", &f.display().to_string())
        .await
        .unwrap();
    assert_eq!(resp["bytes"], 5);
    let set = calls_of(&calls, "DOM.setFileInputFiles");
    assert_eq!(set.len(), 1);
    assert_eq!(
        set[0].param("backendNodeId"),
        42,
        "ref 路径直用 backendNodeId"
    );
    assert!(set[0].params.get("nodeId").is_none(), "ref 路径不带 nodeId");
    assert_eq!(set[0].session.as_deref(), Some("S1"), "主文档 ref 走主会话");
}

/// selector 未命中 → 404 PAGE_ELEMENT_NOT_FOUND（同 fill 定位链语义）
#[tokio::test]
async fn upload_selector_miss_is_element_not_found() {
    let (svc, _w, _calls, script) = armed();
    let f = temp_upload_file("a.png", b"abc");
    given(
        &script,
        "DOM.getDocument",
        vec![Ok(json!({ "root": { "nodeId": 1 } }))],
    );
    given(
        &script,
        "DOM.querySelector",
        vec![Ok(json!({ "nodeId": 0 }))],
    );
    let err = svc
        .upload(INS, "T1", "#none", &f.display().to_string())
        .await
        .unwrap_err();
    assert_eq!((err.code(), err.status()), ("PAGE_ELEMENT_NOT_FOUND", 404));
}

/// 主机侧校验矩阵：绝对路径/拒符号链接/存在性/目录/100MB 上限
#[test]
fn validate_upload_file_enforces_host_side_rules() {
    // 相对路径 → 422
    let err = validate_upload_file("rel/a.png").unwrap_err();
    assert_eq!(
        (err.code(), err.status()),
        ("PAGE_UPLOAD_PATH_INVALID", 422)
    );
    // 不存在（绝对）→ 404
    let err = validate_upload_file("/nonexistent/definitely-missing-file-xyz").unwrap_err();
    assert_eq!(err.code(), "PAGE_UPLOAD_FILE_NOT_FOUND");
    // 目录 → 422
    let dir = temp_download_dir();
    let err = validate_upload_file(&dir.display().to_string()).unwrap_err();
    assert_eq!(err.code(), "PAGE_UPLOAD_PATH_INVALID");
    // 符号链接 → 422（逐组件检查：链接本身存在且指向真实文件，仍拒绝）
    let real = temp_upload_file("real.txt", b"x");
    let link = real.parent().unwrap().join("link.txt");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let err = validate_upload_file(&link.display().to_string()).unwrap_err();
    assert_eq!(
        (err.code(), err.status()),
        ("PAGE_UPLOAD_PATH_INVALID", 422)
    );
    // 超 100MB → 422（稀疏文件，不真写 100MB）
    let big = temp_upload_file("big.bin", b"");
    std::fs::File::options()
        .write(true)
        .open(&big)
        .unwrap()
        .set_len(MAX_UPLOAD_BYTES + 1)
        .unwrap();
    let err = validate_upload_file(&big.display().to_string()).unwrap_err();
    assert_eq!(
        (err.code(), err.status()),
        ("PAGE_UPLOAD_FILE_TOO_LARGE", 422)
    );
    // 合法：返回（路径, 文件名, 大小）
    let ok = temp_upload_file("ok.txt", b"hello");
    let (_p, name, size) = validate_upload_file(&ok.display().to_string()).unwrap();
    assert_eq!((name.as_str(), size), ("ok.txt", 5));
}
