//! page 控制服务编排层（P2-B）：实例级常驻 CDP 连接 + per-tab 状态 + L2 工具编排。
//!
//! 行为规格 = Phase 1 的 TS per-tab daemon（`skills/chrome-host/scripts/lib/daemon.mjs`
//! 及其 tools/*，183 单测验证）。与 TS 架构的差异是刻意的（PAGE-CONTROL-PLAN
//! Phase 2 的 owner 决策）：
//!
//! - **实例级一条连接**（决策 1）：与 diagnostics 的常驻连接并存（Chrome 侧两条
//!   客户端互不干扰），但**不共用**——维持 diagnostics「只订阅不操作」纪律。
//!   连接惰性建立（决策 5）：实例首次 page 命令时 connect_eventing →
//!   Target.setAutoAttach(flatten) → spawn watch_loop；WS 断连（EventSink recv
//!   None，与 diagnostics 同信号）时清理登记，下一条命令重建。
//! - **targetId 路由**（决策 2）：autoAttach 的 attachedToTarget 事件维护
//!   `targetId → sessionId` 映射；命令入参的 targetId 支持唯一前缀（对齐 TS
//!   resolvePrefix：无匹配 404 / 多匹配 409）。空 target 不参与前缀匹配
//!   ——唯一 page target 自动选中，多个 409 并列出全部 targetId（P2-F F1，
//!   真机复测后放弃「最近 attach」近似）。
//! - **per-instance 命令串行**（决策 3）：一条 tokio Mutex 串行整实例的全部
//!   page 命令（含建连），正确性优先；并发需求出现后可演进为 per-tab 锁。
//! - **per-tab RefStore / DialogBuffer**（决策 4）：snap 整表替换，导航/文档
//!   替换事件按 targetId 定向失效（§3.5/§3.6）。
//!
//! IO 全部经 [`PageCdp`] trait（infrastructure/cdp/page_cdp.rs 的 dyn 适配），
//! 测试注入 fake 即可覆盖全部编排路径，不依赖真 Chrome。
//!
//! 与 TS 的已知偏差（详见各处注释及任务报告）：
//! - 对话框应答与命令共用一条连接且天然并发（P2-F 根治）：ws.rs 的 send
//!   由 &mut self 改 &self（写帧短锁 + AtomicU64 id），in-flight 工具命令
//!   （如 click 卡在被 alert 阻塞的页面至 15s 超时）不再阻塞同连接的
//!   Page.handleJavaScriptDialog。真机曾证伪的中转方案：旁路短连接——CDP
//!   sessionId 是**连接级作用域**，携主连接 sessionId 必 -32001
//!   Session not found，已回滚。
//! - dialogs_seen 仅附在成功响应上（TS 对错误响应也附；AppError 无扩展字段，
//!   P2-C 若需可扩展错误负载）。
//! - focus 类错误码（PAGE_FOCUS_FAILED / PAGE_FILL_FOCUS_LOST）与
//!   PAGE_TAB_AMBIGUOUS / PAGE_TOOL_TIMEOUT 为本层新增：P2-A 错误表未含 TS 的
//!   focus_failed / fill_focus_lost（domain 已冻结，编排在 application 层补齐）。
//! - **L2 attach 握手窗口**（冒烟遗留）：连接建立后首批 attachedToTarget 异步
//!   到达，立即执行首条命令会 PAGE_TAB_NOT_FOUND——ensure 的建连分支在返回
//!   前短轮询 tabs 表（≤300ms / 10ms 间隔）等首个 page target 入表；超时也
//!   放行（保留 retry shortly 兜底），连接已在位的命令不经过这段（不阻塞）。
//! - **L3 重连对话框盲探测**（冒烟遗留）：CDP 不重播 javascriptDialogOpening，
//!   重连前已挂着的 JS 对话框对新连接失明（tab 永久卡死）——on_attached 对
//!   每个 page session 在 Page.enable 后盲发一次 handleJavaScriptDialog，
//!   成功 = 历史对话框已被接管（保守默认入缓冲），失败静默（无对话框为常态）。

// P2-C 已接线（api/pages.rs + api/mcp.rs）：仅测试注入构造 with_connector
// 仍无生产调用方，逐项 allow（不再整模块豁免，防新死代码静默滑入）。
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::domain::instance::InstanceStatus;
use crate::domain::page_control::{
    dialog_accept_params, fill_fillable_check_expr, hit_test_expr, largest_quad_centroid,
    locate_expr, normalize_ref, observed_matches_state, parse_dialog_opening, parse_duration,
    parse_key, wait_observe_expr, DialogBuffer, DialogInfo, ObservedState, PageControlError, Point,
    RefStore, WaitState, MAX_DIALOG_BUFFER,
};
use crate::error::AppError;
use crate::infrastructure::cdp::page_cdp::{connect_page, SharedPageConn};
use crate::infrastructure::cdp::ws::EventSink;
use crate::infrastructure::db::repositories::instance_repository::InstanceRepository;

/// 工具级总超时：单条 CDP send 已有内建 15s（ws.rs），这里兜住多命令链与
/// 无响应页面（wait 例外：用户 timeout + 5s）。
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
const WAIT_TOOL_GRACE: Duration = Duration::from_secs(5);
/// wait 轮询缺省超时与间隔（TS wait.mjs 默认；间隔固定起步，不做参数）
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// mousePressed → mouseReleased 的间隔（TS dispatchClickAtPoint 的 50ms）
const CLICK_RELEASE_DELAY: Duration = Duration::from_millis(50);
/// loadall 缺省轮询间隔（TS loadAllStr 的 1500ms）
const DEFAULT_LOADALL_INTERVAL: Duration = Duration::from_millis(1500);
/// loadall 缺省点击上限。TS 是 5min 硬顶（200 × 1500ms = 5min，量级对齐），
/// max-clicks × interval 组合等价但更可控（任务决策，报告披露）。
const DEFAULT_LOADALL_MAX_CLICKS: u32 = 200;
/// L2 attach 握手窗口上界（owner 真机冒烟定调）：establish 完成（连接 +
/// setAutoAttach + watch_loop 就位）后首批 attachedToTarget 仍异步在途，立即
/// 路由首条命令会 PAGE_TAB_NOT_FOUND。ensure 的建连分支在返回前短轮询 tabs
/// 表，首个 page target 入表即放行；窗口耗尽也放行（不报错——兜底仍是
/// PAGE_TAB_NOT_FOUND 的 retry shortly 语义）。300ms 依据：真机首条 attach
/// 事件观测为毫秒级偶发延迟，300ms 覆盖冷启动抖动仍远低于可感知卡顿；
/// 命令预算 30s，占用可忽略。
const ATTACH_HANDSHAKE_WINDOW: Duration = Duration::from_millis(300);
/// 握手窗口的轮询间隔：10ms 在「attach 到表即响应」与「空转次数」间折中
///（300ms 窗口至多 30 次查询，tabs 表是个位数行的 Vec 线性判空）。
const ATTACH_HANDSHAKE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// 连接工厂：生产 = connect_page（browser 级 eventing WS），测试注入 fake。
type Connector = Arc<
    dyn Fn(u16, EventSink) -> BoxFuture<'static, Result<SharedPageConn, AppError>> + Send + Sync,
>;

fn production_connector() -> Connector {
    Arc::new(|port, events| Box::pin(connect_page(port, events)))
}

fn unavailable(message: impl Into<String>) -> AppError {
    AppError::conflict("PAGE_SERVICE_UNAVAILABLE", message)
}

fn focus_failed(message: impl Into<String>) -> AppError {
    AppError::conflict("PAGE_FOCUS_FAILED", message)
}

fn fill_focus_lost(message: impl Into<String>) -> AppError {
    AppError::conflict("PAGE_FILL_FOCUS_LOST", message)
}

fn tool_timeout(tool: &str, budget: Duration) -> AppError {
    AppError::business(
        504,
        "PAGE_TOOL_TIMEOUT",
        format!(
            "page tool {tool} 超出 {}ms 预算（连接停滞或页面无响应）",
            budget.as_millis()
        ),
    )
}

// ============================================================================
// 1. 实例级 Watched：连接槽 + per-tab 状态
// ============================================================================

/// per-instance page tab 表：`(targetId, sessionId)` 按 attach 先后保序。
///
/// 空 target 语义（P2-F）：唯一 page target 自动选中；多个 → 409 并在错误
/// message 列出全部 targetId（曾试过「最近 attach」近似活跃 tab，真机复测
/// 取错页面，改为诚实语义）。列表需稳定可读，HashMap 迭代序随机故用 Vec；
/// 表长 = 标签数（个位数），线性扫描即可。
#[derive(Default)]
struct TabTable {
    /// (targetId, sessionId)，attach 先后序（错误信息列表稳定）
    rows: Vec<(String, String)>,
}

impl TabTable {
    /// attach/重 attach：已存在则原位更新 sessionId（列表顺序稳定）
    fn attach(&mut self, tid: &str, sid: &str) {
        match self.rows.iter_mut().find(|(t, _)| t.as_str() == tid) {
            Some(entry) => entry.1 = sid.to_string(),
            None => self.rows.push((tid.to_string(), sid.to_string())),
        }
    }

    fn detach(&mut self, tid: &str) {
        self.rows.retain(|(t, _)| t.as_str() != tid);
    }

    fn clear(&mut self) {
        self.rows.clear();
    }

    /// L2 握手窗口的判空探针：任一 page target 已入表即视作首批 attach 已到
    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn target_of_session(&self, sid: &str) -> Option<&str> {
        self.rows
            .iter()
            .find(|(_, s)| s.as_str() == sid)
            .map(|(t, _)| t.as_str())
    }
}

