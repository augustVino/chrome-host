//! mock 上游：Fetch 会话层拦截的 mock 响应回源（application 组件）。
//!
//! 为什么需要它（spike 真机定案，结论见任务报告）：CfT 131.0.6778.204 的
//! `Fetch.fulfillRequest` 接受 `binaryBody` 但**不交付响应体**（两条独立
//! 链路 + Request/Response 两阶段实测 bytes=0）；`continueRequest` 改写到
//! `data:` URL 被 Chrome 拒绝。mock 的可行路径 = **本地 loopback HTTP 上游**：
//! 拦截时把请求 `continueRequest` 改写到 `http://127.0.0.1:<port>/m/<token>`，
//! 由本组件按 spec 回任意 status/headers/body/delayMs。
//!
//! CORS 是跨源改写的必过关卡（页面源 ≠ 127.0.0.1 上游源）：所有响应注入
//! `Access-Control-Allow-Origin`（回显请求 Origin），OPTIONS preflight 统一
//! 放行——真机验证页面 fetch 可完整读取 mock body。
//!
//! 生命周期：进程内全局惰性单例（首个 mock 命中时 bind 127.0.0.1:0），仅
//! 监听 loopback；token 一次性消费（每次拦截各注册一份，times>1 的规则逐次
//! 注册），不存在跨规则串台。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 一次 mock 命中的响应规格（domain 规则的 Mock action 运行态）。
#[derive(Debug, Clone)]
pub struct MockSpec {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub delay_ms: u64,
}

pub struct MockUpstream {
    port: u16,
    /// token → spec（一次性：listener 消费即移除；无人消费的 token 随连接
    /// 重建/规则替换过期——常驻个位数条目，无需 GC）
    specs: Mutex<HashMap<String, MockSpec>>,
}

impl MockUpstream {
    /// 进程级惰性单例：首个调用时在 **tauri 全局 runtime 内** bind
    /// 127.0.0.1:0 并 spawn listener，端口经 oneshot 回传。
    /// why 同 runtime：tokio 的 IO 资源注册在**创建它的 runtime** 的 driver 上
    /// ——若在调用方 runtime bind（如测试的短命 current_thread runtime），
    /// 调用方 runtime 退出后 fd 注册随之失效，listener 在 tauri runtime 永不
    /// 唤醒（真机挂起根因）；bind/listen/accept 全部固定在进程级长命的
    /// tauri runtime，与调用方 runtime 生命周期解耦。
    pub async fn shared() -> Result<Arc<MockUpstream>, crate::error::AppError> {
        static CELL: tokio::sync::OnceCell<Arc<MockUpstream>> = tokio::sync::OnceCell::const_new();
        Ok(CELL
            .get_or_try_init(|| async {
                let (tx, rx) = tokio::sync::oneshot::channel();
                tauri::async_runtime::spawn(async move {
                    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
                        Ok(l) => l,
                        Err(e) => {
                            let _ = tx.send(Err(crate::error::AppError::internal(format!(
                                "mock 上游 bind 失败: {e}"
                            ))));
                            return;
                        }
                    };
                    let port = match listener.local_addr() {
                        Ok(a) => a.port(),
                        Err(e) => {
                            let _ = tx.send(Err(crate::error::AppError::internal(format!(
                                "mock 上游端口获取失败: {e}"
                            ))));
                            return;
                        }
                    };
                    let instance = Arc::new(MockUpstream {
                        port,
                        specs: Mutex::new(HashMap::new()),
                    });
                    if tx.send(Ok(Arc::clone(&instance))).is_err() {
                        return; // 等待方已放弃（如初始化超时）：不留孤儿 listener
                    }
                    listener_loop(listener, instance).await;
                });
                rx.await.map_err(|_| {
                    crate::error::AppError::internal("mock 上游初始化任务早退")
                })?
            })
            .await?
            .clone())
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// 注册一次性 spec，返回 mock URL（拦截侧 continueRequest 的改写目标）。
    pub fn register(&self, spec: MockSpec) -> String {
        let token = uuid::Uuid::new_v4().simple().to_string();
        self.specs.lock().unwrap().insert(token.clone(), spec);
        format!("http://127.0.0.1:{}/m/{token}", self.port)
    }

    /// 一次性消费 token（listener 路径；重复/未知 token → None）。
    fn take(&self, token: &str) -> Option<MockSpec> {
        self.specs.lock().unwrap().remove(token)
    }
}

