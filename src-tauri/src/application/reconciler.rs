//! 状态对账：DB 存配置与最近状态，实时状态以真实进程为准。
//! 读路径惰性对账 + 应用启动全量对账，无后台轮询。
//!
//! 判定策略（防误判的三层防线）：
//! 1. pid 探活失败 ≠ 一定死亡：Chrome 可能 relaunch 换 pid / argv 变化——
//!    判死前必须 CDP 二次确认（CDP 端口有响应 = 浏览器必活着），并用 ps 重关联 pid
//! 2. 进程活着 + CDP page 数为 0（启动缓冲期后）= 用户关闭了全部窗口（macOS 关窗不退进程）
//!    → 视为用户主动关闭 → 优雅终止进程 + 置 stopped
//! 3. 退出事件由 reaper 回调直推（instance_service），对账只做兜底修正

use std::path::Path;
use std::sync::Arc;


use super::activity::Activity;
use crate::domain::instance::{Instance, InstanceStatus, RuntimeUpdate};
use crate::error::AppError;
use crate::infrastructure::cdp;
use crate::infrastructure::db::repositories::instance_repository::InstanceRepository;
use crate::infrastructure::platform;
use crate::infrastructure::process::ProcessManager;

/// starting 状态超过该时长仍无 pid → 兜底置 error（launch 从未成功完成的遗留，
/// 如应用在内核下载期间崩溃；给足内核下载时间）
const STARTING_NO_PID_TIMEOUT_MS: i64 = 300_000;

/// starting 且已写入 pid 的在途宽限：start() 持续推进会刷 updated_at（mark_starting/pid 写入），
/// 期间一次探活失败不判死（探活失败≠死亡，防快速 stop→start 时状态闪“异常”）。
/// 覆盖 hosts 拉取(~3s) + CDP 等待超时(10s) + 余量
const STARTING_PID_IN_FLIGHT_MS: i64 = 20_000;

/// 启动缓冲期：CDP 就绪后初始 tab 未必立即可见，此窗口内不做关窗判定
const TAB_CHECK_GRACE_MS: i64 = 10_000;

/// 关窗检测的连续确认：首次读到 0 个 page 只记录不动作，距首次 ≥2.5s 仍为空才动手，
/// 防 Chrome 启动瞬间 / CDP 瞬时异常导致的误杀
const TAB_EMPTY_CONFIRM_MS: i64 = 2_500;

static EMPTY_TAB_SINCE: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<String, i64>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

pub struct Reconciler {
    instances: Arc<InstanceRepository>,
    process: Arc<dyn ProcessManager>,
    activity: Arc<Activity>,
}

impl Reconciler {
    pub fn new(
        instances: Arc<InstanceRepository>,
        process: Arc<dyn ProcessManager>,
        activity: Arc<Activity>,
    ) -> Self {
        Reconciler { instances, process, activity }
    }

    /// 单实例对账：以真实进程状态回写 DB，返回对账后的最新视图。
    pub async fn reconcile(&self, ins: &Instance) -> Instance {
        if !ins.status.is_alive_status() {
            return ins.clone();
        }

        let alive = match ins.pid {
            Some(pid) => self
                .process
                .is_alive(pid, Path::new(&ins.profile_dir))
                .unwrap_or(false),
            None => false,
        };

        if ins.status == InstanceStatus::Starting {
            return self.reconcile_starting(ins, alive).await;
        }

        // status == Running
        if alive {
            // 关窗检测：用户关闭全部窗口（macOS 关窗不退进程）→ 视为主动关闭
            self.check_tabs_closed(ins).await;
            return self.fresh(ins);
        }

        // pid 探活失败 ≠ 一定死亡：CDP 二次确认（端口有响应 = 浏览器必活着）
        let port = match ins.cdp_port {
            Some(p) => p,
            None => return self.mark_dead(ins),
        };
        if cdp::client::version(port).await.is_ok() {
            // 浏览器活着：尝试 pid 重关联（Chrome relaunch 换 pid 的场景）
            match platform::scan_pid_by_profile(Path::new(&ins.profile_dir)) {
                Some(new_pid) if Some(new_pid) != ins.pid => {
                    tracing::info!(
                        "[reconcile] pid 漂移重关联: {} {} -> {}",
                        ins.id, ins.pid.unwrap_or_default(), new_pid
                    );
                    let _ = self.instances.update_runtime(
                        &ins.id,
                        RuntimeUpdate { pid: Some(new_pid), ..Default::default() },
                    );
                }
                _ => {}
            }
            return self.fresh(ins);
        }

        self.mark_dead(ins)
    }

