use super::cdp_send;
use crate::application::fetch_mock::{MockSpec, MockUpstream};
use crate::application::page_service::state::Watched;
use crate::domain::fetch_rules::{first_live_match, Action, InterceptRule};
use crate::error::AppError;
use serde_json::{json, Value};
use std::collections::HashMap;
// ============================================================================
// 3b. Fetch 拦截：事件流内决策（红线：不碰串行锁；任何错误必放行）
// ============================================================================

/// per-tab 拦截运行态（Fetch.enable 生效期间存在；establish/drop_tab 清除）。
/// 规则本体不可变（下发时编译），times 耗尽由 used 计数判定。
pub(crate) struct InterceptState {
    pub(crate) rules: Vec<InterceptRule>,
    /// id → 已命中次数
    pub(crate) used: HashMap<String, u64>,
    /// 干预记录环（含命中与放行，每条已脱敏；容量 domain::fetch_rules::JOURNAL_CAPACITY）
    pub(crate) journal: std::collections::VecDeque<Value>,
    /// 规则集下发时刻（epoch ms；GET 披露）
    pub(crate) live_since_ms: i64,
}

impl InterceptState {
    pub(crate) fn new(rules: Vec<InterceptRule>) -> Self {
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
fn journal_entry(
    request_id: &str,
    url: &str,
    outcome: &str,
    rule: Option<&str>,
    detail: &str,
) -> Value {
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
        if let Some(pos) = out.iter().position(|h| {
            h["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(k))
        }) {
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
pub(crate) async fn on_request_paused(watched: &Watched, session_id: &str, params: &Value) {
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
                (
                    "stranded",
                    format!("decision failed: {e}; fallback also failed"),
                )
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