/// 一个实例的全部 page 控制运行态。
struct Watched {
    /// 当前连接（断连置 None，重建整体替换）。std Mutex 只做槽位读写；
    /// 收发在 [`PageCdp`]（&self 并发 send，无锁包裹）里。
    conn: Mutex<Option<SharedPageConn>>,
    /// per-instance 命令串行（决策 3）：锁的获取即建连检查点——并发首命令
    /// 只有先拿到锁的那个建连，其余直接复用（防双连接竞态）。
    cmd_lock: Arc<tokio::sync::Mutex<()>>,
    /// autoAttach 维护的 targetId → sessionId（仅 type=page），attach 序
    /// 保序（空 target 唯一性路由用，见 [`TabTable`]）
    tabs: Mutex<TabTable>,
    /// per-tab @eN ref 表（snap 整表替换，事件定向失效）
    refs: Mutex<HashMap<String, RefStore>>,
    /// per-tab 对话框缓冲（应答成功才入档）
    dialogs: Mutex<HashMap<String, DialogBuffer>>,
    /// 代际计数：每次建连 +1。旧代 watch_loop 在清理点核对代际，新一代已
    /// 接管时不动登记表/连接槽（防僵尸 loop 误删新代状态）。
    generation: AtomicU64,
}

impl Watched {
    fn new() -> Self {
        Watched {
            conn: Mutex::new(None),
            cmd_lock: Arc::new(tokio::sync::Mutex::new(())),
            tabs: Mutex::new(TabTable::default()),
            refs: Mutex::new(HashMap::new()),
            dialogs: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
        }
    }
}

pub struct PageService {
    watched: Arc<Mutex<HashMap<String, Arc<Watched>>>>,
    repo: Arc<InstanceRepository>,
    connector: Connector,
}

impl PageService {
    pub fn new(repo: Arc<InstanceRepository>) -> Self {
        Self::with_connector(repo, production_connector())
    }

    /// 注入连接工厂的构造（测试用 fake CDP，不依赖真 Chrome）。生产构造走
    /// [`PageService::new`]；本方法仅文件内测试调用，dead_code 逐项豁免。
    #[allow(dead_code)]
    pub fn with_connector(repo: Arc<InstanceRepository>, connector: Connector) -> Self {
        PageService {
            watched: Arc::new(Mutex::new(HashMap::new())),
            repo,
            connector,
        }
    }

    /// 命令前置：实例 Running（复用 diagnostics 的实例状态获取路径）→ 取/建
    /// Watched → 串行锁内惰性建连（决策 5）。返回 (Watched, 串行守卫)：
    /// 守卫由调用方持有到命令结束，全程串行。
    async fn ensure(
        &self,
        ins_id: &str,
    ) -> Result<(Arc<Watched>, tokio::sync::OwnedMutexGuard<()>), AppError> {
        let ins = self.repo.get(ins_id)?;
        if ins.status != InstanceStatus::Running {
            return Err(unavailable(format!(
                "实例 {ins_id} 未在运行，page 控制仅对运行中实例可用"
            )));
        }
        let port = ins
            .cdp_port
            .filter(|p| *p > 0)
            .ok_or_else(|| unavailable(format!("实例 {ins_id} 未上报 CDP 端口")))?;
        let watched = {
            let mut map = self.watched.lock().unwrap();
            Arc::clone(
                map.entry(ins_id.to_string())
                    .or_insert_with(|| Arc::new(Watched::new())),
            )
        };
        // 先拿串行锁再查连接：并发首命令不会双建连（见 cmd_lock 注释）
        let serial = watched.cmd_lock.clone().lock_owned().await;
        if watched.conn.lock().unwrap().is_none() {
            self.establish(ins_id, &watched, port).await?;
            // L2：establish 返回 ≠ 首批 attachedToTarget 已入表（事件异步在
            // 途），建连分支内短等首个 page target 再放行（见常量注释）。
            // 仅建连分支调用：连接已在位（已有命令在跑）时不经过这里，无额外阻塞。
            wait_first_attach(&watched).await;
        }
        Ok((watched, serial))
    }

    /// 建连序列：connect_eventing → Target.setAutoAttach(flatten) → 清旧代
    /// per-tab 状态 → 装槽 → spawn watch_loop。任何一步失败都不留半开状态
    /// （连接槽仍为 None，下一条命令重试）。
    async fn establish(
        &self,
        ins_id: &str,
        watched: &Arc<Watched>,
        port: u16,
    ) -> Result<(), AppError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let conn = (self.connector)(port, tx).await?;
        // waitForDebuggerOnStart 在 CfT 131 为必填（缺省 -32602，M6 实测）；
        // false = attach 不暂停目标页（与 diagnostics 同款 autoAttach 形态）
        cdp_send(
            &conn,
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "flatten": true, "waitForDebuggerOnStart": false }),
            None,
        )
        .await?;
        let gen = watched.generation.fetch_add(1, Ordering::Relaxed) + 1;
        // 旧代残留的 tabs/refs/dialogs 属于死会话（sessionId 已失效），整清
        watched.tabs.lock().unwrap().clear();
        watched.refs.lock().unwrap().clear();
        watched.dialogs.lock().unwrap().clear();
        *watched.conn.lock().unwrap() = Some(Arc::clone(&conn));
        tauri::async_runtime::spawn(watch_loop(
            ins_id.to_string(),
            Arc::clone(&self.watched),
            Arc::clone(watched),
            conn,
            rx,
            gen,
        ));
        Ok(())
    }

    /// 工具命令的统一骨架：预算超时 → ensure → tab 路由 → 对话框游标打标
    /// （§1.6）→ 执行 → 组装响应（instanceId/targetId + dialogs_seen）。
    /// 错误响应不带 dialogs_seen（AppError 无扩展字段，见模块注释偏差项）。
    async fn command<F, Fut>(
        &self,
        budget: Duration,
        tool: &str,
        ins_id: &str,
        tab: &str,
        run: F,
    ) -> Result<Value, AppError>
    where
        F: FnOnce(Arc<Watched>, String, String) -> Fut,
        Fut: Future<Output = Result<Value, AppError>>,
    {
        let tool = tool.to_string();
        within_budget(budget, &tool, async move {
            let (watched, _serial) = self.ensure(ins_id).await?;
            let (tid, sid) = resolve_target(&watched.tabs.lock().unwrap(), tab)?;
            let cursor = watched
                .dialogs
                .lock()
                .unwrap()
                .get(&tid)
                .map(|b| b.cursor())
                .unwrap_or(0);
            let mut resp = run(Arc::clone(&watched), tid.clone(), sid).await?;
            let seen: Vec<Value> = watched
                .dialogs
                .lock()
                .unwrap()
                .get(&tid)
                .map(|b| {
                    b.since(cursor)
                        .iter()
                        .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
                        .collect()
                })
                .unwrap_or_default();
            // 内部工具恒返回 json!({...}) 对象，Index 赋值安全
            resp["instanceId"] = json!(ins_id);
            resp["targetId"] = json!(tid);
            if !seen.is_empty() {
                resp["dialogs_seen"] = Value::Array(seen);
            }
            Ok(resp)
        })
        .await
    }

    /// snap <tab>：AX 树快照 + @eN ref 整表替换（§3.5）。
    /// 返回 { text, refs_count, generation }（任务口径，蛇形键对齐 TS wire 字段）。
    pub async fn snap(&self, ins_id: &str, tab: &str) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "snap",
            ins_id,
            tab,
            |w, tid, sid| async move { snap_tool(&w, &tid, &sid).await },
        )
        .await
    }

    /// click <tab> <@eN | selector>：ref 路径走几何链（quads 质心 + rect 回退），
    /// selector 路径走 locate → hit-test → 三段 Input 派发（§2）。
    pub async fn click(&self, ins_id: &str, tab: &str, target: &str) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "click",
            ins_id,
            tab,
            |w, tid, sid| async move { click_tool(&w, &tid, &sid, target).await },
        )
        .await
    }

    /// fill <tab> <@eN | selector> <value>：可填检查 → focus → 清空 →
    /// insertText → 回读校验（§4.1）。value 为空串 = 仅清空。
    pub async fn fill(
        &self,
        ins_id: &str,
        tab: &str,
        target: &str,
        value: &str,
    ) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "fill",
            ins_id,
            tab,
            |w, tid, sid| async move { fill_tool(&w, &tid, &sid, target, value).await },
        )
        .await
    }

    /// press <tab> <key> [--target]：键名 → rawKeyDown(+char)/keyUp 序列；
    /// --target 先 focus（ref: DOM.focus；selector: 页内 eval）。
    pub async fn press(
        &self,
        ins_id: &str,
        tab: &str,
        key: &str,
        target: Option<&str>,
    ) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "press",
            ins_id,
            tab,
            |w, tid, sid| async move { press_tool(&w, &tid, &sid, key, target).await },
        )
        .await
    }

    /// wait <tab> <selector> [--state] [--timeout]：轮询观测直到状态满足或
    /// 超时。预算 = 用户 timeout + 5s（wait 例外口径）。
    pub async fn wait(
        &self,
        ins_id: &str,
        tab: &str,
        selector: &str,
        state: Option<&str>,
        timeout: Option<&str>,
    ) -> Result<Value, AppError> {
        if selector.is_empty() {
            return Err(AppError::invalid_request("CSS selector required"));
        }
        let state = state.unwrap_or("visible");
        let Some(state) = WaitState::parse(state) else {
            return Err(AppError::invalid_request(format!(
                "--state must be one of visible|hidden|attached|detached (got \"{state}\")"
            )));
        };
        let timeout = match timeout {
            Some(t) => parse_duration(t, "--timeout")?,
            None => DEFAULT_WAIT_TIMEOUT,
        };
        let budget = timeout + WAIT_TOOL_GRACE;
        self.command(budget, "wait", ins_id, tab, move |w, tid, sid| async move {
            wait_tool(&w, &tid, &sid, selector, state, timeout).await
        })
        .await
    }

    /// dialogs <tab> [--since N]：读 per-tab 对话框缓冲（§1.6 游标语义）。
    /// 响应 meta 带证据边界（dropped/capacity）：缓冲 FIFO 逐出对调用方可见。
    pub async fn dialogs(
        &self,
        ins_id: &str,
        tab: &str,
        since: Option<u64>,
    ) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "dialogs",
            ins_id,
            tab,
            |w, tid, _sid| async move {
                let (cursor, dropped, entries): (u64, u64, Vec<Value>) = {
                    let dialogs = w.dialogs.lock().unwrap();
                    match dialogs.get(&tid) {
                        Some(b) => (
                            b.cursor(),
                            b.dropped(),
                            b.since(since.unwrap_or(0))
                                .iter()
                                .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
                                .collect(),
                        ),
                        None => (0, 0, Vec::new()),
                    }
                };
                Ok(json!({
                    "cursor": cursor,
                    "entries": entries,
                    "meta": { "dropped": dropped, "capacity": MAX_DIALOG_BUFFER },
                }))
            },
        )
        .await
    }

    /// eval <tab> <expr>：任意 JS 求值（Runtime.evaluate，returnByValue +
    /// awaitPromise），结果按 TS evalStr 形态序列化成字符串（P2-G）。
    /// **高权限原语**（可执行任意 JS，与退役前 cdp.mjs 同权限）——契约文档
    /// 已标注「仅在需要时使用，优先 page 原语」。
    pub async fn eval(&self, ins_id: &str, tab: &str, expression: &str) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "eval",
            ins_id,
            tab,
            |w, _tid, sid| async move { eval_tool(&w, &sid, expression).await },
        )
        .await
    }

    /// raw <tab> <method> [params]：任意 CDP 命令透传（D5 逃生舱，对齐退役前
    /// cdp.mjs evalraw）。browser=true 发 browser 级（sid 置 None）。
    pub async fn raw(
        &self,
        ins_id: &str,
        tab: &str,
        method: &str,
        params: Value,
        browser: bool,
    ) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "raw",
            ins_id,
            tab,
            |w, _tid, sid| async move { raw_tool(&w, &sid, method, params, browser).await },
        )
        .await
    }

    /// html <tab> [selector]：读 outerHTML（缺省 = 整文档；selector 未命中
    /// → 404 PAGE_ELEMENT_NOT_FOUND，见 html_tool 的行为差异说明）（P2-G）。
    pub async fn html(
        &self,
        ins_id: &str,
        tab: &str,
        selector: Option<&str>,
    ) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "html",
            ins_id,
            tab,
            |w, _tid, sid| async move { html_tool(&w, &sid, selector).await },
        )
        .await
    }

    /// clickxy <tab> <x> <y>：视口 CSS 像素坐标直接三段派发（无 locate /
    /// hit-test——坐标即意图；跨域 iframe 内元素的兜底点击路径）（P2-G）。
    pub async fn clickxy(
        &self,
        ins_id: &str,
        tab: &str,
        x: f64,
        y: f64,
    ) -> Result<Value, AppError> {
        // JSON 无法携带 NaN/Infinity，这道门拦的是 CLI/本地调用方的 inf/nan
        // 字面量（对齐 TS clickXyStr 的 isNaN 检查）
        if !x.is_finite() || !y.is_finite() {
            return Err(AppError::invalid_request(
                "x and y must be finite numbers (CSS pixels)",
            ));
        }
        self.command(
            TOOL_TIMEOUT,
            "clickxy",
            ins_id,
            tab,
            |w, _tid, sid| async move { clickxy_tool(&w, &sid, x, y).await },
        )
        .await
    }

    /// loadall <tab> <sel> [--interval] [--max-clicks]：循环点击直到选择器
    /// 消失或达上限（P2-G）。预算 = interval × max-clicks + 工具兑底
    /// （默认 200 × 1500ms = 5min + 30s，对齐 TS 的 5min 硬顶量级）。
    pub async fn loadall(
        &self,
        ins_id: &str,
        tab: &str,
        selector: &str,
        interval: Option<&str>,
        max_clicks: Option<u32>,
    ) -> Result<Value, AppError> {
        if selector.is_empty() {
            return Err(AppError::invalid_request("CSS selector required"));
        }
        let interval = match interval {
            Some(s) => parse_duration(s, "--interval")?,
            None => DEFAULT_LOADALL_INTERVAL,
        };
        let max_clicks = max_clicks.unwrap_or(DEFAULT_LOADALL_MAX_CLICKS);
        let budget = TOOL_TIMEOUT + interval * max_clicks;
        self.command(
            budget,
            "loadall",
            ins_id,
            tab,
            move |w, _tid, sid| async move {
                loadall_tool(&w, &sid, selector, interval, max_clicks).await
            },
        )
        .await
    }

    /// type <tab> <text>：对当前焦点元素直接 Input.insertText（无定位、无
    /// 校验）——跨域 iframe 输入的唯一路径（eval/fill 的 selector 均不可达
    /// 跨域 frame，先 clickxy 点入获得焦点）（P2-G，owner 补充）。空文本 →
    /// 400（对齐 TS typeStr）。
    pub async fn r#type(&self, ins_id: &str, tab: &str, text: &str) -> Result<Value, AppError> {
        if text.is_empty() {
            return Err(AppError::invalid_request("text required"));
        }
        self.command(
            TOOL_TIMEOUT,
            "type",
            ins_id,
            tab,
            |w, _tid, sid| async move { type_tool(&w, &sid, text).await },
        )
        .await
    }
}

