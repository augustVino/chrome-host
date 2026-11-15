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
    /// kit typed_raw 通道（I1b）：同一脚本表/调用记录，响应帧文本化
    /// （{"result": ...} / {"error": {...}} 信封，对齐 ws.rs 透传形态）
    fn send_raw<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        session_id: Option<&'a str>,
    ) -> crate::infrastructure::cdp::page_cdp::PageCdpRawFuture<'a> {
        let fut = self.send_value(method, params, session_id);
        Box::pin(async move {
            let envelope = match fut.await {
                Ok(v) => json!({ "result": v }),
                Err(e) => json!({ "error": { "code": -32000, "message": e.to_string() } }),
            };
            serde_json::to_string(&envelope)
                .map_err(|e| AppError::business(500, "CDP_COMMAND_FAILED", e.to_string()))
        })
    }

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

/// armed 变体：暴露闸门表（对话框竞速等需要挂起单次 CDP 发送的场景）。
fn armed_with_gates() -> (PageService, Arc<Watched>, Calls, Script, Gates) {
    let (calls, script, _tap, connector) = fake_connector();
    let svc = PageService::with_connector(setup_running(), connector);
    let gates: Gates = Arc::default();
    let watched = Arc::new(Watched::new());
    *watched.conn.lock().unwrap() = Some(fake_conn(&calls, &script, None, &gates));
    watched.tabs.lock().unwrap().attach("T1", "S1");
    watched.tabs.lock().unwrap().attach("TAB2", "S2");
    svc.watched
        .lock()
        .unwrap()
        .insert(INS.into(), Arc::clone(&watched));
    (svc, watched, calls, script, gates)
}

/// kit selector 链的 Runtime.evaluate 响应（returnByValue=true：RemoteObject
/// .value 即页内脚本返回对象——定位 + actionability + 遮挡一体）
fn eval_obj_response(payload: Value) -> Result<Value, String> {
    Ok(json!({ "result": { "type": "object", "value": payload } }))
}

/// kit 对象解析链的 Runtime.evaluate 响应（returnByValue=false：RemoteObject
/// .objectId——fill/select/check 等对目标元素的句柄解析）
fn resolve_obj_eval(object_id: &str) -> Result<Value, String> {
    Ok(json!({ "result": { "type": "object", "objectId": object_id } }))
}

/// 测试 ref 预种子（S5：RefMap::add_with_frame 构造，替代旧 RefStore JSON 形态）。
/// role/name 是 kit stale 恢复链的输入，默认 button/OK；frame 传 Some 时帧内条目。
fn seed_ref(watched: &Watched, tid: &str, ref_id: &str, bni: i64, frame: Option<&str>) {
    seed_ref_as(watched, tid, ref_id, bni, frame, "button", "OK");
}

fn seed_ref_as(
    watched: &Watched,
    tid: &str,
    ref_id: &str,
    bni: i64,
    frame: Option<&str>,
    role: &str,
    name: &str,
) {
    use crate::application::browser_kit::element::RefMap;
    let mut refs = watched.refs.lock().unwrap();
    refs.entry(tid.to_string())
        .or_insert_with(RefMap::new)
        .add_with_frame(ref_id.to_string(), Some(bni), role, name, None, frame);
}

/// kit AXValue 形态（typed_raw 反序列化要求 type 字段；value 原样嵌入）
fn axv(v: Value) -> Value {
    json!({ "type": "string", "value": v })
}

/// DOM.resolveNode 的 kit typed 响应（RemoteObject.type 必填）
fn resolve_node_ok(object_id: &str) -> Result<Value, String> {
    Ok(json!({ "object": { "type": "object", "objectId": object_id } }))
}

/// Runtime.callFunctionOn 的取值响应（returnByValue）
fn fn_value(v: Value) -> Result<Value, String> {
    Ok(json!({ "result": { "type": "object", "value": v } }))
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

// ---------------- 纯逻辑：locate / hit 分类 -----------------------------
// （I2 已删：locate/hit-test JS 分类随 click 换 kit 解析链退役，矩阵由 kit
// element.rs 自带测试 + click_selector_classifies_visibility_and_coverage 覆盖）

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
        .snap(INS, "", SnapParams::default())
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
    let resp2 = svc.snap(INS, "", SnapParams::default()).await.unwrap();
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
    let err = svc.snap(INS, "", SnapParams::default()).await.unwrap_err();
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

/// detachedFromTarget / targetDestroyed：映射 + RefMap / snap_gen /
/// DialogBuffer 同清
#[tokio::test]
async fn detached_and_destroyed_drop_tab_state() {
    let (_svc, watched, _calls, _script) = armed();
    use crate::application::browser_kit::element::RefMap;
    watched
        .refs
        .lock()
        .unwrap()
        .insert("T1".into(), RefMap::new());
    watched.snap_gen.lock().unwrap().insert("T1".into(), 3);
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
    assert!(
        watched.snap_gen.lock().unwrap().get("T1").is_none(),
        "snap_gen 随 per-tab 状态同清（I1b）"
    );
    assert!(watched.dialogs.lock().unwrap().get("T1").is_none());

    // 仅带 sessionId 的 detach：按映射反查清理
    watched.tabs.lock().unwrap().attach("T2", "S9");
    watched
        .refs
        .lock()
        .unwrap()
        .insert("T2".into(), RefMap::new());
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
        .insert("T3".into(), RefMap::new());
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
    let amb = svc.snap(INS, "", SnapParams::default()).await.unwrap_err();
    assert_eq!((amb.code(), amb.status()), ("PAGE_TAB_AMBIGUOUS", 409));
    let msg = amb.to_string();
    assert!(
        msg.contains("T1") && msg.contains("TAB2"),
        "附 tab 列表: {msg}"
    );

    // 剩唯一 target → 自动选中
    watched.tabs.lock().unwrap().detach("TAB2");
    let resp = svc.snap(INS, "", SnapParams::default()).await.unwrap();
    assert_eq!(resp["targetId"], json!("T1"));
}

/// 根 frame 导航失效 ref；子 frame 导航不失效；文档事件按各自 reason
/// I1b 行为变化 ②（durable refs）：导航（根/子帧）、documentOpened、
/// documentUpdated 都**不再主动失效** refs——stale 由解析时判定产生（见
/// click 的 stale 测试）。各事件处理后表内容不变。
#[tokio::test]
async fn navigation_keeps_refs_durable_until_resolve_fails() {
    let (_svc, watched, _calls, _script) = armed();
    seed_ref(&watched, "T1", "e1", 1, None);

    for event in [
        json!({ "method": "Page.frameNavigated", "sessionId": "S1",
            "params": { "frame": { "id": "f1", "parentId": "root" } } }),
        json!({ "method": "Page.frameNavigated", "sessionId": "S1",
            "params": { "frame": { "id": "root" } } }),
        json!({ "method": "Page.documentOpened", "sessionId": "S1" }),
    ] {
        handle_event(&watched, &event).await;
        assert!(
            watched
                .refs
                .lock()
                .unwrap()
                .get("T1")
                .unwrap()
                .get("e1")
                .is_some(),
            "durable：导航/换文档不主动失效（{event}）"
        );
    }

    // DOM.documentUpdated 事件臂已删除（durable 语义下无事可做）——同样不动 refs
    handle_event(
        &watched,
        &json!({ "method": "DOM.documentUpdated", "sessionId": "S1" }),
    )
    .await;
    assert!(watched
        .refs
        .lock()
        .unwrap()
        .get("T1")
        .unwrap()
        .get("e1")
        .is_some());
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
    // kit 快照引擎的 AX 树形态（AXValue 需带 type；无 Page.getFrameTree 脚本
    // → loader 未知 → 本次断言按非 durable 编号递进）
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![
            Ok(json!({ "nodes": [
                { "nodeId": "1", "role": axv("button".into()), "name": axv("OK".into()), "backendDOMNodeId": 101 },
                { "nodeId": "2", "role": axv("link".into()), "name": axv("Home".into()), "backendDOMNodeId": 102 },
            ]})),
            Ok(json!({ "nodes": [
                { "nodeId": "1", "role": axv("button".into()), "name": axv("New".into()), "backendDOMNodeId": 201 },
            ]})),
        ],
    );

    let r1 = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
    assert_eq!(r1["instanceId"], json!(INS));
    assert_eq!(r1["targetId"], json!("T1"));
    assert_eq!(r1["refs_count"], json!(2));
    assert_eq!(r1["generation"], json!(1), "generation 来自 snap_gen 计数");
    // 上游行格式 + 我方 [ref=eN] → @eN 后处理
    assert!(
        r1["text"].as_str().unwrap().contains("- button \"OK\" @e1"),
        "{}",
        r1["text"]
    );
    assert!(
        r1["text"].as_str().unwrap().contains("- link \"Home\" @e2"),
        "{}",
        r1["text"]
    );
    assert!(r1.get("removedRefs").is_none(), "首拍无 removedRefs");

    // 快照重建：旧节点（e1/e2）不在新树 → removedRefs 披露；generation+1
    let r2 = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
    assert_eq!(r2["refs_count"], json!(1));
    assert_eq!(r2["generation"], json!(2));
    assert_eq!(
        r2["removedRefs"],
        json!(["@e1", "@e2"]),
        "消失的旧 ref 逐个披露"
    );
    let binding = watched.refs.lock().unwrap();
    let m = binding.get("T1").unwrap();
    assert!(
        m.get("e1").is_none(),
        "不在新树的旧 ref 必须死（不僵尸残留）"
    );
    assert_eq!(
        m.entries_sorted()
            .first()
            .map(|(id, e)| (id.as_str(), e.backend_node_id)),
        Some(("e3", Some(201))),
        "编号从计数器顺延（loader 未知 → 非 durable 分配）"
    );
}

/// durable refs（I1b 行为变化 ①）：同文档（frameTree loaderId 不变）两次
/// snap，@eN 对同一 backendNodeId 稳定复用；无 ref 消失 → removedRefs 省略。
#[tokio::test]
async fn snap_durable_refs_reuse_ids_across_snaps_same_document() {
    let (svc, watched, _calls, script) = armed();
    let tree = json!({ "nodes": [
        { "nodeId": "1", "role": axv("button".into()), "name": axv("OK".into()), "backendDOMNodeId": 101 },
    ]});
    given(
        &script,
        "Page.getFrameTree",
        vec![
            Ok(json!({ "frameTree": { "frame": { "id": "F0", "loaderId": "L1" } } })),
            Ok(json!({ "frameTree": { "frame": { "id": "F0", "loaderId": "L1" } } })),
        ],
    );
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(tree.clone()), Ok(tree)],
    );

    let r1 = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
    assert!(r1["text"].as_str().unwrap().contains("@e1"));
    let r2 = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
    assert!(
        r2["text"].as_str().unwrap().contains("- button \"OK\" @e1"),
        "{}",
        r2["text"]
    );
    assert!(r2.get("removedRefs").is_none(), "无 ref 消失 → 字段省略");
    assert_eq!(
        watched
            .refs
            .lock()
            .unwrap()
            .get("T1")
            .unwrap()
            .get("e1")
            .map(|e| e.backend_node_id),
        Some(Some(101)),
        "durable：同文档同节点复用 e1"
    );
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
    let resp = svc2.snap(INS, "T1", SnapParams::default()).await.unwrap();
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
            { "nodeId": "1", "role": axv("WebArea".into()), "name": axv("Menu Page".into()), "childIds": [2, 3] },
            { "nodeId": "2", "role": axv("button".into()), "name": axv("Menu".into()), "backendDOMNodeId": 101 },
            { "nodeId": "3", "role": axv("link".into()), "name": axv("Item".into()), "backendDOMNodeId": 102 },
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

    let resp = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
    assert_eq!(resp["hoverScan"], json!("ok"));
    assert_eq!(resp["refs_count"], json!(2));
    let text = resp["text"].as_str().unwrap();
    assert!(text.contains("- button \"Menu\" @e1 [hover]"), "{text}");
    assert!(
        text.contains("- link \"Item\" @e2 [hover-reveal]"),
        "{text}"
    );
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
            { "nodeId": "1", "role": axv("button".into()), "name": axv("B".into()), "backendDOMNodeId": 9 },
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

    let resp = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
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
            { "nodeId": "1", "role": axv("button".into()), "name": axv("OK".into()), "backendDOMNodeId": 7 },
        ]}))],
    );
    let resp = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
    assert_eq!(resp["hoverScan"], json!("skipped"));
    let text = resp["text"].as_str().unwrap();
    assert!(
        text.contains("- button \"OK\" @e1") && !text.contains("[hover"),
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
        vec![
            Ok(json!({ "nodes": [] })),
            Ok(json!({ "nodes": [] })),
            Ok(json!({ "nodes": [] })),
        ],
    );
    // 路径 1：armed 未走 on_attached，无 css_since → 即时 skipped
    let t0 = Instant::now();
    let resp = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
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
    let resp2 = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
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
    let resp3 = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
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
    let resp = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
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
            { "nodeId": "1", "role": axv("button".into()), "name": axv("Menu".into()), "backendDOMNodeId": 101 },
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
    let resp = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
    assert_eq!(resp["hoverScan"], json!("error"));
    let text = resp["text"].as_str().unwrap();
    assert!(
        text.contains("- button \"Menu\" @e1") && !text.contains("[hover"),
        "{text}"
    );
}

