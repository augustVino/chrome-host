use super::cdp_send;
use crate::application::fetch_mock::{MockSpec, MockUpstream};
use crate::application::page_service::state::Watched;
use crate::domain::fetch_rules::{first_live_match, pass_reason, Action, InterceptRule};
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
/// 与上游参考实现同语义（agent-browser resolve_fetch_paused 的 origin
/// headers 注入路径：原头保留 + extra 同名大小写不敏感覆盖——continueRequest
/// 是替换不是合并，不常原头即丢头）。
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

    // 决策（短锁：匹配 + 计数 + journal；无状态/未命中/耗尽 → None = 放行）。
    // I5：未命中时同锁取 pass_reason（最接近命中的失败维度：耗尽/类型被滤
    // /url 未中）作 journal detail——诊断与决策同口径一致。
    let (decision, pass_detail): (Option<(String, Action)>, String) = {
        let mut guard = watched.intercept.lock().unwrap();
        match guard.get_mut(&tid) {
            None => (None, String::new()),
            Some(st) => match first_live_match(&st.rules, &st.used, url, resource_type) {
                Some(rule) => {
                    *st.used.entry(rule.id.clone()).or_insert(0) += 1;
                    (Some((rule.id.clone(), rule.action.clone())), String::new())
                }
                None => (None, pass_reason(&st.rules, &st.used, url, resource_type)),
            },
        }
    };

    // 未命中放行（常态路径：patterns 命中但 resourceTypes/times 不满足）
    let Some((rule_id, action)) = decision else {
        let detail = if pass_detail.is_empty() {
            "no live rule matched".to_string()
        } else {
            pass_detail
        };
        let entry = journal_entry(&request_id, url, "passed", None, &detail);
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
            allow_origin,
        } => {
            outcome = "mocked";
            match MockUpstream::shared().await {
                Ok(upstream) => {
                    let mock_url = upstream.register(MockSpec {
                        status: *status,
                        headers: headers.clone(),
                        body: body.clone(),
                        delay_ms: *delay_ms,
                        allow_origin: allow_origin.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- merge_request_headers（I5 对齐测试）----------------
    // 语义与上游参考（agent-browser resolve_fetch_paused 的 origin headers
    // 注入路径）对齐：continueRequest 替换语义下原头全保留 + 规则头同名
    // 大小写不敏感覆盖；无原头/无覆盖头各自退化正常。

    fn hdrs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(k, w)| (k.to_string(), w.to_string()))
            .collect()
    }

    fn named(v: &[Value]) -> Vec<(String, String)> {
        v.iter()
            .map(|h| {
                (
                    h["name"].as_str().unwrap().to_string(),
                    h["value"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// 原头保留 + 规则头覆盖同名（大小写不敏感）+ 新名追加。
    #[test]
    fn merge_preserves_originals_and_overrides_same_name() {
        let original = serde_json::json!({
            "accept": "text/html", "X-Trace": "orig", "Cookie": "page-native"
        });
        let merged = merge_request_headers(
            Some(&original),
            &hdrs(&[("x-trace", "rule"), ("X-Debug", "42")]),
        );
        let m = named(&merged);
        // 原头全保留（含页面原生 cookie——黑名单拦规则设凭据不拦页面行为）
        assert!(
            m.contains(&("Cookie".to_string(), "page-native".to_string())),
            "{m:?}"
        );
        assert!(
            m.contains(&("accept".to_string(), "text/html".to_string())),
            "{m:?}"
        );
        // 同名大小写不敏感覆盖：x-trace 覆盖 X-Trace，且不产生重复条目
        assert!(
            m.contains(&("x-trace".to_string(), "rule".to_string())),
            "{m:?}"
        );
        assert_eq!(
            m.iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("x-trace"))
                .count(),
            1
        );
        // 新名追加
        assert!(
            m.contains(&("X-Debug".to_string(), "42".to_string())),
            "{m:?}"
        );
        assert_eq!(merged.len(), 4, "{m:?}");
    }

    /// 退化矩阵：无原头（None/非对象）→ 仅规则头；无覆盖头 → 仅原头；
    /// 双空 → 空数组（continueRequest 裸放行形态）。
    #[test]
    fn merge_degenerate_inputs() {
        // 无原头
        let merged = merge_request_headers(None, &hdrs(&[("X-A", "1")]));
        assert_eq!(named(&merged), vec![("X-A".to_string(), "1".to_string())]);
        // 原头是非对象（事件形态异常——防御）
        let merged =
            merge_request_headers(Some(&serde_json::json!("broken")), &hdrs(&[("X-A", "1")]));
        assert_eq!(named(&merged), vec![("X-A".to_string(), "1".to_string())]);
        // 无覆盖头：原头原样转数组形态
        let original = serde_json::json!({"Accept": "*/*"});
        let merged = merge_request_headers(Some(&original), &[]);
        assert_eq!(
            named(&merged),
            vec![("Accept".to_string(), "*/*".to_string())]
        );
        // 双空
        assert!(merge_request_headers(None, &[]).is_empty());
    }

    /// journal 条目形态：字段集与脱敏 url（诚实披露契约，格式不变约束的
    /// 回归锚点——I5 只允许新增字段，不允许改字段语义）。
    #[test]
    fn journal_entry_shape_stable() {
        let e = journal_entry(
            "req-1",
            "https://a.com/x?token=secret",
            "passed",
            None,
            "times exhausted",
        );
        assert_eq!(e["requestId"], json!("req-1"));
        assert_eq!(e["rule"], serde_json::Value::Null);
        assert_eq!(e["outcome"], json!("passed"));
        assert_eq!(e["detail"], json!("times exhausted"));
        // url 走脱敏（query 被抹）
        assert!(e["url"].as_str().unwrap().starts_with("https://a.com/x"));
        assert!(!e["url"].as_str().unwrap().contains("secret"));
    }
}
