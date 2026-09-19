//! CDP 会话服务：token 的下级凭证——**单实例作用域 + TTL** 的代理访问 key
//! （REMOTE-ACCESS-PLAN.md §7.1）。
//!
//! 凭证层级：token（控制面全权）> session（数据面，仅 `/cdp/{ins}/{sid}/*`
//! 代理路径，30 分钟）。发放时的实例 running 校验由 api 薄壳先行完成
//! （require_cdp_port），本服务不依赖 instance 服务——分层单向。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::error::AppError;

/// 会话有效期：固定 30 分钟（过期重取，无刷新/吊销端点——方案 §12 非目标）。
pub const SESSION_TTL_MS: i64 = 30 * 60 * 1000;

/// 审计回调（level, event, target_id, message）：生产落 Activity，测试注入收集器。
type AuditSink = Arc<dyn Fn(&str, &str, Option<&str>, &str) + Send + Sync>;

/// 发放视图（POST /api/v1/instances/{id}/cdp/sessions 响应体）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CdpSessionView {
    pub session_id: String,
    /// 代理路径前缀；客户端拼上自身可达的 scheme://host:port 使用
    /// （经隧道时即 http://127.0.0.1:17890）
    pub base_url: String,
    /// 过期时刻（epoch ms）
    pub expires_at: i64,
}

struct Entry {
    instance_id: String,
    expires_at: i64,
}

pub struct CdpSessionService {
    /// 会话表：进程内运行态（app 重启即清空——方案 §11 已知边界，客户端重取即恢复）
    sessions: Mutex<HashMap<String, Entry>>,
    audit: AuditSink,
    sweeper_started: AtomicBool,
}

impl CdpSessionService {
    /// 生产构造：审计事件落 Activity 流。
    pub fn new(activity: Arc<super::activity::Activity>) -> Self {
        let audit: AuditSink = Arc::new(move |level, event, target, msg| {
            // 事件名仅允许字母数字与 - / : _（Tauri 校验），下划线风格
            activity.record(level, event, None, target, msg);
        });
        Self::with_audit(audit)
    }

    /// 注入审计回调的构造（测试用，无 Activity 依赖）。
    pub fn with_audit(audit: AuditSink) -> Self {
        CdpSessionService {
            sessions: Mutex::new(HashMap::new()),
            audit,
            sweeper_started: AtomicBool::new(false),
        }
    }

    /// 发放会话（uuid v4 会话 key，122bit 熵；running 前置校验由调用方完成）。
    pub fn create(&self, instance_id: &str) -> CdpSessionView {
        let session_id = uuid::Uuid::new_v4().to_string();
        let expires_at = chrono::Utc::now().timestamp_millis() + SESSION_TTL_MS;
        let view = CdpSessionView {
            base_url: format!("/cdp/{instance_id}/{session_id}"),
            session_id,
            expires_at,
        };
        self.sessions.lock().unwrap().insert(
            view.session_id.clone(),
            Entry { instance_id: instance_id.to_string(), expires_at },
        );
        (self.audit)(
            "info",
            "cdp_session_created",
            Some(instance_id),
            "CDP 会话发放（30 分钟有效）",
        );
        view
    }

    /// 校验：存在 + 未过期 + 实例匹配。过期项惰性清理并记审计事件。
    /// 会话 key 走 URL 路径（WS 升级无法携带 header），不匹配一律 false——
    /// 防枚举，不给「key 存在但实例不符」的区分信号。
    pub fn validate(&self, instance_id: &str, session_id: &str) -> bool {
        let now = chrono::Utc::now().timestamp_millis();
        let expired = {
            let mut map = self.sessions.lock().unwrap();
            match map.get(session_id) {
                Some(e) if e.instance_id == instance_id => {
                    if e.expires_at > now {
                        return true;
                    }
                    map.remove(session_id);
                    true // 本分支 = 已过期待清理
                }
                _ => false,
            }
        };
        if expired {
            (self.audit)(
                "info",
                "cdp_session_expired",
                Some(instance_id),
                "CDP 会话过期（访问时惰性清理）",
            );
        }
        false
    }

    /// 清空全部会话（token 轮换联动：旧凭证体系整体作废）。返回清掉的数量。
    pub fn clear_all(&self) -> usize {
        let mut map = self.sessions.lock().unwrap();
        let n = map.len();
        map.clear();
        n
    }