/// 工具级预算：超时即放弃整个命令（连接守卫随 future 取消而释放，tokio
/// Mutex 无中毒语义，无残留锁）。
async fn within_budget<T, F: Future<Output = Result<T, AppError>>>(
    budget: Duration,
    tool: &str,
    f: F,
) -> Result<T, AppError> {
    match tokio::time::timeout(budget, f).await {
        Ok(result) => result,
        Err(_elapsed) => Err(tool_timeout(tool, budget)),
    }
}

/// L2 attach 握手窗口：短轮询 tabs 表至首个 page target 入表，或窗口
///（300ms）耗尽——先到者为准。超时不报错：保留现有 PAGE_TAB_NOT_FOUND 的
/// retry shortly 语义兑底（无 tab 开着 / attach 事件丢失都落到这里）。
/// 只在 ensure 的建连分支调用（连接已在位 = 已有命令在跑，不经过这里）。
async fn wait_first_attach(watched: &Watched) {
    let deadline = Instant::now() + ATTACH_HANDSHAKE_WINDOW;
    while watched.tabs.lock().unwrap().is_empty() {
        if Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(ATTACH_HANDSHAKE_POLL_INTERVAL).await;
    }
}

// ============================================================================
// 2. watch_loop：事件消费（对接 P2-A 模型）
// ============================================================================

/// watcher 主循环：消费事件流直到通道关闭（= reader 结束 = 连接死亡，与
/// diagnostics 同信号）。退出时清理：连接槽置 None（仅当槽里仍是自己的
/// 连接）+ 从登记表移除（仅当仍是当前代且登记表仍指向本 Watched——双守卫
/// 防旧代误删新代状态：同 Watched 重建靠 generation，整体替换靠 ptr_eq）。
async fn watch_loop(
    ins_id: String,
    registry: Arc<Mutex<HashMap<String, Arc<Watched>>>>,
    watched: Arc<Watched>,
    my_conn: SharedPageConn,
    mut rx: UnboundedReceiver<Value>,
    gen: u64,
) {
    while let Some(event) = rx.recv().await {
        if watched.generation.load(Ordering::Relaxed) != gen {
            // 新一代已接管（断连后命令重建了连接）：本代即刻退场，
            // 状态与登记表归新一代所有，什么都不动
            return;
        }
        handle_event(&watched, &event).await;
    }
    let mut slot = watched.conn.lock().unwrap();
    if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &my_conn)) {
        *slot = None;
    }
    drop(slot);
    if watched.generation.load(Ordering::Relaxed) == gen {
        let mut map = registry.lock().unwrap();
        if map
            .get(&ins_id)
            .is_some_and(|cur| Arc::ptr_eq(cur, &watched))
        {
            map.remove(&ins_id);
        }
    }
}

/// 单事件分派。flatten 模式：session 维度事件在顶层带 sessionId。
async fn handle_event(watched: &Watched, event: &Value) {
    let Some(method) = event.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = event.get("params").cloned().unwrap_or(Value::Null);
    match method {
        // §3.6 失效源接线（daemon.mjs bindRefInvalidation）+ attach 映射
        "Target.attachedToTarget" => on_attached(watched, &params).await,
        "Target.detachedFromTarget" => on_detached(watched, &params),
        "Target.targetDestroyed" => {
            if let Some(tid) = params.get("targetId").and_then(Value::as_str) {
                drop_tab(watched, tid);
            }
        }
        "Page.javascriptDialogOpening" => {
            if let Some(sid) = event.get("sessionId").and_then(Value::as_str) {
                on_dialog_opening(watched, sid, &params).await;
            }
        }
        // 仅根 frame（parentId 为空）才失效：子 frame 导航不换文档（§3.6）
        "Page.frameNavigated" => {
            if let Some(tid) = tab_of_session(watched, event) {
                let child_frame = params
                    .pointer("/frame/parentId")
                    .and_then(Value::as_str)
                    .is_some_and(|p| !p.is_empty());
                if !child_frame {
                    invalidate_refs(watched, &tid, "navigation");
                }
            }
        }
        "Page.documentOpened" => {
            if let Some(tid) = tab_of_session(watched, event) {
                invalidate_refs(watched, &tid, "document opened");
            }
        }
        "DOM.documentUpdated" => {
            if let Some(tid) = tab_of_session(watched, event) {
                invalidate_refs(watched, &tid, "document updated");
            }
        }
        _ => {}
    }
}

