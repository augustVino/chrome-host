use crate::application::page_service::ax::SheetRegistry;
use crate::application::page_service::downloads::DownloadState;
use crate::application::page_service::intercept::InterceptState;
use crate::domain::page_control::{DialogBuffer, RefStore};
use crate::infrastructure::cdp::page_cdp::SharedPageConn;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Instant;
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
pub(crate) struct TabTable {
    /// (targetId, sessionId)，attach 先后序（错误信息列表稳定）
    pub(crate) rows: Vec<(String, String)>,
}

impl TabTable {
    /// attach/重 attach：已存在则原位更新 sessionId（列表顺序稳定）
    pub(crate) fn attach(&mut self, tid: &str, sid: &str) {
        match self.rows.iter_mut().find(|(t, _)| t.as_str() == tid) {
            Some(entry) => entry.1 = sid.to_string(),
            None => self.rows.push((tid.to_string(), sid.to_string())),
        }
    }

    pub(crate) fn detach(&mut self, tid: &str) {
        self.rows.retain(|(t, _)| t.as_str() != tid);
    }

    pub(crate) fn clear(&mut self) {
        self.rows.clear();
    }

    /// L2 握手窗口的判空探针：任一 page target 已入表即视作首批 attach 已到
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub(crate) fn target_of_session(&self, sid: &str) -> Option<&str> {
        self.rows
            .iter()
            .find(|(_, s)| s.as_str() == sid)
            .map(|(t, _)| t.as_str())
    }

    /// targetId 是否为已登记的 page tab（drop_target 分流用）
    pub(crate) fn contains_target(&self, tid: &str) -> bool {
        self.rows.iter().any(|(t, _)| t.as_str() == tid)
    }
}

/// per-tab OOPIF 帧登记（W-oopif）。不变式：OOPIF 的 targetId == frameId
///（Chrome 实现稳定不变式，Puppeteer 同样依赖）；同源 iframe 不是独立
/// target（session 恒 None，内容已在主树中，不参与任何路由）。
/// owner 元素（主文档/父帧中的 <iframe>）backendNodeId 惰性解析
///（DOM.getFrameOwner，挂接渲染与坐标投影用），解析后缓存。
#[derive(Default, Clone)]
pub(crate) struct FrameTable {
    /// 根 frame 的 frameId（frameNavigated 无 parentId 路径记录；
    /// 投影链的终止判定用）
    pub(crate) root: Option<String>,
    /// frameId（== targetId）→ 信息
    pub(crate) frames: HashMap<String, FrameInfo>,
}

#[derive(Default, Clone)]
pub(crate) struct FrameInfo {
    /// frame session（attach 后填；None = 同源 iframe 或 attach 未到）
    pub(crate) session_id: Option<String>,
    /// 父 frameId（frameAttached 记录；None = 未知）
    pub(crate) parent_frame_id: Option<String>,
    /// owner 元素 backendNodeId（DOM.getFrameOwner 惰性解析后缓存）
    pub(crate) owner_backend_node_id: Option<i64>,
}