    /// starting 状态的对账：pid 死 = 启动失败；无 pid 在宽限期内不动（内核下载中）
    async fn reconcile_starting(&self, ins: &Instance, alive: bool) -> Instance {
        if alive {
            return ins.clone();
        }
        let now = chrono::Utc::now().timestamp_millis();
        if ins.pid.is_none() && now - ins.updated_at < STARTING_NO_PID_TIMEOUT_MS {
            return ins.clone(); // create/start 流程仍在推进（如内核下载）。
            // 注意用 updated_at（每次 start 都刷新）而非 created_at，
            // 否则老实例重新 start 会被误判为启动失败（状态闪“异常”）
        }
        // 补充防线：pid 已写入（spawn 完成）但 CDP 未就绪的在途窗口，
        // 一次探活毛刺（ps 竞态 / ps spawn 失败 Err→false）不可判死——start() 在途
        // 会持续推进（mark_starting/pid 写入都刷 updated_at），宽限 20s > hosts 拉取
        // (~3s) + CDP 超时(10s) + 余量。否则快速 stop→start 衔接时会闪“异常”再变
        // “运行中”（用户实证，日志见 [reconcile] starting 兕底判失败）
        if ins.pid.is_some() && now - ins.updated_at < STARTING_PID_IN_FLIGHT_MS {
            return ins.clone();
        }
        let _ = self.instances.update_runtime(
            &ins.id,
            RuntimeUpdate { status: Some(InstanceStatus::Error), ..Default::default() },
        );
        tracing::warn!(
            "[reconcile] starting 兕底判失败: ins={} pid={:?} updated_at 距今 {}ms（宽限期外）",
            ins.id,
            ins.pid,
            now - ins.updated_at
        );
        self.emit("instance-error", &ins.id);
        self.fresh(ins)
    }

    /// 关窗检测（macOS）：全部窗口关闭后进程仍在，视为用户主动关闭 → 优雅终止。
    /// CDP 查询失败（Chrome 忙/暂时不可达）→ 跳过本次判定，保守不动作。
    async fn check_tabs_closed(&self, ins: &Instance) {
        let Some(port) = ins.cdp_port else { return };
        let Some(started_at) = ins.started_at else { return };
        let now = chrono::Utc::now().timestamp_millis();
        if now - started_at < TAB_CHECK_GRACE_MS {
            return;
        }
        let pages = match cdp::client::list_targets(port).await {
            Ok(targets) => targets.iter().filter(|t| t.target_type == "page").count(),
            Err(_) => return, // 不可达不代表关窗，交给 pid 判死路径
        };
        if pages > 0 {
            EMPTY_TAB_SINCE.lock().unwrap().remove(&ins.id);
            return;
        }

        // 连续确认：距首次读到空 ≥2.5s 仍为空才动手
        let now = chrono::Utc::now().timestamp_millis();
        {
            let mut map = EMPTY_TAB_SINCE.lock().unwrap();
            match map.get(&ins.id) {
                None => {
                    map.insert(ins.id.clone(), now);
                    return;
                }
                Some(first) if now - first < TAB_EMPTY_CONFIRM_MS => return,
                Some(_) => {
                    map.remove(&ins.id);
                }
            }
        }

        tracing::info!("[reconcile] 实例 {} 的全部窗口已关闭（连续确认），优雅终止进程", ins.id);
        if let Some(pid) = ins.pid {
            let _ = self.process.terminate(pid);
        }
        let _ = self.instances.update_runtime(
            &ins.id,
            RuntimeUpdate {
                status: Some(InstanceStatus::Stopped),
                stopped_at: Some(chrono::Utc::now().timestamp_millis()),
                ..Default::default()
            },
        );
        self.emit("instance-stopped", &ins.id);
    }

    /// 确认死亡：running → stopped；带 started_at 的按用户关闭语义记 stopped_at
    fn mark_dead(&self, ins: &Instance) -> Instance {
        let now = chrono::Utc::now().timestamp_millis();
        let update = if ins.status == InstanceStatus::Starting {
            RuntimeUpdate { status: Some(InstanceStatus::Error), ..Default::default() }
        } else {
            RuntimeUpdate {
                status: Some(InstanceStatus::Stopped),
                stopped_at: Some(now),
                ..Default::default()
            }
        };
        let event = if ins.status == InstanceStatus::Starting {
            "instance-error"
        } else {
            "instance-stopped"
        };
        let _ = self.instances.update_runtime(&ins.id, update);
        tracing::warn!("[reconcile] 确认死亡: ins={} status={:?} pid={:?} → {event}", ins.id, ins.status, ins.pid);
        self.emit(event, &ins.id);
        self.fresh(ins)
    }

    /// 启动时对账全部实例
    pub async fn reconcile_all(&self) {
        match self.instances.list_all() {
            Ok(all) => {
                for ins in &all {
                    self.reconcile(ins).await;
                }
                tracing::info!("启动对账完成: {} 个实例", all.len());
            }
            Err(e) => tracing::error!("启动对账失败: {e}"),
        }
    }

    /// 读取对账后的实例（供 Service 读路径使用）
    pub async fn reconciled(&self, id: &str) -> Result<Instance, AppError> {
        let ins = self.instances.get(id)?;
        Ok(self.reconcile(&ins).await)
    }

    fn fresh(&self, ins: &Instance) -> Instance {
        self.instances.get(&ins.id).unwrap_or_else(|_| ins.clone())
    }

    fn emit(&self, event: &str, id: &str) {
        self.activity.record("warn", event, None, Some(id), "");
    }
}