/// attachedToTarget：type=page 建 targetId→sessionId 映射，并对该 session
/// 补 Page.enable（对话框事件前置）+ DOM.enable（documentUpdated 前置）+
/// 重连对话框盲探测（L3，见 [`probe_reconnected_dialog`]）。best-effort：
/// enable/探测失败只缺对应能力，不影响命令路径。
async fn on_attached(watched: &Watched, params: &Value) {
    let ty = params
        .pointer("/targetInfo/type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(tid) = params
        .pointer("/targetInfo/targetId")
        .and_then(Value::as_str)
    else {
        return;
    };
    let Some(sid) = params.get("sessionId").and_then(Value::as_str) else {
        return;
    };
    if ty != "page" {
        return; // iframe/worker 等 target 不参与 page 命令路由
    }
    watched.tabs.lock().unwrap().attach(tid, sid);
    let Some(conn) = watched.conn.lock().unwrap().clone() else {
        return;
    };
    // L3：盲探测必须**先于** enable 发出——对话框悬挂时唯一不被 renderer
    // 阻塞的命令就是 handleJavaScriptDialog（Page/Runtime.enable 等都会挂起
    // 至超时）；若 alert 挂着，先探测 accept 再 enable 才能全部成功。探测
    // 与常规事件驱动路径互不干扰（无对话框时 -32602 静默）。
    probe_reconnected_dialog(watched, &conn, tid, sid).await;
    for method in ["Page.enable", "DOM.enable"] {
        if let Err(e) = cdp_send(&conn, method, json!({}), Some(sid)).await {
            tracing::debug!("[page] session {sid} {method} 失败: {e}");
        }
    }
}

/// L3 重连对话框盲探测：CDP 断连重连后**不重播** javascriptDialogOpening——
/// 重连前已挂着的 JS 对话框对 PageService 失明（无事件可驱动
/// [`on_dialog_opening`]，该 tab 的全部 CDP 命令被阻塞至超时、永久卡死）。
/// 故每个新 page session 在 Page.enable 后盲发一次 handleJavaScriptDialog
///（accept）充当探测：
/// - 成功 = 真有挂着的历史对话框，已被本探测接管——无事件参数可得，用保守
///   默认构造 DialogInfo（type=alert、message 固定文案）入缓冲，sequence 随
///   [`DialogBuffer::record`] 顺延；
/// - 失败（无对话框时 Chrome 回 -32602 No dialog is showing，或其他错误）→
///   debug 日志静默忽略（首连/无对话框的常态路径，不 Warning 刷屏）。
async fn probe_reconnected_dialog(watched: &Watched, conn: &SharedPageConn, tid: &str, sid: &str) {
    match cdp_send(
        conn,
        "Page.handleJavaScriptDialog",
        json!({ "accept": true }),
        Some(sid),
    )
    .await
    {
        Ok(_) => {
            tracing::debug!("[page] session {sid} 探测接管了重连前挂着的历史对话框（tab {tid}）");
            watched
                .dialogs
                .lock()
                .unwrap()
                .entry(tid.to_string())
                .or_insert_with(|| DialogBuffer::new(tid.to_string()))
                .record(DialogInfo {
                    r#type: "alert".to_string(),
                    message: "reconnected dialog (auto-accepted)".to_string(),
                    url: None,
                    default_prompt: None,
                    has_browser_handler: None,
                });
        }
        Err(e) => {
            tracing::debug!("[page] session {sid} 无挂着的历史对话框（探测静默）: {e}");
        }
    }
}

/// detachedFromTarget / targetDestroyed 的共同清理：映射 + 该 tab 的
/// RefStore / DialogBuffer 一起删（会话已死，状态随之作废）。
fn drop_tab(watched: &Watched, tid: &str) {
    watched.tabs.lock().unwrap().detach(tid);
    watched.refs.lock().unwrap().remove(tid);
    watched.dialogs.lock().unwrap().remove(tid);
}

fn on_detached(watched: &Watched, params: &Value) {
    let tid = params.get("targetId").and_then(Value::as_str);
    let sid = params.get("sessionId").and_then(Value::as_str);
    match (tid, sid) {
        (Some(t), _) => drop_tab(watched, t),
        (None, Some(s)) => {
            let target = watched
                .tabs
                .lock()
                .unwrap()
                .target_of_session(s)
                .map(str::to_string);
            if let Some(t) = target {
                drop_tab(watched, &t);
            }
        }
        (None, None) => {}
    }
}

/// 顶层 sessionId → targetId（session 维度事件定位 tab 用）。
fn tab_of_session(watched: &Watched, event: &Value) -> Option<String> {
    let sid = event.get("sessionId").and_then(Value::as_str)?;
    watched
        .tabs
        .lock()
        .unwrap()
        .target_of_session(sid)
        .map(str::to_string)
}

fn invalidate_refs(watched: &Watched, tid: &str, reason: &str) {
    if let Some(store) = watched.refs.lock().unwrap().get_mut(tid) {
        store.invalidate(reason);
    }
}

/// 对话框接管（§1.4/§1.5）：无条件 accept（prompt 带 promptText =
/// defaultPrompt ?? ""，P2-A 构造）；**成功后**才入缓冲——失败仅日志，
/// 对话框保持打开、CDP 命令继续阻塞直至各自超时（与上游一致）。
///
/// handle 走主连接（P2-F 根治）：send 已 &self 并发化，与 in-flight 工具
/// 命令天然并发——sessionId 是**连接级作用域**，换连接发必 -32001
/// Session not found（旁路短连接方案真机已证伪并回滚）。
async fn on_dialog_opening(watched: &Watched, session_id: &str, params: &Value) {
    let tid = watched
        .tabs
        .lock()
        .unwrap()
        .target_of_session(session_id)
        .map(str::to_string);
    let Some(tid) = tid else {
        return; // 未映射 session（attach 竞态/非 page target）：无法归属 tab
    };
    let Some(conn) = watched.conn.lock().unwrap().clone() else {
        return;
    };
    let info = parse_dialog_opening(params);
    let accept = dialog_accept_params(&info);
    match cdp_send(
        &conn,
        "Page.handleJavaScriptDialog",
        accept,
        Some(session_id),
    )
    .await
    {
        Ok(_) => {
            watched
                .dialogs
                .lock()
                .unwrap()
                .entry(tid.clone())
                .or_insert_with(|| DialogBuffer::new(tid.clone()))
                .record(info);
        }
        Err(e) => {
            tracing::warn!(
                "[page] Page.handleJavaScriptDialog 失败（不入缓冲，对话框保持打开）: {e}"
            )
        }
    }
}

// ============================================================================
// 3. tab 路由与共享小件
// ============================================================================

/// targetId 唯一前缀解析（对齐 TS pages.mjs resolvePrefix：大小写不敏感；
/// 无匹配 404 / 多匹配 409）。空 target 不参与前缀匹配（P2-F F1 真机复测
/// 修正）：唯一 page target 自动选中；多个 → 409 并在 message 列出全部
/// targetId（不猜「最近 attach」近似——真机会取错页面）；无 target → 404。
fn resolve_target(tabs: &TabTable, input: &str) -> Result<(String, String), AppError> {
    if input.is_empty() {
        return match tabs.rows.as_slice() {
            [] => Err(AppError::not_found(
                "PAGE_TAB_NOT_FOUND",
                "no page targets attached yet — open a tab or retry shortly",
            )),
            [one] => Ok(one.clone()),
            many => Err(AppError::conflict(
                "PAGE_TAB_AMBIGUOUS",
                format!(
                    "Ambiguous tab: {} page targets attached [{}] — pass an explicit tab (targetId or unique prefix, from instance tabs)",
                    many.len(),
                    many.iter()
                        .map(|(t, _)| t.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
        };
    }
    let upper = input.to_uppercase();
    let matches: Vec<&(String, String)> = tabs
        .rows
        .iter()
        .filter(|(k, _)| k.to_uppercase().starts_with(&upper))
        .collect();
    match matches.as_slice() {
        [] => Err(PageControlError::tab_not_found(input).into()),
        [one] => Ok((one.0.clone(), one.1.clone())),
        many => Err(AppError::conflict(
            "PAGE_TAB_AMBIGUOUS",
            format!(
                "Ambiguous prefix \"{input}\" — matches {} targets. Use more characters.",
                many.len()
            ),
        )),
    }
}

/// TS interact.mjs isRefTarget：'@…' 宽松（CSS 选择器不可能以 @ 开头）、
/// 裸 'e<digits>' 严格；'E3'/'@E3' 走 ref 路径后大小写敏感失配（§3.7），
/// 而不是被误读成选择器。
fn is_ref_target(target: &str) -> bool {
    if target.starts_with('@') {
        return true;
    }
    let rest = target.strip_prefix('e').unwrap_or("");
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

fn require_conn(watched: &Watched) -> Result<SharedPageConn, AppError> {
    watched
        .conn
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| unavailable("page 控制连接未就绪（建立中或已断开），稍后重试"))
}

/// 发送：连接 send 已 &self 并发化（P2-F 根治，无锁包裹）——wait 轮询间隔、
/// click 释放间隙、对话框 handle 与 in-flight 工具命令全部天然并发，不再
/// 依赖「per-send 拿锁不跨 sleep」的间隙仲裁。
async fn cdp_send(
    conn: &SharedPageConn,
    method: &str,
    params: Value,
    session_id: Option<&str>,
) -> Result<Value, AppError> {
    conn.send_value(method, params, session_id).await
}

/// JS Number 的 toString 形态（150.0 → "150"）：结果文案里的坐标格式与 TS 一致。
fn js_num(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// TS truncate：40 字符 + '…'（fill 回读摘要用）
fn truncate40(s: &str) -> String {
    if s.chars().count() > 40 {
        let mut t: String = s.chars().take(40).collect();
        t.push('…');
        t
    } else {
        s.to_string()
    }
}

// ============================================================================
// 4. 页内 eval 基件（TS evaluate.mjs evalStr / click-native.mjs evalJson）
// ============================================================================

/// Runtime.evaluate 的完整 result RemoteObject（eval 取值 / eval 工具序列化 /
/// html 共用基件；TS evalStr 形态：每次先 Runtime.enable（幂等），
/// returnByValue + awaitPromise）。页面异常 → 500 PAGE_EVAL_FAILED。
async fn eval_remote_object(
    conn: &SharedPageConn,
    sid: &str,
    expression: &str,
) -> Result<Value, AppError> {
    let _: Value = cdp_send(conn, "Runtime.enable", json!({}), Some(sid)).await?;
    let resp = cdp_send(
        conn,
        "Runtime.evaluate",
        json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
        Some(sid),
    )
    .await?;
    if let Some(d) = resp.get("exceptionDetails") {
        let text = d
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| d.pointer("/exception/description").and_then(Value::as_str))
            .unwrap_or("unknown error");
        return Err(AppError::business(
            500,
            "PAGE_EVAL_FAILED",
            text.to_string(),
        ));
    }
    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
}

/// eval + 值提取：returnByValue 下 undefined 与 null 都坍缩为 Value::Null
/// （需要区分二者的工具直接用 [`eval_remote_object`] 读 result.type）。
async fn eval_raw(conn: &SharedPageConn, sid: &str, expression: &str) -> Result<Value, AppError> {
    Ok(eval_remote_object(conn, sid, expression)
        .await?
        .get("value")
        .cloned()
        .unwrap_or(Value::Null))
}

/// eval + JSON.parse（TS evalJson）：页内脚本约定返回 JSON 字符串；
/// 垃圾返回 → 500（wait 把一切 eval 错误都当 detached 继续轮询）。
async fn eval_json(
    conn: &SharedPageConn,
    sid: &str,
    expression: &str,
    tool: &str,
) -> Result<Value, AppError> {
    let raw = eval_raw(conn, sid, expression).await?;
    let s = match raw.as_str() {
        Some(s) => s.to_string(),
        None => raw.to_string(),
    };
    let brief = s.chars().take(200).collect::<String>();
    serde_json::from_str(&s).map_err(|_| {
        AppError::business(
            500,
            "PAGE_EVAL_FAILED",
            format!("{tool}: unexpected eval result: {brief}"),
        )
    })
}

/// Runtime.callFunctionOn（this = objectId 的页内函数调用，fill/几何回退用）。
/// 页面异常 → 500 PAGE_EVAL_FAILED（消息带 fill 前缀与 TS 对齐）。
async fn call_fn(
    conn: &SharedPageConn,
    sid: &str,
    object_id: &str,
    declaration: &str,
    args: Vec<Value>,
) -> Result<Value, AppError> {
    let resp = cdp_send(
        conn,
        "Runtime.callFunctionOn",
        json!({
            "objectId": object_id,
            "functionDeclaration": declaration,
            "arguments": args.into_iter().map(|v| json!({ "value": v })).collect::<Vec<_>>(),
            "returnByValue": true,
        }),
        Some(sid),
    )
    .await?;
    if let Some(d) = resp.get("exceptionDetails") {
        let text = d
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| d.pointer("/exception/description").and_then(Value::as_str))
            .unwrap_or("unknown error");
        return Err(AppError::business(
            500,
            "PAGE_EVAL_FAILED",
            format!("fill: page script failed: {text}"),
        ));
    }
    Ok(resp
        .pointer("/result/value")
        .cloned()
        .unwrap_or(Value::Null))
}

// fill 的 clear/verify 页内脚本（TS fill.mjs 逐字）。check 脚本已由 P2-A 的
/// fill_fillable_check_expr 提供；clear/verify 需要把同一个 fillEditable 内联
/// 进各自模板（页面无模块作用域），故保留一份源码文本（TS 的 toString 技巧）。
const FILL_EDITABLE: &str = r#"function fillEditable(el) {
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
}"#;

fn fill_clear_expr() -> String {
    r#"function() {
  const check = (__FILL_EDITABLE__)(this);
  if (!check.editable) return { not_fillable: check.why };
  // DOM focus (getRootNode semantics work in background tabs, unlike
  // document.hasFocus()).
  if (this.getRootNode().activeElement !== this) return { focus_lost: true };
  if (check.kind === 'native') {
    const proto = this.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(this, '');
  } else {
    this.textContent = '';
  }
  this.dispatchEvent(new Event('input', { bubbles: true }));
  return { cleared: true };
}"#
    .replace("__FILL_EDITABLE__", FILL_EDITABLE)
}

fn fill_verify_expr() -> String {
    r#"function(multiLine, singleLine) {
  if (!this.isConnected) return { gone: true };
  const check = (__FILL_EDITABLE__)(this);
  const actual = check.kind === 'native' ? this.value : this.textContent;
  const expected = check.kind === 'native' && this.tagName === 'INPUT' ? singleLine : multiLine;
  return { matches: actual === expected, actual: String(actual === undefined ? '' : actual), expected };
}"#
    .replace("__FILL_EDITABLE__", FILL_EDITABLE)
}

/// press --target 的 selector 路径 focus 脚本（TS press.mjs focusPressTarget）。
fn press_focus_expr(selector: &str) -> String {
    r#"(function() {
  const el = document.querySelector(__SEL__);
  if (!el) return JSON.stringify({ error: 'not_found' });
  el.scrollIntoView({ block: 'center' });
  el.focus();
  if (el.getRootNode().activeElement !== el) return JSON.stringify({ error: 'focus_failed' });
  const label = el.getAttribute('aria-label') || el.getAttribute('name')
    || el.getAttribute('placeholder') || el.id || '';
  return JSON.stringify({ tag: el.tagName, label });
})()"#
        .replace(
            "__SEL__",
            &crate::domain::page_control::json_escape(selector),
        )
}

