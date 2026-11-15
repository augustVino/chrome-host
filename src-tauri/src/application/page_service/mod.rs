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
//!   替换事件不再主动失效——I1b 起 ref 表 = browser_kit RefMap（loaderId
//!   代际 durable refs，stale 由解析时 is_detached/唯一匹配恢复判定产生）。
//! - **导航命令族**（navigate/back/forward/reload，page 族后补）：同一 page
//!   session 内 Page.navigate / 导航历史回退 / Page.reload——保留历史（实例级
//!   navigate 的 new+close 组合近似会丢历史，该端点保留给 GUI，文档已指向
//!   本族）。加载等待走 Page.loadEventFired 事件（命令发送前登记等待者，窗口
//!   NAV_LOAD_TIMEOUT，未见 → loaded:false；同文档导航/同文档历史条目不触发
//!   load，走响应分类 / 历史索引 + readyState 快路径）。
//! - **文件传输**（W-file）：establish 时 browser 级
//!   Browser.setDownloadBehavior{allow, <profile>/downloads, events}，
//!   downloadWillBegin/Progress 事件在 handle_event 登记/结算（终态入环 20
//!   条 + 命令式等待者先到先得）；upload = 定位 + describeNode 校验
//!   input[type=file] + DOM.setFileInputFiles（不拷贝文件，主机侧拒软链/
//!   超 100MB）。download_wait **不占实例串行锁**（等待期 click 正是要并发
//!   执行的命令）。
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
//!
//! **I1a 结构拆分**（2026-11-15，任务书 I1a）：本模块原为 9566 行单文件，现按
//! SPLIT-MAP.md 的域边界平移为模块目录（mod/state/events/ax/snap/frames/refs/
//! input/eval/extract/capture/navigation/dialogs/downloads/upload/intercept +
//! tests，16 文件）。纯结构移动、零行为变化：函数体、错误文案、常量值与
//! 118 个测试逐字保留；变化仅限代码位置、可见性（被跨模块引用者 →
//! `pub(crate)`）与 use 路径。

// P2-C 已接线（api/pages.rs + api/mcp.rs）：仅测试注入构造 with_connector
// 仍无生产调用方，逐项 allow（不再整模块豁免，防新死代码静默滑入）。
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::domain::fetch_rules::{cdp_patterns, compile_rules, Action};
use crate::domain::instance::InstanceStatus;
#[cfg(test)]
use crate::domain::page_control::DialogBuffer;
use crate::domain::page_control::{
    parse_duration, validate_http_url, PageControlError, WaitState, MAX_DIALOG_BUFFER,
};
use crate::error::AppError;
use crate::infrastructure::cdp::page_cdp::{connect_page, SharedPageConn};
use crate::infrastructure::cdp::ws::EventSink;
use crate::infrastructure::db::repositories::instance_repository::InstanceRepository;

use super::fetch_mock::MockUpstream;

mod capture;
mod dialogs;
mod diff_tool;
mod downloads;
mod eval;
mod events;
mod extract;
mod frames;
mod input;
mod intercept;
mod navigation;
mod profiler;
mod refs;
mod snap;
mod state;
mod upload;

use self::capture::*;
#[cfg(test)]
use self::downloads::*;
use self::eval::*;
use self::events::*;
use self::extract::*;
use self::input::*;
use self::intercept::*;
use self::navigation::*;
#[cfg(test)]
use self::refs::*;
use self::snap::*;
use self::state::*;
use self::upload::*;

/// snap hover 感知：backendNodeId → 标记（"hover" = hover 触发器，
/// "hover-reveal" = hover 后才显示/变化的目标）。原 ax.rs 随 I1b 删除，
/// 类型随唯一消费者迁入。
pub(crate) type HoverMarks = HashMap<i64, &'static str>;
/// per-tab 样式表登记：targetId → { styleSheetId → (origin, disabled) }。
/// 原 ax.rs 随 I1b 删除，类型随唯一消费者迁入（state.rs 引用）。
pub(crate) type SheetRegistry = HashMap<String, HashMap<String, (String, bool)>>;