/// 根 frame 导航 = 换文档：登记表整清（Chrome 不保证逐张 Removed，死表
/// 残留会让 getStyleSheetText 恒 Err → 假 truncated，真机实测 total=4
/// texts=1）；子 frame 导航不清。新表由 styleSheetAdded 重新登记后 snap 恢复 ok。
#[tokio::test]
async fn navigation_clears_sheet_registry_root_frame_only() {
    let (svc, watched, _calls, script) = armed();
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
    // I1b：refs 不随导航失效（durable）；sheets 清理独立发生
    assert!(
        watched.sheets.lock().unwrap().get("T1").is_none(),
        "根 frame 导航整清登记"
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
            { "nodeId": "1", "role": axv("button".into()), "name": axv("B".into()), "backendDOMNodeId": 9 },
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
    let resp = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
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

/// ref 路径（I1b：kit resolve_element_center 解析链——DOM.resolveNode →
/// 页内 JS 连接检查 + 点击点；坐标 + 我方三段派发）
#[tokio::test]
async fn click_ref_path_resolves_center_and_dispatches() {
    let (svc, watched, calls, script) = armed();
    seed_ref(&watched, "T1", "e1", 101, None);
    given(&script, "DOM.resolveNode", vec![resolve_node_ok("obj101")]);
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![fn_value(
            json!({ "connected": true, "x": 100.0, "y": 50.0 }),
        )],
    );

    let resp = svc.click(INS, "T1", "@e1", false, false).await.unwrap();
    assert_eq!(
        resp["result"],
        json!("Clicked @e1 \"OK\" at CSS (100, 50)"),
        "坐标 = kit 解析链返回的点击点（同会话 = 顶层视口坐标）"
    );
    let resolve = calls_of(&calls, "DOM.resolveNode");
    assert_eq!(
        (resolve.len(), resolve[0].param("backendNodeId")),
        (1, json!(101))
    );
    let mouse = calls_of(&calls, "Input.dispatchMouseEvent");
    assert_eq!(
        mouse.iter().map(|c| c.param("type")).collect::<Vec<_>>(),
        vec![
            json!("mouseMoved"),
            json!("mousePressed"),
            json!("mouseReleased")
        ],
        "kit ClickPhase 序列（Move → Press → Release）"
    );
    // kit 事件形态：Move 无 button 字段；Press buttons=1；Release buttons=0
    assert!(mouse[0].params.get("button").is_none());
    assert_eq!(
        (
            mouse[1].param("button"),
            mouse[1].param("buttons"),
            mouse[1].param("clickCount")
        ),
        (json!("left"), json!(1), json!(1))
    );
    assert_eq!(
        (
            mouse[2].param("button"),
            mouse[2].param("buttons"),
            mouse[2].param("clickCount")
        ),
        (json!("left"), json!(0), json!(1))
    );
    for c in &mouse {
        assert_eq!(c.param("x"), json!(100.0));
        assert_eq!(c.param("y"), json!(50.0));
        assert_eq!(c.session.as_deref(), Some("S1"));
    }
}

/// 双击（count=2）与右键（button=right）的 ClickSpec 透传：Press/Release
/// 两轮、clickCount 递增；右键 button/buttons 换位掩码
#[tokio::test]
async fn click_double_and_right_specs_pass_through() {
    let (svc, watched, calls, script) = armed();
    seed_ref(&watched, "T1", "e1", 101, None);
    given(&script, "DOM.resolveNode", vec![resolve_node_ok("obj101")]);
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![fn_value(json!({ "connected": true, "x": 1.0, "y": 2.0 }))],
    );
    let resp = svc.click(INS, "T1", "@e1", true, false).await.unwrap();
    assert!(resp["result"]
        .as_str()
        .unwrap()
        .starts_with("Double-clicked @e1 \"OK\""));
    let mouse = calls_of(&calls, "Input.dispatchMouseEvent");
    assert_eq!(
        mouse.iter().map(|c| c.param("type")).collect::<Vec<_>>(),
        vec![
            json!("mouseMoved"),
            json!("mousePressed"),
            json!("mouseReleased"),
            json!("mousePressed"),
            json!("mouseReleased")
        ],
        "count=2：Press/Release 两轮"
    );
    assert_eq!(
        (mouse[3].param("clickCount"), mouse[4].param("clickCount")),
        (json!(2), json!(2))
    );

    // 右键：button=right、buttons 位掩码 2
    let (svc2, watched2, calls2, script2) = armed();
    seed_ref(&watched2, "T1", "e1", 101, None);
    given(&script2, "DOM.resolveNode", vec![resolve_node_ok("obj101")]);
    given(
        &script2,
        "Runtime.callFunctionOn",
        vec![fn_value(json!({ "connected": true, "x": 1.0, "y": 2.0 }))],
    );
    let resp2 = svc2.click(INS, "T1", "@e1", false, true).await.unwrap();
    assert!(resp2["result"]
        .as_str()
        .unwrap()
        .starts_with("Right-clicked @e1 \"OK\""));
    let m2 = calls_of(&calls2, "Input.dispatchMouseEvent");
    assert_eq!(
        (m2[1].param("button"), m2[1].param("buttons")),
        (json!("right"), json!(2))
    );
}

/// 对话框竞速：press 挂起期间对话框事件到达 bus → kit 中止序列返回
/// pending_release → 我方补发 release。生产时序中 watch_loop 的
/// on_dialog_opening 先 accept 对话框再把事件转发到 bus，补发 release 落
/// 在已解锁的渲染进程上；本测试直接向 bus 注入事件（watch_loop 未跑）。
#[tokio::test]
async fn click_dialog_mid_sequence_compensates_pending_release() {
    let (svc, watched, calls, script, gates) = armed_with_gates();
    seed_ref(&watched, "T1", "e1", 101, None);
    given(&script, "DOM.resolveNode", vec![resolve_node_ok("obj101")]);
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![fn_value(json!({ "connected": true, "x": 10.0, "y": 20.0 }))],
    );
    // 两道闸门：mouseMoved 与 mousePressed 依次挂起（FakeCdp 按调用序弹
    // 闸门）；测试在 press 挂起窗口注入对话框事件
    let (release_move, gate_move) = tokio::sync::oneshot::channel::<()>();
    let (_release_press, gate_press) = tokio::sync::oneshot::channel::<()>();
    gates.lock().unwrap().insert(
        "Input.dispatchMouseEvent".into(),
        VecDeque::from([gate_move, gate_press]),
    );

    let click_fut = svc.click(INS, "T1", "@e1", false, false);
    tokio::pin!(click_fut);
    // select 同时驱动 click 与轮询（pin 后不 poll 不推进）
    let waits = |n: usize| {
        let calls = Arc::clone(&calls);
        async move {
            loop {
                if calls_of(&calls, "Input.dispatchMouseEvent").len() >= n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    };
    tokio::select! {
        _ = &mut click_fut => panic!("click 不应在 move 闸门前完成"),
        _ = waits(1) => {}
    }
    release_move.send(()).unwrap();
    tokio::select! {
        _ = &mut click_fut => panic!("click 不应在 press 闸门前完成"),
        _ = waits(2) => {}
    }
    let _ = watched.event_bus.send(
        crate::application::browser_kit::cdp_event_from_frame(&json!({
            "method": "Page.javascriptDialogOpening", "sessionId": "S1",
            "params": { "type": "alert", "message": "hi" }
        }))
        .expect("bus 事件构造"),
    );
    // 竞速胜出：click 不等被对话框阻塞的 press ack，立即返回
    let resp = tokio::time::timeout(Duration::from_secs(2), &mut click_fut)
        .await
        .expect("对话框竞速必须在 ack 前返回")
        .unwrap();
    let text = resp["result"].as_str().unwrap();
    assert!(
        text.contains("a dialog opened and was auto-accepted"),
        "{text}"
    );
    let _ = _release_press.send(()); // 清理挂起的 press（发送端 drop 也无妨）
    let mouse = calls_of(&calls, "Input.dispatchMouseEvent");
    assert_eq!(
        mouse.iter().map(|c| c.param("type")).collect::<Vec<_>>(),
        vec![
            json!("mouseMoved"),
            json!("mousePressed"),
            json!("mouseReleased")
        ],
        "补偿 release 已发出（按钮不滞留按下态）"
    );
    assert_eq!(mouse[2].session.as_deref(), Some("S1"));
}

/// kit 解析链的 stale 恢复（durable 核心）：旧节点脱离文档（connected:false）
/// → AX 唯一匹配恢复 → 正常点击。SPA 同文档旧 ref 可用的机制。
#[tokio::test]
async fn click_stale_ref_recovers_by_unique_role_name_match() {
    let (svc, watched, _calls, script) = armed();
    seed_ref(&watched, "T1", "e1", 101, None);
    // resolveNode 两次：旧节点（失联）→ 恢复命中的新节点 205
    given(
        &script,
        "DOM.resolveNode",
        vec![resolve_node_ok("objOld"), resolve_node_ok("obj205")],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![
            fn_value(json!({ "connected": false })), // 旧节点已脱离文档
            fn_value(json!({ "connected": true, "x": 80.0, "y": 60.0 })), // 恢复后的点击点
        ],
    );
    // AX 恢复查询：同 role/name 唯一命中（backendDOMNodeId=205）
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [
            { "nodeId": "1", "role": axv("button".into()), "name": axv("OK".into()), "backendDOMNodeId": 205 },
        ]}))],
    );
    let resp = svc.click(INS, "T1", "@e1", false, false).await.unwrap();
    assert_eq!(resp["result"], json!("Clicked @e1 \"OK\" at CSS (80, 60)"));
}

/// ref 未命中：无表 / miss（含 @E3 大写形态）→ 404 PAGE_REF_NOT_FOUND；
/// 解析时判 stale（脱离 + 恢复无果 / 多候选不可恢复）→ 409 PAGE_REF_STALE
#[tokio::test]
async fn click_ref_miss_and_stale_errors() {
    let (svc, _w, _calls, _script) = armed();
    for t in ["@e9", "@E3"] {
        let miss = svc.click(INS, "T1", t, false, false).await.unwrap_err();
        assert_eq!(
            (miss.code(), miss.status()),
            ("PAGE_REF_NOT_FOUND", 404),
            "{t}"
        );
    }

    // 旧 ref 在解析时判 stale：节点脱离 + AX 恢复无果
    //（I1b 行为变化 ②：stale 不再来自事件失效，来自解析链判定）
    let (svc2, watched2, _c2, script2) = armed();
    seed_ref(&watched2, "T1", "e1", 1, None);
    given(&script2, "DOM.resolveNode", vec![resolve_node_ok("obj1")]);
    given(
        &script2,
        "Runtime.callFunctionOn",
        vec![fn_value(json!({ "connected": false }))],
    );
    given(
        &script2,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [] }))],
    );
    let stale = svc2
        .click(INS, "T1", "@e1", false, false)
        .await
        .unwrap_err();
    assert_eq!((stale.code(), stale.status()), ("PAGE_REF_STALE", 409));
    assert!(
        stale.to_string().contains("Could not locate element"),
        "kit 恢复无果原文透传: {stale}"
    );

    // 多候选恢复不可判定 → 同样 stale（kit cannot be recovered 文案）
    let (svc3, watched3, _c3, script3) = armed();
    seed_ref(&watched3, "T1", "e1", 1, None);
    given(&script3, "DOM.resolveNode", vec![resolve_node_ok("obj1")]);
    given(
        &script3,
        "Runtime.callFunctionOn",
        vec![fn_value(json!({ "connected": false }))],
    );
    given(
        &script3,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [
            { "nodeId": "1", "role": axv("button".into()), "name": axv("OK".into()), "backendDOMNodeId": 2 },
            { "nodeId": "2", "role": axv("button".into()), "name": axv("OK".into()), "backendDOMNodeId": 3 },
        ]}))],
    );
    let ambiguous = svc3
        .click(INS, "T1", "@e1", false, false)
        .await
        .unwrap_err();
    assert_eq!(
        (ambiguous.code(), ambiguous.status()),
        ("PAGE_REF_STALE", 409)
    );
}

#[tokio::test]
async fn click_selector_path_locates_hit_tests_and_dispatches() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![eval_obj_response(json!({ "x": 150.0, "y": 130.0 }))],
    );

    let resp = svc.click(INS, "TA", "#save", false, false).await.unwrap();
    // 前缀 "TA" 唯一解析到 TAB2 → 全链带 session S2
    assert_eq!(resp["targetId"], json!("TAB2"));
    assert_eq!(resp["result"], json!("Clicked \"#save\" at CSS (150, 130)"));
    let evals = calls_of(&calls, "Runtime.evaluate");
    assert_eq!(
        evals.len(),
        1,
        "kit selector 链单次 evaluate（定位 + actionability + 遮挡一体）"
    );
    let expr = evals[0].param("expression");
    let expr = expr.as_str().unwrap();
    assert!(
        expr.contains("querySelector") && expr.contains("elementFromPoint"),
        "定位 + 遮挡检查在同一页内脚本: {expr}"
    );
    assert_eq!(evals[0].session.as_deref(), Some("S2"));
    let mouse = calls_of(&calls, "Input.dispatchMouseEvent");
    assert_eq!(mouse.len(), 3);
    for c in &mouse {
        assert_eq!(c.session.as_deref(), Some("S2"));
    }
}

/// 可见性 / 遮挡分类（kit 文案）：reason not visible → 422；blocker → 409，
/// 且零派发（错误码不变；文案从旧 locate/hit 自有文案换 kit 文案）
#[tokio::test]
async fn click_selector_classifies_visibility_and_coverage() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![eval_obj_response(
            json!({ "x": 5.0, "y": 5.0, "reason": "not visible" }),
        )],
    );
    let e = svc.click(INS, "T1", ".x", false, false).await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ELEMENT_NOT_VISIBLE", 422));
    assert!(e.to_string().contains("not visible"), "kit 文案透传");
    assert_eq!(calls_of(&calls, "Input.dispatchMouseEvent").len(), 0);

    let (svc2, _w2, _c2, script2) = armed();
    given(
        &script2,
        "Runtime.evaluate",
        vec![eval_obj_response(
            json!({ "x": 5.0, "y": 5.0, "blocker": "div.overlay" }),
        )],
    );
    let e2 = svc2.click(INS, "T1", ".x", false, false).await.unwrap_err();
    assert_eq!((e2.code(), e2.status()), ("PAGE_ELEMENT_BLOCKED", 409));
    assert!(e2.to_string().contains("<div.overlay>"), "kit 遮挡文案");
}