/// listener 主循环：逐连接 spawn（mock 并发量 = 页面并发请求，个位数量级）。
async fn listener_loop(listener: tokio::net::TcpListener, upstream: Arc<MockUpstream>) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("[fetch-mock] accept 失败（继续）: {e}");
                continue;
            }
        };
        tauri::async_runtime::spawn(handle_conn(stream, Arc::clone(&upstream)));
    }
}

/// 单连接处理：读一个 HTTP/1.1 请求头（body 不读——mock 响应不依赖请求体，
/// 连接随响应关闭），回 spec 后 Connection: close。
async fn handle_conn(mut stream: tokio::net::TcpStream, upstream: Arc<MockUpstream>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 64 * 1024 {
                    break;
                }
            }
        }
    }
    let req = String::from_utf8_lossy(&buf);
    let mut lines = req.lines();
    let first_line = lines.next().unwrap_or_default().to_string();
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let header = |name: &str| -> Option<String> {
        req.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            (k.trim().eq_ignore_ascii_case(name)).then(|| v.trim().to_string())
        })
    };
    let origin = header("origin").unwrap_or_else(|| "*".to_string());
    let acrh = header("access-control-request-headers");

    let token = path.strip_prefix("/m/").unwrap_or_default().to_string();
    let (status, headers, body, delay_ms) = if method == "OPTIONS" {
        // CORS preflight 统一放行（跨源改写 + 自定义请求头必经）
        (
            204,
            vec![
                ("Access-Control-Allow-Origin".to_string(), origin.clone()),
                (
                    "Access-Control-Allow-Methods".to_string(),
                    "GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS".to_string(),
                ),
                (
                    "Access-Control-Allow-Headers".to_string(),
                    acrh.unwrap_or_else(|| "*".to_string()),
                ),
                ("Access-Control-Max-Age".to_string(), "600".to_string()),
            ],
            Vec::new(),
            0,
        )
    } else if !token.is_empty() {
        match upstream.take(&token) {
            Some(spec) => {
                // CORS 注入与规则头合并：规则同名覆盖注入（规则显式设置
                // CORS 头优先——黑名单不含响应侧 CORS 头，允许规则自控）
                let mut headers = vec![
                    ("Access-Control-Allow-Origin".to_string(), origin.clone()),
                    ("Content-Length".to_string(), spec.body.len().to_string()),
                ];
                for (k, v) in &spec.headers {
                    headers.retain(|(hk, _)| !hk.eq_ignore_ascii_case(k));
                    headers.push((k.clone(), v.clone()));
                }
                (spec.status, headers, spec.body, spec.delay_ms)
            }
            None => (
                404,
                vec![("Access-Control-Allow-Origin".to_string(), origin)],
                b"mock token consumed or unknown".to_vec(),
                0,
            ),
        }
    } else {
        (404, Vec::new(), b"not found".to_vec(), 0)
    };
    if delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
    let resp = http_response(status, &headers, &body);
    let _ = stream.write_all(&resp).await;
    let _ = stream.shutdown().await;
}

