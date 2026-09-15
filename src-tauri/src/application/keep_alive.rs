//! keepAlive 监控：环境开启后实例异常退出自动重启。
//!
//! 韧性语义：
//! - `kill -9` 主进程 → 自动重启并恢复 CDP ready；
//! - 60s 窗口内连续崩溃 ≥3 次 → 停止监控 + 置 error（防无限重启风暴）；
//! - 用户 stop 先取消监控再杀进程（防误重启刚停的实例）。
//!
//! 判死三防线：探活失败 ≠ 死亡，CDP 二次确认后才处置。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::domain::instance::{InstanceStatus, RuntimeUpdate};
use crate::infrastructure::cdp;
use crate::infrastructure::db::repositories::instance_repository::InstanceRepository;
use crate::infrastructure::process::ProcessManager;

use super::activity::Activity;
use super::instance_service::InstanceService;

const POLL_INTERVAL: Duration = Duration::from_secs(3);
const RESTART_WINDOW_MS: i64 = 60_000;
const MAX_RESTARTS_PER_WINDOW: usize = 3;

struct EntryInner {
    cancel: AtomicBool,
    /// 重启时间戳（60s 滑动窗口计数）
    restart_at: Mutex<Vec<i64>>,
}

pub struct KeepAliveWatcher {
    instances: Arc<InstanceRepository>,
    process: Arc<dyn ProcessManager>,
    activity: Arc<Activity>,
    entries: Mutex<HashMap<String, Arc<EntryInner>>>,
    /// 回引 InstanceService 执行重启（Weak 打破循环依赖；main.rs 构造完成后注入）
    service: OnceLock<std::sync::Weak<InstanceService>>,
}

impl KeepAliveWatcher {
    pub fn new(
        instances: Arc<InstanceRepository>,
        process: Arc<dyn ProcessManager>,
        activity: Arc<Activity>,
    ) -> Self {
        KeepAliveWatcher {
            instances,
            process,
            activity,
            entries: Mutex::new(HashMap::new()),
            service: OnceLock::new(),
        }
    }

    /// main.rs 在 InstanceService 构造完成后注入（Weak，避免 Arc 循环引用）
    pub fn set_service(&self, service: std::sync::Weak<InstanceService>) {
        let _ = self.service.set(service);
    }

    /// 注册监控（实例进入 running 后调用；已注册则幂等跳过）
    pub fn register(&self, ins_id: &str) {
        let entry = {
            let mut entries = match self.entries.lock() {
                Ok(m) => m,
                Err(_) => return,
            };
            entries
                .entry(ins_id.to_string())
                .or_insert_with(|| {
                    Arc::new(EntryInner {
                        cancel: AtomicBool::new(false),
                        restart_at: Mutex::new(Vec::new()),
                    })
                })
                .clone()
        };
        let instances = self.instances.clone();
        let process = self.process.clone();
        let activity = self.activity.clone();
        let service = self.service.clone();
        tauri::async_runtime::spawn(Self::monitor(entry, ins_id.to_string(), instances, process, activity, service));
    }

    /// 取消监控（用户 stop / 删除实例 / 环境关闭 keepAlive）
    pub fn cancel(&self, ins_id: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            if let Some(entry) = entries.remove(ins_id) {
                entry.cancel.store(true, Ordering::SeqCst);
            }
        }
    }

    pub fn cancel_all(&self, ins_ids: &[String]) {
        for id in ins_ids {
            self.cancel(id);
        }
    }

    /// 启动扫描：keepAlive 环境 + running 实例 → 恢复监控（应用重启后监控不丢）
    pub fn scan_register(&self) {
        let Ok(conn) = self.instances.pool().get() else { return };
        let Ok(ids) = conn
            .prepare(
                "SELECT i.id FROM instances i JOIN environments e ON i.environment_id = e.id
                 WHERE e.keep_alive = 1 AND i.status IN ('running', 'starting')",
            )
            .and_then(|mut stmt| {
                stmt.query_map([], |row| row.get::<_, String>(0))
                    .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<String>>())
            })
        else {
            return;
        };
        for id in ids {
            self.register(&id);
        }
    }

    async fn monitor(
        entry: Arc<EntryInner>,
        ins_id: String,
        instances: Arc<InstanceRepository>,
        process: Arc<dyn ProcessManager>,
        activity: Arc<Activity>,
        service: OnceLock<std::sync::Weak<InstanceService>>,
    ) {
        let cancelled = || entry.cancel.load(Ordering::SeqCst);
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            if cancelled() {
                return;
            }
            let Ok(ins) = instances.get(&ins_id) else { return };
            match ins.status {
                InstanceStatus::Starting => continue,           // 等启动流程收尾
                InstanceStatus::Running => {}
                _ => return,                                    // 用户停止/已处置 → 退出监控
            }
            let Some(pid) = ins.pid else { continue };
            let dir = std::path::Path::new(&ins.profile_dir);
            if process.is_alive(pid, dir).unwrap_or(false) {
                continue;
            }
            // 探活失败 ≠ 死亡：CDP 二次确认（端口有响应 = 活着，探活误报）
            if let Some(port) = ins.cdp_port {
                if cdp::client::version(port).await.is_ok() {
                    continue;
                }
            }
            if cancelled() {
                return;
            }
            // 确认崩溃：标记 stopped（start 流程要求非 running），Activity 留痕
            let _ = instances.update_runtime(
                &ins_id,
                RuntimeUpdate {
                    status: Some(InstanceStatus::Stopped),
                    stopped_at: Some(chrono::Utc::now().timestamp_millis()),
                    ..Default::default()
                },
            );
            activity.record(
                "error",
                "instance-crashed",
                Some(&ins.environment_id),
                Some(&ins_id),
                &format!("进程异常退出（pid {pid}），keepAlive 自动恢复"),
            );

            // 60s 滑动窗口崩溃计数：≥3 次 → 停止监控 + 置 error（防无限重启）
            let now_ms = chrono::Utc::now().timestamp_millis();
            let recent = {
                let mut at = entry.restart_at.lock().unwrap_or_else(|e| e.into_inner());
                at.retain(|t| now_ms - *t < RESTART_WINDOW_MS);
                let over_limit = at.len() + 1 >= MAX_RESTARTS_PER_WINDOW;
                at.push(now_ms);
                over_limit
            };
            if recent {
                let _ = instances.update_runtime(
                    &ins_id,
                    RuntimeUpdate { status: Some(InstanceStatus::Error), ..Default::default() },
                );
                activity.record(
                    "error",
                    "instance-error",
                    Some(&ins.environment_id),
                    Some(&ins_id),
                    "keepAlive：60s 内连续崩溃 3 次，停止自动重启",
                );
                return;
            }

            // 重启（start 流程：reconcile 回正状态 → hosts 现拉 → spawn → CDP）
            let Some(svc) = service.get().and_then(|w| w.upgrade()) else { return };
            match svc.start(&ins_id).await {
                Ok(_) => {
                    activity.record(
                        "info",
                        "instance-started",
                        Some(&ins.environment_id),
                        Some(&ins_id),
                        "keepAlive 自动重启成功",
                    );
                    tracing::info!("[keepAlive] 实例 {ins_id} 已自动重启");
                }
                Err(e) => {
                    tracing::warn!("[keepAlive] 实例 {ins_id} 自动重启失败: {e}");
                    activity.record(
                        "error",
                        "instance-error",
                        Some(&ins.environment_id),
                        Some(&ins_id),
                        &format!("keepAlive 自动重启失败: {e}"),
                    );
                    return;
                }
            }
        }
    }
}