/// 几何回退用的 rect 读取函数（TS resolveRefPoint 尾段逐字）。
const RECT_CENTER_FN: &str = "function(){ const r = this.getBoundingClientRect(); return { x: r.x, y: r.y, w: r.width, h: r.height }; }";

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

fn ax_field<'a>(node: &'a Value, field: &str) -> Option<&'a str> {
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

fn format_ax_line(node: &Value, depth: usize, r#ref: Option<&str>) -> String {
    let role = ax_field(node, "role").unwrap_or("");
    let name = ax_field(node, "name").unwrap_or("");
    let value = node.get("value").and_then(|v| v.get("value"));
    let mut line = "  ".repeat(depth.min(10));
    if let Some(r) = r#ref {
        line.push_str(&format!("@{r} "));
    }
    line.push_str(&format!("[{role}]"));
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

/// 前序 DFS：只有 ref-worthy 节点消耗编号（从 1 起）；无 backendDOMNodeId
/// 的节点不发 ref（无法解析到 CDP 节点的 ref 必是死点击）；childIds 优先、
/// parent 反查表补位（TS orderedAxChildren）；游离节点兜底遍历（visited 去重）。
fn visit_ax(
    node: &Value,
    depth: usize,
    v: &mut AxVisit,
    by_id: &HashMap<String, &Value>,
    by_parent: &HashMap<String, Vec<&Value>>,
) {
    let key = ax_key(node.get("nodeId").unwrap_or(&Value::Null));
    if !v.visited.insert(key) {
        return;
    }
    let mut assigned: Option<String> = None;
    if is_ref_worthy(node)
        && node
            .get("backendDOMNodeId")
            .and_then(Value::as_i64)
            .is_some()
    {
        let r = format!("e{}", v.next_ref);
        v.next_ref += 1;
        let mut entry = json!({
            "ref": r,
            "backendNodeId": node.get("backendDOMNodeId").cloned().unwrap_or(Value::Null),
        });
        if let Some(name) = ax_field(node, "name").filter(|n| !n.is_empty()) {
            entry["name"] = json!(name);
        }
        v.refs.push(entry);
        assigned = Some(r);
    }
    if should_show(node) {
        v.lines
            .push(format_ax_line(node, depth, assigned.as_deref()));
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
        visit_ax(child, depth + 1, v, by_id, by_parent);
    }
}

/// Accessibility.getFullAXTree 的 nodes → (text, refs)。refs 形状 = TS
/// collectSnapshot 的 `[{ref, backendNodeId, name?}]`（RefStore::replace 入参）。
fn collect_ax_tree(nodes: &[Value]) -> (String, Vec<Value>) {
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
    let mut v = AxVisit {
        lines: Vec::new(),
        refs: Vec::new(),
        visited: HashSet::new(),
        next_ref: 1,
    };
    for n in nodes {
        if is_root(n, &by_id) {
            visit_ax(n, 0, &mut v, &by_id, &by_parent);
        }
    }
    for n in nodes {
        visit_ax(n, 0, &mut v, &by_id, &by_parent); // 兜底：游离节点（visited 已去重）
    }
    (v.lines.join("\n"), v.refs)
}

// ============================================================================
// 6. 工具编排（每个 = 一个内部 async fn，公开入口在 PageService 上）
// ============================================================================

