use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddr};

use crate::error::AppError;

pub const START_PORT: u16 = 9222;
const MAX_ATTEMPTS: u16 = 200;

/// 端口分配测试串行锁：测试真实绑定端口，并行会互相干扰
#[cfg(test)]
static PORT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 本机端口是否可绑定（bind 探测，用完即关）。
/// 带 SO_REUSEADDR：排除 TIME_WAIT 连接的误报（Chrome 的 CDP server 自身也带
/// reuseaddr，能正常启动；不带的话 stop 后立即 start 会误报占用）。
pub async fn is_free(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let socket = match socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    ) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = socket.set_reuse_address(true);
    if socket.bind(&addr.into()).is_err() {
        return false;
    }
    socket.listen(1).is_ok()
}

/// 从 start 起找第一个可用 CDP 端口。
/// 可用 = 本机未监听 且 不在 used 集合（DB 已登记端口，含已停止实例）。
/// 生产从 9222 起；测试传入高位端口避开真实环境占用。
pub async fn allocate(start: u16, used: &BTreeSet<u16>) -> Result<u16, AppError> {
    for offset in 0..MAX_ATTEMPTS {
        let port = start.saturating_add(offset);
        if port == 0 || used.contains(&port) {
            continue;
        }
        if is_free(port).await {
            return Ok(port);
        }
    }
    Err(AppError::business(
        500,
        "CDP_PORT_UNAVAILABLE",
        format!("未能找到可用的调试端口（{start} 起连续 {MAX_ATTEMPTS} 个均不可用）"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn skips_used_and_occupied_ports() {
        let _guard = PORT_TEST_LOCK.lock().unwrap();
        // 占住 29222，把 29223 放进 used → 应跳到 29224。
        // 短重试：上一测试刚释放 29222 的 listening socket 时，macOS 内核回收存在
        // 瞬态窗口（EADDRINUse 且无实际持有者），100ms 内消散（见上方同款注释）
        let listener = {
            let mut l = None;
            for _ in 0..5 {
                if let Ok(x) = tokio::net::TcpListener::bind(("127.0.0.1", 29222)).await {
                    l = Some(x);
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            l.expect("29222 在 100ms 重试窗口内持续不可绑定")
        };
        let mut used = BTreeSet::new();
        used.insert(29223u16);

        let port = allocate(29222, &used).await.unwrap();
        assert_eq!(port, 29224);

        drop(listener);
    }

    #[tokio::test]
    async fn returns_a_bindable_port_when_nothing_reserved() {
        let _guard = PORT_TEST_LOCK.lock().unwrap();
        let port = allocate(29222, &BTreeSet::new()).await.unwrap();
        // 分配结果必须是可绑定端口
        assert!(port >= 29222);
        // probe（socket2 drop）→ rebind 之间存在 macOS 内核级瞬态 EADDRINUSE
        // （无实际持有者，并行测试的 fork/exec 负载会放大该窗口）。
        // 短重试窗口内消散即可视为可绑定；分配语义的验证不受影响。
        let bindable = {
            let mut ok = false;
            for _ in 0..5 {
                if tokio::net::TcpListener::bind(("127.0.0.1", port)).await.is_ok() {
                    ok = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            ok
        };
        assert!(bindable, "分配端口 {port} 在 100ms 重试窗口内均不可绑定");
    }
}