    /// 单轮清扫：删除过期项并统一记 `cdp_session_expired`（清理与审计责任归属
    /// sweeper——从未再被访问的会话也有审计闭环）。独立暴露便于测试。
    pub fn sweep_once(&self) -> usize {
        let now = chrono::Utc::now().timestamp_millis();
        let expired: Vec<(String, String)> = {
            let map = self.sessions.lock().unwrap();
            map.iter()
                .filter(|(_, e)| e.expires_at <= now)
                .map(|(k, e)| (k.clone(), e.instance_id.clone()))
                .collect()
        };
        if expired.is_empty() {
            return 0;
        }
        {
            let mut map = self.sessions.lock().unwrap();
            for (k, _) in &expired {
                map.remove(k);
            }
        }
        for (_, ins) in &expired {
            (self.audit)("info", "cdp_session_expired", Some(ins), "CDP 会话过期（sweeper 清扫）");
        }
        expired.len()
    }

    /// 后台清扫循环（60s；幂等启动）。main.rs 装配时调用。
    pub fn start_sweeper(self: &Arc<Self>) {
        if self.sweeper_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let n = this.sweep_once();
                if n > 0 {
                    tracing::info!("[cdp-session] sweeper 清扫 {n} 个过期会话");
                }
            }
        });
    }
}

// 占位使 AppError 在文档注释引用场景可用（无运行时依赖）
#[allow(dead_code)]
fn _err_type_anchor(_: AppError) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn collector() -> (Arc<StdMutex<Vec<String>>>, AuditSink) {
        let log: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let l2 = Arc::clone(&log);
        let sink: AuditSink = Arc::new(move |_level, event, _target, _msg| {
            l2.lock().unwrap().push(event.to_string());
        });
        (log, sink)
    }

    #[test]
    fn create_validate_roundtrip() {
        let (_log, sink) = collector();
        let svc = CdpSessionService::with_audit(sink);
        let v = svc.create("ins_a");
        assert!(svc.validate("ins_a", &v.session_id), "未过期会话应通过");
        assert_eq!(v.base_url, format!("/cdp/ins_a/{}", v.session_id));
        assert!(v.expires_at > chrono::Utc::now().timestamp_millis());
    }

    /// 会话与实例强绑定：同 key 换实例 → 拒绝（不泄露 key 是否存在）
    #[test]
    fn validate_binds_instance() {
        let (_log, sink) = collector();
        let svc = CdpSessionService::with_audit(sink);
        let v = svc.create("ins_a");
        assert!(!svc.validate("ins_b", &v.session_id), "实例不匹配应拒绝");
        assert!(!svc.validate("ins_a", "not-a-session"), "未知 key 拒绝");
    }

    /// 过期（访问时惰性清理）：过期后拒绝 + 条目删除 + 审计事件
    #[test]
    fn expired_session_rejected_and_cleaned() {
        let (log, sink) = collector();
        let svc = CdpSessionService::with_audit(sink);
        let v = svc.create("ins_a");
        // 手动置过期（等价 TTL 流逝）
        svc.sessions
            .lock()
            .unwrap()
            .get_mut(&v.session_id)
            .unwrap()
            .expires_at = chrono::Utc::now().timestamp_millis() - 1;
        assert!(!svc.validate("ins_a", &v.session_id), "过期会话应拒绝");
        assert!(svc.sessions.lock().unwrap().is_empty(), "过期条目被惰性清理");
        assert!(log.lock().unwrap().iter().any(|e| e == "cdp_session_expired"));
    }

    /// sweeper：清扫过期项 + 审计；未过期保留
    #[test]
    fn sweeper_removes_expired_only() {
        let (log, sink) = collector();
        let svc = CdpSessionService::with_audit(sink);
        let fresh = svc.create("ins_a");
        let stale = svc.create("ins_b");
        svc.sessions
            .lock()
            .unwrap()
            .get_mut(&stale.session_id)
            .unwrap()
            .expires_at = 0;
        assert_eq!(svc.sweep_once(), 1, "只清扫过期项");
        assert!(svc.validate("ins_a", &fresh.session_id), "未过期保留");
        assert!(!svc.sessions.lock().unwrap().contains_key(&stale.session_id));
        assert!(log.lock().unwrap().iter().any(|e| e == "cdp_session_expired"));
    }

    /// token 轮换联动：清空全部
    #[test]
    fn clear_all_wipes_sessions() {
        let (_log, sink) = collector();
        let svc = CdpSessionService::with_audit(sink);
        svc.create("ins_a");
        svc.create("ins_b");
        assert_eq!(svc.clear_all(), 2);
        assert!(svc.sessions.lock().unwrap().is_empty());
    }
}