async fn snap_tool(watched: &Watched, _tid: &str, sid: &str) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let resp = cdp_send(&conn, "Accessibility.getFullAXTree", json!({}), Some(sid)).await?;
    let nodes = resp
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let (text, refs) = collect_ax_tree(&nodes);
    let refs_count = refs.len();
    // 整表替换 + generation+1 + 清 stale（§3.5）：refs 只描述最后一次 snap
    let generation = {
        let mut tabs = watched.refs.lock().unwrap();
        let store = tabs.entry(_tid.to_string()).or_default();
        store.replace(&Value::Array(refs));
        store.generation()
    };
    Ok(json!({ "text": text, "refs_count": refs_count, "generation": generation }))
}

/// ref → backendNodeId 查表（miss 按 P2-A ref_miss 语义选 stale/not_found）。
fn resolve_ref_entry(
    watched: &Watched,
    tid: &str,
    raw_ref: &str,
) -> Result<crate::domain::page_control::RefEntry, AppError> {
    let tabs = watched.refs.lock().unwrap();
    match tabs.get(tid) {
        // 无表 = 从未 snap：等价 TS 的 throwaway store → ref_not_found
        None => Err(PageControlError::ref_not_found(raw_ref).into()),
        Some(store) => match store.resolve(raw_ref) {
            Some(entry) => Ok(entry.clone()),
            None => Err(PageControlError::ref_miss(store, raw_ref).into()),
        },
    }
}

/// ref 几何链（TS resolveRefPoint）：scrollIntoView best-effort →
/// getContentQuads 最大四边形质心（P2-A）→ 回退 resolveNode + rect 中心。
async fn resolve_ref_point(
    conn: &SharedPageConn,
    sid: &str,
    backend_node_id: i64,
) -> Result<Point, PageControlError> {
    let _ = cdp_send(
        conn,
        "DOM.scrollIntoViewIfNeeded",
        json!({ "backendNodeId": backend_node_id }),
        Some(sid),
    )
    .await;
    if let Ok(resp) = cdp_send(
        conn,
        "DOM.getContentQuads",
        json!({ "backendNodeId": backend_node_id }),
        Some(sid),
    )
    .await
    {
        let quads: Vec<Vec<f64>> = resp
            .get("quads")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .flat_map(|q| {
                        q.as_array()
                            .map(|nums| nums.iter().filter_map(Value::as_f64).collect::<Vec<f64>>())
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(p) = largest_quad_centroid(&quads) {
            return Ok(p);
        }
    }
    let fallback: Result<Point, String> = async {
        let resp = cdp_send(conn, "DOM.resolveNode", json!({ "backendNodeId": backend_node_id }), Some(sid))
            .await
            .map_err(|e| e.to_string())?;
        let object_id = resp
            .pointer("/object/objectId")
            .and_then(Value::as_str)
            .ok_or("no objectId")?;
        let resp = cdp_send(
            conn,
            "Runtime.callFunctionOn",
            json!({ "objectId": object_id, "functionDeclaration": RECT_CENTER_FN, "returnByValue": true }),
            Some(sid),
        )
        .await
        .map_err(|e| e.to_string())?;
        let rect = resp.pointer("/result/value").ok_or("empty rect")?;
        let (x, y, w, h) = (
            rect.get("x").and_then(Value::as_f64),
            rect.get("y").and_then(Value::as_f64),
            rect.get("w").and_then(Value::as_f64),
            rect.get("h").and_then(Value::as_f64),
        );
        match (x, y, w, h) {
            (Some(x), Some(y), Some(w), Some(h)) if w > 0.0 && h > 0.0 => {
                Ok(Point { x: x + w / 2.0, y: y + h / 2.0 })
            }
            _ => Err("empty rect".to_string()),
        }
    }
    .await;
    fallback.map_err(|detail| PageControlError::target_moved(&detail))
}

/// locate 结果分类（TS classifyLocate）→ (x, y, tag, text)。
fn classify_locate(r: &Value, selector: &str) -> Result<(f64, f64, String, String), AppError> {
    match r.get("error").and_then(Value::as_str) {
        Some("not_found") => return Err(PageControlError::element_not_found(selector).into()),
        Some("not_visible") => {
            let why = r.get("why").and_then(Value::as_str).unwrap_or("");
            return Err(PageControlError::element_not_visible(selector, why).into());
        }
        _ => {}
    }
    // 页面必须回有限坐标，否则 hit-test 表达式可能被注入内容构造
    match (
        r.get("x").and_then(Value::as_f64),
        r.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Ok((
            x,
            y,
            r.get("tag")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            r.get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        )),
        _ => Err(AppError::internal(format!(
            "click: unexpected locate result for {selector}"
        ))),
    }
}

/// hit-test 结果分类（TS classifyHit）：命中静默通过，其余按原因报错。
fn classify_hit(r: &Value, selector: &str) -> Result<(), AppError> {
    if r.get("hit").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    match r.get("reason").and_then(Value::as_str) {
        Some("target-gone") => Err(PageControlError::element_not_found(selector).into()),
        Some("covered") => Err(PageControlError::element_blocked_covered(
            selector,
            r.get("by").and_then(Value::as_str).unwrap_or(""),
        )
        .into()),
        Some("nothing-at-point") => Err(PageControlError::element_blocked_nothing(selector).into()),
        _ => Err(AppError::internal(format!(
            "click: unexpected hit-test result for {selector}"
        ))),
    }
}

/// 三段鼠标派发（§2.4）：mouseMoved → mousePressed → 50ms → mouseReleased。
/// press 后失败补一发 release（按钮绝不卡死按下态），effect_state 按位次
/// 标注（§2.7：pre-press = none / post-press = unknown）。
async fn dispatch_click(conn: &SharedPageConn, sid: &str, x: f64, y: f64) -> Result<(), AppError> {
    let base = |ty: &str| json!({ "type": ty, "x": x, "y": y, "button": "left", "clickCount": 1, "modifiers": 0 });
    cdp_send(
        conn,
        "Input.dispatchMouseEvent",
        base("mouseMoved"),
        Some(sid),
    )
    .await
    .map_err(|e| AppError::from(PageControlError::dispatch_failed(&e.to_string(), false)))?;
    cdp_send(
        conn,
        "Input.dispatchMouseEvent",
        base("mousePressed"),
        Some(sid),
    )
    .await
    .map_err(|e| AppError::from(PageControlError::dispatch_failed(&e.to_string(), false)))?;
    tokio::time::sleep(CLICK_RELEASE_DELAY).await;
    if let Err(e) = cdp_send(
        conn,
        "Input.dispatchMouseEvent",
        base("mouseReleased"),
        Some(sid),
    )
    .await
    {
        let _ = cdp_send(
            conn,
            "Input.dispatchMouseEvent",
            base("mouseReleased"),
            Some(sid),
        )
        .await;
        return Err(PageControlError::dispatch_failed(&e.to_string(), true).into());
    }
    Ok(())
}

/// selector 路径点击链（click 与 loadall 共用，TS clickSelectorStr 原生分支）：
/// locate → hit-test → 三段派发，返回人性化结果串。
async fn click_selector(
    conn: &SharedPageConn,
    sid: &str,
    selector: &str,
) -> Result<String, AppError> {
    let located = eval_json(conn, sid, &locate_expr(selector), "click").await?;
    let (x, y, tag, text_val) = classify_locate(&located, selector)?;
    let hit = eval_json(conn, sid, &hit_test_expr(selector, x, y), "click").await?;
    classify_hit(&hit, selector)?;
    dispatch_click(conn, sid, x, y).await?;
    Ok(format!(
        "Clicked <{tag}> \"{text_val}\" (native input @{},{})",
        js_num(x),
        js_num(y)
    ))
}

async fn click_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: &str,
) -> Result<Value, AppError> {
    if target.is_empty() {
        return Err(AppError::invalid_request("CSS selector required"));
    }
    let conn = require_conn(watched)?;
    let text = if is_ref_target(target) {
        // ref 路径：查表 → 几何 → 原生坐标点击（refs 永远走原生输入）
        let entry = resolve_ref_entry(watched, tid, target)?;
        let p = resolve_ref_point(&conn, sid, entry.backend_node_id).await?;
        dispatch_click(&conn, sid, p.x, p.y).await?;
        let label = entry.name.map(|n| format!(" \"{n}\"")).unwrap_or_default();
        format!(
            "Clicked @{}{label} at CSS ({}, {})",
            normalize_ref(target),
            js_num(p.x),
            js_num(p.y)
        )
    } else {
        // selector 路径：locate → hit-test → 三段派发（TS clickSelectorStr）
        click_selector(&conn, sid, target).await?
    };
    Ok(json!({ "result": text }))
}

/// fill 目标解析 → 稳定 objectId（SPEC DOM-script 原则：两次 eval 之间选择器
/// 可能命中不同元素，对象句柄跨调用稳定）。ref 路径失败 = target_moved；
/// selector 路径 nodeId 0 = element_not_found（与 click 同码）。
async fn resolve_fill_target(
    watched: &Watched,
    conn: &SharedPageConn,
    tid: &str,
    sid: &str,
    target: &str,
) -> Result<(String, Option<String>, String), AppError> {
    if is_ref_target(target) {
        let entry = resolve_ref_entry(watched, tid, target)?;
        let resp = match cdp_send(
            conn,
            "DOM.resolveNode",
            json!({ "backendNodeId": entry.backend_node_id }),
            Some(sid),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => return Err(PageControlError::target_moved(&e.to_string()).into()),
        };
        let Some(object_id) = resp.pointer("/object/objectId").and_then(Value::as_str) else {
            return Err(PageControlError::target_moved("no objectId").into());
        };
        Ok((
            object_id.to_string(),
            entry.name,
            format!("@{}", normalize_ref(target)),
        ))
    } else {
        if target.is_empty() {
            return Err(AppError::invalid_request(
                "CSS selector or @eN ref required",
            ));
        }
        let doc = cdp_send(conn, "DOM.getDocument", json!({}), Some(sid)).await?;
        let root = doc
            .pointer("/root/nodeId")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let q = cdp_send(
            conn,
            "DOM.querySelector",
            json!({ "nodeId": root, "selector": target }),
            Some(sid),
        )
        .await?;
        let node_id = q.get("nodeId").and_then(Value::as_i64).unwrap_or(0);
        if node_id == 0 {
            return Err(PageControlError::element_not_found(target).into());
        }
        let resp = match cdp_send(
            conn,
            "DOM.resolveNode",
            json!({ "nodeId": node_id }),
            Some(sid),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(AppError::internal(format!(
                    "fill: could not resolve {target} to a node ({e})"
                )))
            }
        };
        let Some(object_id) = resp.pointer("/object/objectId").and_then(Value::as_str) else {
            return Err(AppError::internal(format!(
                "fill: could not resolve {target} to a node (no objectId)"
            )));
        };
        Ok((object_id.to_string(), None, target.to_string()))
    }
}