/// 一个实例的全部 page 控制运行态。
pub(crate) struct Watched {
    /// 当前连接（断连置 None，重建整体替换）。std Mutex 只做槽位读写；
    /// 收发在 [`PageCdp`]（&self 并发 send，无锁包裹）里。
    pub(crate) conn: Mutex<Option<SharedPageConn>>,
    /// per-instance 命令串行（决策 3）：锁的获取即建连检查点——并发首命令
    /// 只有先拿到锁的那个建连，其余直接复用（防双连接竞态）。
    pub(crate) cmd_lock: Arc<tokio::sync::Mutex<()>>,
    /// autoAttach 维护的 targetId → sessionId（仅 type=page），attach 序
    /// 保序（空 target 唯一性路由用，见 [`TabTable`]）
    pub(crate) tabs: Mutex<TabTable>,
    /// per-tab @eN ref 表（snap 整表替换，事件定向失效）
    pub(crate) refs: Mutex<HashMap<String, RefStore>>,
    /// per-tab OOPIF 帧登记（W-oopif）：frameAttached/attachedToTarget 维护
    pub(crate) frames: Mutex<HashMap<String, FrameTable>>,
    /// attach 先于 frameAttached 到达的孤儿 frame 会话（targetId → sessionId；
    /// frameAttached 归属 tab 后认领）
    pub(crate) orphan_frame_sessions: Mutex<HashMap<String, String>>,
    /// per-tab 对话框缓冲（应答成功才入档）
    pub(crate) dialogs: Mutex<HashMap<String, DialogBuffer>>,
    /// per-tab load 事件等待者（导航命令族）：命令发送前登记 oneshot Sender，
    /// watch_loop 收到该 tab 的 Page.loadEventFired 时放行全部等待者。key =
    /// targetId（与 refs/dialogs 同键，drop_tab 一并清理）；超时/失败路径的
    /// Sender 残留至下次 load 事件（send 落空即弃，仅句柄暂存无泄漏风险）。
    pub(crate) load_waiters: Mutex<HashMap<String, Vec<tokio::sync::oneshot::Sender<()>>>>,
    /// per-tab 样式表登记（hover 扫描用）：CSS.enable 后 styleSheetAdded/
    /// Removed 事件维护。key = targetId → { styleSheetId → (origin, disabled) }；
    /// 读取侧才按 origin=regular 且未 disabled 过滤（登记全量，事件流不打折扣）。
    pub(crate) sheets: Mutex<SheetRegistry>,
    /// per-tab CSS.enable 成功时刻：hover 扫描的结算窗口只在该时刻后的首个
    /// HOVER_SETTLE_WINDOW 内生效（styleSheetAdded 回放可能仍在途）；窗口过后
    /// 登记仍空 = 真无表，后续 snap 即时返回（不为无表页付每快照等待）。
    pub(crate) css_since: Mutex<HashMap<String, Instant>>,
    /// per-tab Fetch 拦截运行态（会话生命周期：establish 整清 = 连接重建
    /// 自动失效，GET 披露 liveSince=null；规则由 intercept_set 重新下发）。
    /// **决策在事件流内进行**（on_request_paused，watch_loop 路径，不碰
    /// cmd_lock）；拦截响应命令走 conn 的并发 send（&self），与 in-flight
    /// 工具命令互不阻塞。
    pub(crate) intercept: Mutex<HashMap<String, InterceptState>>,
    /// 实例级下载运行态（W-file）：Browser 域下载事件无 session 维度，整实例
    /// 一份（非 per-tab）。in_flight = WillBegin 登记；history = 终态环形
    /// 记录（容量 20）；waiters = download-wait 的等待者（先注册后触发，
    /// 命令式等待）。establish 整体重置（等待者 rx 收 Err 诚实报失效）。
    pub(crate) downloads: Mutex<DownloadState>,
    /// 代际计数：每次建连 +1。旧代 watch_loop 在清理点核对代际，新一代已
    /// 接管时不动登记表/连接槽（防僵尸 loop 误删新代状态）。
    pub(crate) generation: AtomicU64,
    /// 事件总线（browser_kit 能力层的订阅面）：watch_loop 在 handle_event
    /// 后逐事件转发广播。生命周期与 Watched 相同（跨建连代存活：新一代
    /// loop 接管后继续转发）；订阅者需容忍重连间隙——旧连接的 sessionId
    /// 随连接死亡失效，能力层的会话态本就该随之重建。
    pub(crate) event_bus: tokio::sync::broadcast::Sender<crate::application::browser_kit::CdpEvent>,
}

/// 事件总线容量：鎴屏帧流（~30fps）+常规事件并发的安全余量。
/// broadcast 慢订阅者收 Lagged 丢旧帧（能力层语义容忍，如 screencast）。
const EVENT_BUS_CAPACITY: usize = 4096;

impl Watched {
    pub(crate) fn new() -> Self {
        Watched {
            conn: Mutex::new(None),
            cmd_lock: Arc::new(tokio::sync::Mutex::new(())),
            tabs: Mutex::new(TabTable::default()),
            refs: Mutex::new(HashMap::new()),
            frames: Mutex::new(HashMap::new()),
            orphan_frame_sessions: Mutex::new(HashMap::new()),
            dialogs: Mutex::new(HashMap::new()),
            load_waiters: Mutex::new(HashMap::new()),
            sheets: Mutex::new(HashMap::new()),
            css_since: Mutex::new(HashMap::new()),
            intercept: Mutex::new(HashMap::new()),
            downloads: Mutex::new(DownloadState::default()),
            generation: AtomicU64::new(0),
            event_bus: tokio::sync::broadcast::channel(EVENT_BUS_CAPACITY).0,
        }
    }
}