/// snap 的可选参数（CLI -c/-i/-d/-u/-s、API body、MCP 参数三面同源；全缺省
/// = 现行为语义）。字段语义 = kit SnapshotOptions 逐字透传。
#[derive(Default, Clone)]
pub struct SnapParams {
    pub compact: bool,
    pub interactive: bool,
    pub depth: Option<usize>,
    pub urls: bool,
    pub selector: Option<String>,
}

/// 工具级总超时：单条 CDP send 已有内建 15s（ws.rs），这里兜住多命令链与
/// 无响应页面（wait 例外：用户 timeout + 5s）。
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
const WAIT_TOOL_GRACE: Duration = Duration::from_secs(5);
/// wait 轮询缺省超时与间隔（TS wait.mjs 默认；间隔固定起步，不做参数）
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// mousePressed → mouseReleased 的间隔（TS dispatchClickAtPoint 的 50ms）
const CLICK_RELEASE_DELAY: Duration = Duration::from_millis(50);

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
/// 全页截图单边上限（**设备像素**，W-shot 真机 spike 定准）：Chrome 光栅面
/// 单边硬顶 16384 device px——真机实测（CfT 131 / Retina DPR=2）clip
/// 8192 CSS px（=16384 device）成功、8193 CSS px（=16386 device）即被
/// CDP 拒绝（-32000 Unable to capture screenshot）。故 CSS 上限按
/// devicePixelRatio 动态换算：cap_css = 16384 / DPR（DPR=1 → 16384，
/// DPR=2 → 8192）。超限显式截断并标 truncated（诚实证据，不产出可疑图，
/// 也不让调用方拿到裸 500）。png 实际尺寸 = 响应 width/height × DPR。
const FULL_SHOT_MAX_DEVICE_SIDE: f64 = 16384.0;
/// download-wait 缺省等待窗口（W-file 任务口径：30s）。
const DEFAULT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// 下载终态环形记录容量（GET /page/downloads 查询面；W-file 任务口径 20）。
const MAX_DOWNLOAD_HISTORY: usize = 20;

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

/// 409：download-wait 超时 = 窗口内无终态到达。in_flight 非空时披露
/// 「已有下载在进行但未完成」——与 PAGE_DOWNLOAD_FAILED（下载了但失败）
/// 严格区分（任务红线：超时 = 无下载完成的证据，不是失败断言）。
fn download_timeout(in_flight: usize, timeout: Duration) -> AppError {
    let detail = if in_flight > 0 {
        format!("（{} 个下载仍在进行中未完成）", in_flight)
    } else {
        "（窗口内无任何下载终态——确认触发动作是否已执行）".to_string()
    };
    AppError::conflict(
        "PAGE_DOWNLOAD_TIMEOUT",
        format!("等待下载完成超时（{}ms 上限）{detail}", timeout.as_millis()),
    )
}

