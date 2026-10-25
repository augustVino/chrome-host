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
//! - **导航命令族**（navigate/back/forward/reload，page 族后补）：同一 page
//!   session 内 Page.navigate / 导航历史回退 / Page.reload——保留历史（实例级
//!   navigate 的 new+close 组合近似会丢历史，该端点保留给 GUI，文档已指向
//!   本族）。加载等待走 Page.loadEventFired 事件（命令发送前登记等待者，窗口
//!   NAV_LOAD_TIMEOUT，未见 → loaded:false；同文档导航/同文档历史条目不触发
//!   load，走响应分类 / 历史索引 + readyState 快路径）。
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
use tokio::sync::oneshot;

use crate::domain::instance::InstanceStatus;
use crate::domain::fetch_rules::{cdp_patterns, compile_rules, first_live_match, Action, InterceptRule};
use crate::domain::page_control::{
    dialog_accept_params, fill_fillable_check_expr, hit_test_expr, largest_quad_centroid,
    locate_expr, normalize_ref, observed_matches_state, parse_dialog_opening, parse_duration,
    parse_key, quads_bbox, rounded_clip, scan_hover_rules, validate_http_url, wait_observe_expr,
    DialogBuffer, DialogInfo, HoverRules, ObservedState, PageControlError, Point, RefStore,
    WaitState, MAX_DIALOG_BUFFER, MAX_HOVER_SHEETS,
};
use crate::error::AppError;
use crate::infrastructure::cdp::page_cdp::{connect_page, SharedPageConn};
use crate::infrastructure::cdp::ws::EventSink;
use crate::infrastructure::db::repositories::instance_repository::InstanceRepository;

use super::fetch_mock::{MockSpec, MockUpstream};

/// 工具级总超时：单条 CDP send 已有内建 15s（ws.rs），这里兜住多命令链与
/// 无响应页面（wait 例外：用户 timeout + 5s）。
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
const WAIT_TOOL_GRACE: Duration = Duration::from_secs(5);
/// wait 轮询缺省超时与间隔（TS wait.mjs 默认；间隔固定起步，不做参数）
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// mousePressed → mouseReleased 的间隔（TS dispatchClickAtPoint 的 50ms）
const CLICK_RELEASE_DELAY: Duration = Duration::from_millis(50);
/// 导航命令族（navigate/back/forward/reload）的加载等待窗口：CDP 导航命令返回
/// ≠ 加载完成，命令后在同 session 上等待 Page.loadEventFired 至此窗口。
/// 窗口耗尽 → 返回 loaded:false（诚实证据哲学：导航已开始但加载未证实，
/// 不编造成功也不无限等死）。取 25s 而非 30s：给 CDP send（内建 15s）与响应
/// 组装留余量，使「加载未确认」优先于工具预算 504 呈现；SPA 路由不触发 load
/// 的场景见各工具注释与 AGENT-API。
const NAV_LOAD_TIMEOUT: Duration = Duration::from_secs(25);
/// back/forward 的提交确认轮询间隔（历史索引 currentIndex 到位即提交；与
/// wait 轮询同量级，无需参数化）。
const NAV_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// hover 扫描：样式表登记回放等待窗口（attach 后 CSS.enable 的
/// styleSheetAdded 事件异步在途，首条 snap 可能赶在回放前）；与 attach 握手
/// 窗口同量级。无表页（about:blank）每次 snap 付此窗口——接受（低频场景）。
const HOVER_SETTLE_WINDOW: Duration = Duration::from_millis(300);
const HOVER_SETTLE_POLL: Duration = Duration::from_millis(10);
/// hover 扫描：单选择器命中上限，超出按截断处理（防 `div` 类选择器命中
/// 万级节点；漏标优于错标，状态诚实报 truncated）。
const MAX_HOVER_QUERY_HITS: usize = 1000;
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

/// 409：back/forward 已在历史边界（无更早/更晚条目）。message 携带方向，
/// 调用方区分「到头了」与「失败」；这不属于错误用法，是可预期的边界探测。
fn nav_no_history(direction: &str) -> AppError {
    AppError::conflict(
        "PAGE_NAV_NO_HISTORY",
        format!("no {direction} history entry — already at the boundary of this tab's navigation history"),
    )
}