fn require_fillable(r: &Value, desc: &str) -> Result<(), AppError> {
    if let Some(why) = r.get("not_fillable").and_then(Value::as_str) {
        return Err(PageControlError::target_not_fillable(desc, why).into());
    }
    Ok(())
}

async fn fill_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: &str,
    value: &str,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let (object_id, ref_name, desc) = resolve_fill_target(watched, &conn, tid, sid, target).await?;
    let outcome = fill_pipeline(&conn, sid, &object_id, &desc, ref_name, value).await;
    // finally 语义（TS）：无论成败都归还对象句柄（best-effort）
    let _ = cdp_send(
        &conn,
        "Runtime.releaseObject",
        json!({ "objectId": object_id }),
        Some(sid),
    )
    .await;
    Ok(json!({ "result": outcome? }))
}

async fn fill_pipeline(
    conn: &SharedPageConn,
    sid: &str,
    object_id: &str,
    desc: &str,
    ref_name: Option<String>,
    value: &str,
) -> Result<String, AppError> {
    // focus 前滚动 best-effort
    let _ = cdp_send(
        conn,
        "DOM.scrollIntoViewIfNeeded",
        json!({ "objectId": object_id }),
        Some(sid),
    )
    .await;
    let check = call_fn(conn, sid, object_id, &fill_fillable_check_expr(), vec![]).await?;
    require_fillable(&check, desc)?;
    if let Err(e) = cdp_send(
        conn,
        "DOM.focus",
        json!({ "objectId": object_id }),
        Some(sid),
    )
    .await
    {
        // Chrome 对 disabled 控件拒绝 DOM.focus：复查后按不可填报，而非泛化 focus 失败
        let recheck = call_fn(conn, sid, object_id, &fill_fillable_check_expr(), vec![]).await?;
        require_fillable(&recheck, desc)?;
        return Err(fill_focus_lost(format!(
            "fill target {desc} could not be focused ({e})"
        )));
    }
    let clear = call_fn(conn, sid, object_id, &fill_clear_expr(), vec![]).await?;
    require_fillable(&clear, desc)?; // 页面可能在检查后禁用了它
    if clear.get("focus_lost").and_then(Value::as_bool) == Some(true) {
        return Err(fill_focus_lost(format!(
            "fill target {desc} does not have focus (page moved it after focus)"
        )));
    }
    // insertText 丢弃 CR 载体；单行 input 把换行折成空格——两种期望值都预计算
    let multi_line = value.replace("\r\n", "\n").replace('\r', "\n");
    if !multi_line.is_empty() {
        cdp_send(
            conn,
            "Input.insertText",
            json!({ "text": multi_line }),
            Some(sid),
        )
        .await?;
    }
    let single_line = multi_line.replace('\n', " ");
    let verify = call_fn(
        conn,
        sid,
        object_id,
        &fill_verify_expr(),
        vec![
            Value::String(multi_line.clone()),
            Value::String(single_line),
        ],
    )
    .await?;
    if verify.get("gone").and_then(Value::as_bool) == Some(true) {
        return Err(PageControlError::fill_value_mismatch(format!(
            "fill target {desc} was removed or replaced during fill — observe the page before deciding whether to retry"
        ))
        .into());
    }
    if verify.get("matches").and_then(Value::as_bool).is_none() {
        return Err(PageControlError::fill_value_mismatch(format!(
            "fill could not verify the value of {desc} (unexpected verify result)"
        ))
        .into());
    }
    if verify.get("matches").and_then(Value::as_bool) != Some(true) {
        let expected = verify
            .get("expected")
            .and_then(Value::as_str)
            .unwrap_or(&multi_line);
        let actual = verify.get("actual").and_then(Value::as_str).unwrap_or("");
        return Err(PageControlError::fill_value_mismatch(format!(
            "fill could not verify the expected value (the write may have partially applied — observe before retrying): expected \"{}\", got \"{}\"",
            truncate40(expected),
            truncate40(actual)
        ))
        .into());
    }
    let tag = check
        .get("tag")
        .and_then(Value::as_str)
        .unwrap_or("ELEMENT");
    let label = ref_name
        .or_else(|| {
            check
                .get("label")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|l| !l.is_empty());
    let shown = truncate40(value);
    let n = value.chars().count();
    let unit = if n == 1 { "char" } else { "chars" };
    Ok(format!(
        "Filled <{tag}>{} with \"{shown}\" ({n} {unit})",
        label.map(|l| format!(" \"{l}\"")).unwrap_or_default()
    ))
}

/// press 的 --target focus（TS focusPressTarget）：ref 走 DOM.focus
/// （backendNodeId），selector 走页内 eval（scrollIntoView + focus + 复查）。
async fn focus_press_target(
    watched: &Watched,
    conn: &SharedPageConn,
    tid: &str,
    sid: &str,
    target: &str,
) -> Result<String, AppError> {
    if is_ref_target(target) {
        let entry = resolve_ref_entry(watched, tid, target)?;
        let _ = cdp_send(
            conn,
            "DOM.scrollIntoViewIfNeeded",
            json!({ "backendNodeId": entry.backend_node_id }),
            Some(sid),
        )
        .await;
        if let Err(e) = cdp_send(
            conn,
            "DOM.focus",
            json!({ "backendNodeId": entry.backend_node_id }),
            Some(sid),
        )
        .await
        {
            return Err(focus_failed(format!(
                "press --target {target} could not be focused ({e})"
            )));
        }
        let key = normalize_ref(target);
        Ok(entry
            .name
            .map(|n| format!("@{key} \"{n}\""))
            .unwrap_or(format!("@{key}")))
    } else {
        let r = eval_json(conn, sid, &press_focus_expr(target), "press").await?;
        match r.get("error").and_then(Value::as_str) {
            Some("not_found") => Err(PageControlError::element_not_found(target).into()),
            Some("focus_failed") => Err(focus_failed(format!(
                "press --target {target} could not be focused"
            ))),
            _ => {
                let Some(tag) = r.get("tag").and_then(Value::as_str) else {
                    return Err(AppError::internal(format!(
                        "press: unexpected focus result for {target}"
                    )));
                };
                let label = r
                    .get("label")
                    .and_then(Value::as_str)
                    .filter(|l| !l.is_empty());
                Ok(label
                    .map(|l| format!("<{tag}> \"{l}\""))
                    .unwrap_or(format!("<{tag}>")))
            }
        }
    }
}

async fn press_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    key: &str,
    target: Option<&str>,
) -> Result<Value, AppError> {
    if key.is_empty() {
        return Err(AppError::invalid_request(
            "key required (e.g. Enter, a, Control+A)",
        ));
    }
    let kp = parse_key(key)?; // PAGE_KEY_UNKNOWN 400（P2-A）
    let conn = require_conn(watched)?;
    let where_on = match target.filter(|t| !t.is_empty()) {
        None => String::new(),
        Some(t) => format!(
            " on {}",
            focus_press_target(watched, &conn, tid, sid, t).await?
        ),
    };
    // rawKeyDown (+char) → keyUp（P2-A 序列生成，绝不发裸 keyDown）。
    // char/keyUp 失败补一发 keyUp（按键绝不卡死按下态）；rawKeyDown 失败
    // 原样上抛（尚无输入生效，TS 同）
    let seq = kp.dispatch_sequence();
    let last = serde_json::to_value(seq.last().expect("序列至少 rawKeyDown+keyUp")).unwrap();
    for (i, ev) in seq.iter().enumerate() {
        if let Err(e) = cdp_send(
            &conn,
            "Input.dispatchKeyEvent",
            serde_json::to_value(ev).unwrap(),
            Some(sid),
        )
        .await
        {
            if i > 0 {
                let _ = cdp_send(&conn, "Input.dispatchKeyEvent", last.clone(), Some(sid)).await;
            }
            return Err(e);
        }
    }
    Ok(json!({ "result": format!("Pressed {}{where_on}", kp.display()) }))
}

async fn wait_tool(
    watched: &Watched,
    _tid: &str,
    sid: &str,
    selector: &str,
    state: WaitState,
    timeout: Duration,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let expr = wait_observe_expr(selector);
    let start = Instant::now();
    let mut polls: usize = 0;
    let mut last;
    loop {
        polls += 1;
        // eval 失败（导航中）= detached 继续轮询；返回垃圾 = unknown（永不
        // 满足除 attached 外的任何状态——P2-A 语义原样保留）
        last = match eval_json(&conn, sid, &expr, "wait").await {
            Ok(v) => v
                .get("observed")
                .and_then(Value::as_str)
                .map(ObservedState::parse)
                .unwrap_or(ObservedState::Unknown),
            Err(_) => ObservedState::Detached,
        };
        if observed_matches_state(last, state) {
            return Ok(json!({
                "result": format!(
                    "Waited: {selector} is {} (after {}ms, {polls} polls)",
                    state.as_str(),
                    start.elapsed().as_millis()
                )
            }));
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return Err(PageControlError::wait_timeout(
                selector,
                state.as_str(),
                elapsed.as_millis() as i64,
                last.as_str(),
                polls,
            )
            .into());
        }
        // 绝不睡过截止线（TS 同）
        tokio::time::sleep(WAIT_POLL_INTERVAL.min(timeout.saturating_sub(elapsed))).await;
    }
}