/// 500：下载已开始但终态为失败/取消/被拒——与超时（无下载）严格区分。
fn download_failed(state: &str, file_name: &str) -> AppError {
    AppError::business(
        500,
        "PAGE_DOWNLOAD_FAILED",
        format!("下载 \"{file_name}\" 终态 = {state}（interrupted=中断 / canceled=取消 / rejected=文件名不安全拒绝）"),
    )
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
            self.establish(ins_id, &watched, port, &ins.profile_dir)
                .await?;
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
        profile_dir: &str,
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
        // W-file：下载落盘目录 = 实例 profile 下 downloads/ 子目录（每实例
        // 隔离，实例删除随 profile 清理——天然生命周期）。browser 级设置
        //（session=None）：downloadWillBegin/Progress 事件因此无 session 维度，
        // 由 handle_event 直接分派。目录创建失败 = establish 失败（不静默
        // 降级：缺目录时 download-wait 会永远等不到事件，宁可建连期暴露）。
        let dl_dir = std::path::Path::new(profile_dir).join("downloads");
        std::fs::create_dir_all(&dl_dir)?;
        cdp_send(
            &conn,
            "Browser.setDownloadBehavior",
            json!({
                "behavior": "allow",
                "downloadPath": dl_dir,
                "eventsEnabled": true
            }),
            None,
        )
        .await?;
        let gen = watched.generation.fetch_add(1, Ordering::Relaxed) + 1;
        // 旧代残留的 tabs/refs/dialogs/load_waiters/sheets/帧登记属于死会话
        // （sessionId 已失效），整清（等待中的导航命令各自走到自己的超时
        // 分支；CSS.enable 重发后 styleSheetAdded 会重新回放）
        watched.tabs.lock().unwrap().clear();
        watched.refs.lock().unwrap().clear();
        watched.snap_gen.lock().unwrap().clear();
        watched.frames.lock().unwrap().clear();
        watched.orphan_frame_sessions.lock().unwrap().clear();
        watched.dialogs.lock().unwrap().clear();
        watched.load_waiters.lock().unwrap().clear();
        watched.sheets.lock().unwrap().clear();
        watched.css_since.lock().unwrap().clear();
        // Fetch 拦截随死会话失效（Fetch.enable 是 session 级，新连接需重新
        // 下发规则——契约：连接重建即失效，GET intercept 返回空 + liveSince null）
        watched.intercept.lock().unwrap().clear();
        // I3：diff 滚动基线与 profiler 运行态同理属于死会话——tracing session
        // 随连接失效，基线快照的 @eN 也随 refs 代际作废，整清不残留
        watched.last_snap.lock().unwrap().clear();
        *watched.profiler.lock().unwrap() = None;
        // 下载运行态同理重建：新连接重发 setDownloadBehavior + 新目录；
        // 等待者被 drop → 各自 download_wait 的 rx Err 分支诚实报「等待失效」
        watched.downloads.lock().unwrap().reset(dl_dir);
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

    /// snap <tab>：kit take_snapshot 管线（durable refs + 上游行格式，我方
    /// hover_scan 与 meta 契约保留）。返回 { text, refs_count, generation,
    /// hoverScan, removedRefs? }。
    pub async fn snap(
        &self,
        ins_id: &str,
        tab: &str,
        params: SnapParams,
    ) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "snap",
            ins_id,
            tab,
            move |w, tid, sid| async move { snap_tool(&w, &tid, &sid, &params).await },
        )
        .await
    }

    /// click <tab> <@eN | selector> [--double] [--right]（I2：内核换 kit
    /// interaction::click——resolve 链 + ClickPhase 派发 + settle +
    /// actionability 全开：可见/启用/遮挡 + 对话框竞速；双击/右键经
    /// ClickSpec 透传）。
    pub async fn click(
        &self,
        ins_id: &str,
        tab: &str,
        target: &str,
        double: bool,
        right: bool,
    ) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "click",
            ins_id,
            tab,
            |w, tid, sid| async move { click_tool(&w, &tid, &sid, target, double, right).await },
        )
        .await
    }

    /// fill <tab> <@eN | selector> <value>（I2：主链换 kit interaction::fill
    ///——focus + select/value 清空 + insertText；可填前置检查、contenteditable
    /// 预清空与回读校验为我方契约保全层）。value 为空串 = 仅清空。
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

    /// press <tab> <key> [--target]（I2：按键序列换 kit press_key(_with_modifiers)
    ///——keyDown+keyUp 两段，Ctrl/Meta 组合抑制 text；--target 先聚焦
    ///（ref 与 selector 两路径统一 kit resolve）。
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

    /// download-wait <tab>（W-file）：等待**下一次**下载完成（从调用时刻起
    /// 算，命令式等待：先调 wait 再触发点击）。语义决策：
    /// - **不占实例串行锁**（drop(serial) 后再等）：典型用法是 wait 挂着、
    ///   随后 click 触发下载——若等待期持锁会死锁到超时（click 排不上队）；
    /// - 等待者先注册后触发：终态由 watch_loop 结算（completed → 记录；
    ///   canceled/interrupted/名字不安全 → PAGE_DOWNLOAD_FAILED 如实报错）；
    /// - 超时 → PAGE_DOWNLOAD_TIMEOUT（无终态证据；in_flight 非空时披露
    ///   「进行中未完成」）；连接重建 → 等待登记被清，诚实报失效可重试；
    /// - 并发下载（罕见）：多个等待者按终态到达序先到先得；无等待者的
    ///   终态留在环形记录（GET /page/downloads 可查）。
    pub async fn download_wait(
        &self,
        ins_id: &str,
        tab: &str,
        timeout_ms: Option<u64>,
    ) -> Result<Value, AppError> {
        let timeout = timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_DOWNLOAD_TIMEOUT);
        let (watched, serial) = self.ensure(ins_id).await?;
        let (tid, _sid) = resolve_target(&watched.tabs.lock().unwrap(), tab)?;
        // 关键：释放实例串行锁再等——触发下载的 click/fill 正是等待期间
        // 要执行的命令（与 command() 骨架的差异点，见方法注释）
        drop(serial);
        let (tx, rx) = oneshot::channel();
        watched.downloads.lock().unwrap().waiters.push_back(tx);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(mut record))) => {
                record["instanceId"] = json!(ins_id);
                record["targetId"] = json!(tid);
                Ok(record)
            }
            Ok(Ok(Err(e))) => Err(e),
            // 发送端被 drop 而未送数：等待期间连接重建（establish 清空等待
            // 登记）——诚实报失效，调用方可立即重新 download-wait
            Ok(Err(_dropped)) => Err(unavailable(
                "等待期间 page 连接重建（等待登记已随死会话清空），请重新 download-wait",
            )),
            Err(_elapsed) => {
                let in_flight = watched.downloads.lock().unwrap().in_flight.len();
                Err(download_timeout(in_flight, timeout))
            }
        }
    }

    /// downloads <tab>（W-file）：终态记录环查询（最近 MAX_DOWNLOAD_HISTORY
    /// 条，含被等待者消费的——审计面；matched 字段区分）。
    pub async fn downloads_list(&self, ins_id: &str, tab: &str) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "downloads",
            ins_id,
            tab,
            |w, _tid, _sid| async move {
                let st = w.downloads.lock().unwrap();
                Ok(json!({
                    "downloads": st.history.iter().cloned().collect::<Vec<_>>(),
                    "capacity": MAX_DOWNLOAD_HISTORY,
                }))
            },
        )
        .await
    }

    /// upload <tab> <target> <filePath>（W-file）：主机文件设入页面 file
    /// input（DOM.setFileInputFiles， chooser 模式）。主机侧校验（绝对路径 /
    /// 拒符号链接 / 存在性 / ≤100MB）在锁外快速失败；页面侧校验（必须是
    /// input[type=file]，否则 422）+ setFileInputFiles 在命令内完成。
    /// 不拷贝文件：CDP 直接引用主机路径（实例与本机同机；远程接入场景
    /// 本端点走 API 侧主机路径——文档已披露）。
    pub async fn upload(
        &self,
        ins_id: &str,
        tab: &str,
        target: &str,
        file_path: &str,
    ) -> Result<Value, AppError> {
        if target.is_empty() {
            return Err(AppError::invalid_request(
                "target required（@eN 或 CSS 选择器）",
            ));
        }
        let (path, file_name, size) = validate_upload_file(file_path)?;
        self.command(
            TOOL_TIMEOUT,
            "upload",
            ins_id,
            tab,
            move |w, tid, sid| async move {
                upload_tool(&w, &tid, &sid, target, &path, &file_name, size).await
            },
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

    /// extract <tab> [target]：表格 → 结构化 JSON（W-ext 两段式：收集器
    /// 只读采 DOM 事实 → domain/table_extract 归一化）。target 缺省 = 主文档
    /// 最大表格自动发现；支持 @eN（含跨帧 ref）与 CSS 选择器。
    pub async fn extract(
        &self,
        ins_id: &str,
        tab: &str,
        target: Option<&str>,
    ) -> Result<Value, AppError> {
        self.command(
            TOOL_TIMEOUT,
            "extract",
            ins_id,
            tab,
            |w, tid, sid| async move { extract_tool(&w, &tid, &sid, target).await },
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
    /// 消失或达上限（P2-G；I2：点击层换 kit click selector 路径）。预算 =
    /// interval × max-clicks + 工具兑底
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

    /// type <tab> <text>：对当前焦点元素输入（I2：换 kit
    /// type_text_into_active_context——逐字符派发，\n/\t 走真按键事件；
    /// 无定位、无校验语义不变）——跨域 iframe 输入的唯一路径（eval/fill 的
    /// selector 均不可达跨域 frame，先 clickxy 点入获得焦点）（P2-G，owner
    /// 补充）。空文本 →
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

    /// screenshot <tab> [--target] [--full]：元素级（@eN / CSS 选择器，复用定位链）、
    /// 整视口、或全页 png 截图（W-cap；--full 形态 W-shot）。target 缺省 = 当前
    /// 视口；元素路径：scrollIntoView best-effort → quads 联合 bbox → 回退
    /// getBoundingClientRect → Page.captureScreenshot { clip }（与 click 的几何
    /// 链同源，clip 是区域取并集，点击是点取最大 quad）。full = 发起时刻的
    /// 整文档尺寸（Runtime.evaluate 读 scrollWidth/scrollHeight → clip 全文档
    /// + captureBeyondViewport），与 target 互斥（两个语义拼在一起只会掩盖
    /// 调用方 bug，显式 400 不静默取舍）。
    pub async fn screenshot(
        &self,
        ins_id: &str,
        tab: &str,
        target: Option<&str>,
        full: bool,
    ) -> Result<Value, AppError> {
        if target == Some("") {
            return Err(AppError::invalid_request(
                "target must be an @eN ref or CSS selector (omit it to capture the viewport)",
            ));
        }
        if full && target.is_some() {
            return Err(AppError::invalid_request(
                "full and target are mutually exclusive: full captures the whole document, target captures one element",
            ));
        }
        self.command(
            TOOL_TIMEOUT,
            "screenshot",
            ins_id,
            tab,
            |w, tid, sid| async move { screenshot_tool(&w, &tid, &sid, target, full).await },
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
        let rules_json = body.get("rules").and_then(Value::as_array).ok_or_else(|| {
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
                                Action::Mock { status, delay_ms, allow_origin, .. } =>
                                    json!({"type": "mock", "status": status, "delayMs": delay_ms,
                                           "allowOrigin": allow_origin}),
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

fn require_conn(watched: &Watched) -> Result<SharedPageConn, AppError> {
    watched
        .conn
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| unavailable("page 控制连接未就绪（建立中或已断开），稍后重试"))
}

/// browser_kit 能力层的 client 构造（I1b：snap 管线与元素解析链的入口）。
/// active_frame=None（CSS 选择器的 `frame <sel>` 会话态是 I2 范畴；构造期
/// 定值，见 kit client.rs 的 V1 适配说明）。
pub(crate) fn kit_client(
    watched: &Watched,
) -> Result<crate::application::browser_kit::CdpClient, AppError> {
    Ok(crate::application::browser_kit::CdpClient::new(
        require_conn(watched)?,
        watched.event_bus.clone(),
    ))
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

// ============================================================================
// 测试：纯逻辑（AX 快照/路由/分类）+ fake CDP 编排全链路（不依赖真 Chrome）
// ============================================================================
#[cfg(test)]
mod tests;