/// kit 派发失败：错误码保持 PAGE_DISPATCH_FAILED，effect 语义保守 unknown
///（kit 不外露失败相位；绝不标 none——盲重试 = 二次点击）。kit 链无补偿
/// release（相位未知）——与 clickxy 旧链的「补一发 release」差异点，提交
/// 信息已披露。
#[tokio::test]
async fn click_dispatch_failure_maps_to_conservative_dispatch_failed() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![eval_obj_response(json!({ "x": 1.0, "y": 2.0 }))],
    );
    given(
        &script,
        "Input.dispatchMouseEvent",
        vec![Err("CDP 命令 Input.dispatchMouseEvent 被拒绝: boom".into())],
    );

    let e = svc.click(INS, "T1", ".x", false, false).await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_DISPATCH_FAILED", 500));
    let msg = e.to_string();
    assert!(msg.contains("effect_state: unknown"), "{msg}");
    assert!(!msg.contains("effect_state: none"), "{msg}");
    let mouse = calls_of(&calls, "Input.dispatchMouseEvent");
    assert_eq!(mouse.len(), 1, "首个事件失败即中止（相位未知，不补发）");
}

// ---------------- 工具：fill ---------------------------------------------

/// I2 fill 链：可填前置检查（我方）→ kit fill（focus + select/value 清空
/// + insertText）→ 回读校验追加（我方）。成功路径结果串与旧链同构。
#[tokio::test]
async fn fill_check_kit_fill_and_readback_verify() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![
            resolve_obj_eval("obj9"),  // 我方解析（预检查/回读的句柄锚点）
            resolve_obj_eval("obj9b"), // kit fill 内部解析
        ],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![
            fn_value(json!({ "tag": "INPUT", "kind": "native", "label": "q" })), // ① 可填检查
            Ok(json!({ "result": { "type": "object" } })),                       // ③ kit focus
            Ok(json!({ "result": { "type": "object" } })),                       // ③ kit clear
            fn_value(json!({ "matches": true })),                                // ④ 回读校验
        ],
    );

    let resp = svc.fill(INS, "T1", "#q", "hello").await.unwrap();
    assert_eq!(
        resp["result"],
        json!("Filled <INPUT> \"q\" with \"hello\" (5 chars)")
    );

    let insert = calls_of(&calls, "Input.insertText");
    assert_eq!((insert.len(), insert[0].param("text")), (1, json!("hello")));
    assert_eq!(
        insert[0].session.as_deref(),
        Some("S1"),
        "kit fill 的 insertText 发 page 级会话（渲染进程按焦点路由）"
    );
    let release = calls_of(&calls, "Runtime.releaseObject");
    assert_eq!(
        (release.len(), release[0].param("objectId")),
        (1, json!("obj9")),
        "finally 归还（我方解析的句柄，发解析会话）"
    );
    let call_fns = calls_of(&calls, "Runtime.callFunctionOn");
    assert_eq!(call_fns.len(), 4, "check → kit focus → kit clear → verify");
    // verify 参数预计算两形态（multiLine / singleLine）
    assert_eq!(
        call_fns[3].param("arguments"),
        json!([{ "value": "hello" }, { "value": "hello" }])
    );
    assert!(
        call_fns[2]
            .param("functionDeclaration")
            .as_str()
            .unwrap()
            .contains("this.select"),
        "kit 清空 = select() + value 赋空（表单控件路径）"
    );
}

/// contenteditable 目标：kit 清空覆盖不了编辑根，我方预清空补位（回读
/// 期望 multiLine 口径）
#[tokio::test]
async fn fill_preclears_contenteditable_before_kit_fill() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![resolve_obj_eval("objE"), resolve_obj_eval("objE2")],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![
            fn_value(json!({ "tag": "DIV", "kind": "editable", "label": "" })),
            fn_value(json!({ "precleared": true })), // ② 预清空（textContent）
            Ok(json!({ "result": { "type": "object" } })),
            Ok(json!({ "result": { "type": "object" } })),
            fn_value(json!({ "matches": true })),
        ],
    );
    let resp = svc
        .fill(INS, "T1", "[contenteditable]", "line1\nline2")
        .await
        .unwrap();
    assert_eq!(
        resp["result"],
        json!("Filled <DIV> with \"line1\nline2\" (11 chars)")
    );
    let call_fns = calls_of(&calls, "Runtime.callFunctionOn");
    assert_eq!(
        call_fns.len(),
        5,
        "check → preclear → kit focus → kit clear → verify"
    );
    assert!(
        call_fns[1]
            .param("functionDeclaration")
            .as_str()
            .unwrap()
            .contains("textContent"),
        "预清空脚本对编辑根清 textContent"
    );
}

#[tokio::test]
async fn fill_rejects_not_fillable_and_reports_mismatch() {
    let (svc, _w, calls, script) = armed();
    given(&script, "Runtime.evaluate", vec![resolve_obj_eval("obj9")]);
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![Ok(
            json!({ "result": { "type": "object", "value": { "not_fillable": "disabled" } } }),
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
        "Runtime.evaluate",
        vec![resolve_obj_eval("obj9"), resolve_obj_eval("obj9b")],
    );
    given(
        &script2,
        "Runtime.callFunctionOn",
        vec![
            fn_value(json!({ "tag": "INPUT", "kind": "native", "label": "" })),
            Ok(json!({ "result": { "type": "object" } })),
            Ok(json!({ "result": { "type": "object" } })),
            fn_value(json!({ "matches": false, "actual": "hell", "expected": "hello" })),
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

/// 回读阶段 focus_lost：值不一致且焦点不在目标上（页面抢焦点——
/// insertText 可能落到别处）→ 409 PAGE_FILL_FOCUS_LOST（码与状态不变，
/// 触发点从旧链的清空阶段移到回读阶段）
#[tokio::test]
async fn fill_reports_focus_lost_when_page_steals_focus() {
    let (svc, _w, _calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![resolve_obj_eval("obj9"), resolve_obj_eval("obj9b")],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![
            fn_value(json!({ "tag": "INPUT", "kind": "native", "label": "" })),
            Ok(json!({ "result": { "type": "object" } })),
            Ok(json!({ "result": { "type": "object" } })),
            fn_value(json!({ "focus_lost": true })),
        ],
    );
    let e = svc.fill(INS, "T1", "#q", "x").await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_FILL_FOCUS_LOST", 409));
}

// ---------------- 工具：press --------------------------------------------

/// kit 按键序列：keyDown（带 text）+ keyUp 两段；Ctrl 组合抑制 text
///（旧链 rawKeyDown/char/keyUp 三段退役——行为差异已在提交信息披露）
#[tokio::test]
async fn press_dispatches_keydown_keyup_and_suppresses_combo_text() {
    let (svc, _w, calls, _script) = armed();
    let resp = svc.press(INS, "T1", "Enter", None).await.unwrap();
    assert_eq!(resp["result"], json!("Pressed Enter"));
    let events = calls_of(&calls, "Input.dispatchKeyEvent");
    assert_eq!(events.len(), 2, "kit keyDown + keyUp 两段");
    assert_eq!(
        events[0].params,
        json!({ "type": "keyDown", "key": "Enter", "code": "Enter", "text": "\r", "unmodifiedText": "\r", "windowsVirtualKeyCode": 13 })
    );
    assert_eq!(
        events[1].params,
        json!({ "type": "keyUp", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13 }),
        "keyUp 不带 text"
    );

    // Ctrl+A：组合键 text 被抑制（不能打出 "a"），modifiers=2 透传
    let (svc2, _w2, calls2, _s2) = armed();
    svc2.press(INS, "T1", "Control+A", None).await.unwrap();
    let events2 = calls_of(&calls2, "Input.dispatchKeyEvent");
    assert_eq!(events2.len(), 2);
    for e in &events2 {
        assert_eq!(e.param("modifiers"), json!(2));
        assert!(
            e.params.get("text").is_none(),
            "命令和弦不带 text: {:?}",
            e.params
        );
    }

    // Shift+ArrowDown：shift 不抑制（非命令和弦），但 ArrowDown 无 text
    let (svc3, _w3, calls3, _s3) = armed();
    let resp3 = svc3
        .press(INS, "T1", "Shift+ArrowDown", None)
        .await
        .unwrap();
    assert_eq!(resp3["result"], json!("Pressed Shift+ArrowDown"));
    let events3 = calls_of(&calls3, "Input.dispatchKeyEvent");
    for e in &events3 {
        assert_eq!(e.param("modifiers"), json!(8));
    }
}

/// --target ref：kit 对象解析（isConnected 检查）→ DOM.focus(objectId)
/// → 键序（帧会话）；对象归还到解析它的同一会话
#[tokio::test]
async fn press_focuses_ref_target_before_dispatch() {
    let (svc, watched, calls, script) = armed();
    seed_ref_as(&watched, "T1", "e1", 42, None, "textbox", "search");
    given(&script, "DOM.resolveNode", vec![resolve_node_ok("obj42")]);
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![fn_value(json!(true))],
    );
    let resp = svc.press(INS, "T1", "Enter", Some("@e1")).await.unwrap();
    assert_eq!(resp["result"], json!("Pressed Enter on @e1 \"search\""));
    let focus = calls_of(&calls, "DOM.focus");
    assert_eq!(
        (focus.len(), focus[0].param("objectId")),
        (1, json!("obj42"))
    );
    assert_eq!(
        calls_of(&calls, "Input.dispatchKeyEvent").len(),
        2,
        "kit keyDown + keyUp"
    );
    assert_eq!(
        calls_of(&calls, "Runtime.releaseObject")[0].param("objectId"),
        json!("obj42"),
        "对象句柄归还（fill 同构）"
    );
}

/// --target selector：统一 kit 解析链（旧链页内 eval focus 脚本退役），
/// 未命中 → 404 PAGE_ELEMENT_NOT_FOUND（与 click 同码）
#[tokio::test]
async fn press_target_selector_via_kit_resolve() {
    let (svc, _w, calls, script) = armed();
    given(&script, "Runtime.evaluate", vec![resolve_obj_eval("objQ")]);
    let resp = svc.press(INS, "T1", "Enter", Some("#q")).await.unwrap();
    assert_eq!(resp["result"], json!("Pressed Enter on #q"));
    assert_eq!(
        calls_of(&calls, "DOM.focus")[0].param("objectId"),
        json!("objQ")
    );

    let (svc2, _w2, calls2, script2) = armed();
    given(
        &script2,
        "Runtime.evaluate",
        vec![Ok(json!({ "result": { "type": "object" } }))], // 无 objectId
    );
    let e = svc2
        .press(INS, "T1", "Enter", Some("#gone"))
        .await
        .unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ELEMENT_NOT_FOUND", 404));
    assert_eq!(calls_of(&calls2, "Input.dispatchKeyEvent").len(), 0);
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
    let e = svc
        .click(INS, "NOPE", "@e1", false, false)
        .await
        .unwrap_err();
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
    seed_ref(&watched, "T1", "e1", 7, None);
    // kit 对象解析：DOM.resolveNode → isConnected 检查 → 收集器调用
    given(&script, "DOM.resolveNode", vec![resolve_node_ok("OID-1")]);
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![
            fn_value(json!(true)),
            Ok(json!({ "result": { "type": "string", "value": facts_wire().to_string() } })),
        ],
    );
    let r = svc.extract(INS, "T1", Some("@e1")).await.unwrap();
    assert_eq!(r["rows"], json!([["张三", ""]]));
    assert_eq!(r["meta"]["target"], "@e1");
    let rn = calls_of(&calls, "DOM.resolveNode");
    assert_eq!(rn[0].param("backendNodeId"), json!(7));
    assert_eq!(rn[0].session.as_deref(), Some("S1"));
    let cfo = calls_of(&calls, "Runtime.callFunctionOn");
    assert_eq!(cfo[1].param("objectId"), json!("OID-1"));
    assert_eq!(cfo[1].param("returnByValue"), json!(true));
    assert!(cfo[1]
        .param("functionDeclaration")
        .as_str()
        .unwrap()
        .contains("BUDGET = 10000"));
    assert_eq!(
        calls_of(&calls, "Runtime.releaseObject")[0].param("objectId"),
        json!("OID-1"),
        "对象归还（fill 同构）"
    );

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

/// kit type_text_into_active_context：对焦点元素逐字符派发（可打印字符
/// insertText，\n/\t 走真按键事件），无任何定位/校验 eval
#[tokio::test]
async fn type_inserts_text_into_focused_element() {
    let (svc, _w, calls, _script) = armed();
    let resp = svc.r#type(INS, "T1", "hello").await.unwrap();
    assert_eq!(resp["result"], json!("Typed 5 characters"));
    let insert = calls_of(&calls, "Input.insertText");
    assert_eq!(
        insert.iter().map(|c| c.param("text")).collect::<Vec<_>>(),
        vec![json!("h"), json!("e"), json!("l"), json!("l"), json!("o")],
        "逐字符 insertText"
    );
    assert_eq!(insert[0].session.as_deref(), Some("S1"));

    // 换行走真按键（keyDown+keyUp）而非字面插入——表单提交语义可达
    let (svc2, _w2, calls2, _s2) = armed();
    svc2.r#type(INS, "T1", "a\nb").await.unwrap();
    let keys = calls_of(&calls2, "Input.dispatchKeyEvent");
    assert_eq!(
        (keys.len(), keys[0].param("type"), keys[0].param("key")),
        (2, json!("keyDown"), json!("Enter")),
        "\\n = Enter keyDown/keyUp"
    );
    assert_eq!(calls_of(&calls2, "Input.insertText").len(), 2);
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

/// kit click selector 路径的定位响应（定位 + actionability + 遮挡一体）
fn located_ok() -> Result<Value, String> {
    eval_obj_response(json!({ "x": 10.0, "y": 20.0 }))
}

/// 分支一：存在 → 原生点击一次 → 下一轮探针消失 → 'until it disappeared'
///（I2：点击层 = kit click selector 路径，定位与派发一体）
#[tokio::test]
async fn loadall_clicks_until_selector_disappears() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![
            rt_bool(true),  // 探针：存在
            located_ok(),   // kit click 定位
            rt_bool(false), // 探针：已消失
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
        "每次存活轮次一次原生点击（Move/Press/Release）"
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
            rt_bool(true),
            located_ok(),
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

// ---------------- 工具：hover / scroll / drag / select / check / tap（I2）--

/// hover：ref 路径解析链 + 单发 mouseMoved；结果坐标 = 顶层视口
#[tokio::test]
async fn hover_moves_pointer_to_element_center() {
    let (svc, watched, calls, script) = armed();
    seed_ref(&watched, "T1", "e1", 101, None);
    given(&script, "DOM.resolveNode", vec![resolve_node_ok("obj101")]);
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![fn_value(json!({ "connected": true, "x": 42.0, "y": 24.0 }))],
    );
    let resp = svc.hover(INS, "T1", "@e1").await.unwrap();
    assert_eq!(resp["result"], json!("Hovered @e1 \"OK\" at CSS (42, 24)"));
    let mouse = calls_of(&calls, "Input.dispatchMouseEvent");
    assert_eq!(
        (
            mouse.len(),
            mouse[0].param("type"),
            mouse[0].param("x"),
            mouse[0].param("y")
        ),
        (1, json!("mouseMoved"), json!(42.0), json!(24.0)),
        "单发 mouseMoved，无 press/release"
    );
    assert!(mouse[0].params.get("button").is_none());
}

/// hover 错误路径：遮挡（kit 解析链 blocker 检查；require_actionable=false
///——hover 不要求可见性，但遮挡仍拦）→ 409
#[tokio::test]
async fn hover_reports_blocked_element() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![eval_obj_response(
            json!({ "x": 1.0, "y": 2.0, "blocker": "div.mask" }),
        )],
    );
    let e = svc.hover(INS, "T1", "#menu").await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ELEMENT_BLOCKED", 409));
    assert_eq!(calls_of(&calls, "Input.dispatchMouseEvent").len(), 0);
}

/// scroll：有目标滚元素（callFunctionOn scrollBy），无目标滚页面
///（window.scrollBy）
#[tokio::test]
async fn scroll_element_and_page_paths() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![resolve_obj_eval("objFeed")],
    );
    let resp = svc
        .scroll(INS, "T1", Some("#feed"), 0.0, -300.0)
        .await
        .unwrap();
    assert_eq!(resp["result"], json!("Scrolled \"#feed\" by (0, -300)"));
    let fns = calls_of(&calls, "Runtime.callFunctionOn");
    assert_eq!((fns.len(), fns[0].param("objectId")), (1, json!("objFeed")));
    assert_eq!(
        fns[0].param("arguments"),
        json!([{ "value": 0.0 }, { "value": -300.0 }]),
        "scrollBy(dx, dy) 透传"
    );

    // 无目标：window.scrollBy
    let (svc2, _w2, calls2, script2) = armed();
    given(
        &script2,
        "Runtime.evaluate",
        vec![Ok(json!({ "result": { "type": "undefined" } }))],
    );
    let resp2 = svc2.scroll(INS, "T1", None, 0.0, 500.0).await.unwrap();
    assert_eq!(resp2["result"], json!("Scrolled page by (0, 500)"));
    let evals = calls_of(&calls2, "Runtime.evaluate");
    assert_eq!(
        evals[0].param("expression"),
        json!("window.scrollBy(0, 500)")
    );
}

