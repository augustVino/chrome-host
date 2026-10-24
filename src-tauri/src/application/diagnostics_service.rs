//! 常驻诊断服务：实例级 watcher 生命周期 + 查询投影。
//!
//! 形态：launch 置 running 后 spawn（不阻塞启动路径）；browser 级常驻 WS +
//! `Target.setAutoAttach(flatten)` —— console/network 事件都是 per-session 维度，
//! 按 target 归属环形缓冲（domain/diagnostics.rs，进程内态重启即清）。
//! 断线指数退避重连（1s 起步、15s 封顶、连上即重置）；实例退出/删除/重启由
//! 代际守卫（AtomicBool）+ 存活检查双路回收，退出即 drop 缓冲防泄漏。
//!
//! 共存纪律：常驻连接只订阅不操作（autoAttach/enable 之外仅截图的
//! Page.captureScreenshot），与 agent 自建的 CDP 会话天然互不干扰。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::domain::diagnostics::{ApplyOutcome, DiagBuffer, QueryResult};
use crate::domain::instance::InstanceStatus;
use crate::error::AppError;
use crate::infrastructure::cdp::ws::CdpConnection;
use crate::infrastructure::db::repositories::instance_repository::InstanceRepository;

const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(15);
/// 查询投影的缺省/上限条数（防一次拖全环）
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;

fn unavailable(message: impl Into<String>) -> AppError {
    AppError::conflict("DIAGNOSTICS_UNAVAILABLE", message)
}

struct Watched {
    buffer: Arc<Mutex<DiagBuffer>>,
    /// 当前代连接（断线置 None，重连整体替换）；截图查询复用它发命令
    conn: Mutex<Option<Arc<tokio::sync::Mutex<CdpConnection>>>>,
    /// 代际守卫：restart 产生新一代后旧代自行退出（防双 watcher 双缓冲）
    current: AtomicBool,
}

impl Watched {
    fn new() -> Self {
        Watched {
            buffer: Arc::new(Mutex::new(DiagBuffer::default())),
            conn: Mutex::new(None),
            current: AtomicBool::new(true),
        }
    }
}

pub struct DiagnosticsService {
    registry: Arc<Mutex<HashMap<String, Arc<Watched>>>>,
    repo: Arc<InstanceRepository>,
    version: String,
}

impl DiagnosticsService {
    pub fn new(repo: Arc<InstanceRepository>, version: String) -> Self {
        DiagnosticsService {
            registry: Arc::new(Mutex::new(HashMap::new())),
            repo,
            version,
        }
    }

    /// 实例置 running 后由 InstanceService 调用。同步部分只有登记 + spawn，
    /// 绝不阻塞启动路径。同实例重复调用（restart）触发代际切换。
    pub fn spawn(&self, ins_id: &str, port: u16) {
        let mut map = self.registry.lock().unwrap();
        if let Some(old) = map.get(ins_id) {
            old.current.store(false, Ordering::Relaxed); // 旧代在下一个检查点退出
        }
        let watched = Arc::new(Watched::new());
        map.insert(ins_id.to_string(), Arc::clone(&watched));
        drop(map);

        let repo = Arc::clone(&self.repo);
        let registry = Arc::clone(&self.registry);
        let ins_id = ins_id.to_string();
        // tauri::async_runtime::spawn：调用方含 main 的同步启动段（P0-2 重挂）
        // 与 async 启动路径，tokio::spawn 在同步上下文会 panic（无 reactor）。
        // tauri runtime 全局常驻，任意线程可调度（与 exit watcher 钩子同款）
        tauri::async_runtime::spawn(async move {
            watch_loop(ins_id, port, repo, registry, watched).await
        });
    }

    fn alive(&self, ins_id: &str) -> bool {
        self.repo
            .get(ins_id)
            .map(|i| i.status != InstanceStatus::Stopped && i.status != InstanceStatus::Error)
            .unwrap_or(false)
    }

    /// 查询前置：实例活着（否则 409）；watcher 尚未起来/刚重启 → 空数据而非错误
    fn require(&self, ins_id: &str) -> Result<Option<Arc<Watched>>, AppError> {
        if !self.alive(ins_id) {
            return Err(unavailable(format!(
                "实例 {ins_id} 未在运行，常驻诊断仅对运行中实例可用"
            )));
        }
        Ok(self.registry.lock().unwrap().get(ins_id).cloned())
    }