/// 500：Page.navigate 响应携带 errorText（net::ERR_* 等导航失败）——导航
/// 未发生，是失败；与 loaded:false（导航已开始、load 未确认）严格区分。
fn nav_failed(error_text: &str) -> AppError {
    AppError::business(
        500,
        "PAGE_NAV_FAILED",
        format!("navigation failed: {error_text}"),
    )
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
    /// per-tab load 事件等待者（导航命令族）：命令发送前登记 oneshot Sender，
    /// watch_loop 收到该 tab 的 Page.loadEventFired 时放行全部等待者。key =
    /// targetId（与 refs/dialogs 同键，drop_tab 一并清理）；超时/失败路径的
    /// Sender 残留至下次 load 事件（send 落空即弃，仅句柄暂存无泄漏风险）。
    load_waiters: Mutex<HashMap<String, Vec<tokio::sync::oneshot::Sender<()>>>>,
    /// per-tab 样式表登记（hover 扫描用）：CSS.enable 后 styleSheetAdded/
    /// Removed 事件维护。key = targetId → { styleSheetId → (origin, disabled) }；
    /// 读取侧才按 origin=regular 且未 disabled 过滤（登记全量，事件流不打折扣）。
    sheets: Mutex<SheetRegistry>,
    /// per-tab CSS.enable 成功时刻：hover 扫描的结算窗口只在该时刻后的首个
    /// HOVER_SETTLE_WINDOW 内生效（styleSheetAdded 回放可能仍在途）；窗口过后
    /// 登记仍空 = 真无表，后续 snap 即时返回（不为无表页付每快照等待）。
    css_since: Mutex<HashMap<String, Instant>>,
    /// per-tab Fetch 拦截运行态（会话生命周期：establish 整清 = 连接重建
    /// 自动失效，GET 披露 liveSince=null；规则由 intercept_set 重新下发）。
    /// **决策在事件流内进行**（on_request_paused，watch_loop 路径，不碰
    /// cmd_lock）；拦截响应命令走 conn 的并发 send（&self），与 in-flight
    /// 工具命令互不阻塞。
    intercept: Mutex<HashMap<String, InterceptState>>,
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
            load_waiters: Mutex::new(HashMap::new()),
            sheets: Mutex::new(HashMap::new()),
            css_since: Mutex::new(HashMap::new()),
            intercept: Mutex::new(HashMap::new()),
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
        // 旧代残留的 tabs/refs/dialogs/load_waiters/sheets 属于死会话（sessionId
        // 已失效），整清（等待中的导航命令各自走到自己的超时分支；CSS.enable
        // 重发后 styleSheetAdded 会重新回放）
        watched.tabs.lock().unwrap().clear();
        watched.refs.lock().unwrap().clear();
        watched.dialogs.lock().unwrap().clear();
        watched.load_waiters.lock().unwrap().clear();
        watched.sheets.lock().unwrap().clear();
        watched.css_since.lock().unwrap().clear();
        // Fetch 拦截随死会话失效（Fetch.enable 是 session 级，新连接需重新
        // 下发规则——契约：连接重建即失效，GET intercept 返回空 + liveSince null）
        watched.intercept.lock().unwrap().clear();
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

    /// navigate <tab> <url>：该 tab 的 page session 上直发 Page.navigate——
    /// 与实例级 navigate（new+close 组合，丢历史）不同，这里在同一 target 内
    /// 导航，历史/back/forward 全保留。等待 load 事件（NAV_LOAD_TIMEOUT 窗口，
    /// 窗口内未见 → loaded:false 诚实返回），语义详见 navigate_tool。URL 白名单
    /// 与实例级 tabs/navigate 同源（domain 共享校验），不合法在进串行锁前 400。
    pub async fn navigate(&self, ins_id: &str, tab: &str, url: &str) -> Result<Value, AppError> {
        validate_http_url(url)?;
        self.command(
            TOOL_TIMEOUT,
            "navigate",
            ins_id,
            tab,
            move |w, tid, sid| async move { navigate_tool(&w, &tid, &sid, url, NAV_LOAD_TIMEOUT).await },
        )
        .await
    }

    /// back <tab>：历史后退一步（无更早条目 → 409 PAGE_NAV_NO_HISTORY）。
    /// 完成信号双轨（新文档 = load 事件；同文档条目 = 历史索引提交 +
    /// readyState），详见 history_step_tool。
    pub async fn back(&self, ins_id: &str, tab: &str) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "back",
            ins_id,
            tab,
            move |w, tid, sid| async move {
                history_step_tool(&w, &tid, &sid, -1, NAV_LOAD_TIMEOUT).await
            },
        )
        .await
    }

    /// screenshot <tab> [--target]：元素级（@eN / CSS 选择器，复用定位链）或
    /// 整视口 png 截图（W-cap）。target 缺省 = 当前视口；元素路径：
    /// scrollIntoView best-effort → quads 联合 bbox → 回退 getBoundingClientRect
    /// → Page.captureScreenshot { clip }（与 click 的几何链同源，clip 是区域
    /// 取并集，点击是点取最大 quad）。
    pub async fn screenshot(
        &self,
        ins_id: &str,
        tab: &str,
        target: Option<&str>,
    ) -> Result<Value, AppError> {
        if target == Some("") {
            return Err(AppError::invalid_request(
                "target must be an @eN ref or CSS selector (omit it to capture the viewport)",
            ));
        }
        self.command(
            TOOL_TIMEOUT,
            "screenshot",
            ins_id,
            tab,
            |w, tid, sid| async move { screenshot_tool(&w, &tid, &sid, target).await },
        )
        .await
    }

    /// forward <tab>：历史前进一步（无更晚条目 → 409 PAGE_NAV_NO_HISTORY）。
    pub async fn forward(&self, ins_id: &str, tab: &str) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "forward",
            ins_id,
            tab,
            move |w, tid, sid| async move {
                history_step_tool(&w, &tid, &sid, 1, NAV_LOAD_TIMEOUT).await
            },
        )
        .await
    }

    /// pdf <tab>：Page.printToPDF 整页 PDF（W-cap）。landscape 缺省 false、
    /// printBackground 缺省 true、纸张缺省 Letter 8.5×11 英寸。前置
    /// Page.enable 已由 on_attached 在 session 建立时统一补启（与对话框事件
    /// 共用同一域启用集，无需命令内按需 enable——见任务报告决策 2）。
    pub async fn pdf(
        &self,
        ins_id: &str,
        tab: &str,
        landscape: Option<bool>,
        print_background: Option<bool>,
        paper_width: Option<f64>,
        paper_height: Option<f64>,
    ) -> Result<Value, AppError> {
        for (label, v) in [("paperWidth", paper_width), ("paperHeight", paper_height)] {
            if let Some(v) = v {
                if !v.is_finite() || v <= 0.0 {
                    return Err(AppError::invalid_request(format!(
                        "pdf: {label} 必须为正数英寸（got {v}）"
                    )));
                }
            }
        }
        self.command(
            TOOL_TIMEOUT,
            "pdf",
            ins_id,
            tab,
            |w, _tid, sid| async move {
                pdf_tool(
                    &w,
                    &sid,
                    landscape.unwrap_or(false),
                    print_background.unwrap_or(true),
                    paper_width.unwrap_or(8.5),
                    paper_height.unwrap_or(11.0),
                )
                .await
            },
        )
        .await
    }

    /// reload <tab> [--ignore-cache]：Page.reload；等待语义同 navigate
    /// （load 事件窗口，未见 → loaded:false）。
    pub async fn reload(
        &self,
        ins_id: &str,
        tab: &str,
        ignore_cache: bool,
    ) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "reload",
            ins_id,
            tab,
            move |w, tid, sid| async move {
                reload_tool(&w, &tid, &sid, ignore_cache, NAV_LOAD_TIMEOUT).await
            },
        )
        .await
    }

    /// POST intercept：规则集整表替换（空 rules = 清空 + Fetch.disable）。
    /// 编译/校验在串行锁外快速失败；enable 成功才落状态（失败不动旧规则）。
    /// wire：`{"rules": [...], "tab?": ...}`（rules 数组必填；空数组 = 清空）。
    pub async fn intercept_set(
        &self,
        ins_id: &str,
        tab: &str,
        body: &Value,
    ) -> Result<Value, AppError> {
        let rules_json = body
            .get("rules")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                AppError::invalid_request("body.rules 必须是数组（空数组 = 清空拦截）")
            })?;
        let rules = compile_rules(rules_json)?;
        let patterns = cdp_patterns(&rules)?;
        self.command(
            TOOL_TIMEOUT,
            "intercept-set",
            ins_id,
            tab,
            move |w, tid, sid| async move {
                let conn = require_conn(&w)?;
                if rules.is_empty() {
                    // 清空：先 Fetch.disable（失败也继续清状态——残留的 enable
                    // 随连接重建自然失效，状态与查询面必须一致）
                    let _ = cdp_send(&conn, "Fetch.disable", json!({}), Some(&sid)).await;
                    w.intercept.lock().unwrap().remove(&tid);
                    return Ok(json!({
                        "result": "intercept cleared",
                        "rules": 0,
                        "instanceId": "", "targetId": "",
                    }));
                }
                cdp_send(
                    &conn,
                    "Fetch.enable",
                    json!({ "patterns": patterns }),
                    Some(&sid),
                )
                .await?;
                let n = rules.len();
                w.intercept
                    .lock()
                    .unwrap()
                    .insert(tid.clone(), InterceptState::new(rules));
                // 披露 mock 上游（mock 命中的请求会被改写到该 loopback 源，
                // 页面侧 response.url 随之变化——诚实披露机制）
                let upstream = MockUpstream::shared()
                    .await
                    .map(|u| json!(format!("http://127.0.0.1:{}", u.port())))
                    .unwrap_or(serde_json::Value::Null);
                Ok(json!({
                    "result": "intercept active",
                    "rules": n,
                    "patterns": patterns.len(),
                    "mockUpstream": upstream,
                }))
            },
        )
        .await
    }

    /// GET intercept：规则状态（命中/剩余/活跃）+ 干预记录环 + liveSince。
    /// 无状态（未下发或连接重建失效）→ 空表 + liveSince null + 提示。
    pub async fn intercept_get(&self, ins_id: &str, tab: &str) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "intercept-get",
            ins_id,
            tab,
            |w, tid, _sid| async move {
                let guard = w.intercept.lock().unwrap();
                let Some(st) = guard.get(&tid) else {
                    return Ok(json!({
                        "rules": [],
                        "journal": [],
                        "liveSince": null,
                        "note": "无活跃拦截（未下发，或连接重建后自动失效——重新 POST 下发）",
                    }));
                };
                let rules: Vec<Value> = st
                    .rules
                    .iter()
                    .map(|r| {
                        let hits = st.used.get(&r.id).copied().unwrap_or(0);
                        let exhausted = r.times > 0 && hits >= u64::from(r.times);
                        json!({
                            "id": r.id,
                            "urlPattern": r.url_pattern,
                            "action": match &r.action {
                                Action::Mock { status, delay_ms, .. } =>
                                    json!({"type": "mock", "status": status, "delayMs": delay_ms}),
                                Action::SetHeaders { .. } => json!({"type": "setHeaders"}),
                                Action::Block => json!({"type": "block"}),
                            },
                            "times": r.times,
                            "hits": hits,
                            "remaining": if r.times == 0 { Value::Null } else { json!(r.times.saturating_sub(hits as u32)) },
                            "active": !exhausted,
                        })
                    })
                    .collect();
                Ok(json!({
                    "rules": rules,
                    "journal": st.journal.iter().cloned().collect::<Vec<_>>(),
                    "liveSince": st.live_since_ms,
                }))
            },
        )
        .await
    }

    /// emulate <tab>：设备仿真（W-cap）。preset / viewport / off 三形态，
    /// 解析（含 422 未知预设）在 domain::emulate 完成——建连前即可拒绝。
    /// 语义：同一 tab 重复 emulate = 覆盖（未给字段恢复默认，非合并）；
    /// off 清全部；override 绑定 page session，连接重建/实例重启自然清除。
    pub async fn emulate(
        &self,
        ins_id: &str,
        tab: &str,
        req: crate::domain::emulate::EmulateRequest,
    ) -> Result<Value, AppError> {
        let action = crate::domain::emulate::resolve(&req)?;
        self.command(
            TOOL_TIMEOUT,
            "emulate",
            ins_id,
            tab,
            move |w, _tid, sid| async move { emulate_tool(&w, &sid, action).await },
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
        // 导航命令族（navigate/reload/back/forward）的加载信号：等待者在命令
        // 发送前登记（见 register_load_waiter），load 事件到达即放行该 tab 全部
        // 等待者。Page.enable 在 attach 时已发（on_attached），事件必然可达。
        "Page.loadEventFired" => {
            if let Some(tid) = tab_of_session(watched, event) {
                if let Some(waiters) = watched.load_waiters.lock().unwrap().remove(&tid) {
                    for tx in waiters {
                        let _ = tx.send(());
                    }
                }
            }
        }
        // hover 扫描的样式表登记：CSS.enable（on_attached）后 Chrome 回放/增量
        // 推送 styleSheetAdded；Removed 按表撤销。读取侧才过滤 origin/disabled。
        // 扩展注入表（sourceURL 以 chrome-extension:// 开头）登记即排除：对
        // 页面 hover 语义无贡献，且取文本不稳定（真机复测：混入 regular 扫描
        // 会恒触发 truncated，狼来了效应压死 skipped）。
        "CSS.styleSheetAdded" => {
            if let Some(tid) = tab_of_session(watched, event) {
                let Some(header) = params.get("header") else { return };
                let (Some(id), Some(origin)) = (
                    header.get("styleSheetId").and_then(Value::as_str),
                    header.get("origin").and_then(Value::as_str),
                ) else {
                    return;
                };
                let source_url = header
                    .get("sourceURL")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if source_url.starts_with("chrome-extension://") {
                    tracing::debug!(
                        "[page] tab {tid} 跳过扩展样式表 {id}（sourceURL={source_url}，origin={origin}）"
                    );
                    return;
                }
                let disabled = header
                    .get("disabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                watched
                    .sheets
                    .lock()
                    .unwrap()
                    .entry(tid)
                    .or_default()
                    .insert(id.to_string(), (origin.to_string(), disabled));
            }
        }
        "CSS.styleSheetRemoved" => {
            if let Some(tid) = tab_of_session(watched, event) {
                if let Some(id) = params.get("styleSheetId").and_then(Value::as_str) {
                    if let Some(m) = watched.sheets.lock().unwrap().get_mut(&tid) {
                        m.remove(id);
                    }
                }
            }
        }
        // Fetch 拦截决策（任务红线 ①）：requestPaused 在**事件流内**决策并回——
        // on_request_paused 不碰 per-instance 命令串行锁（watch_loop 天然不持
        // 锁），拦截响应命令走 conn 并发 send，与 in-flight 工具命令互不阻塞；
        // 否则规则拦截会堵死全部 page 命令。
        "Fetch.requestPaused" => {
            if let Some(sid) = event.get("sessionId").and_then(Value::as_str) {
                on_request_paused(watched, sid, &params).await;
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
                    // 换文档 = 旧表全部失效：Chrome 在文档销毁时不保证逐张发
                    // styleSheetRemoved（真机实测：导航后登记表残留死表，
                    // getStyleSheetText 报 No style sheet with given id found →
                    // hoverScan 恒 truncated）。新文档的表由 styleSheetAdded
                    // 重新登记（CSS 域 session 级持续有效，不依赖 Removed 语义）。
                    watched.sheets.lock().unwrap().remove(&tid);
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
    // CSS.enable 与 Page/DOM 同点位：hover 扫描的 styleSheetAdded 回放从 attach
    // 起积累（snap 时按需 enable 无法同步拿到回放——事件异步，注册表会是空）。
    // 成功时刻入 css_since：hover 扫描的结算窗口以此为起点。
    let mut css_enabled = false;
    for method in ["Page.enable", "DOM.enable", "CSS.enable"] {
        let ok = match cdp_send(&conn, method, json!({}), Some(sid)).await {
            Ok(_) => true,
            Err(e) => {
                tracing::debug!("[page] session {sid} {method} 失败: {e}");
                false
            }
        };
        if method == "CSS.enable" {
            css_enabled = ok;
        }
    }
    if css_enabled {
        watched
            .css_since
            .lock()
            .unwrap()
            .insert(tid.to_string(), Instant::now());
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
/// RefStore / DialogBuffer / load 等待者一起删（会话已死，状态随之作废；
/// 等待中的导航命令各自走到自己的超时分支）。
fn drop_tab(watched: &Watched, tid: &str) {
    watched.tabs.lock().unwrap().detach(tid);
    watched.refs.lock().unwrap().remove(tid);
    watched.dialogs.lock().unwrap().remove(tid);
    watched.load_waiters.lock().unwrap().remove(tid);
    watched.sheets.lock().unwrap().remove(tid);
    watched.css_since.lock().unwrap().remove(tid);
    watched.intercept.lock().unwrap().remove(tid);
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

// ============================================================================
// 3b. Fetch 拦截：事件流内决策（红线：不碰串行锁；任何错误必放行）
// ============================================================================

/// per-tab 拦截运行态（Fetch.enable 生效期间存在；establish/drop_tab 清除）。
/// 规则本体不可变（下发时编译），times 耗尽由 used 计数判定。
struct InterceptState {
    rules: Vec<InterceptRule>,
    /// id → 已命中次数
    used: HashMap<String, u64>,
    /// 干预记录环（含命中与放行，每条已脱敏；容量 domain::fetch_rules::JOURNAL_CAPACITY）
    journal: std::collections::VecDeque<Value>,
    /// 规则集下发时刻（epoch ms；GET 披露）
    live_since_ms: i64,
}

impl InterceptState {
    fn new(rules: Vec<InterceptRule>) -> Self {
        InterceptState {
            rules,
            used: HashMap::new(),
            journal: std::collections::VecDeque::with_capacity(4),
            live_since_ms: chrono::Utc::now().timestamp_millis(),
        }
    }

    /// 干预记录入环（超容量滚出旧条）。
    fn journal_push(&mut self, mut entry: Value) {
        if self.journal.len() >= crate::domain::fetch_rules::JOURNAL_CAPACITY {
            self.journal.pop_front();
        }
        entry["ts"] = json!(chrono::Utc::now().timestamp_millis());
        self.journal.push_back(entry);
    }
}

/// 干预记录条目（诚实证据第一公民：命中与放行都留痕，url 脱敏）。
fn journal_entry(request_id: &str, url: &str, outcome: &str, rule: Option<&str>, detail: &str) -> Value {
    json!({
        "requestId": request_id,
        "url": crate::domain::redact::redact_url(url),
        "rule": rule,
        "outcome": outcome,   // mocked | headers-set | blocked | passed | failed-then-passed | stranded
        "detail": detail,
    })
}

/// 请求头合并：pause 事件的原始头（对象形态）∪ 规则头（数组形态，同名覆盖）
/// → CDP continueRequest 要的 [{name, value}] 数组（替换语义下原头全部保留
/// ——包括页面自身的 cookie：黑名单拦的是**规则**设凭据，不动页面原生行为）。
fn merge_request_headers(original: Option<&Value>, overrides: &[(String, String)]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let upsert = |out: &mut Vec<Value>, k: &str, v: &str| {
        if let Some(pos) = out
            .iter()
            .position(|h| h["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(k)))
        {
            out[pos] = json!({ "name": k, "value": v });
        } else {
            out.push(json!({ "name": k, "value": v }));
        }
    };
    if let Some(Value::Object(map)) = original {
        for (k, v) in map {
            upsert(&mut out, k, v.as_str().unwrap_or_default());
        }
    }
    for (k, v) in overrides {
        upsert(&mut out, k, v);
    }
    out
}

/// requestPaused 事件流内决策（红线 ①不碰串行锁；红线 ②任何错误必放行）。
/// 无规则/未命中/耗尽 → continueRequest 裸放行；决策命令失败 → 裸 continue
/// 兑底（requestPaused 不回应 = 页面挂死，spike 实测）；兑底也失败（连接
/// 已死）→ journal 记 stranded（请求随连接死亡，无从挽救）。
async fn on_request_paused(watched: &Watched, session_id: &str, params: &Value) {
    let Some(conn) = watched.conn.lock().unwrap().clone() else {
        return; // 无连接：请求随连接死亡（与 watch_loop 同生命周期，理论不可达）
    };
    let Some(tid) = watched
        .tabs
        .lock()
        .unwrap()
        .target_of_session(session_id)
        .map(str::to_string)
    else {
        return;
    };
    let Some(request_id) = params.get("requestId").and_then(Value::as_str) else {
        return;
    };
    let request_id = request_id.to_string();
    let url = params
        .pointer("/request/url")
        .and_then(Value::as_str)
        .unwrap_or("");
    let resource_type = params
        .get("resourceType")
        .and_then(Value::as_str)
        .unwrap_or("Other");

    // 决策（短锁：匹配 + 计数 + journal；无状态/未命中/耗尽 → None = 放行）
    let decision: Option<(String, Action)> = {
        let mut guard = watched.intercept.lock().unwrap();
        let state = guard.get_mut(&tid);
        state.and_then(|st| {
            let matched = first_live_match(&st.rules, &st.used, url, resource_type);
            matched.map(|rule| {
                *st.used.entry(rule.id.clone()).or_insert(0) += 1;
                (rule.id.clone(), rule.action.clone())
            })
        })
    };

    // 未命中放行（常态路径：patterns 命中但 resourceTypes/times 不满足）
    let Some((rule_id, action)) = decision else {
        let entry = journal_entry(&request_id, url, "passed", None, "no live rule matched");
        watched
            .intercept
            .lock()
            .unwrap()
            .get_mut(&tid)
            .map(|st| st.journal_push(entry));
        let _ = cdp_send(
            &conn,
            "Fetch.continueRequest",
            json!({ "requestId": request_id }),
            Some(session_id),
        )
        .await;
        return;
    };

    // 执行决策（锁外；命令走 conn 并发 send）
    let outcome: &str;
    let executed: Result<Value, AppError> = match &action {
        Action::Mock {
            status,
            headers,
            body,
            delay_ms,
        } => {
            outcome = "mocked";
            match MockUpstream::shared().await {
                Ok(upstream) => {
                    let mock_url = upstream.register(MockSpec {
                        status: *status,
                        headers: headers.clone(),
                        body: body.clone(),
                        delay_ms: *delay_ms,
                    });
                    cdp_send(
                        &conn,
                        "Fetch.continueRequest",
                        json!({ "requestId": request_id, "url": mock_url }),
                        Some(session_id),
                    )
                    .await
                }
                Err(e) => Err(e),
            }
        }
        Action::SetHeaders { headers } => {
            outcome = "headers-set";
            let merged = merge_request_headers(params.pointer("/request/headers"), headers);
            cdp_send(
                &conn,
                "Fetch.continueRequest",
                json!({ "requestId": request_id, "headers": merged }),
                Some(session_id),
            )
            .await
        }
        Action::Block => {
            outcome = "blocked";
            cdp_send(
                &conn,
                "Fetch.failRequest",
                json!({ "requestId": request_id, "errorReason": "BlockedByClient" }),
                Some(session_id),
            )
            .await
        }
    };

    match executed {
        Ok(_) => {
            let entry = journal_entry(&request_id, url, outcome, Some(&rule_id), "");
            watched
                .intercept
                .lock()
                .unwrap()
                .get_mut(&tid)
                .map(|st| st.journal_push(entry));
        }
        Err(e) => {
            // 红线 ②：决策失败必放行（裸 continue，不改写任何东西）；
            // 兑底也失败 = 连接已死，journal 记 stranded（诚实披露）
            let fallback = cdp_send(
                &conn,
                "Fetch.continueRequest",
                json!({ "requestId": request_id }),
                Some(session_id),
            )
            .await;
            let (outcome_fb, detail) = if fallback.is_ok() {
                ("failed-then-passed", format!("decision failed: {e}"))
            } else {
                ("stranded", format!("decision failed: {e}; fallback also failed"))
            };
            let entry = journal_entry(&request_id, url, outcome_fb, Some(&rule_id), &detail);
            watched
                .intercept
                .lock()
                .unwrap()
                .get_mut(&tid)
                .map(|st| st.journal_push(entry));
        }
    }
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

fn format_ax_line(
    node: &Value,
    depth: usize,
    r#ref: Option<&str>,
    hover: Option<&str>,
) -> String {
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

/// 前序 DFS：只有 ref-worthy 节点消耗编号（从 1 起）；无 backendDOMNodeId
/// 的节点不发 ref（无法解析到 CDP 节点的 ref 必是死点击）；childIds 优先、
/// parent 反查表补位（TS orderedAxChildren）；游离节点兜底遍历（visited 去重）。
fn visit_ax(
    node: &Value,
    depth: usize,
    v: &mut AxVisit,
    by_id: &HashMap<String, &Value>,
    by_parent: &HashMap<String, Vec<&Value>>,
    hover: &HoverMarks,
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
        let mark = node
            .get("backendDOMNodeId")
            .and_then(Value::as_i64)
            .and_then(|b| hover.get(&b).copied());
        v.lines
            .push(format_ax_line(node, depth, assigned.as_deref(), mark));
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
        visit_ax(child, depth + 1, v, by_id, by_parent, hover);
    }
}

/// snap hover 感知：backendNodeId → 标记（"hover" = hover 触发器，
/// "hover-reveal" = hover 后才显示/变化的目标）。
type HoverMarks = HashMap<i64, &'static str>;
/// per-tab 样式表登记：targetId → { styleSheetId → (origin, disabled) }。
type SheetRegistry = HashMap<String, HashMap<String, (String, bool)>>;

/// Accessibility.getFullAXTree 的 nodes → (text, refs)。refs 形状 = TS
/// collectSnapshot 的 `[{ref, backendNodeId, name?}]`（RefStore::replace 入参）。
/// hover：ref 命中的 hover 标记（第二遍渲染传入；第一遍传空表只取 refs）。
fn collect_ax_tree(nodes: &[Value], hover: &HoverMarks) -> (String, Vec<Value>) {
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
            visit_ax(n, 0, &mut v, &by_id, &by_parent, hover);
        }
    }
    for n in nodes {
        visit_ax(n, 0, &mut v, &by_id, &by_parent, hover); // 兜底：游离节点（visited 已去重）
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
    // 两遍渲染：第一遍只取 refs（backendNodeId 全集，hover 桥接的输入），
    // hover 扫描后第二遍带标记渲染文本树（纯内存重入，代价可忽略）
    let (_, refs) = collect_ax_tree(&nodes, &HoverMarks::default());
    // hover 感知（增强项：任何失败不阻断 snap 主体，状态如实上报）
    let hover = hover_scan(watched, &conn, _tid, sid, &refs).await;
    let (text, refs) = collect_ax_tree(&nodes, &hover.marks);
    let refs_count = refs.len();
    // 整表替换 + generation+1 + 清 stale（§3.5）：refs 只描述最后一次 snap
    let generation = {
        let mut tabs = watched.refs.lock().unwrap();
        let store = tabs.entry(_tid.to_string()).or_default();
        store.replace(&Value::Array(refs));
        store.generation()
    };
    Ok(json!({ "text": text, "refs_count": refs_count, "generation": generation, "hoverScan": hover.status }))
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
            return HoverOutcome { marks: HoverMarks::default(), status: "skipped" };
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
        return HoverOutcome { marks: HoverMarks::default(), status: "skipped" };
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
        return HoverOutcome { marks: HoverMarks::default(), status };
    }
    // DOM 求值：根 nodeId（fill 定位链同款 getDocument；退化值 1 = 主文档恒为
    // 首个 getDocument 的 nodeId，spike 实测）
    let root = match cdp_send(conn, "DOM.getDocument", json!({}), Some(sid)).await {
        Ok(r) => r.pointer("/root/nodeId").and_then(Value::as_i64).unwrap_or(1),
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
            return HoverOutcome { marks: HoverMarks::default(), status: "error" };
        }
    };
    if node_ids.len() != backends.len() {
        // 形状防御（逐位映射的前提）：宁可放弃标记也不错位错标
        return HoverOutcome { marks: HoverMarks::default(), status: "error" };
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
/// 选择器非法（CDP 报错，启发式提取产物可能的形态）→ 跳过该选择器 + 截断。
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
        Err(_) => *truncated = true,
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

// ============================================================================
// 6b. 截图 / PDF / 仿真（W-cap）
// ============================================================================

/// DOM 节点句柄：ref 路径拿到 backendNodeId，selector 路径（getDocument →
/// querySelector）拿到 nodeId——scrollIntoViewIfNeeded / getContentQuads /
/// resolveNode 三个命令同时接受两者，统一透传不换算。
#[derive(Clone, Copy)]
enum NodeHandle {
    BackendNode(i64),
    Node(i64),
}

impl NodeHandle {
    /// CDP 参数键值对（json! 里展开：{ key: value }）
    fn param(&self) -> (&'static str, i64) {
        match self {
            NodeHandle::BackendNode(id) => ("backendNodeId", *id),
            NodeHandle::Node(id) => ("nodeId", *id),
        }
    }
}

/// 元素在视口中的可截图矩形：与 click 的 resolve_ref_point 同链（scroll
/// best-effort → quads → rect 回退），差异仅在产出——点 → 联合包围盒，
/// 且退化几何统一报 PAGE_TARGET_MOVED（与点击链同语义，不另造码）。
async fn resolve_clip_rect(
    conn: &SharedPageConn,
    sid: &str,
    handle: NodeHandle,
) -> Result<crate::domain::page_control::Rect, AppError> {
    let (key, id) = handle.param();
    let _ = cdp_send(
        conn,
        "DOM.scrollIntoViewIfNeeded",
        json!({ key: id }),
        Some(sid),
    )
    .await;
    if let Ok(resp) = cdp_send(
        conn,
        "DOM.getContentQuads",
        json!({ key: id }),
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
        if let Some(r) = quads_bbox(&quads) {
            return Ok(r);
        }
    }
    // 回退：resolveNode → getBoundingClientRect（与 resolve_ref_point 同源）
    let resp = cdp_send(conn, "DOM.resolveNode", json!({ key: id }), Some(sid)).await
        .map_err(|e| PageControlError::target_moved(&e.to_string()))?;
    let object_id = resp
        .pointer("/object/objectId")
        .and_then(Value::as_str)
        .ok_or_else(|| PageControlError::target_moved("no objectId"))?;
    let resp = cdp_send(
        conn,
        "Runtime.callFunctionOn",
        json!({ "objectId": object_id, "functionDeclaration": RECT_CENTER_FN, "returnByValue": true }),
        Some(sid),
    )
    .await
    .map_err(|e| PageControlError::target_moved(&e.to_string()))?;
    let rect = resp.pointer("/result/value").cloned().unwrap_or(Value::Null);
    let (x, y, w, h) = (
        rect.get("x").and_then(Value::as_f64),
        rect.get("y").and_then(Value::as_f64),
        rect.get("w").and_then(Value::as_f64),
        rect.get("h").and_then(Value::as_f64),
    );
    match (x, y, w, h) {
        (Some(x), Some(y), Some(w), Some(h)) if w > 0.0 && h > 0.0 => {
            Ok(crate::domain::page_control::Rect { x, y, width: w, height: h })
        }
        _ => Err(PageControlError::target_moved("empty rect").into()),
    }
}

/// screenshot 工具：无 target = 整视口；有 target = ref / selector 定位链 →
/// clip（整数化、scale 1）→ Page.captureScreenshot。响应与诊断面 screenshot
/// 同形状（format/data），command() 补 instanceId/targetId。
async fn screenshot_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    target: Option<&str>,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let params = match target {
        None => json!({ "format": "png" }),
        Some(t) => {
            let handle = if is_ref_target(t) {
                let entry = resolve_ref_entry(watched, tid, t)?;
                NodeHandle::BackendNode(entry.backend_node_id)
            } else {
                // selector 路径：与 fill 同链（getDocument → querySelector），
                // nodeId 0 = 未命中（同码 PAGE_ELEMENT_NOT_FOUND）
                let doc = cdp_send(&conn, "DOM.getDocument", json!({}), Some(sid)).await?;
                let root = doc
                    .pointer("/root/nodeId")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let q = cdp_send(
                    &conn,
                    "DOM.querySelector",
                    json!({ "nodeId": root, "selector": t }),
                    Some(sid),
                )
                .await?;
                let node_id = q.get("nodeId").and_then(Value::as_i64).unwrap_or(0);
                if node_id == 0 {
                    return Err(PageControlError::element_not_found(t).into());
                }
                NodeHandle::Node(node_id)
            };
            let rect = resolve_clip_rect(&conn, sid, handle).await?;
            let clip = rounded_clip(&rect);
            json!({
                "format": "png",
                "clip": { "x": clip.x, "y": clip.y, "width": clip.width, "height": clip.height, "scale": 1 }
            })
        }
    };
    let resp = cdp_send(&conn, "Page.captureScreenshot", params, Some(sid)).await?;
    let data = resp
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::internal("截图响应缺少 data 字段"))?
        .to_string();
    Ok(json!({ "format": "png", "data": data }))
}

/// pdf 工具：参数透传（默认值已在 service 入口归一）。缺 data = 显式报错，
/// 不静默降级（如被 page preview 拦截等异常路径）。
async fn pdf_tool(
    watched: &Watched,
    sid: &str,
    landscape: bool,
    print_background: bool,
    paper_width: f64,
    paper_height: f64,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let resp = cdp_send(
        &conn,
        "Page.printToPDF",
        json!({
            "landscape": landscape,
            "printBackground": print_background,
            "paperWidth": paper_width,
            "paperHeight": paper_height,
        }),
        Some(sid),
    )
    .await?;
    let data = resp
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::internal("PDF 响应缺少 data 字段"))?
        .to_string();
    Ok(json!({ "format": "pdf", "data": data }))
}

/// emulate 工具：无条件发 set/clear（CDP 幂等），零状态实现覆盖语义——
/// 应用时 UA 未给则恢复默认（空串 setUserAgentOverride：CfT 131 无
/// clearUserAgentOverride，真机实测）、touch 未给则关闭（未给 = 恢复默认，
/// 非保持旧值）；off 清全部三域（metrics clear + UA 空串 + touch 关）。
/// 仿真只作用于本 tab 的 page session（CDP session 级），与实例级无关。
async fn emulate_tool(
    watched: &Watched,
    sid: &str,
    action: crate::domain::emulate::EmulateAction,
) -> Result<Value, AppError> {
    use crate::domain::emulate::{EmulateAction, EmulateOrigin};
    let conn = require_conn(watched)?;
    match action {
        EmulateAction::Apply(spec) => {
            cdp_send(
                &conn,
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": spec.width,
                    "height": spec.height,
                    "deviceScaleFactor": 0,
                    "mobile": spec.mobile,
                }),
                Some(sid),
            )
            .await?;
            match &spec.user_agent {
                Some(ua) => {
                    cdp_send(
                        &conn,
                        "Emulation.setUserAgentOverride",
                        json!({ "userAgent": ua }),
                        Some(sid),
                    )
                    .await?;
                }
                None => {
                    // 覆盖语义：本次不带 UA = 不仿真 UA，恢复默认。CfT 131
                    // 无 Emulation.clearUserAgentOverride（-32601，真机实测），
                    // 改发空串 UA——真机验证可恢复默认 UA
                    cdp_send(
                        &conn,
                        "Emulation.setUserAgentOverride",
                        json!({ "userAgent": "" }),
                        Some(sid),
                    )
                    .await?;
                }
            }
            cdp_send(
                &conn,
                "Emulation.setTouchEmulationEnabled",
                json!({ "enabled": spec.touch }),
                Some(sid),
            )
            .await?;
            let origin = match spec.origin {
                EmulateOrigin::Preset(name) => format!("preset {name}"),
                EmulateOrigin::Custom => "custom viewport".to_string(),
            };
            let ua_note = if spec.user_agent.is_some() {
                ", UA overridden"
            } else {
                ""
            };
            Ok(json!({
                "result": format!(
                    "Emulated {} ({}×{}, mobile={}, touch={}{ua_note})",
                    origin, spec.width, spec.height, spec.mobile, spec.touch
                )
            }))
        }
        EmulateAction::Off => {
            // clearDeviceMetricsOverride 存在且有效（真机验证）；UA 恢复用
            // 空串 setUserAgentOverride（CfT 131 无 clearUserAgentOverride，
            // 真机实测 -32601；空串已验证恢复默认 UA）
            cdp_send(
                &conn,
                "Emulation.clearDeviceMetricsOverride",
                json!({}),
                Some(sid),
            )
            .await?;
            cdp_send(
                &conn,
                "Emulation.setUserAgentOverride",
                json!({ "userAgent": "" }),
                Some(sid),
            )
            .await?;
            cdp_send(
                &conn,
                "Emulation.setTouchEmulationEnabled",
                json!({ "enabled": false }),
                Some(sid),
            )
            .await?;
            Ok(json!({ "result": "Cleared device emulation (viewport / UA / touch restored)" }))
        }
    }
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
// 6c. 导航命令族（navigate/back/forward/reload）
// ============================================================================

/// load 等待登记：必须在导航命令发送**之前**调用——事件经 watch_loop 异步
/// 分派，若先发送后登记，极速加载（磁盘缓存）的 load 事件可能在登记前到达，
/// 造成永久等不到（假 loaded:false）。登记后发送即无错过窗口（命令响应与
/// 事件同连接保序，load 必要晚于 Page.navigate 响应才可能发生）。
fn register_load_waiter(watched: &Watched, tid: &str) -> oneshot::Receiver<()> {
    let (tx, rx) = oneshot::channel();
    watched
        .load_waiters
        .lock()
        .unwrap()
        .entry(tid.to_string())
        .or_default()
        .push(tx);
    rx
}

/// 等待 load 事件至截止线：true = 窗口内观察到 Page.loadEventFired。
/// 已知边界（诚实披露而非修复）：loadEventFired 不携带 loaderId，若命令
/// 发出时该 tab 恰有一个更早的在途加载，其 load 事件可能被计入（puppeteer
/// 同类时序靠 loaderId 匹配根治，超出本层事件面）。
async fn await_load(rx: oneshot::Receiver<()>, deadline: Instant) -> bool {
    matches!(
        tokio::time::timeout_at(deadline.into(), rx).await,
        Ok(Ok(()))
    )
}

/// navigate <url>：Page.navigate 直发到 page session。
/// 等待语义（任务决策记录）：CDP 导航命令返回 ≠ 加载完成，本工具等待
/// Page.loadEventFired（窗口 NAV_LOAD_TIMEOUT）后返回：
/// - 窗口内观察到 → loaded:true；
/// - 窗口耗尽 → loaded:false（导航已开始、加载未证实——不编造成功，也不
///   无限等死；调用方用 snap/wait/eval 观察实际状态）；
/// - 同文档导航（#fragment / pushState 型）：响应无新 loader
///   （sameDocumentNavigation / loaderId 缺失）→ 不等待，loaded:true（DOM
///   同步替换，无 load 事件可等）。
///
/// SPA 路由说明：页面内已加载 SPA 的路由切换（pushState/replaceState）不经
/// 过本命令，自然也不触发 load 事件——观察 SPA 路由后的 DOM 用 snap/wait。
async fn navigate_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    url: &str,
    load_timeout: Duration,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let load = register_load_waiter(watched, tid);
    let resp = cdp_send(&conn, "Page.navigate", json!({ "url": url }), Some(sid)).await?;
    // errorText（net::ERR_* 等）= 导航未发生 → 失败而非 loaded:false
    if let Some(err) = resp
        .get("errorText")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Err(nav_failed(err));
    }
    let same_doc = resp.get("sameDocumentNavigation").and_then(Value::as_bool) == Some(true)
        || resp.get("loaderId").is_none_or(Value::is_null);
    let deadline = Instant::now() + load_timeout;
    let (loaded, note) = if same_doc {
        (true, "same-document navigation")
    } else if await_load(load, deadline).await {
        (true, "load complete")
    } else {
        (
            false,
            "load not confirmed within the window — page may still be loading",
        )
    };
    Ok(json!({
        "result": format!("Navigated to {url} ({note})"),
        "url": url,
        "loaded": loaded,
    }))
}

/// 取导航历史并做边界检查：返回 (target index, entryId, url)。
/// 响应形状并带 → INTERNAL_ERROR（防御，CDP 正常不会发垃圾）。
async fn nav_history_target(
    conn: &SharedPageConn,
    sid: &str,
    delta: i64,
) -> Result<(i64, Value, String), AppError> {
    let hist = cdp_send(conn, "Page.getNavigationHistory", json!({}), Some(sid)).await?;
    let current = hist
        .get("currentIndex")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            AppError::internal("unexpected Page.getNavigationHistory response: no currentIndex")
        })?;
    let entries = hist.get("entries").and_then(Value::as_array).ok_or_else(|| {
        AppError::internal("unexpected Page.getNavigationHistory response: no entries")
    })?;
    let target = current + delta;
    if target < 0 || target >= entries.len() as i64 {
        let direction = if delta < 0 { "earlier (back)" } else { "later (forward)" };
        return Err(nav_no_history(direction));
    }
    let entry = &entries[target as usize];
    let id = entry
        .get("id")
        .cloned()
        .ok_or_else(|| AppError::internal("navigation history entry missing id"))?;
    let url = entry
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok((target, id, url))
}

/// back/forward 共用（delta = -1/+1）：取历史 → 边界检查（409
/// PAGE_NAV_NO_HISTORY）→ navigateToHistoryEntry → 等待完成。
/// 完成信号双轨（历史条目分两类，事件面无法提前区分）：
/// - 新文档条目：load 事件（navigate/reload 同款等待）；
/// - 同文档条目（SPA pushState 产生）：不触发 load 事件——以历史索引提交
///   为信号；提交确认后 readyState 已 complete 则立即返回（快路径，不等满
///   窗口）。readyState=complete 与 load 事件触发是同一时刻（HTML 规范：
///   readyState 在 load 触发时变为 complete），loaded:true 证据等价。
async fn history_step_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    delta: i64,
    load_timeout: Duration,
) -> Result<Value, AppError> {
    let direction = if delta < 0 { "back" } else { "forward" };
    let conn = require_conn(watched)?;
    let (target, entry_id, url) = nav_history_target(&conn, sid, delta).await?;
    let load = register_load_waiter(watched, tid);
    cdp_send(
        &conn,
        "Page.navigateToHistoryEntry",
        json!({ "entryId": entry_id }),
        Some(sid),
    )
    .await?;
    let deadline = Instant::now() + load_timeout;
    // 提交确认轮询：currentIndex 到位 = 目标条目已激活（同文档条目同步到位，
    // 新文档条目在提交时到位）
    loop {
        let committed =
            match cdp_send(&conn, "Page.getNavigationHistory", json!({}), Some(sid)).await {
                Ok(h) => h.get("currentIndex").and_then(Value::as_i64) == Some(target),
                Err(_) => false, // 轮询期连接抖动按未提交继续（要么恢复要么超时）
            };
        if committed {
            let ready_complete = matches!(
                eval_raw(&conn, sid, "document.readyState").await,
                Ok(v) if v.as_str() == Some("complete")
            );
            if ready_complete {
                return Ok(json!({
                    "result": format!(
                        "Navigated {direction} to entry {target}: {url} (ready — same-document entry or already loaded)"
                    ),
                    "url": url,
                    "index": target,
                    "loaded": true,
                }));
            }
            break; // 新文档已提交仍在加载 → 转 load 事件等待
        }
        if Instant::now() >= deadline {
            return Ok(json!({
                "result": format!(
                    "Navigated {direction} to entry {target}: {url} (commit not confirmed within the window — navigation may still be pending)"
                ),
                "url": url,
                "index": target,
                "loaded": false,
            }));
        }
        tokio::time::sleep(NAV_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now()))).await;
    }
    let loaded = await_load(load, deadline).await;
    let note = if loaded {
        "load complete"
    } else {
        "committed, load not confirmed within the window — page may still be loading"
    };
    Ok(json!({
        "result": format!("Navigated {direction} to entry {target}: {url} ({note})"),
        "url": url,
        "index": target,
        "loaded": loaded,
    }))
}