/// scroll 错误路径：非有限 delta 建连前 400（拦 CLI/本地调用方的 inf/nan）
#[tokio::test]
async fn scroll_rejects_non_finite_deltas() {
    let (svc, _w, calls, _script) = armed();
    let e = svc
        .scroll(INS, "T1", None, f64::INFINITY, 5.0)
        .await
        .unwrap_err();
    assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400));
    assert!(calls_of(&calls, "Runtime.evaluate").is_empty());
}

/// drag 轨迹（纯逻辑）：端点精确、步数 clamp(dist/12, 1, 60)、smoothstep
/// 单调（相邻步距先增后减）、短距最小 1 步、确定性（同端点同轨迹）
#[test]
fn drag_trajectory_shape_and_clamp() {
    let from = (10.0, 20.0);
    let to = (110.0, 80.0); // dist = √(100²+60²) ≈ 116.6 → 10 步
    let pts = drag_trajectory(from, to);
    assert_eq!(pts.len(), 11, "116.6px / 12 → 10 步 = 11 点");
    assert_eq!(pts.first().unwrap(), &from, "首点 = 起点（贝塞尔 t=0）");
    let last = pts.last().unwrap();
    assert!((last.0 - to.0).abs() < 1e-9 && (last.1 - to.1).abs() < 1e-9);
    // smoothstep 缓动：相邻步距先增后减（非匀速直线）
    let dists: Vec<f64> = pts
        .windows(2)
        .map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1))
        .collect();
    let mid = dists.len() / 2;
    assert!(
        dists[mid] > dists[0] && dists[mid] > *dists.last().unwrap(),
        "中段步距大于首尾（缓动形态）: {dists:?}"
    );
    // 弧线：非共线（LCG 垂直扰动至少把部分点推离直线）
    let collinear = pts
        .iter()
        .all(|&(x, y)| ((y - 20.0) * 100.0 - (x - 10.0) * 60.0).abs() < 1e-9);
    assert!(!collinear, "轨迹带弧线（非直线插值）");
    // 短距：最小 1 步（2 点）；同端点确定性
    let short = drag_trajectory((0.0, 0.0), (5.0, 0.0));
    assert_eq!(short.len(), 2);
    assert_eq!(drag_trajectory(from, to), pts, "同端点同轨迹（无时钟依赖）");
}

/// drag 集成：两端点同帧 → 初始 move → press → 插值 moves（buttons=1）
/// → release；步数随距离
#[tokio::test]
async fn drag_interpolates_between_endpoints() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![
            eval_obj_response(json!({ "x": 10.0, "y": 20.0 })), // from
            eval_obj_response(json!({ "x": 110.0, "y": 80.0 })), // to
        ],
    );
    let resp = svc.drag(INS, "T1", "#thumb", "#drop").await.unwrap();
    assert_eq!(
        resp["result"],
        json!("Dragged \"#thumb\" to \"#drop\" (10 steps)")
    );
    let mouse = calls_of(&calls, "Input.dispatchMouseEvent");
    // 1 初始 move + 1 press + 10 插值 move + 1 release = 13
    assert_eq!(mouse.len(), 13);
    assert_eq!(mouse[0].param("type"), json!("mouseMoved"));
    assert_eq!(
        (mouse[0].param("x"), mouse[0].param("y")),
        (json!(10.0), json!(20.0)),
        "初始 move 到起点"
    );
    assert_eq!(
        (
            mouse[1].param("type"),
            mouse[1].param("buttons"),
            mouse[1].param("clickCount")
        ),
        (json!("mousePressed"), json!(1), json!(1))
    );
    let last = mouse.last().unwrap();
    assert_eq!(
        (last.param("type"), last.param("x"), last.param("y")),
        (json!("mouseReleased"), json!(110.0), json!(80.0)),
        "release 在终点"
    );
    // 插值 moves 携带 buttons=1（按住态）
    for m in &mouse[2..mouse.len() - 1] {
        assert_eq!(m.param("type"), json!("mouseMoved"));
        assert_eq!(m.param("buttons"), json!(1));
    }
}

/// drag 错误路径：两端点在不同帧 → 422 PAGE_DRAG_CROSS_FRAME，零派发
#[tokio::test]
async fn drag_across_frames_is_rejected() {
    let (svc, watched, calls, script) = armed();
    register_frame(
        &watched,
        "T1",
        "F_ROOT",
        "FRM_A",
        Some("S_FRM_A"),
        "F_ROOT",
        Some(501),
    );
    register_frame(
        &watched,
        "T1",
        "F_ROOT",
        "FRM_B",
        Some("S_FRM_B"),
        "F_ROOT",
        Some(502),
    );
    seed_ref(&watched, "T1", "e1", 601, Some("FRM_A"));
    seed_ref(&watched, "T1", "e2", 602, Some("FRM_B"));
    given(
        &script,
        "DOM.resolveNode",
        vec![resolve_node_ok("objA"), resolve_node_ok("objB")],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![
            fn_value(json!({ "connected": true, "x": 1.0, "y": 1.0 })),
            fn_value(json!({ "connected": true, "x": 2.0, "y": 2.0 })),
        ],
    );
    let e = svc.drag(INS, "T1", "@e1", "@e2").await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_DRAG_CROSS_FRAME", 422));
    assert_eq!(
        calls_of(&calls, "Input.dispatchMouseEvent").len(),
        0,
        "跨帧拖拽零派发"
    );
}

/// select 成功：原生 select 按 value/label 匹配
#[tokio::test]
async fn select_matches_option_and_reports() {
    let (svc, _w, calls, script) = armed();
    given(&script, "Runtime.evaluate", vec![resolve_obj_eval("objS")]);
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![fn_value(json!({ "matched": 1 }))],
    );
    let resp = svc.select(INS, "T1", "#state", "2").await.unwrap();
    assert_eq!(resp["result"], json!("Selected \"2\" on \"#state\""));
    let fns = calls_of(&calls, "Runtime.callFunctionOn");
    assert_eq!(fns.len(), 1);
    assert_eq!(
        fns[0].param("arguments"),
        json!([{ "value": ["2"] }]),
        "values 数组透传（单选 = 单元素）"
    );
    assert_eq!(
        fns[0].param("awaitPromise"),
        json!(true),
        "combobox 路径需要 await"
    );
}

/// select 错误路径：选项未命中 → 422（kit 原文列出可用项，透传不吞）
#[tokio::test]
async fn select_reports_available_options_on_miss() {
    let (svc, _w, _calls, script) = armed();
    given(&script, "Runtime.evaluate", vec![resolve_obj_eval("objS")]);
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![fn_value(json!({
            "error": "No option matched [\"9\"]. Available options: 1 (\"一\"), 2 (\"二\")"
        }))],
    );
    let e = svc.select(INS, "T1", "#state", "9").await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_SELECT_NO_OPTION", 422));
    assert!(e.to_string().contains("Available options"));
}

/// check：未勾选 → kit 坐标点击（is_checked false → click → 复查 true）
#[tokio::test]
async fn check_clicks_unchecked_box_and_verifies() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![
            resolve_obj_eval("objC"),                         // is_checked 解析
            eval_obj_response(json!({ "x": 8.0, "y": 9.0 })), // click 定位
            resolve_obj_eval("objC2"),                        // 复查解析
        ],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![
            Ok(json!({ "result": { "type": "boolean", "value": false } })), // 初始未勾
            Ok(json!({ "result": { "type": "boolean", "value": true } })),  // 点击后已勾
        ],
    );
    let resp = svc.check(INS, "T1", "#cb", false).await.unwrap();
    assert_eq!(resp["result"], json!("Checked \"#cb\""));
    assert_eq!(calls_of(&calls, "Input.dispatchMouseEvent").len(), 3);
}

/// check 错误路径 + 已勾选 no-op：未命中 → 404；已勾选 → 文案明示 already
#[tokio::test]
async fn check_already_checked_is_noop_and_reports_missing() {
    let (svc, _w, calls, script) = armed();
    given(&script, "Runtime.evaluate", vec![resolve_obj_eval("objC")]);
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![Ok(
            json!({ "result": { "type": "boolean", "value": true } }),
        )],
    );
    let resp = svc.check(INS, "T1", "#cb", false).await.unwrap();
    assert_eq!(
        resp["result"],
        json!("Checked \"#cb\" (already checked)"),
        "已勾选 = no-op 成功，文案诚实"
    );
    assert_eq!(
        calls_of(&calls, "Input.dispatchMouseEvent").len(),
        0,
        "已勾选不派发"
    );

    // 未命中 → 404
    let (svc2, _w2, calls2, script2) = armed();
    given(
        &script2,
        "Runtime.evaluate",
        vec![Ok(json!({ "result": { "type": "object" } }))], // 无 objectId
    );
    let e = svc2.check(INS, "T1", "#gone", false).await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ELEMENT_NOT_FOUND", 404));
    assert_eq!(calls_of(&calls2, "Input.dispatchMouseEvent").len(), 0);
}

/// tap：resolve + touchStart/touchEnd（无鼠标事件）
#[tokio::test]
async fn tap_dispatches_touch_sequence() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![eval_obj_response(json!({ "x": 30.0, "y": 40.0 }))],
    );
    let resp = svc.tap(INS, "T1", "#btn").await.unwrap();
    assert_eq!(resp["result"], json!("Tapped \"#btn\""));
    let touch = calls_of(&calls, "Input.dispatchTouchEvent");
    assert_eq!(
        (touch.len(), touch[0].param("type"), touch[1].param("type")),
        (2, json!("touchStart"), json!("touchEnd"))
    );
    assert_eq!(
        touch[0].param("touchPoints"),
        json!([{ "x": 30.0, "y": 40.0 }])
    );
    assert!(calls_of(&calls, "Input.dispatchMouseEvent").is_empty());
}