// ============================================================================
// 6b. 工具编排续（P2-G）：eval / html / clickxy / type / loadall
// （cdp.mjs 退役前置——规格照抄 TS evaluate.mjs / interact.mjs，复用既有
//  eval 基件与 click 派发链，纯增量）
// ============================================================================

/// TS evalStr 的结果序列化：对象/数组 → pretty JSON 字符串（对齐
/// JSON.stringify(v, null, 2) 的 2 空格缩进）、字符串原样、undefined/null
/// → "undefined"/"null"、数字/布尔 → JS String 形态。与 TS 的微差：
/// undefined 在 TS 是空串（String(val ?? '')），任务口径取字面量
/// "undefined"（空串与「无返回」不可区分，字面量更诚实）。
fn eval_str_from_remote(result: &Value) -> String {
    match result.get("value") {
        // returnByValue 下 undefined 无 value 键、null 的 value 为 JSON null，
        // 二者在 Value 上坍缩，用 result.type 区分
        None | Some(Value::Null) => match result.get("type").and_then(Value::as_str) {
            Some("object") => "null".to_string(),
            _ => "undefined".to_string(),
        },
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => js_num(n.as_f64().unwrap_or(0.0)),
        Some(Value::Bool(b)) => b.to_string(),
        Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".into()),
    }
}

async fn eval_tool(watched: &Watched, sid: &str, expression: &str) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let result = eval_remote_object(&conn, sid, expression).await?;
    Ok(json!({ "result": eval_str_from_remote(&result) }))
}

/// raw <method> [json]：任意 CDP 命令透传（PLAN D5 逃生舱；与退役前 cdp.mjs
/// evalraw 同权限）。browser=true 发 browser 级（Target.getTargets 等），
/// 否则发到 tab 的 page session（Runtime/DOM/Input/Emulation 全域可达）。
/// 响应 = CDP 原始 result 对象，原样透传不裁剪。
async fn raw_tool(
    watched: &Watched,
    sid: &str,
    method: &str,
    params: Value,
    browser: bool,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let result: Value = cdp_send(&conn, method, params, if browser { None } else { Some(sid) })
        .await
        .map_err(|e| AppError::from(PageControlError::dispatch_failed(&e.to_string(), false)))?;
    Ok(json!({ "result": result }))
}

/// TS htmlStr：selector 缺省 = 整文档 outerHTML；命中元素 → outerHTML。
/// 未命中的行为差异（任务两选一，取错误码）：TS 返回字符串哨兵
/// 'Element not found'，此处报 404 PAGE_ELEMENT_NOT_FOUND——与 click/locate
/// 的未命中语义一致，调用方按码分支而非解析文案。
async fn html_tool(
    watched: &Watched,
    sid: &str,
    selector: Option<&str>,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let expr = match selector {
        None => "document.documentElement.outerHTML".to_string(),
        Some(sel) => format!(
            "(function() {{ const el = document.querySelector({sel}); return el ? el.outerHTML : null; }})()",
            sel = crate::domain::page_control::json_escape(sel)
        ),
    };
    let result = eval_remote_object(&conn, sid, &expr).await?;
    match result.get("value") {
        Some(Value::String(html)) => Ok(json!({ "html": html })),
        // selector 路径的 null = 页内脚本显式回的未命中（区别于 eval 异常）
        _ if selector.is_some() => {
            Err(PageControlError::element_not_found(selector.unwrap_or("")).into())
        }
        // 整文档路径不会返回 null；防御空串（不装作成功也不 panic）
        _ => Ok(json!({ "html": "" })),
    }
}

/// TS clickXyStr：复用 click 的三段派发链，无 locate/hit-test（坐标即意图）。
async fn clickxy_tool(watched: &Watched, sid: &str, x: f64, y: f64) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    dispatch_click(&conn, sid, x, y).await?;
    Ok(json!({
        "result": format!("Clicked at CSS ({}, {})", js_num(x), js_num(y))
    }))
}

/// TS typeStr：对当前焦点元素直接 Input.insertText（无定位、无校验）。
/// 字符计数用 Unicode 标量（fill 同口径；TS 的 .length 是 UTF-16 单元，
/// 仅增补平面字符有差）。
async fn type_tool(watched: &Watched, sid: &str, text: &str) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    cdp_send(
        &conn,
        "Input.insertText",
        json!({ "text": text }),
        Some(sid),
    )
    .await?;
    Ok(json!({
        "result": format!("Typed {} characters", text.chars().count())
    }))
}

/// 存在性探针（TS loadAllStr 的 `!!document.querySelector(sel)`）：eval
/// 失败（导航中）原样上抛，循环不吞异常（TS 同为抛错退出）。
async fn selector_exists(
    conn: &SharedPageConn,
    sid: &str,
    exists_expr: &str,
) -> Result<bool, AppError> {
    Ok(eval_raw(conn, sid, exists_expr).await? == Value::Bool(true))
}

/// TS loadAllStr 的循环骨架，两处任务口径偏差：
/// - 点击用原生 selector 链（locate/hit-test/三段派发）替换 TS 的页内
///   el.click() 合成点击——项目纪律是原生输入（synthetic 仅 --unsafe-synth
///   场景才对，此处不引入）；
/// - 上限从 5min 硬顶改为 max-clicks，达上限后补一次存在性复查定文案
///   （诚实区分「消失了」与「仍在页面上」，调用方据此决定是否继续）。
async fn loadall_tool(
    watched: &Watched,
    sid: &str,
    selector: &str,
    interval: Duration,
    max_clicks: u32,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let exists_expr = format!(
        "!!document.querySelector({})",
        crate::domain::page_control::json_escape(selector)
    );
    let mut clicks: u32 = 0;
    let mut disappeared = false;
    while clicks < max_clicks {
        if !selector_exists(&conn, sid, &exists_expr).await? {
            disappeared = true;
            break;
        }
        click_selector(&conn, sid, selector).await?;
        clicks += 1;
        tokio::time::sleep(interval).await;
    }
    if !disappeared {
        // 达上限退出（或 max-clicks=0）：末次观察仍在页面——复查一次定文案
        // （selector_exists 返回「存在」，disappeared 取反）
        disappeared = !selector_exists(&conn, sid, &exists_expr).await?;
    }
    let result = if disappeared {
        format!("Clicked \"{selector}\" {clicks} time(s) until it disappeared")
    } else {
        format!(
            "Clicked \"{selector}\" {clicks} time(s) but it is still present (--max-clicks {max_clicks} reached)"
        )
    };
    Ok(json!({ "result": result }))
}

// ============================================================================
// 测试：纯逻辑（AX 快照/路由/分类）+ fake CDP 编排全链路（不依赖真 Chrome）
// ============================================================================

#[cfg(test)]
mod tests {
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
        let (text, refs) = collect_ax_tree(&nodes);
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
            ax_node("7", "heading", json!({ "backendDOMNodeId": 7 })), // 无名无值且不可引用 → 隐藏
        ];
        let (text, refs) = collect_ax_tree(&nodes);
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
        let (text, refs) = collect_ax_tree(&with_parent);
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
        let (empty_text, empty_refs) = collect_ax_tree(&[]);
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
        let e =
            classify_hit(&json!({ "reason": "covered", "by": "DIV.overlay" }), "#a").unwrap_err();
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

    /// attachedToTarget(page) 建映射 + 双 enable；非 page target 不入表
    #[tokio::test]
    async fn attached_registers_page_tab_and_enables_domains() {
        let (svc, calls, _script, tap) = fake_service();
        let (watched, g) = svc.ensure(INS).await.unwrap();
        drop(g);

        let tx = tap.lock().unwrap().clone().unwrap();
        tx.send(attach_event("TAB_A", "SES_A", "page")).unwrap();
        tx.send(attach_event("TAB_B", "SES_B", "iframe")).unwrap();

        eventually("TAB_A 入表", || tab_session(&watched, "TAB_A").is_some()).await;
        assert_eq!(tab_session(&watched, "TAB_A").as_deref(), Some("SES_A"));
        // iframe 等 target 不参与 page 命令路由（enable 只对 page session 发）
        eventually("双 enable 已发出", || {
            calls_of(&calls, "Page.enable").len() == 1
        })
        .await;
        assert_eq!(
            calls_of(&calls, "Page.enable")[0].session.as_deref(),
            Some("SES_A")
        );
        assert_eq!(
            calls_of(&calls, "DOM.enable")[0].session.as_deref(),
            Some("SES_A")
        );
        assert!(tab_session(&watched, "TAB_B").is_none());
        assert_eq!(
            calls_of(&calls, "Page.enable").len(),
            1,
            "非 page 不发 enable"
        );
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
                Ok(
                    json!({ "result": { "value": { "tag": "INPUT", "kind": "native", "label": "" } } }),
                ),
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
                Ok(
                    json!({ "result": { "value": { "tag": "INPUT", "kind": "native", "label": "" } } }),
                ),
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
        assert!(
            result.starts_with("Waited: .ready is visible (after ") && result.contains("1 polls")
        );

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
        let e =
            within_budget::<Value, _>(Duration::from_millis(20), "test", std::future::pending())
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
}