/// reload [--ignore-cache]：Page.reload 无响应负载（错误走 CDP error 通道），
/// 恒为新文档加载 → 一律等待 load 事件（窗口内未见 → loaded:false）。
async fn reload_tool(
    watched: &Watched,
    tid: &str,
    sid: &str,
    ignore_cache: bool,
    load_timeout: Duration,
) -> Result<Value, AppError> {
    let conn = require_conn(watched)?;
    let load = register_load_waiter(watched, tid);
    cdp_send(
        &conn,
        "Page.reload",
        json!({ "ignoreCache": ignore_cache }),
        Some(sid),
    )
    .await?;
    let loaded = await_load(load, Instant::now() + load_timeout).await;
    let note = if loaded {
        "load complete"
    } else {
        "load not confirmed within the window — page may still be loading"
    };
    Ok(json!({
        "result": format!("Reloaded ({note})"),
        "loaded": loaded,
    }))
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
            ax_node("7", "heading", json!({ "backendDOMNodeId": 7 })), // 无名无值且不可引用 → 隐藏
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
        handle_event(&watched, &sheet_added_event("S1", "inline-1", "regular", false)).await;
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
        handle_event(&watched, &sheet_added_event("S1", "s1", "user-agent", false)).await;
        given(
            &script,
            "CSS.getStyleSheetText",
            vec![Ok(json!({ "text": ".menu-item:hover { background: yellow; }\n.menu-item:hover a.drop-link { color: blue; }" }))],
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
        given(&script, "DOM.getDocument", vec![Ok(json!({ "root": { "nodeId": 1 } }))]);
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
        given(&script, "Accessibility.getFullAXTree", vec![Ok(json!({ "nodes": [
            { "nodeId": "1", "role": { "value": "button" }, "name": { "value": "OK" }, "backendDOMNodeId": 7 },
        ]}))]);
        let resp = svc.snap(INS, "T1").await.unwrap();
        assert_eq!(resp["hoverScan"], json!("skipped"));
        let text = resp["text"].as_str().unwrap();
        assert!(text.contains("@e1 [button] OK") && !text.contains("[hover"), "{text}");
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
        watched
            .css_since
            .lock()
            .unwrap()
            .insert("T1".into(), Instant::now() - HOVER_SETTLE_WINDOW - Duration::from_millis(50));
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
        given(&script, "Accessibility.getFullAXTree", vec![Ok(json!({ "nodes": [] }))]);
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
        given(&script, "Accessibility.getFullAXTree", vec![Ok(json!({ "nodes": [
            { "nodeId": "1", "role": { "value": "button" }, "name": { "value": "Menu" }, "backendDOMNodeId": 101 },
        ]}))]);
        given(&script, "DOM.getDocument", vec![Ok(json!({ "root": { "nodeId": 1 } }))]);
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
        assert!(text.contains("@e1 [button] Menu") && !text.contains("[hover"), "{text}");
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
        assert_eq!(watched.sheets.lock().unwrap()["T1"].len(), 2, "子 frame 导航不清登记");

        // 根 frame 导航：整清
        handle_event(
            &watched,
            &json!({
                "method": "Page.frameNavigated", "sessionId": "S1",
                "params": { "frame": { "id": "root" } }
            }),
        )
        .await;
        assert!(watched.sheets.lock().unwrap().get("T1").is_none(), "根 frame 导航整清登记");
        // refs 失效链路与 sheets 清理同一事件分支触发（既有语义不回归；
        // 须在 snap 前断言——snap 的 replace 会清 stale 与 reason）
        assert_eq!(
            watched.refs.lock().unwrap().get("T1").unwrap().last_invalid_reason(),
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
        given(&script, "DOM.getDocument", vec![Ok(json!({ "root": { "nodeId": 1 } }))]);
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

    /// styleSheetAdded/Removed 事件维护登记表；drop_tab 随 per-tab 状态清理。
    #[tokio::test]
    async fn style_sheet_events_maintain_registry_per_tab() {
        let (_svc, watched, _calls, _script) = armed();
        handle_event(&watched, &sheet_added_event("S1", "s1", "regular", false)).await;
        handle_event(&watched, &sheet_added_event("S1", "s2", "user-agent", false)).await;
        handle_event(&watched, &sheet_added_event("S2", "other-tab-sheet", "regular", false)).await;
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

    // ---------------- 工具：navigate / back / forward / reload ---------------

    /// URL 白名单在进串行锁/建连前拦截（400），与实例级 tabs/navigate 同源
    ///（domain validate_http_url）；非 http(s) scheme 零 CDP 发送。
    #[tokio::test]
    async fn navigate_rejects_non_http_url_before_connecting() {
        let (svc, _w, calls, _script) = armed();
        for bad in ["ftp://x", "javascript:alert(1)", "file:///etc/hosts", ""] {
            let e = svc.navigate(INS, "T1", bad).await.unwrap_err();
            assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400), "应拒绝: {bad}");
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
            vec![Ok(json!({ "frameId": "F1", "errorText": "net::ERR_NAME_NOT_RESOLVED" }))],
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
            vec![Ok(json!({ "frameId": "F1", "sameDocumentNavigation": true }))],
        );
        // 无 load 事件注入：若实现误等 load，此 await 会挂到超时
        let resp = svc.navigate(INS, "T1", "https://a/#frag").await.unwrap();
        assert_eq!(resp["loaded"], json!(true));
        assert_eq!(resp["url"], json!("https://a/#frag"));
        assert!(
            resp["result"]
                .as_str()
                .unwrap()
                .contains("same-document navigation")
        );
        assert_eq!(calls_of(&calls, "Page.navigate")[0].session.as_deref(), Some("S1"));
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
        assert_eq!((nav.len(), nav[0].param("entryId"), nav[0].session.as_deref()), (1, json!(1), Some("S1")));
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
            eventually("Page.reload 已发出", || calls_of(&calls, "Page.reload").len() == 1).await;
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
        let resp = svc.screenshot(INS, "T1", None).await.unwrap();
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
        let resp = svc.screenshot(INS, "T1", Some("@e1")).await.unwrap();
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
        given(&script, "DOM.getDocument", vec![Ok(json!({ "root": { "nodeId": 3 } }))]);
        given(
            &script,
            "DOM.querySelector",
            vec![Ok(json!({ "nodeId": 7 }))],
        );
        given(
            &script,
            "DOM.getContentQuads",
            vec![Ok(json!({ "quads": [[0.0, 0.0, 50.0, 0.0, 50.0, 50.0, 0.0, 50.0]] }))],
        );
        given(
            &script,
            "Page.captureScreenshot",
            vec![Ok(json!({ "data": "aGVsbG8=" }))],
        );
        svc.screenshot(INS, "T1", Some("#hero")).await.unwrap();
        let q = calls_of(&calls, "DOM.querySelector");
        assert_eq!(q[0].param("selector"), json!("#hero"));
        assert_eq!(q[0].param("nodeId"), json!(3));
        let scroll = calls_of(&calls, "DOM.scrollIntoViewIfNeeded");
        assert_eq!(scroll[0].param("nodeId"), json!(7), "selector 路径走 nodeId 句柄");
        let cap = calls_of(&calls, "Page.captureScreenshot");
        assert_eq!(
            cap[0].params.get("clip"),
            Some(&json!({ "x": 0.0, "y": 0.0, "width": 50.0, "height": 50.0, "scale": 1 }))
        );

        // 未命中 → 404，且不截屏
        let (svc2, _w2, _c2, script2) = armed();
        given(&script2, "DOM.getDocument", vec![Ok(json!({ "root": { "nodeId": 3 } }))]);
        given(&script2, "DOM.querySelector", vec![Ok(json!({ "nodeId": 0 }))]);
        let e = svc2.screenshot(INS, "T1", Some(".missing")).await.unwrap_err();
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
        given(&script, "DOM.getContentQuads", vec![Ok(json!({ "quads": [] }))]);
        given(
            &script,
            "DOM.resolveNode",
            vec![Ok(json!({ "object": { "objectId": "obj9" } }))],
        );
        given(
            &script,
            "Runtime.callFunctionOn",
            vec![Ok(
                json!({ "result": { "value": { "x": 5.1, "y": 6.2, "w": 100.4, "h": 40.9 } } })
            )],
        );
        given(
            &script,
            "Page.captureScreenshot",
            vec![Ok(json!({ "data": "aGVsbG8=" }))],
        );
        svc.screenshot(INS, "T1", Some("e1")).await.unwrap();
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
        given(&script2, "DOM.getContentQuads", vec![Ok(json!({ "quads": [] }))]);
        given(
            &script2,
            "DOM.resolveNode",
            vec![Ok(json!({ "object": { "objectId": "obj9" } }))],
        );
        given(
            &script2,
            "Runtime.callFunctionOn",
            vec![Ok(json!({ "result": { "value": { "x": 5.0, "y": 6.0, "w": 100.0, "h": 0.0 } } }))],
        );
        let e = svc2.screenshot(INS, "T1", Some("e1")).await.unwrap_err();
        assert_eq!((e.code(), e.status()), ("PAGE_TARGET_MOVED", 409));
    }

    /// 响应缺 data 字段：显式 500 INTERNAL_ERROR，不装作成功（诊断面同款）。
    #[tokio::test]
    async fn screenshot_missing_data_is_explicit_internal_error() {
        let (svc, _w, _calls, script) = armed();
        given(&script, "Page.captureScreenshot", vec![Ok(json!({}))]);
        let e = svc.screenshot(INS, "T1", None).await.unwrap_err();
        assert_eq!((e.code(), e.status()), ("INTERNAL_ERROR", 500));
        assert!(e.to_string().contains("data"));
    }

    /// 空字符串 target：与 click 的空 selector 同门（400），缺省视口应传 None。
    #[tokio::test]
    async fn screenshot_empty_target_is_rejected() {
        let (svc, _w, calls, _script) = armed();
        let e = svc.screenshot(INS, "T1", Some("")).await.unwrap_err();
        assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400));
        assert_eq!(calls_of(&calls, "Page.captureScreenshot").len(), 0);
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
            vec![Ok(json!({ "data": "JVBER" })), Ok(json!({ "data": "JVBER" }))],
        );
        let resp = svc
            .pdf(INS, "T1", None, None, None, None)
            .await
            .unwrap();
        assert_eq!((resp["format"].as_str().unwrap(), resp["data"].as_str().unwrap()), ("pdf", "JVBER"));
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
        for (w, h) in [(Some(0.0), None), (None, Some(-1.0)), (Some(f64::NAN), None)] {
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
        assert!(ua[0].param("userAgent").as_str().unwrap().contains("CriOS/"));
        let touch = calls_of(&calls, "Emulation.setTouchEmulationEnabled");
        assert_eq!(touch[0].param("enabled"), json!(true));
        for c in [m[0].clone(), ua[0].clone(), touch[0].clone()] {
            assert_eq!(c.session.as_deref(), Some("S1"), "仿真发 page session，非 browser 级");
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
            viewport: Some(crate::domain::emulate::Viewport { width: w, height: h }),
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
        assert_eq!(ua.len(), 2, "两次都发 setUserAgentOverride（第二次为空串恢复）");
        assert_eq!(ua[0].param("userAgent"), json!("Bot/1.0"));
        assert_eq!(
            ua[1].param("userAgent"),
            json!(""),
            "第二次未给 UA 必须恢复默认（空串，非 clear 命令）"
        );
        assert_eq!(calls_of(&calls, "Emulation.clearUserAgentOverride").len(), 0);
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
        assert_eq!(calls_of(&calls, "Emulation.clearDeviceMetricsOverride").len(), 1);
        let ua = calls_of(&calls, "Emulation.setUserAgentOverride");
        assert_eq!(
            (ua.len(), ua[0].param("userAgent")),
            (1, json!("")),
            "UA 恢复 = 空串 setUserAgentOverride（CfT 131 无 clear 命令）"
        );
        assert_eq!(calls_of(&calls, "Emulation.clearUserAgentOverride").len(), 0);
        let touch = calls_of(&calls, "Emulation.setTouchEmulationEnabled");
        assert_eq!((touch.len(), touch[0].param("enabled")), (1, json!(false)));
        assert_eq!(calls_of(&calls, "Emulation.setDeviceMetricsOverride").len(), 0);
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

    // ---------------- 工具：Fetch 拦截（W-fetch）---------------------------

    /// Fetch.requestPaused 事件（flatten 模式：sessionId 顶层）
    fn pause_event(sid: &str, rid: &str, url: &str, rt: &str, headers: Value) -> Value {
        json!({
            "method": "Fetch.requestPaused", "sessionId": sid,
            "params": {
                "requestId": rid, "resourceType": rt,
                "request": { "url": url, "method": "GET", "headers": headers }
            }
        })
    }

    const MOCK_RULES: &str = r#"{ "rules": [
        { "id": "m1", "match": { "urlPattern": "*://127.0.0.1:18799/api/*", "resourceTypes": ["xhr","fetch"] },
          "action": { "type": "mock", "status": 200, "headers": {"Content-Type":"application/json"}, "body": "{\"ok\":true}" } },
        { "id": "h1", "match": { "urlPattern": "*://127.0.0.1:18799/hdr/*" },
          "action": { "type": "setHeaders", "headers": {"X-Debug-Uid":"42"} } },
        { "id": "b1", "match": { "urlPattern": "*://ads.example.com/*" },
          "action": { "type": "block" } }
    ] }"#;

    /// 下发 + enable 形态：patterns 展开（resourceTypes 逐条 + requestStage）。
    #[tokio::test]
    async fn intercept_set_enables_fetch_with_expanded_patterns() {
        let (svc, _w, calls, _s) = armed();
        let resp = svc
            .intercept_set(INS, "T1", &serde_json::from_str(MOCK_RULES).unwrap())
            .await
            .unwrap();
        assert_eq!(resp["rules"], json!(3));
        assert_eq!(resp["patterns"], json!(4), "m1 两类型 + h1 + b1");
        let enable = calls_of(&calls, "Fetch.enable");
        assert_eq!(enable.len(), 1);
        let pats = enable[0].param("patterns");
        assert_eq!(pats.as_array().unwrap().len(), 4);
        assert_eq!(pats[0]["urlPattern"], json!("*://127.0.0.1:18799/api/*"));
        assert_eq!(pats[0]["requestStage"], json!("Request"));
        assert_eq!(pats[0]["resourceType"], json!("xhr"));
        assert!(pats[2].get("resourceType").is_none(), "缺省类型不带 resourceType");
    }

    /// mock 命中：continueRequest 改写到 mock 上游 URL；GET 披露 hits/journal/
    /// liveSince；journal 的 url 已脱敏。
    #[tokio::test]
    async fn intercept_mock_rewrites_to_upstream_and_journals() {
        let (svc, watched, calls, _s) = armed();
        svc.intercept_set(INS, "T1", &serde_json::from_str(MOCK_RULES).unwrap())
            .await
            .unwrap();
        handle_event(
            &watched,
            &pause_event("S1", "job-1", "http://127.0.0.1:18799/api/data?n=1&t=2", "XHR", json!({"X-Spike":"yes"})),
        )
        .await;
        eventually("continueRequest 已发出", || {
            calls_of(&calls, "Fetch.continueRequest").len() == 1
        })
        .await;
        let cr = calls_of(&calls, "Fetch.continueRequest")[0].clone();
        let url = cr.param("url").as_str().unwrap().to_string();
        assert!(
            url.starts_with("http://127.0.0.1:") && url.contains("/m/"),
            "改写到 mock 上游: {url}"
        );
        assert_eq!(cr.session.as_deref(), Some("S1"));

        let status = svc.intercept_get(INS, "T1").await.unwrap();
        assert_eq!(status["liveSince"].is_number(), true);
        let r0 = &status["rules"][0];
        // m1 默认 times=1：命中 1 次即耗尽（active=false，剩余 0）——耗尽
        // 后规则失效但可查询（GET 仍完整披露）
        assert_eq!((r0["id"].as_str(), r0["hits"].as_u64(), r0["active"].as_bool()), (Some("m1"), Some(1), Some(false)));
        let j0 = &status["journal"][0];
        assert_eq!(j0["outcome"], json!("mocked"));
        assert_eq!(j0["rule"], json!("m1"));
        assert_eq!(j0["requestId"], json!("job-1"));
        let jurl = j0["url"].as_str().unwrap();
        assert!(jurl.contains("127.0.0.1:18799"), "url 保留: {jurl}");
    }

    /// setHeaders 命中：原始头全保留 + 规则头覆盖/追加（替换语义下的合并）。
    #[tokio::test]
    async fn intercept_set_headers_merges_originals() {
        let (svc, watched, calls, _s) = armed();
        svc.intercept_set(INS, "T1", &serde_json::from_str(MOCK_RULES).unwrap())
            .await
            .unwrap();
        handle_event(
            &watched,
            &pause_event("S1", "job-2", "http://127.0.0.1:18799/hdr/x", "Document", json!({"X-Spike":"yes", "X-Debug-Uid":"old"})),
        )
        .await;
        eventually("continueRequest 已发出", || {
            calls_of(&calls, "Fetch.continueRequest").len() == 1
        })
        .await;
        let headers = calls_of(&calls, "Fetch.continueRequest")[0].param("headers");
        let by_name = |k: &str| -> String {
            headers
                .as_array()
                .unwrap()
                .iter()
                .find(|h| h["name"].as_str().unwrap().eq_ignore_ascii_case(k))
                .and_then(|h| h["value"].as_str())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(by_name("X-Spike"), "yes", "原头保留");
        assert_eq!(by_name("X-Debug-Uid"), "42", "规则头覆盖同名列");
        let status = svc.intercept_get(INS, "T1").await.unwrap();
        assert_eq!(status["journal"][0]["outcome"], json!("headers-set"));
        assert_eq!(status["journal"][0]["rule"], json!("h1"));
    }

    /// block 命中：failRequest BlockedByClient。
    #[tokio::test]
    async fn intercept_block_fails_request() {
        let (svc, watched, calls, _s) = armed();
        svc.intercept_set(INS, "T1", &serde_json::from_str(MOCK_RULES).unwrap())
            .await
            .unwrap();
        handle_event(
            &watched,
            &pause_event("S1", "job-3", "https://ads.example.com/pixel?x=1", "Image", json!({})),
        )
        .await;
        eventually("failRequest 已发出", || calls_of(&calls, "Fetch.failRequest").len() == 1).await;
        let fr = calls_of(&calls, "Fetch.failRequest")[0].clone();
        assert_eq!(fr.param("requestId"), json!("job-3"));
        assert_eq!(fr.param("errorReason"), json!("BlockedByClient"));
        let status = svc.intercept_get(INS, "T1").await.unwrap();
        assert_eq!(status["journal"][0]["outcome"], json!("blocked"));
    }

    /// 红线②常态面：不匹配的 url / resourceType 不满足 → 裸 continueRequest
    /// 放行 + journal passed（绝不能悬挂页面）。
    #[tokio::test]
    async fn intercept_no_match_passes_through() {
        let (svc, watched, calls, _s) = armed();
        svc.intercept_set(INS, "T1", &serde_json::from_str(MOCK_RULES).unwrap())
            .await
            .unwrap();
        // url 不匹配任何规则
        handle_event(
            &watched,
            &pause_event("S1", "job-4", "https://other.example.com/x", "XHR", json!({})),
        )
        .await;
        // resourceType 不匹配 m1（Script 而非 xhr/fetch；url 匹配 m1 的 pattern）
        handle_event(
            &watched,
            &pause_event("S1", "job-5", "http://127.0.0.1:18799/api/data", "Script", json!({})),
        )
        .await;
        eventually("两次裸放行", || {
            calls_of(&calls, "Fetch.continueRequest")
                .iter()
                .filter(|c| c.params.as_object().is_some_and(|m| m.len() == 1))
                .count()
                == 2
        })
        .await;
        let status = svc.intercept_get(INS, "T1").await.unwrap();
        let outcomes: Vec<&str> = status["journal"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["outcome"].as_str().unwrap())
            .collect();
        assert_eq!(outcomes, vec!["passed", "passed"], "未命中都留痕");
        // hits 不动（未命中不计）
        assert_eq!(status["rules"][0]["hits"], json!(0));
    }

    /// times 耗尽后放行（默认 times=1：第二次命中同 url → passed）。
    #[tokio::test]
    async fn intercept_times_exhausted_passes() {
        let (svc, watched, calls, _s) = armed();
        svc.intercept_set(INS, "T1", &serde_json::from_str(MOCK_RULES).unwrap())
            .await
            .unwrap();
        for i in 0..2 {
            handle_event(
                &watched,
                &pause_event("S1", &format!("job-{i}"), "https://ads.example.com/x", "Image", json!({})),
            )
            .await;
        }
        eventually("block 一次 + 裸放行一次", || {
            calls_of(&calls, "Fetch.failRequest").len() == 1
                && calls_of(&calls, "Fetch.continueRequest").len() == 1
        })
        .await;
        let status = svc.intercept_get(INS, "T1").await.unwrap();
        let b1 = status["rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == json!("b1"))
            .unwrap()
            .clone();
        assert_eq!((b1["hits"].as_u64(), b1["remaining"].as_u64(), b1["active"].as_bool()), (Some(1), Some(0), Some(false)));
    }

    /// 红线②错误路径：决策命令失败 → 裸 continueRequest 兑底放行 + journal
    /// failed-then-passed（页面绝不悬挂）。
    #[tokio::test]
    async fn intercept_decision_failure_falls_back_to_bare_continue() {
        let (svc, watched, calls, script) = armed();
        // 第一次 continueRequest（带 url 的 mock 决策）失败；兑底的第二次调用
        // 落到队列耗尽后的 Ok(Null) → 成功
        given(&script, "Fetch.continueRequest", vec![Err("connection reset".into())]);
        svc.intercept_set(INS, "T1", &serde_json::from_str(MOCK_RULES).unwrap())
            .await
            .unwrap();
        handle_event(
            &watched,
            &pause_event("S1", "job-6", "http://127.0.0.1:18799/api/data", "XHR", json!({})),
        )
        .await;
        eventually("决策 + 兑底两次 continueRequest", || {
            calls_of(&calls, "Fetch.continueRequest").len() == 2
        })
        .await;
        let all = calls_of(&calls, "Fetch.continueRequest");
        assert!(all[0].param("url").is_string(), "首次是决策（带 url）");
        assert_eq!(all[1].params.as_object().unwrap().len(), 1, "兑底是裸放行（仅 requestId）");
        let status = svc.intercept_get(INS, "T1").await.unwrap();
        assert_eq!(status["journal"][0]["outcome"], json!("failed-then-passed"));
        assert!(status["journal"][0]["detail"].as_str().unwrap().contains("connection reset"));
    }

    /// 红线①：拦截决策 in-flight（continueRequest 被闸门挂住）不阻塞同实例的
    /// 其它 page 命令（决策不碰串行锁 + 连接并发 send）。组装方式对齐
    /// dialog_handle_completes_while_command_in_flight（Script/Gates 分离）。
    #[tokio::test]
    async fn intercept_decision_does_not_block_page_commands() {
        let calls: Calls = Arc::default();
        let script: Script = Arc::default();
        let gates: Gates = Arc::default();
        given(
            &script,
            "Accessibility.getFullAXTree",
            vec![Ok(json!({ "nodes": [] })), Ok(json!({ "nodes": [] }))],
        );
        // 闸门挂住决策的 continueRequest（send 记录后停在闸门）
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        gates
            .lock()
            .unwrap()
            .insert("Fetch.continueRequest".into(), std::collections::VecDeque::from([held]));

        let watched = Arc::new(Watched::new());
        *watched.conn.lock().unwrap() = Some(fake_conn(&calls, &script, None, &gates));
        watched.tabs.lock().unwrap().attach("T1", "S1");
        // 拦截状态直接武装（决策路径只依赖 intercept 状态 + conn，不经 command 骨架）
        let rules: Value = serde_json::from_str(MOCK_RULES).unwrap();
        watched.intercept.lock().unwrap().insert(
            "T1".to_string(),
            InterceptState::new(compile_rules(rules["rules"].as_array().unwrap()).unwrap()),
        );
        // snap 走完整 service 路径：watched 登记 + Running 实例
        let (svc, _c2, _s2, _t2) = fake_service();
        {
            let mut map = svc.watched.lock().unwrap();
            map.insert(INS.to_string(), Arc::clone(&watched));
        }
        // 已有连接槽位（不触发 ensure 重建，走命令骨架 + 串行锁）
        // —— watched.conn 已置位；ensure 只查实例状态与端口（armed 同款）

        // 决策任务：pause → failRequest？不——block 规则走 failRequest；
        // 用 mock 规则（continueRequest 被闸门挂住）体现并发
        let w2 = Arc::clone(&watched);
        let decision = tokio::spawn(async move {
            handle_event(
                &w2,
                &pause_event("S1", "job-7", "http://127.0.0.1:18799/api/data", "XHR", json!({})),
            )
            .await;
        });
        eventually("决策停在闸门", || {
            calls_of(&calls, "Fetch.continueRequest").len() == 1
        })
        .await;
        // 同实例 snap：不得被 in-flight 决策阻塞（决策不持串行锁、send 并发）
        let snap_ok = tokio::time::timeout(Duration::from_secs(3), svc.snap(INS, "T1"))
            .await
            .is_ok_and(|r| r.is_ok());
        assert!(snap_ok, "snap 不得被拦截决策阻塞");
        release.send(()).unwrap();
        decision.await.unwrap();
    }

    /// 清空：空 rules → Fetch.disable + 状态移除；GET 空 + liveSince null。
    #[tokio::test]
    async fn intercept_clear_disables_and_gets_empty() {
        let (svc, _w, calls, _s) = armed();
        svc.intercept_set(INS, "T1", &serde_json::from_str(MOCK_RULES).unwrap())
            .await
            .unwrap();
        let resp = svc
            .intercept_set(INS, "T1", &json!({ "rules": [] }))
            .await
            .unwrap();
        assert_eq!(resp["result"], json!("intercept cleared"));
        assert_eq!(calls_of(&calls, "Fetch.disable").len(), 1);
        let status = svc.intercept_get(INS, "T1").await.unwrap();
        assert_eq!(status["rules"].as_array().unwrap().len(), 0);
        assert_eq!(status["journal"].as_array().unwrap().len(), 0);
        assert_eq!(status["liveSince"], Value::Null);
        assert!(status["note"].as_str().unwrap().contains("失效"));
    }

    /// 非法规则在进串行锁前 400（黑名单头 / 缺 rules 键）。
    #[tokio::test]
    async fn intercept_invalid_rules_rejected_400() {
        let (svc, _w, calls, _s) = armed();
        let e = svc
            .intercept_set(
                INS,
                "T1",
                &json!({ "rules": [ { "id": "x", "match": { "urlPattern": "*://a/*" },
                    "action": { "type": "setHeaders", "headers": { "Authorization": "Bearer x" } } } ] }),
            )
            .await
            .unwrap_err();
        assert_eq!((e.code(), e.status()), ("INVALID_REQUEST", 400));
        assert!(e.to_string().contains("黑名单"));
        let e2 = svc
            .intercept_set(INS, "T1", &json!({ "noRules": [] }))
            .await
            .unwrap_err();
        assert_eq!(e2.code(), "INVALID_REQUEST");
        assert_eq!(calls_of(&calls, "Fetch.enable").len(), 0, "非法不入连接");
    }

    /// 生命周期红线：连接重建（establish）→ 拦截状态整清（Fetch.enable 是
    /// session 级，新会话需要重新下发）。
    #[tokio::test]
    async fn intercept_state_lost_on_reconnect() {
        let (svc, _calls, _script, _tap) = fake_service();
        let rules: Value = serde_json::from_str(MOCK_RULES).unwrap();
        let (w1, _g1) = svc.ensure(INS).await.unwrap();
        svc.intercept_set(INS, "", &rules).await.unwrap_err(); // 无 tab：409（仅证明建连）
        // 直接构造状态（绕过 tab 路由）：手动放入 T1 模拟已生效
        w1.intercept
            .lock()
            .unwrap()
            .insert("T1".to_string(), InterceptState::new(compile_rules(rules["rules"].as_array().unwrap()).unwrap()));
        drop(_g1);
        // 模拟断连：连接槽置空 → 下一条命令重建（establish 整清 intercept）
        *w1.conn.lock().unwrap() = None;
        let (w2, g2) = svc.ensure(INS).await.unwrap();
        assert!(Arc::ptr_eq(&w1, &w2), "同 Watched 重建");
        drop(g2);
        let guard = w2.intercept.lock().unwrap();
        assert!(guard.get("T1").is_none(), "重建后拦截状态已清（自动失效）");
    }
}