/// 极简 HTTP/1.1 响应拼装（字节精确：body 原样字节，头值防御性剔除 CR/LF
/// ——domain 校验未限头值字符集，此处兜底防响应拆分）。
fn http_response(status: u16, headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    let reason = match status {
        200..=299 => "OK",
        400..=499 => "Client Error",
        500..=599 => "Server Error",
        _ => "Status",
    };
    let mut out = format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\n").into_bytes();
    for (k, v) in headers {
        let clean = |s: &str| s.chars().filter(|c| *c != '\r' && *c != '\n').collect::<String>();
        out.extend_from_slice(format!("{}: {}\r\n", clean(k), clean(v)).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// 读 HTTP 响应的极简客户端（Connection: close 读到 EOF）。
    async fn http_get(port: u16, path: &str, extra: &[(&str, &str)]) -> Vec<u8> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect mock upstream");
        let mut req =
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
        for (k, v) in extra {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        buf
    }

    /// 端到端：注册 → 请求 → status/headers/body/CORS/一次性/延迟全链
    ///（真实 listener + 真实 loopback TCP，组件本身即被测对象）。
    #[tokio::test]
    async fn mock_upstream_serves_spec_once_with_cors_and_delay() {
        let upstream = MockUpstream::shared().await.unwrap();
        let port = upstream.port();
        let url = upstream.register(MockSpec {
            status: 201,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: b"{\"ok\":true}".to_vec(),
            delay_ms: 120,
        });
        let token = url.rsplit('/').next().unwrap().to_string();

        let t0 = Instant::now();
        let resp = http_get(port, &format!("/m/{token}"), &[("Origin", "http://page.example")]).await;
        let elapsed = t0.elapsed();
        let head = String::from_utf8_lossy(&resp[..resp.len().min(200)]);
        assert!(head.starts_with("HTTP/1.1 201"), "status: {}", &head[..head.len().min(40)]);
        assert!(head.contains("Content-Type: application/json"));
        assert!(
            head.contains("Access-Control-Allow-Origin: http://page.example"),
            "CORS 回显 Origin"
        );
        assert!(resp.ends_with(b"{\"ok\":true}"), "body 字节精确");
        assert!(elapsed >= Duration::from_millis(100), "延迟生效（{elapsed:?}）");

        // 一次性：同 token 二次请求 → 404
        let resp2 = http_get(port, &format!("/m/{token}"), &[]).await;
        assert!(resp2.starts_with(b"HTTP/1.1 404"), "token 已消费");

        // 未知路径 404
        let resp3 = http_get(port, "/other", &[]).await;
        assert!(resp3.starts_with(b"HTTP/1.1 404"));
    }

    /// 二进制 body 字节精确（base64 通道的 mock 语义）。
    #[tokio::test]
    async fn mock_upstream_binary_body_exact() {
        let upstream = MockUpstream::shared().await.unwrap();
        let bin: Vec<u8> = vec![0, 1, 2, 255, 254, 0xDE, 0xAD];
        let url = upstream.register(MockSpec {
            status: 200,
            headers: vec![],
            body: bin.clone(),
            delay_ms: 0,
        });
        let token = url.rsplit('/').next().unwrap();
        let resp = http_get(upstream.port(), &format!("/m/{token}"), &[]).await;
        assert!(resp.ends_with(&bin), "二进制 body 原样字节");
        let head = String::from_utf8_lossy(&resp[..resp.len() - bin.len()]);
        assert!(head.contains(&format!("Content-Length: {}", bin.len())));
    }

    /// CORS preflight：OPTIONS → 204 + 放行头（回显 ACRH）。
    #[tokio::test]
    async fn mock_upstream_answers_preflight() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let upstream = MockUpstream::shared().await.unwrap();
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", upstream.port()))
            .await
            .unwrap();
        s.write_all(
            b"OPTIONS /m/whatever HTTP/1.1\r\nHost: x\r\nOrigin: http://page.example\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: x-custom\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf);
        assert!(
            resp.starts_with("HTTP/1.1 204"),
            "{}",
            &resp[..resp.len().min(40)]
        );
        assert!(resp.contains("Access-Control-Allow-Origin: http://page.example"));
        assert!(resp.contains("Access-Control-Allow-Headers: x-custom"));
    }

    /// 规则头覆盖 CORS 注入（同名 ACAO 显式设置优先）。
    #[tokio::test]
    async fn mock_upstream_rule_headers_override_cors_injection() {
        let upstream = MockUpstream::shared().await.unwrap();
        let url = upstream.register(MockSpec {
            status: 200,
            headers: vec![("Access-Control-Allow-Origin".to_string(), "*".to_string())],
            body: Vec::new(),
            delay_ms: 0,
        });
        let token = url.rsplit('/').next().unwrap();
        let resp = http_get(upstream.port(), &format!("/m/{token}"), &[("Origin", "http://x")]).await;
        let head = String::from_utf8_lossy(&resp);
        assert!(head.contains("Access-Control-Allow-Origin: *"));
        assert!(!head.contains("Access-Control-Allow-Origin: http://x"));
    }

    /// 头值 CR/LF 防御（响应拆分兜底）：值被剔除换行后原样响应。
    #[tokio::test]
    async fn mock_upstream_strips_crlf_in_header_values() {
        let upstream = MockUpstream::shared().await.unwrap();
        let url = upstream.register(MockSpec {
            status: 200,
            headers: vec![("X-Evil".to_string(), "a\r\nX-Injected: 1".to_string())],
            body: Vec::new(),
            delay_ms: 0,
        });
        let token = url.rsplit('/').next().unwrap();
        let resp = http_get(upstream.port(), &format!("/m/{token}"), &[]).await;
        let head = String::from_utf8_lossy(&resp);
        assert!(head.contains("X-Evil: aX-Injected: 1"), "换行剔除后并入值: {head}");
        // 无响应拆分：注入内容不构成独立头行（无 CRLF 前导的 X-Injected 头）
        assert!(!head.contains("\r\nX-Injected:"), "无独立注入头行");
    }
}