/// tap 错误路径：未命中 → 404，零触摸派发
#[tokio::test]
async fn tap_reports_element_not_found() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![Ok(json!({ "result": { "type": "object" } }))], // 无坐标 = 未命中
    );
    let e = svc.tap(INS, "T1", "#gone").await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ELEMENT_NOT_FOUND", 404));
    assert!(calls_of(&calls, "Input.dispatchTouchEvent").is_empty());
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

// ---------------- 工具：screenshot（W-cap；I3 内核 = kit screenshot.rs）----

/// 无 target = 整视口：kit captureScreenshot（fromSurface=true，无 clip——
/// 与 CDP 缺省 fromSurface 同义）；响应形状契约不变（format/data）。
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
    assert_eq!(
        cap[0].param("fromSurface"),
        json!(true),
        "kit 恒带 fromSurface"
    );
    assert!(cap[0].params.get("clip").is_none(), "无 target 不带 clip");
    assert!(cap[0].params.get("captureBeyondViewport").is_none());
    assert_eq!(cap[0].session.as_deref(), Some("S1"));
}

/// ref 路径（I3：kit 对象解析 → getBoundingClientRect（视口 rect）→ 文档
/// 坐标换算（帧偏移 + window scroll）→ 浮点 clip + captureBeyondViewport）。
/// **不滚动页面**（kit 文档坐标换算不依赖滚动，截图无副作用——与旧实现
/// 的 scrollIntoView 差异，报告披露）；clip 不再外扩取整（kit 浮点直传）。
#[tokio::test]
async fn screenshot_ref_clips_document_rect_without_scrolling() {
    let (svc, watched, calls, script) = armed();
    seed_ref(&watched, "T1", "e1", 101, None);
    given(&script, "DOM.resolveNode", vec![resolve_node_ok("obj101")]);
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![
            fn_value(json!(true)), // kit isConnected 检查
            // getBoundingClientRect（视口坐标）
            fn_value(json!({ "x": 10.2, "y": 20.7, "width": 100.0, "height": 119.8 })),
            // 文档偏移（帧偏移 + window scroll）
            fn_value(json!({ "x": 0.0, "y": 503.0 })),
        ],
    );
    given(
        &script,
        "Page.captureScreenshot",
        vec![Ok(json!({ "data": "aGVsbG8=" }))],
    );
    let resp = svc.screenshot(INS, "T1", Some("@e1"), false).await.unwrap();
    assert_eq!(resp["data"], json!("aGVsbG8="));
    // 不滚动（旧实现的 scrollIntoViewIfNeeded 在 kit 路径不存在）
    assert_eq!(
        calls_of(&calls, "DOM.scrollIntoViewIfNeeded").len(),
        0,
        "kit 文档坐标换算不依赖滚动"
    );
    let cap = calls_of(&calls, "Page.captureScreenshot");
    assert_eq!(cap[0].param("captureBeyondViewport"), json!(true));
    // kit 浮点 clip：视口 rect + 文档偏移（20.7 + 503 = 523.7），不取整
    assert_eq!(
        cap[0].params.get("clip"),
        Some(&json!({ "x": 10.2, "y": 523.7, "width": 100.0, "height": 119.8, "scale": 1.0 }))
    );
    assert_eq!(cap[0].param("fromSurface"), json!(true));
}

/// selector 路径（I3：kit Runtime.evaluate 对象解析，不再走 nodeId 链）；
/// 未命中（result 无 objectId）→ kit "Element not found" → 404 同码。
#[tokio::test]
async fn screenshot_selector_resolves_via_kit_and_reports_not_found() {
    let (svc, _w, calls, script) = armed();
    given(
        &script,
        "Runtime.evaluate",
        vec![Ok(
            json!({ "result": { "type": "object", "objectId": "obj7" } }),
        )],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![
            fn_value(json!({ "x": 0.0, "y": 0.0, "width": 50.0, "height": 50.0 })),
            fn_value(json!({ "x": 0.0, "y": 0.0 })), // 文档偏移
        ],
    );
    given(
        &script,
        "Page.captureScreenshot",
        vec![Ok(json!({ "data": "aGVsbG8=" }))],
    );
    svc.screenshot(INS, "T1", Some("#hero"), false)
        .await
        .unwrap();
    let ev = calls_of(&calls, "Runtime.evaluate");
    assert!(
        ev[0]
            .param("expression")
            .as_str()
            .unwrap()
            .contains("#hero"),
        "kit 页内解析脚本携带 selector"
    );
    let cap = calls_of(&calls, "Page.captureScreenshot");
    assert_eq!(
        cap[0].params.get("clip"),
        Some(&json!({ "x": 0.0, "y": 0.0, "width": 50.0, "height": 50.0, "scale": 1.0 }))
    );

    // 未命中（result 无 objectId）→ 404，且不截屏
    let (svc2, _w2, _c2, script2) = armed();
    given(
        &script2,
        "Runtime.evaluate",
        vec![Ok(json!({ "result": { "type": "undefined" } }))],
    );
    let e = svc2
        .screenshot(INS, "T1", Some(".missing"), false)
        .await
        .unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ELEMENT_NOT_FOUND", 404));
}

/// 零面积 rect（I3 行为变化，报告披露）：kit 无退化前置校验，0 高 rect
/// 原样进 clip → CDP 拒绝 → 500 PAGE_ACTION_FAILED 带原文（旧实现为 409
/// PAGE_TARGET_MOVED——截图三形态契约不变，错误码语义变化已披露）。
#[tokio::test]
async fn screenshot_zero_height_rect_fails_via_cdp_rejection() {
    let (svc, watched, _calls, script) = armed();
    seed_ref(&watched, "T1", "e1", 9, None);
    given(&script, "DOM.resolveNode", vec![resolve_node_ok("obj9")]);
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![
            fn_value(json!(true)),
            fn_value(json!({ "x": 5.0, "y": 6.0, "width": 100.0, "height": 0.0 })),
            fn_value(json!({ "x": 0.0, "y": 0.0 })),
        ],
    );
    given(
        &script,
        "Page.captureScreenshot",
        vec![Err("Unable to capture screenshot".into())],
    );
    let e = svc
        .screenshot(INS, "T1", Some("e1"), false)
        .await
        .unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ACTION_FAILED", 500));
    assert!(
        e.to_string().contains("screenshot"),
        "tool 上下文在前缀: {}",
        e
    );
}

/// kit typed 反序列化缺 data：captureScreenshot 响应无 data 字段 → kit
/// Err → 500（不装作成功，与旧实现的显式报错同语义）。
#[tokio::test]
async fn screenshot_missing_data_is_explicit_failure() {
    let (svc, _w, _calls, script) = armed();
    given(&script, "Page.captureScreenshot", vec![Ok(json!({}))]);
    let e = svc.screenshot(INS, "T1", None, false).await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_ACTION_FAILED", 500));
}

/// 空字符串 target：service 入口 400（缺省视口应传 None）。
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

// ---------------- 工具：screenshot --full（W-shot；I3 尺寸源 = kit 同源）--

/// full 未超限（I3）：我方预读（eval DPR + getLayoutMetrics cssContentSize）
/// → kit full_page（kit 内部再读一次 getLayoutMetrics 定 clip——幂等读，
/// 与预读同源 cssContentSize）。响应尺寸证据字段 = 预读值；clip 由 kit
/// 构造（0,0,w,h,scale 1）+ captureBeyondViewport。
#[tokio::test]
async fn screenshot_full_clips_whole_document_with_evidence_fields() {
    let (svc, _w, calls, script) = armed();
    given(&script, "Runtime.evaluate", vec![rt_result(json!(2))]);
    let metrics = json!({ "cssContentSize": { "width": 1280, "height": 2592 } });
    given(
        &script,
        "Page.getLayoutMetrics",
        vec![Ok(metrics.clone()), Ok(metrics)], // 我方预读 + kit 内部读
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
        Some(&json!({ "x": 0.0, "y": 0.0, "width": 1280.0, "height": 2592.0, "scale": 1.0 }))
    );
    assert_eq!(cap[0].param("format"), json!("png"));
    // DPR 读数经 tab 的 page session（不是 browser 级）
    assert_eq!(cap[0].session.as_deref(), Some("S1"));
}

/// 超限截断（DPR 感知，契约不变）：上限按设备像素 16384 换算 CSS。DPR=2 →
/// 上限 8192 CSS px：20000×9000.8 文档 → 双边截到 8192、truncated=true、
/// documentWidth/Height 保留原始读数。**此罕见路径 kit 无法表达截断 clip**
///（ScreenshotOptions 无自定义 clip 入口），保留我方 CDP 直发序列——契约
/// 优先于「全走 kit」（报告披露）。DPR=1 → 上限 16384：宽截高不截，
/// truncated 仍 true（任一边）。
#[tokio::test]
async fn screenshot_full_caps_each_side_by_dpr_and_marks_truncated() {
    let (svc, _w, calls, script) = armed();
    given(&script, "Runtime.evaluate", vec![rt_result(json!(2))]);
    given(
        &script,
        "Page.getLayoutMetrics",
        vec![Ok(
            json!({ "cssContentSize": { "width": 20000, "height": 9000.8 } }),
        )],
    );
    given(
        &script,
        "Page.captureScreenshot",
        vec![Ok(json!({ "data": "aGVsbG8=" }))],
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
    assert_eq!(
        calls_of(&calls, "Page.getLayoutMetrics").len(),
        1,
        "超限路径不走 kit（无第二次 getLayoutMetrics）"
    );

    // DPR=1：上限 16384；高 9000 未超限不截，宽 20000 截断
    let (svc2, _w2, _c2, script2) = armed();
    given(&script2, "Runtime.evaluate", vec![rt_result(json!(1))]);
    given(
        &script2,
        "Page.getLayoutMetrics",
        vec![Ok(
            json!({ "cssContentSize": { "width": 20000, "height": 9000 } }),
        )],
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

/// DPR 读数非有限正数 → 500（预读层显式报错，不静默猜测）。
#[tokio::test]
async fn screenshot_full_rejects_bad_dpr_reading() {
    let (svc, _w, _calls, script) = armed();
    given(&script, "Runtime.evaluate", vec![rt_result(Value::Null)]);
    let e = svc.screenshot(INS, "T1", None, true).await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("INTERNAL_ERROR", 500));
    assert!(e.to_string().contains("devicePixelRatio"));
}

/// full + target 互斥：service 入口 400（校验早于建连/尺寸读数，不变）。
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

// ---------------- 工具：diff（I3：快照文本 diff）--------------------------

/// 快照 mock：单按钮 AX 树（name 可变，构造两次 diff 间的变化）
fn snap_tree(name: &str) -> Result<Value, String> {
    Ok(json!({ "nodes": [
        { "nodeId": "1", "role": axv("button".into()), "name": axv(name.into()), "backendDOMNodeId": 101 },
    ]}))
}

/// durable ref 前置（同 loaderId 的 frameTree，次数 = 快照次数）：无它则
/// 编号顺延（e1→e3…），diff 的 @eN 记号漂移会掩盖文本变化断言
fn durable_frames(n: usize) -> Vec<Result<Value, String>> {
    let tree = json!({ "frameTree": { "frame": { "id": "F0", "loaderId": "L1" } } });
    (0..n).map(|_| Ok(tree.clone())).collect()
}

/// 无基线首次 diff：与空比较（一切皆 added），滚动基线入库——第二次同
/// 快照 diff → changed:false（identical 快路径）。
#[tokio::test]
async fn diff_first_run_counts_all_added_then_baseline_updates() {
    let (svc, watched, _calls, script) = armed();
    given(&script, "Page.getFrameTree", durable_frames(2));
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![snap_tree("OK"), snap_tree("OK")],
    );

    let r1 = svc
        .diff(INS, "T1", SnapParams::default(), None)
        .await
        .unwrap();
    assert_eq!(r1["instanceId"], json!(INS));
    assert_eq!(r1["changed"], json!(true), "首次（空基线）全部计为 added");
    assert_eq!(r1["counts"]["added"].as_u64().unwrap(), 1);
    assert_eq!(r1["counts"]["removed"].as_u64().unwrap(), 0);
    let added: Vec<&str> = r1["added"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        added.iter().any(|l| l.contains("button \"OK\"")),
        "added 行含快照行: {added:?}"
    );
    assert!(
        r1["diff"].as_str().unwrap().contains("+++"),
        "diff = unified 文本"
    );
    assert!(
        watched.last_snap.lock().unwrap().contains_key("T1"),
        "滚动基线入库"
    );

    // 第二次同快照：identical → changed:false，added/removed 空
    let r2 = svc
        .diff(INS, "T1", SnapParams::default(), None)
        .await
        .unwrap();
    assert_eq!(r2["changed"], json!(false));
    assert_eq!(r2["added"], json!([]));
    assert_eq!(r2["removed"], json!([]));
}

/// 变化检测：按钮 name 变化 → removed 旧行 + added 新行 + counts 对齐；
/// @eN 记号不剥（durable refs 下记号漂移即真实变化信号——I1b 决策）。
#[tokio::test]
async fn diff_detects_content_change_and_keeps_ref_tokens() {
    let (svc, watched, _calls, script) = armed();
    given(&script, "Page.getFrameTree", durable_frames(2));
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![snap_tree("OK"), snap_tree("Changed")],
    );
    svc.diff(INS, "T1", SnapParams::default(), None)
        .await
        .unwrap();
    let r2 = svc
        .diff(INS, "T1", SnapParams::default(), None)
        .await
        .unwrap();
    assert_eq!(r2["changed"], json!(true));
    assert_eq!(r2["counts"]["added"].as_u64().unwrap(), 1);
    assert_eq!(r2["counts"]["removed"].as_u64().unwrap(), 1);
    let added = r2["added"][0].as_str().unwrap();
    let removed = r2["removed"][0].as_str().unwrap();
    assert!(
        added.contains("Changed") && added.contains("@e1"),
        "{added}"
    );
    assert!(
        removed.contains("OK") && removed.contains("@e1"),
        "{removed}"
    );
    assert!(
        watched
            .last_snap
            .lock()
            .unwrap()
            .get("T1")
            .unwrap()
            .contains("Changed"),
        "基线滚动到新快照"
    );
}