    /// GET console?level=&limit=&tab=：level 精确匹配（error/warning/info/debug/log）；
    /// tab 按 targetId 归属过滤（环中存在无归属条目时降级全量，meta 标注）
    pub fn console(
        &self,
        ins_id: &str,
        level: Option<&str>,
        tab: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Value, AppError> {
        let watched = self.require(ins_id)?;
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let result = watched
            .map(|w| w.buffer.lock().unwrap().console_entries(level, tab, limit))
            .unwrap_or_else(empty_result);
        Ok(query_response(ins_id, result))
    }

    /// GET network?failedOnly=&limit=&tab=：failedOnly = 失败请求或 HTTP ≥400；
    /// tab 按 targetId 归属过滤（同上）
    pub fn network(
        &self,
        ins_id: &str,
        failed_only: bool,
        tab: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Value, AppError> {
        let watched = self.require(ins_id)?;
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let result = watched
            .map(|w| {
                w.buffer
                    .lock()
                    .unwrap()
                    .network_entries(failed_only, tab, limit)
            })
            .unwrap_or_else(empty_result);
        Ok(query_response(ins_id, result))
    }

    /// GET har：network 环生成 HAR 1.2 文档原样返回（har-recorder 落地点）
    pub fn har(&self, ins_id: &str) -> Result<Value, AppError> {
        let watched = self.require(ins_id)?;
        let har = watched
            .map(|w| w.buffer.lock().unwrap().to_har(&self.version))
            .unwrap_or_else(|| DiagBuffer::default().to_har(&self.version));
        Ok(har)
    }

    /// GET screenshot：复用常驻连接对第一个 http(s) 页面 session 截图（png base64）。
    /// 连接未就绪/无页面 → 409 DIAGNOSTICS_UNAVAILABLE（与 M5 同码同语义）。
    pub async fn screenshot(&self, ins_id: &str) -> Result<Value, AppError> {
        let watched = self
            .require(ins_id)?
            .ok_or_else(|| unavailable("常驻诊断连接未就绪（实例刚启动，稍后重试）"))?;
        let shared = watched
            .conn
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| unavailable("常驻诊断连接断开重连中，稍后重试"))?;
        let session_id = watched
            .buffer
            .lock()
            .unwrap()
            .pick_page_session()
            .ok_or_else(|| unavailable("无可截图的 http(s) 页面目标"))?;
        let mut conn = shared.lock().await;
        let resp: Value = conn
            .send(
                "Page.captureScreenshot",
                json!({ "format": "png" }),
                Some(&session_id),
            )
            .await?;
        let data = resp
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::internal("截图响应缺少 data 字段"))?
            .to_string();
        Ok(json!({ "instanceId": ins_id, "format": "png", "data": data }))
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// watcher 未就绪时的空投影（空环过滤任意 tab 结果仍为空，无需降级标注）
fn empty_result<T>() -> QueryResult<T> {
    QueryResult {
        entries: Vec::new(),
        meta: None,
    }
}

/// 查询响应组装：meta 仅在请求了 tab 过滤时出现（未过滤查询形状不变）
fn query_response<T: Serialize>(ins_id: &str, result: QueryResult<T>) -> Value {
    let QueryResult { entries, meta } = result;
    let mut resp = json!({ "instanceId": ins_id, "entries": entries });
    if let Some(m) = meta {
        resp["meta"] = json!(m);
    }
    resp
}

/// watcher 主循环：连接 → 泵事件 → 死亡后退避重连；退出条件（代际失效或实例
/// 不再存活）满足后从登记表移除（仅当仍是当前代，防旧代误删新代条目）
async fn watch_loop(
    ins_id: String,
    port: u16,
    repo: Arc<InstanceRepository>,
    registry: Arc<Mutex<HashMap<String, Arc<Watched>>>>,
    watched: Arc<Watched>,
) {
    let mut backoff = BACKOFF_START;
    loop {
        if !watched.current.load(Ordering::Relaxed) {
            break;
        }
        if !alive(&repo, &ins_id) {
            break;
        }
        match start_session(port, &watched).await {
            Ok(mut rx) => {
                backoff = BACKOFF_START;
                while let Some(event) = rx.recv().await {
                    if !watched.current.load(Ordering::Relaxed) {
                        break;
                    }
                    let outcome = watched.buffer.lock().unwrap().apply(&event, now_ms());
                    if let ApplyOutcome::Attach(sid) = outcome {
                        enable_session(&watched, &sid).await;
                    }
                }
                // 通道关闭 = reader 结束 = 连接已死；清空连接引用后进入退避重连
                *watched.conn.lock().unwrap() = None;
            }
            Err(e) => {
                tracing::warn!("[diagnostics] 实例 {ins_id} 常驻连接建立失败（退避重试）: {e}");
            }
        }
        if !watched.current.load(Ordering::Relaxed) || !alive(&repo, &ins_id) {
            break;
        }
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff * 2, BACKOFF_MAX);
    }
    let mut map = registry.lock().unwrap();
    if map
        .get(&ins_id)
        .is_some_and(|cur| Arc::ptr_eq(cur, &watched))
    {
        map.remove(&ins_id);
    }
}

fn alive(repo: &InstanceRepository, ins_id: &str) -> bool {
    repo.get(ins_id)
        .map(|i| i.status != InstanceStatus::Stopped && i.status != InstanceStatus::Error)
        .unwrap_or(false)
}

/// 建立常驻连接并挂 autoAttach；事件通道由 reader 侧持有（连接死 → 通道 drop）
async fn start_session(
    port: u16,
    watched: &Watched,
) -> Result<tokio::sync::mpsc::UnboundedReceiver<Value>, AppError> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let conn = CdpConnection::connect_eventing(port, tx).await?;
    let shared = Arc::new(tokio::sync::Mutex::new(conn));
    *watched.conn.lock().unwrap() = Some(Arc::clone(&shared));
    let mut g = shared.lock().await;
    // waitForDebuggerOnStart 在 CfT 131 为必填参数（缺省即 -32602 Invalid parameters，
    // 实测确认）；false = attach 不暂停目标页，符合「只订阅不操作」纪律
    let _: Value = g
        .send(
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "flatten": true, "waitForDebuggerOnStart": false }),
            None,
        )
        .await?;
    Ok(rx)
}

/// per-session 域启用（console/network/log 三源）。best-effort：失败仅该 session
/// 缺数据，重连后自愈；不因单个 attach 失败拖垮 watcher
async fn enable_session(watched: &Watched, session_id: &str) {
    let Some(shared) = watched.conn.lock().unwrap().clone() else {
        return;
    };
    let mut g = shared.lock().await;
    for method in ["Runtime.enable", "Network.enable", "Log.enable"] {
        if let Err(e) = g.send::<Value>(method, (), Some(session_id)).await {
            tracing::debug!("[diagnostics] session {session_id} {method} 失败: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::instance::Instance;
    use crate::infrastructure::db::client::init_pool;
    use crate::infrastructure::db::schema::init_schema;

    fn setup() -> (Arc<InstanceRepository>, Instance) {
        let dir = std::env::temp_dir().join(format!("cem-diag-svc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = init_pool(&dir.join("test.db")).unwrap();
        init_schema(&pool).unwrap();
        // instances.environment_id 有 FK，先落一条环境行
        let now = chrono::Utc::now().timestamp_millis();
        pool.get()
            .unwrap()
            .execute(
                "INSERT INTO environments (id, name, keep_alive, created_at, updated_at) VALUES ('env_x', 'x', 0, ?1, ?1)",
                rusqlite::params![now],
            )
            .unwrap();
        let repo = Arc::new(InstanceRepository::new(pool));
        let ins = Instance {
            id: "ins_diag".into(),
            environment_id: "env_x".into(),
            login_profile_id: None,
            profile_dir: "/tmp/x".into(),
            pid: None,
            cdp_port: Some(9333),
            status: InstanceStatus::Running,
            host_rules: None,
            browser_version: None,
            started_at: None,
            stopped_at: None,
            created_at: now,
            updated_at: now,
        };
        repo.insert(&ins).unwrap();
        (repo, ins)
    }

    /// 查询门卫：未运行 → 409；运行中但 watcher 未起（刚启动）→ 空数据而非错误
    #[test]
    fn require_gates_on_liveness_and_treats_missing_watcher_as_empty() {
        let (repo, ins) = setup();
        let svc = DiagnosticsService::new(repo.clone(), "0.2.3".into());

        let console = svc.console(&ins.id, None, None, None).unwrap();
        assert_eq!(
            console["entries"].as_array().unwrap().len(),
            0,
            "无 watcher = 空数据"
        );
        let har = svc.har(&ins.id).unwrap();
        assert_eq!(har["log"]["entries"].as_array().unwrap().len(), 0);

        repo.update_runtime(
            &ins.id,
            crate::domain::instance::RuntimeUpdate {
                status: Some(InstanceStatus::Stopped),
                ..Default::default()
            },
        )
        .unwrap();
        let err = svc.console(&ins.id, None, None, None).unwrap_err();
        assert_eq!(err.status(), 409);
        assert_eq!(err.code(), "DIAGNOSTICS_UNAVAILABLE");
    }

    /// 代际守卫的登记语义：spawn 同实例两次，旧代标记失效且登记表指向新代。
    /// watcher 内含 tokio::spawn，需在 runtime 内驱动（连接必然失败进入退避循环，
    /// 测试结束时随 runtime 一同丢弃）
    #[tokio::test]
    async fn spawn_replaces_generation() {
        let (repo, ins) = setup();
        let svc = DiagnosticsService::new(repo, "0.2.3".into());
        svc.spawn(&ins.id, 9333); // 无 CDP 可连：watcher 会退避循环，不影响登记语义
        let first = svc.registry.lock().unwrap().get(&ins.id).cloned().unwrap();
        svc.spawn(&ins.id, 9333);
        let second = svc.registry.lock().unwrap().get(&ins.id).cloned().unwrap();
        assert!(!Arc::ptr_eq(&first, &second), "登记表已指向新代");
        assert!(!first.current.load(Ordering::Relaxed), "旧代已失效");
        assert!(second.current.load(Ordering::Relaxed));
        // 实例停止后 watcher 自行退出并清理登记（这里直接驱动其退出路径）
        second.current.store(false, Ordering::Relaxed);
    }

    /// tab 过滤透传：直接登记 Watched 并喂事件（绕过 spawn 的网络路径），
    /// 验证 service 层的 tab 参数透传与 meta 组装（filteredByTab/tabFilterUnavailable）
    #[test]
    fn console_network_tab_filter_meta_surfaced() {
        let (repo, ins) = setup();
        let svc = DiagnosticsService::new(repo, "0.2.3".into());
        let watched = Arc::new(Watched::new());
        svc.registry
            .lock()
            .unwrap()
            .insert(ins.id.clone(), Arc::clone(&watched));

        let mut buffer = watched.buffer.lock().unwrap();
        buffer.apply(
            &json!({ "method": "Target.attachedToTarget",
                "params": { "sessionId": "s1", "targetInfo": { "targetId": "tab-1", "type": "page", "url": "https://a/" } } }),
            1,
        );
        buffer.apply(
            &json!({ "method": "Runtime.consoleAPICalled", "sessionId": "s1",
                "params": { "type": "error", "args": [{ "type": "string", "value": "boom" }] } }),
            2,
        );
        // 归属缺失样本：未登记 session 的迟到 console 帧
        buffer.apply(
            &json!({ "method": "Runtime.consoleAPICalled", "sessionId": "ghost",
                "params": { "type": "error", "args": [{ "type": "string", "value": "late" }] } }),
            3,
        );
        drop(buffer);

        // 带 tab 查询：降级全量 + meta 标注（domain 语义在 service 响应中如实透出）
        let resp = svc
            .console(&ins.id, Some("error"), Some("tab-1"), None)
            .unwrap();
        assert_eq!(resp["entries"].as_array().unwrap().len(), 2);
        assert_eq!(resp["meta"]["filteredByTab"], "tab-1");
        assert_eq!(resp["meta"]["tabFilterUnavailable"], true);
        assert_eq!(resp["meta"]["before"], 2);
        assert_eq!(resp["meta"]["after"], 2);

        // network 透传：空环 + tab 过滤 → 空 entries，meta 正常（不降级）
        let resp = svc.network(&ins.id, false, Some("tab-1"), None).unwrap();
        assert_eq!(resp["entries"].as_array().unwrap().len(), 0);
        assert_eq!(resp["meta"]["tabFilterUnavailable"], false);

        // 未请求 tab：响应形状与 M6 一致（无 meta 键）
        let resp = svc.console(&ins.id, None, None, None).unwrap();
        assert_eq!(resp["entries"].as_array().unwrap().len(), 2);
        assert!(resp.get("meta").is_none());
    }
}