/// 显式 baseline：只比较不更新滚动基线（库内仍是上次 diff 的快照）。
#[tokio::test]
async fn diff_explicit_baseline_compares_without_updating_store() {
    let (svc, watched, _calls, script) = armed();
    given(&script, "Page.getFrameTree", durable_frames(2));
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![snap_tree("Stored"), snap_tree("Now")],
    );
    // 第一次无显式：入库 Stored 快照
    svc.diff(INS, "T1", SnapParams::default(), None)
        .await
        .unwrap();
    // 第二次显式基线（Stored 文本）vs 当前（Now）→ changed:true（真实差异），
    // 但滚动基线不被 Now 覆盖——否则下次 diff 会假报 identical
    let r2 = svc
        .diff(
            INS,
            "T1",
            SnapParams::default(),
            Some("- button \"Stored\" @e1".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(r2["changed"], json!(true), "显式 Stored 基线 vs 当前 Now");
    assert_eq!(r2["counts"]["removed"].as_u64().unwrap(), 1);
    assert!(
        watched
            .last_snap
            .lock()
            .unwrap()
            .get("T1")
            .unwrap()
            .contains("Stored"),
        "显式基线不更新滚动基线"
    );
}

/// 错误路径：snap 管线失败（AX 树命令报错）→ 500 原样上抛，基线不动。
#[tokio::test]
async fn diff_snap_failure_propagates_and_keeps_baseline() {
    let (svc, watched, _calls, script) = armed();
    given(&script, "Page.getFrameTree", durable_frames(2));
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![snap_tree("OK"), Err("boom".into())],
    );
    svc.diff(INS, "T1", SnapParams::default(), None)
        .await
        .unwrap();
    let e = svc
        .diff(INS, "T1", SnapParams::default(), None)
        .await
        .unwrap_err();
    assert_eq!(e.status(), 500);
    assert!(
        watched.last_snap.lock().unwrap().contains_key("T1"),
        "失败不清洗基线"
    );
}

/// 快照超 2MiB：不入库 + 清旧基线 + baselineCleared 披露。
#[tokio::test]
async fn diff_oversized_snapshot_clears_baseline_with_disclosure() {
    let (svc, watched, _calls, script) = armed();
    let big = "x".repeat(3 * 1024 * 1024);
    given(&script, "Page.getFrameTree", durable_frames(2));
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![snap_tree("OK"), snap_tree(&big)],
    );
    svc.diff(INS, "T1", SnapParams::default(), None)
        .await
        .unwrap();
    let r2 = svc
        .diff(INS, "T1", SnapParams::default(), None)
        .await
        .unwrap();
    assert_eq!(r2["baselineCleared"], json!(true));
    assert!(
        !watched.last_snap.lock().unwrap().contains_key("T1"),
        "超限不入库且清旧基线"
    );
}

// ---------------- 工具：diff-shot（I3：像素 diff）------------------------

/// 构造纯色 png bytes（image crate 编码，与 kit diff_screenshot 同依赖）
fn solid_png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(w, h, |_, _| image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// 成功路径：同尺寸异色 → differing + ratio=100 + 差异图（红标 png）。
#[tokio::test]
async fn diff_shot_reports_differing_ratio_and_diff_image() {
    let (svc, _w, _calls, script) = armed();
    given(
        &script,
        "Page.captureScreenshot",
        vec![Ok(json!({ "data": b64(&solid_png(4, 4, [0, 0, 0])) }))],
    );
    let resp = svc
        .diff_shot(INS, "T1", false, &b64(&solid_png(4, 4, [255, 255, 255])))
        .await
        .unwrap();
    assert_eq!(resp["instanceId"], json!(INS));
    assert_eq!(resp["differing"], json!(true));
    let ratio = resp["ratio"].as_f64().unwrap();
    assert!(
        (ratio - 100.0).abs() < 0.01,
        "全像素异色 ratio=100: {ratio}"
    );
    assert!(
        resp["image"].as_str().unwrap().starts_with("iVBOR"),
        "差异图为 png base64"
    );
}

/// identical：同图 → differing:false、无 image 字段（不传无谓负载）。
#[tokio::test]
async fn diff_shot_identical_yields_no_image() {
    let (svc, _w, _calls, script) = armed();
    let png = b64(&solid_png(4, 4, [10, 20, 30]));
    given(
        &script,
        "Page.captureScreenshot",
        vec![Ok(json!({ "data": png.clone() }))],
    );
    let resp = svc.diff_shot(INS, "T1", false, &png).await.unwrap();
    assert_eq!(resp["differing"], json!(false));
    assert_eq!(resp["ratio"], json!(0.0));
    assert!(resp.get("image").is_none());
}

/// 尺寸不一致：differing + ratio=100 + dimensionMismatch 披露（不产图）。
#[tokio::test]
async fn diff_shot_dimension_mismatch_is_disclosed() {
    let (svc, _w, _calls, script) = armed();
    given(
        &script,
        "Page.captureScreenshot",
        vec![Ok(json!({ "data": b64(&solid_png(4, 4, [0, 0, 0])) }))],
    );
    let resp = svc
        .diff_shot(INS, "T1", false, &b64(&solid_png(8, 4, [0, 0, 0])))
        .await
        .unwrap();
    assert_eq!(resp["differing"], json!(true));
    assert!(resp.get("image").is_none(), "尺寸不一致无差异图");
    assert_eq!(
        resp["dimensionMismatch"],
        json!({ "expected": { "width": 8, "height": 4 }, "actual": { "width": 4, "height": 4 } }),
        "expected = baseline 尺寸，actual = 当前截图尺寸"
    );
}

/// 错误路径：baseline 空 / 非 png → 400（不触发截图）。
#[tokio::test]
async fn diff_shot_rejects_bad_baseline() {
    let (svc, _w, calls, _script) = armed();
    let e = svc.diff_shot(INS, "T1", false, "").await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400));
    assert_eq!(calls_of(&calls, "Page.captureScreenshot").len(), 0);

    let (svc2, _w2, _c2, script2) = armed();
    given(
        &script2,
        "Page.captureScreenshot",
        vec![Ok(json!({ "data": b64(&solid_png(2, 2, [0, 0, 0])) }))],
    );
    let e2 = svc2
        .diff_shot(INS, "T1", false, &b64(b"not a png at all"))
        .await
        .unwrap_err();
    assert_eq!((e2.code(), e2.status()), ("INVALID_REQUEST", 400));
    assert!(e2.to_string().contains("baseline"), "{e2}");
}

// ---------------- 工具：profiler（I3：kit tracing.rs）--------------------

/// start → stop 全链路：Tracing.start（categories 透传 + ReportEvents 采样
/// 模式）→ 事件总线注入 dataCollected/tracingComplete → Tracing.end →
/// traceEvents JSON 落盘（文件真实存在 + eventCount）。
/// 注入时序确定性：kit profiler_stop 的 subscribe 先于 Tracing.end 发送，
/// 注入任务等 Tracing.end 进入调用记录（= subscribe 已就绪）再广播事件。
#[tokio::test]
async fn profiler_start_stop_roundtrip_writes_trace_json() {
    let (svc, watched, calls, _script) = armed();
    let out_path = std::env::temp_dir()
        .join(format!("ins-page-prof-{}.json", uuid::Uuid::new_v4()))
        .to_string_lossy()
        .to_string();

    let r1 = svc
        .profiler_start(INS, "T1", Some(vec!["a.b".into(), "c.d".into()]))
        .await
        .unwrap();
    assert_eq!(r1["instanceId"], json!(INS));
    assert_eq!(r1["targetId"], json!("T1"));
    assert!(r1["result"].as_str().unwrap().contains("profiler started"));
    let start = calls_of(&calls, "Tracing.start");
    assert_eq!(
        start[0].params.pointer("/traceConfig/includedCategories"),
        Some(&json!(["a.b", "c.d"])),
        "categories 透传"
    );
    assert_eq!(start[0].param("transferMode"), json!("ReportEvents"));
    assert!(watched.profiler.lock().unwrap().is_some(), "运行态入库");

    // 事件注入任务：等 Tracing.end 记录在案（subscribe 已就绪）→ 广播事件
    let calls2 = Arc::clone(&calls);
    let bus = watched.event_bus.clone();
    let injector = tokio::spawn(async move {
        eventually("Tracing.end 发出（subscribe 已就绪）", || {
            calls2
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.method == "Tracing.end")
        })
        .await;
        let _ = bus.send(crate::application::browser_kit::CdpEvent {
            method: "Tracing.dataCollected".into(),
            params: json!({ "value": [{ "name": "a" }, { "name": "b" }] }),
            session_id: Some("S1".into()),
        });
        let _ = bus.send(crate::application::browser_kit::CdpEvent {
            method: "Tracing.tracingComplete".into(),
            params: json!({}),
            session_id: Some("S1".into()),
        });
    });
    let r2 = svc
        .profiler_stop(INS, Some(out_path.clone()))
        .await
        .unwrap();
    injector.await.unwrap();
    assert!(r2["result"].as_str().unwrap().contains("stopped"));
    assert_eq!(r2["eventCount"], json!(2));
    assert_eq!(r2["path"].as_str().unwrap(), out_path);
    let written = std::fs::read_to_string(&out_path).unwrap();
    assert!(
        written.contains("\"traceEvents\"") && written.contains("\"name\":\"a\""),
        "traceEvents JSON（serde 紧凑形态）: {}",
        &written[..written.len().min(200)]
    );
    assert!(
        watched.profiler.lock().unwrap().is_none(),
        "stop 成功后运行态终结"
    );
    let _ = std::fs::remove_file(&out_path);
}

/// 重复 start → 409 PAGE_PROFILER_ALREADY_ACTIVE（Chrome tracing 单会话资源）。
#[tokio::test]
async fn profiler_double_start_conflicts() {
    let (svc, _w, calls, _script) = armed();
    svc.profiler_start(INS, "T1", None).await.unwrap();
    let default_cats = calls_of(&calls, "Tracing.start");
    assert!(
        default_cats[0]
            .params
            .pointer("/traceConfig/includedCategories")
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.len() > 5),
        "categories 缺省 = kit 全家桶"
    );
    assert!(default_cats[0]
        .params
        .pointer("/traceConfig/enableSampling")
        .is_some_and(|v| v == &json!(true)));
    let e = svc.profiler_start(INS, "T1", None).await.unwrap_err();
    assert_eq!(
        (e.code(), e.status()),
        ("PAGE_PROFILER_ALREADY_ACTIVE", 409)
    );
}

/// 无运行中 profiler 的 stop → 409 PAGE_PROFILER_NOT_RUNNING（message 披露
/// 废弃原因路径）；tab 关闭（drop_tab 锚点匹配）后运行态废弃、stop 同码。
#[tokio::test]
async fn profiler_stop_without_running_conflicts_and_tab_drop_discards() {
    let (svc, watched, _calls, _script) = armed();
    let e = svc.profiler_stop(INS, None).await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_PROFILER_NOT_RUNNING", 409));
    assert!(
        e.to_string().contains("tab 已关闭"),
        "message 披露废弃路径: {e}"
    );

    // start 后锚点 tab 关闭 → 运行态废弃（session 死，事件流无处可收）
    svc.profiler_start(INS, "T1", None).await.unwrap();
    assert!(watched.profiler.lock().unwrap().is_some());
    drop_tab(&watched, "T1");
    assert!(
        watched.profiler.lock().unwrap().is_none(),
        "锚点 tab 关闭 → 运行态废弃"
    );
    let e2 = svc.profiler_stop(INS, None).await.unwrap_err();
    assert_eq!((e2.code(), e2.status()), ("PAGE_PROFILER_NOT_RUNNING", 409));
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
/// frameAttached → iframe attach 认领 → frameDetached 摘除登记（I1b 起不
/// 标记 ref 失效：durable refs，帧死亡由解析链自然判定）；attach 先于
/// frameAttached 的孤儿路径同样收敛
#[tokio::test]
async fn frame_registry_lifecycle_via_events() {
    let (svc, calls, _script, tap) = fake_service();
    let (watched, g) = svc.ensure(INS).await.unwrap();
    drop(g);
    // 预置 ref（帧条目 + 主条目），验证事件驱动的登记维护不动 refs（durable）
    {
        use crate::application::browser_kit::element::RefMap;
        let mut m = RefMap::new();
        m.add_with_frame("e1".to_string(), Some(101), "button", "OK", None, None);
        m.add_with_frame(
            "e2".to_string(),
            Some(202),
            "button",
            "提交",
            None,
            Some("FRM_A"),
        );
        watched.refs.lock().unwrap().insert("TAB1".into(), m);
    }

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

    // frameDetached：登记摘除；refs 保持 durable（条目不动，stale 由解析判定）
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
    let m = refs.get("TAB1").unwrap();
    assert!(
        m.get("e1").is_some() && m.get("e2").is_some(),
        "refs 不随帧摘除被动失效"
    );
}

/// frameNavigated：子帧 → 仅该帧条目 stale；根帧 → 整表失效 + 帧登记清空
/// I1b durable 语义下的帧导航：子帧导航只换该帧文档（帧表登记不动、refs
/// 不失效）；根导航重建帧表（root 先清后写）+ 清 sheets，refs 同样不动。
#[tokio::test]
async fn frame_navigation_scopes_invalidation() {
    let (svc, _calls, _script, tap) = fake_service();
    let (watched, g) = svc.ensure(INS).await.unwrap();
    drop(g);
    {
        use crate::application::browser_kit::element::RefMap;
        let mut m = RefMap::new();
        m.add_with_frame("e1".to_string(), Some(101), "button", "OK", None, None);
        m.add_with_frame(
            "e2".to_string(),
            Some(202),
            "button",
            "提交",
            None,
            Some("FRM_A"),
        );
        watched.refs.lock().unwrap().insert("TAB1".into(), m);
    }
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

    // 子帧导航：登记与 refs 全部保持（durable——旧 ref 是否可用由解析判定）
    tx.send(json!({ "method": "Page.frameNavigated", "sessionId": "SES1",
            "params": { "frame": { "id": "FRM_A", "parentId": "F_ROOT", "url": "https://b/x2" } } })).unwrap();
    eventually("子帧导航后登记仍在", || {
        watched
            .frames
            .lock()
            .unwrap()
            .get("TAB1")
            .map(|t| t.frames.contains_key("FRM_A"))
            .unwrap_or(false)
    })
    .await;
    {
        let refs = watched.refs.lock().unwrap();
        let m = refs.get("TAB1").unwrap();
        assert!(
            m.get("e1").is_some() && m.get("e2").is_some(),
            "durable：子帧导航不动 refs"
        );
    }

    // 根导航：帧登记清空 + sheets 清空（hover 共存）；refs 仍不动
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
    {
        let refs = watched.refs.lock().unwrap();
        let m = refs.get("TAB1").unwrap();
        assert!(
            m.get("e1").is_some(),
            "durable：根导航也不动 refs（stale 由解析判定）"
        );
    }
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
/// snap 合并（I1b：kit 帧递归）：Iframe 节点行下内嵌帧子树（缩进 +2），
/// @eN 跨帧连续编号（主树先、帧树后）；RefMap 条目携带 frame_id（主树条目
/// 无）；帧 AX 拉取走帧会话（OOPIF），主树走主会话。
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
        { "nodeId": "1", "role": axv("WebArea".into()), "childIds": [2, 3] },
        { "nodeId": "2", "role": axv("Iframe".into()), "name": axv("跨域表单".into()), "backendDOMNodeId": 501 },
        { "nodeId": "3", "role": axv("button".into()), "name": axv("OK".into()), "backendDOMNodeId": 101 },
    ]});
    let frame_tree = json!({ "nodes": [
        { "nodeId": "f1", "role": axv("WebArea".into()), "childIds": ["f2", "f3"] },
        { "nodeId": "f2", "role": axv("button".into()), "name": axv("提交".into()), "backendDOMNodeId": 601 },
        { "nodeId": "f3", "role": axv("textbox".into()), "name": axv("用户名".into()), "backendDOMNodeId": 602 },
    ]});
    // kit 调用序：主树 getFullAXTree（S1）→ Iframe describeNode（帧 id 解析）
    // → 帧树 getFullAXTree（S_FRM_A）——FakeCdp 按队列序消耗
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(main_tree), Ok(frame_tree)],
    );
    given(
        &script,
        "DOM.describeNode",
        vec![Ok(
            json!({ "node": { "contentDocument": { "frameId": "FRM_A" } } }),
        )],
    );

    let resp = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
    let text = resp["text"].as_str().unwrap();
    // Iframe 节点行 + 帧内容缩进内嵌（kit 上游格式 + @eN 后处理）
    assert!(
        text.contains("- Iframe \"跨域表单\" @e1"),
        "挂接点（Iframe 节点）行存在:\n{text}"
    );
    assert!(
        text.contains("  - button \"提交\" @e3"),
        "帧内按钮缩进 +2 内嵌:\n{text}"
    );
    assert!(
        text.contains("- button \"OK\" @e2"),
        "主树按钮与 Iframe 同批编号（主树先、帧树后）:\n{text}"
    );
    assert_eq!(
        resp["refs_count"], 4,
        "Iframe + 帧 button + 帧 textbox + 主 button"
    );

    let refs = watched.refs.lock().unwrap();
    let m = refs.get("T1").unwrap();
    let e3 = m.get("e3").unwrap();
    assert_eq!(e3.backend_node_id, Some(601));
    assert_eq!(e3.frame_id.as_deref(), Some("FRM_A"));
    assert_eq!(m.get("e2").unwrap().frame_id, None, "主树条目无帧维度");
    drop(refs);
}

/// click 帧内 ref：元素 quad 在帧会话解析 → owner quad 在主会话解析 →
/// OOPIF 帧本地坐标——浏览器按帧会话路由，取代旧的根视口投影链）。
#[tokio::test]
async fn click_ref_in_frame_resolves_and_dispatches_on_frame_session() {
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
    seed_ref(&watched, "T1", "e1", 601, Some("FRM_A"));
    given(&script, "DOM.resolveNode", vec![resolve_node_ok("obj601")]);
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![fn_value(json!({ "connected": true, "x": 60.0, "y": 40.0 }))],
    );
    // kit click 的顶层坐标投影：帧内容 quad 首点 (10, 20) = 帧偏移
    given(
        &script,
        "DOM.getFrameOwner",
        vec![Ok(json!({ "backendNodeId": 501 }))],
    );
    given(
        &script,
        "DOM.getBoxModel",
        vec![Ok(
            json!({ "model": { "content": [10.0, 20.0, 110.0, 20.0, 110.0, 60.0, 10.0, 60.0] } }),
        )],
    );

    let resp = svc.click(INS, "T1", "@e1", false, false).await.unwrap();
    assert_eq!(
        resp["result"],
        json!("Clicked @e1 \"OK\" at CSS (70, 60)"),
        "结果坐标 = 帧本地 (60, 40) + 帧偏移 (10, 20)（I2：文案坐标基准改为顶层视口）"
    );
    let resolve = calls_of(&_calls, "DOM.resolveNode");
    assert_eq!(
        resolve[0].session.as_deref(),
        Some("S_FRM_A"),
        "resolveNode 在帧会话（kit 帧路由）"
    );
    let inputs = calls_of(&_calls, "Input.dispatchMouseEvent");
    assert_eq!(inputs.len(), 3, "kit ClickPhase 三事件");
    assert_eq!(
        inputs[0].param("x"),
        json!(60.0),
        "Input 坐标 = 帧本地（发帧会话）"
    );
    assert!(
        inputs
            .iter()
            .all(|r| r.session.as_deref() == Some("S_FRM_A")),
        "Input 派发在 kit 返回的帧会话"
    );
}

/// 事件路径登记的帧内点击投影（不用 register_frame 预设 root——它总把
/// root 填好，掩住了生产路径 root 丢失的缺陷）：(1) root 经 frameNavigated
/// 写入，frameAttached 登记 parent=主帧 id → pf == root 终止；(2) root
/// 未知（attach 到已加载页，连接后从未见 frameNavigated——SPA 动态注入
/// iframe 的常态）→ 兜底「parent 未登记在帧表 = 主文档直属」终止。
/// 供 kit 帧路由消费——不依赖 register_frame 预设。
#[tokio::test]
async fn click_frame_ref_via_event_path_registered_frame() {
    let (svc, watched, _calls, script) = armed();
    handle_event(
        &watched,
        &json!({ "method": "Page.frameNavigated", "sessionId": "S1",
            "params": { "frame": { "id": "F_ROOT" } } }),
    )
    .await;
    handle_event(
        &watched,
        &json!({ "method": "Page.frameAttached", "sessionId": "S1",
            "params": { "frameId": "FRM_B", "parentFrameId": "F_ROOT_UNSEEN" } }),
    )
    .await;
    handle_event(&watched, &attach_event("FRM_B", "S_FRM_B", "iframe")).await;
    seed_ref(&watched, "T1", "e2", 602, Some("FRM_B"));
    given(&script, "DOM.resolveNode", vec![resolve_node_ok("obj602")]);
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![fn_value(json!({ "connected": true, "x": 30.0, "y": 20.0 }))],
    );
    // 投影链走查：owner 未登记在帧表 → getFrameOwner 探测（quad 首点 0,0）
    given(
        &script,
        "DOM.getFrameOwner",
        vec![Ok(json!({ "backendNodeId": 602 }))],
    );
    given(
        &script,
        "DOM.getBoxModel",
        vec![Ok(
            json!({ "model": { "content": [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0] } }),
        )],
    );

    let resp = svc.click(INS, "T1", "@e2", false, false).await.unwrap();
    assert_eq!(
        resp["result"],
        json!("Clicked @e2 \"OK\" at CSS (30, 20)"),
        "事件路径登记的帧（root 未知）同样可解析，不 409"
    );
}

/// fill 帧内 ref：解析/检查/回读（DOM/Runtime 命令）路由到 frame session；
/// kit fill 的 insertText 发 page 级会话（渲染进程按焦点路由——I2 语义）
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
    seed_ref_as(
        &watched,
        "T1",
        "e1",
        602,
        Some("FRM_A"),
        "textbox",
        "用户名",
    );
    // 我方解析 + kit fill 内部解析（各一次 resolveNode + isConnected）
    given(
        &script,
        "DOM.resolveNode",
        vec![resolve_node_ok("objF"), resolve_node_ok("objF2")],
    );
    given(
        &script,
        "Runtime.callFunctionOn",
        vec![
            fn_value(json!(true)), // 我方解析的 isConnected
            fn_value(json!({ "tag": "INPUT", "kind": "native", "label": "用户名" })), // 可填检查
            fn_value(json!(true)), // kit fill 解析的 isConnected
            Ok(json!({ "result": { "type": "object" } })), // kit focus
            Ok(json!({ "result": { "type": "object" } })), // kit clear
            fn_value(json!({ "matches": true })), // 回读校验
        ],
    );

    let resp = svc.fill(INS, "T1", "@e1", "vincent").await.unwrap();
    assert_eq!(
        resp["result"],
        json!("Filled <INPUT> \"用户名\" with \"vincent\" (7 chars)")
    );
    let resolve = calls_of(&_calls, "DOM.resolveNode");
    assert_eq!(resolve.len(), 2, "我方解析 + kit fill 内部解析各一次");
    assert_eq!(
        resolve[0].session.as_deref(),
        Some("S_FRM_A"),
        "resolveNode 在帧会话"
    );
    // 我方 call_fn（检查/回读）在帧会话；kit focus/clear 同样（effective
    // session）；唯 insertText 在 page 级会话
    let inserts = calls_of(&_calls, "Input.insertText");
    assert_eq!(inserts.len(), 1);
    assert_eq!(
        inserts[0].session.as_deref(),
        Some("S1"),
        "kit fill 的 insertText 发 page 级会话（I2 语义差异，提交信息披露）"
    );
    let release = calls_of(&_calls, "Runtime.releaseObject");
    assert_eq!(
        (release.len(), release[0].session.as_deref()),
        (1, Some("S_FRM_A")),
        "对象归还到解析它的同一会话"
    );
}

/// 帧级失效后的 ref 使用 → 409 PAGE_REF_STALE（曾存在、已随帧死亡，
///（曾存在、解析失败——语义区别于 NOT_FOUND；I1b 起由解析链产生）
#[tokio::test]
async fn stale_frame_ref_reports_ref_stale() {
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
    seed_ref(&watched, "T1", "e2", 601, Some("FRM_A"));
    // 帧内旧节点：resolveNode 失败（帧已换文档）→ AX 恢复无果 → stale
    given(
        &script,
        "DOM.resolveNode",
        vec![Err("No node with given id found".into())],
    );
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(json!({ "nodes": [] }))],
    );
    let err = svc.click(INS, "T1", "@e2", false, false).await.unwrap_err();
    assert_eq!((err.status(), err.code()), (409, "PAGE_REF_STALE"));
    // 未知 ref 仍 404（NOT_FOUND 与 STALE 的区分保留）
    let err2 = svc
        .click(INS, "T1", "@e99", false, false)
        .await
        .unwrap_err();
    assert_eq!(err2.code(), "PAGE_REF_NOT_FOUND", "未知 ref 仍 404");
}

/// sync 帧树主动路径（真机打磨核心）：未注入登记时，snap 内联完成
/// getTargets 发现 → getFrameOwner 归属（owner 校验：主树 bni 必须是
/// iframe 类节点，防死帧/异页污染）→ attachToTarget 认领会话 → 帧树
/// sync 帧树主动路径（W-oopif 保留）：未注入登记时，snap 内联完成
/// getTargets 发现 → getFrameOwner 归属 → attachToTarget 认领会话 →
/// kit 帧递归合并。I1b：owner 校验（主树 bni 必须是 iframe 类节点）随 AX
/// 中间树删除移交 kit——死帧登记不再被拒，但表现为惰性（快照递归按 AX 树
/// 的 Iframe 节点发起，死帧不在树中不会被请求；iframe_sessions 只含带
/// 会话的活帧）。
#[tokio::test]
async fn snap_syncs_frame_registry_via_target_probe() {
    let (svc, watched, _calls, script) = armed();
    let main_tree = json!({ "nodes": [
        { "nodeId": "1", "role": axv("WebArea".into()), "childIds": [2, 3] },
        { "nodeId": "2", "role": axv("Iframe".into()), "name": axv("跨域".into()), "backendDOMNodeId": 10 },
        { "nodeId": "3", "role": axv("button".into()), "name": axv("OK".into()), "backendDOMNodeId": 9 },
    ]});
    let frame_tree = json!({ "nodes": [
        { "nodeId": "f1", "role": axv("WebArea".into()), "childIds": ["f2"] },
        { "nodeId": "f2", "role": axv("button".into()), "name": axv("帧按钮".into()), "backendDOMNodeId": 600 },
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
    // 归属探测：FRM_SYNC / FRM_DEAD 都命中主会话（返回 bni）
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
    // kit 调用序：主树（S1）→ Iframe describeNode → 帧树（S_FRM_SYNC）
    given(
        &script,
        "Accessibility.getFullAXTree",
        vec![Ok(main_tree), Ok(frame_tree)],
    );
    given(
        &script,
        "DOM.describeNode",
        vec![Ok(
            json!({ "node": { "contentDocument": { "frameId": "FRM_SYNC" } } }),
        )],
    );

    let resp = svc.snap(INS, "T1", SnapParams::default()).await.unwrap();
    // Iframe(e1) + 主按钮(e2) + 帧按钮(e3)
    assert_eq!(resp["refs_count"], 3);
    let text = resp["text"].as_str().unwrap();
    assert!(text.contains("- Iframe \"跨域\" @e1"), "{text}");
    assert!(text.contains("  - button \"帧按钮\" @e3"), "{text}");
    assert!(text.contains("- button \"OK\" @e2"), "{text}");

    // 登记：FRM_SYNC 会话/owner 就位；FRM_DEAD（死帧）也登记但无会话
    //（attach 未脚本化第二轮 → 无 sessionId；惰性残留，targetDestroyed 摘除）
    let tables = watched.frames.lock().unwrap();
    let table = tables.get("T1").unwrap();
    let synced = table.frames.get("FRM_SYNC").unwrap();
    assert_eq!(synced.session_id.as_deref(), Some("S_FRM_SYNC"));
    assert_eq!(synced.owner_backend_node_id, Some(10));
    let dead = table.frames.get("FRM_DEAD").unwrap();
    assert!(
        dead.session_id.is_none(),
        "死帧登记为惰性（无会话，不参与路由）"
    );
    drop(tables);
    // RefMap 条目带帧维度（帧按钮 e3 属 FRM_SYNC）
    let refs = watched.refs.lock().unwrap();
    let m = refs.get("T1").unwrap();
    assert_eq!(m.get("e3").unwrap().frame_id.as_deref(), Some("FRM_SYNC"));
    assert!(m.get("e2").unwrap().frame_id.is_none());
}

/// nodeId 碰撞回归（owner 复测缺陷）：主树与帧树的 nodeId 是两套独立
/// 计数（都从 1 起）——裸键共享 visited 时帧节点被误判重复剪掉，
/// 帧内容**稳定**整体缺席（id 全碰撞形态）。修复 = 帧树 visited 键加

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
    seed_ref(&watched, "T1", "e1", 42, None);
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

// ---------------- 工具：record（I4：kit recording.rs）--------------------

use crate::application::browser_kit::recording::{
    CaptureSession, RecordingOptions, RecordingState,
};
use crate::application::page_service::recording_tool::record_cursor;

/// 手工武装「录制中」的 Watched（绕过 ffmpeg/attach 链——豁免与钩子逻辑
/// 单测化；capture_sid None = 模拟 attach in-flight（slot 只有 target_id））
fn arm_recording(watched: &Watched, tid: &str, capture_sid: Option<&str>) {
    let mut state = RecordingState::new();
    crate::application::browser_kit::recording::recording_start(
        &mut state,
        "/tmp/i4-exempt.webm",
        RecordingOptions::default(),
    )
    .unwrap();
    *state.capture_session.lock().unwrap() = Some(CaptureSession {
        target_id: tid.to_string(),
        session_id: capture_sid.map(str::to_string),
    });
    *watched.recording.lock().unwrap() = Some(RecordingSlot {
        state,
        tid: tid.to_string(),
    });
}

/// start 校验链全前置：无扩展名 / fps 越界 → 400（kit 原文透传）；槽位
/// 占用 → 409 ALREADY_ACTIVE——三者均不动浏览器（无 attach 命令发出）。
#[tokio::test]
async fn record_start_validation_chain_rejects_before_io() {
    let (svc, _watched, calls, _script) = armed();

    let e1 = svc
        .record_start(INS, "T1", "/tmp/noext", None, false)
        .await
        .unwrap_err();
    assert_eq!((e1.code(), e1.status()), ("INVALID_REQUEST", 400));
    assert!(e1.to_string().to_lowercase().contains("extension"), "{e1}");

    let e2 = svc
        .record_start(INS, "T1", "/tmp/ok.webm", Some(61), false)
        .await
        .unwrap_err();
    assert_eq!((e2.code(), e2.status()), ("INVALID_REQUEST", 400));
    assert!(e2.to_string().to_lowercase().contains("fps"), "{e2}");

    assert!(
        calls_of(&calls, "Target.attachToTarget").is_empty(),
        "校验链失败不得发任何浏览器 IO"
    );

    let (svc2, watched2, calls2, _s2) = armed();
    arm_recording(&watched2, "T1", Some("CAP0"));
    let e3 = svc2
        .record_start(INS, "T1", "/tmp/ok.webm", None, false)
        .await
        .unwrap_err();
    assert_eq!(
        (e3.code(), e3.status()),
        ("PAGE_RECORD_ALREADY_ACTIVE", 409)
    );
    assert!(
        calls_of(&calls2, "Target.attachToTarget").is_empty(),
        "互斥检查先于 ffmpeg/attach"
    );
}

/// 无运行中录制的 stop → 409 PAGE_RECORD_NOT_RUNNING（message 披露废弃
/// 路径，同 profiler 文案风格）
#[tokio::test]
async fn record_stop_without_running_conflicts() {
    let (svc, _watched, _calls, _script) = armed();
    let e = svc.record_stop(INS, false).await.unwrap_err();
    assert_eq!((e.code(), e.status()), ("PAGE_RECORD_NOT_RUNNING", 409));
    assert!(e.to_string().contains("连接重建"), "{e}");
}

/// attach/detach 豁免（I4 重点）：capture session 的事件不得进 tabs 生命周期
/// ——attachedToTarget 不得覆写锚点 tab 的 session（否则后续命令全路由到
/// capture session）、detachedFromTarget 不得误删锚点 tab。两层判定：
/// kit owns_attachment（slot 有 sid / in-flight 无 sid）+ 豁免表（stop 后
/// slot 已清的窗口，消费式摘除）。对照组：普通 tab 生命周期不受影响。
#[tokio::test]
async fn record_capture_session_events_exempt_from_tab_lifecycle() {
    let (_svc, watched, _calls, _script) = armed();

    // 形态 1：slot 已有 capture sessionId（attach 响应已回）
    arm_recording(&watched, "T1", Some("CAP1"));
    handle_event(&watched, &attach_event("T1", "CAP1", "page")).await;
    assert_eq!(
        tab_session(&watched, "T1").as_deref(),
        Some("S1"),
        "capture session attach 不得覆写锚点 tab 的 page session"
    );

    // 形态 2：attach in-flight（slot 只有 target_id，Chrome 先发事件再回响应）
    {
        let mut guard = watched.recording.lock().unwrap();
        let slot = guard.as_mut().unwrap();
        let mut cap = slot.state.capture_session.lock().unwrap();
        if let Some(c) = cap.as_mut() {
            c.session_id = None;
        }
    }
    handle_event(&watched, &attach_event("T1", "CAP2", "page")).await;
    assert_eq!(
        tab_session(&watched, "T1").as_deref(),
        Some("S1"),
        "in-flight capture attach 按 target 匹配豁免"
    );

    // 形态 3：stop 后（kit slot 已清）在途 detach 事件——豁免表是唯一防线
    watched.recording.lock().unwrap().take();
    watched
        .capture_sessions
        .lock()
        .unwrap()
        .insert("CAP2".into(), "T1".into());
    handle_event(
        &watched,
        &json!({
            "method": "Target.detachedFromTarget",
            "params": { "targetId": "T1", "sessionId": "CAP2" }
        }),
    )
    .await;
    assert_eq!(
        tab_session(&watched, "T1").as_deref(),
        Some("S1"),
        "capture session detach（带锚点 targetId）不得误删整 tab"
    );
    assert!(
        !watched
            .capture_sessions
            .lock()
            .unwrap()
            .contains_key("CAP2"),
        "豁免条目由 detach 事件消费式摘除"
    );

    // 对照组：普通 tab 的 attach/detach 生命周期不受豁免影响
    handle_event(&watched, &attach_event("TABX", "SX", "page")).await;
    assert_eq!(tab_session(&watched, "TABX").as_deref(), Some("SX"));
    handle_event(
        &watched,
        &json!({
            "method": "Target.detachedFromTarget",
            "params": { "targetId": "TABX", "sessionId": "SX" }
        }),
    )
    .await;
    assert!(
        tab_session(&watched, "TABX").is_none(),
        "普通 tab 关闭照常摘除"
    );
}

/// --cursor 采样钩子：仅 cursor 录制中积累（kit record 的 enabled 判定），
/// 无录制时零副作用；at() 读最近样本（buttons/坐标透传）。
#[tokio::test]
async fn record_cursor_samples_only_during_cursor_recording() {
    let (_svc, watched, _calls, _script) = armed();
    // 无录制：零开销路径（一次 lock 读 Option），不 panic 不采样
    record_cursor(&watched, 1.0, 2.0, 0);

    let mut state = RecordingState::new();
    crate::application::browser_kit::recording::recording_start(
        &mut state,
        "/tmp/i4-cursor.webm",
        RecordingOptions {
            cursor: true,
            ..Default::default()
        },
    )
    .unwrap();
    *watched.recording.lock().unwrap() = Some(RecordingSlot {
        state,
        tid: "T1".into(),
    });
    record_cursor(&watched, 10.0, 20.0, 1);
    let guard = watched.recording.lock().unwrap();
    let slot = guard.as_ref().unwrap();
    let cur = slot
        .state
        .shared_cursor
        .lock()
        .unwrap()
        .at(crate::application::browser_kit::recording::cursor_timestamp());
    assert_eq!(
        (cur.x, cur.y, cur.buttons, cur.visible),
        (10.0, 20.0, 1, true)
    );
}

/// ffmpeg 是否可用（真机链路测试的软门槛：缺二进制则 skip 不 fail——
/// cargo test 任何环境必须可绿）
fn ffmpeg_present() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// start → stop 全链路（需本机 ffmpeg，缺则 skip）：getTargetInfo →
/// attachToTarget（capture session）→ 初始帧（viewport + 截图）→ spawn
/// 常驻采集（startScreencast）→ stop：cancel → teardown（stopScreencast/
/// detachFromTarget）→ ffmpeg 收尾写产物。锚点 tab 的 session 全程不被
/// capture session 覆写（豁免在真实事件时序下生效）。
#[tokio::test]
async fn record_start_stop_roundtrip_writes_video() {
    if !ffmpeg_present() {
        eprintln!("[skip] 本机无 ffmpeg，record 全链路测试跳过（真机门槛由 owner 验证）");
        return;
    }
    let (svc, watched, calls, script) = armed();
    let out_path = std::env::temp_dir()
        .join(format!("ins-page-rec-{}.webm", uuid::Uuid::new_v4()))
        .to_string_lossy()
        .to_string();

    given(
        &script,
        "Target.getTargetInfo",
        vec![Ok(json!({ "targetInfo": { "targetId": "T1" } }))],
    );
    given(
        &script,
        "Target.attachToTarget",
        vec![Ok(json!({ "sessionId": "CAP1" }))],
    );
    given(
        &script,
        "Runtime.evaluate",
        vec![Ok(
            json!({ "result": { "type": "object", "value": [1280.0, 800.0, 2.0] } }),
        )],
    );
    given(
        &script,
        "Page.captureScreenshot",
        vec![Ok(json!({ "data": b64(&solid_png(8, 8, [10, 20, 30])) }))],
    );

    let r1 = svc
        .record_start(INS, "T1", &out_path, Some(24), false)
        .await
        .unwrap();
    assert_eq!(r1["instanceId"], json!(INS));
    assert_eq!(r1["targetId"], json!("T1"));
    assert!(r1["result"].as_str().unwrap().contains("recording started"));
    assert_eq!(r1["fps"], json!(24), "显式 fps 生效");
    assert!(watched.recording.lock().unwrap().is_some(), "运行态入库");
    assert_eq!(
        watched
            .capture_sessions
            .lock()
            .unwrap()
            .get("CAP1")
            .map(String::as_str),
        Some("T1"),
        "豁免表已登记 capture session"
    );

    eventually("常驻采集任务已发 startScreencast", || {
        !calls_of(&calls, "Page.startScreencast").is_empty()
    })
    .await;

    let r2 = svc.record_stop(INS, true).await.unwrap();
    assert!(r2["result"].as_str().unwrap().contains("stopped"));
    assert_eq!(r2["path"].as_str().unwrap(), out_path);
    assert!(
        r2["frames"].as_u64().unwrap_or(0) >= 1,
        "初始播种帧至少 1 帧: {}",
        r2["frames"]
    );
    assert!(r2["status"].is_object(), "--status 附完整终态");
    assert!(
        watched.recording.lock().unwrap().is_none(),
        "stop 后槽位清空"
    );
    let bytes = std::fs::read(&out_path).unwrap();
    assert!(!bytes.is_empty(), "ffmpeg 产物非空");
    // stop 后在途 detach 事件（真实时序里 Chrome 在 detach 响应前后发出）：
    // 豁免表此时仍持有条目（消费式），不得误删锚点 tab
    handle_event(
        &watched,
        &json!({
            "method": "Target.detachedFromTarget",
            "params": { "targetId": "T1", "sessionId": "CAP1" }
        }),
    )
    .await;
    assert_eq!(
        tab_session(&watched, "T1").as_deref(),
        Some("S1"),
        "stop 后在途 detach 不得误删锚点 tab（豁免表竞态防御）"
    );
}
