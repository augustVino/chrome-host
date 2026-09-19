//! 远程接入隧道监督器：单条静态 SSH 反向转发（目标机 17890 → 本机 17891）。
//!
//! 设计依据 REMOTE-ACCESS-PLAN.md §5：
//! - **守尸包裹**：ssh 经 `/bin/sh` 包裹 spawn，stdin 是 socketpair（app 持另一端）。
//!   app 无论怎么死（含 SIGKILL），内核关闭其 fd → 包裹层 `cat` 读到 EOF → kill ssh
//!   → 孤儿不产生。这是内核 fd 生命周期保证，与信号、清理代码无关。
//! - **状态机** disabled / connecting / connected / reconnecting，退避 3/6/12/30s 无限重试。
//! - **判活推理**：BatchMode（无交互挂起）+ ConnectTimeout（无 TCP 挂起）+
//!   ExitOnForwardFailure（无半通）⇒ 子进程存活 >20s 即 established。
//! - **启动对账**：spawn 前 pgrep 特征串清理孤儿（守尸机制之外的人祸兜底）。
//! - **stale 自愈**：stderr 命中 bind 失败特征 → 一次性 ssh 会话在远端杀 stale sshd。
//!
//! spawn 技术样板（进程组 / stderr 排水 / TERM→KILL）取自
//! `infrastructure/process/manager.rs` 平台实现段；不走 ProcessManager trait
//! （该 trait 为 Chrome 专属：LaunchSpec 携带 user-data-dir / cdp-port）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::Notify;

/// 远端转发端口（目标机侧监听）——与本地 Agent API 同号，远程客户端零配置。
pub const REMOTE_PORT: u16 = 17890;
/// 隧道落地监听端口（本机鉴权监听器，见 api::server）。
pub const LOCAL_LANDING_PORT: u16 = 17891;
/// `-R` 转发规格：既是 ssh 参数，也是孤儿对账的 pgrep 特征串。
pub const FORWARD_SPEC: &str = "17890:127.0.0.1:17891";

/// ssh 命令行参数（除 `-R` 与 target 外的全部），逐条依据见方案 §5.1。
/// `-F <净化配置>`：指向应用数据目录里剔除转发行后的用户配置副本——
/// ① 用户 ~/.ssh/config 的 RemoteForward（如代理转发 7890）不进入应用隧道，
///    其 bind 失败不会在 ExitOnForwardFailure 下杀死本隧道（实测踩坑 A）；
/// ② 别名/HostName/IdentityFile/ProxyJump 解析完整保留，目标仍可写 "vino@yun"。
/// 注意不能用 ClearAllForwardings：它连命令行 -R 一并清除（实测踩坑 B），
/// 隧道会零转发却零报错地"假活"。
pub const SSH_OPTIONS: &[&str] = &[
    "-N",
    "-o",
    "ExitOnForwardFailure=yes",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=30",
    "-o",
    "ServerAliveCountMax=3",
    "-o",
    "StrictHostKeyChecking=accept-new",
];

/// 子进程存活该时长即判 established（2×ConnectTimeout 裕量，方案 §5.1 判活推理）。
const CONNECTED_GRACE_MS: i64 = 20_000;
/// 监控轮询间隔。
const MONITOR_TICK: Duration = Duration::from_millis(500);
/// 重连退避序列（封顶 30s）。
const BACKOFF_SECS: [u64; 4] = [3, 6, 12, 30];
/// stderr 环形缓冲上限（bind 失败等诊断信息足够）。
const STDERR_RING_CAP: usize = 4096;
/// stderr 中 bind 失败的特征串（触发远端 stale 自愈）。
const STALE_SIGNATURE: &str = "remote port forwarding failed for listen port";

/// 守尸包裹脚本（两次实测修正后的终版）：
/// - `exec 3<&0` 先备份 stdin：POSIX sh 对后台任务会把 stdin 重定向为 /dev/null，
///   EOF 观察子壳必须显式读 fd3 才能看到真实管道（否则秒读 EOF 误杀）；
/// - `( cat <&3; kill $p ) &`：stdin EOF（app 死亡）→ kill ssh（孤儿防护）；
/// - `wait $p`：ssh 死亡（任何原因）→ sh 随即退出（监督器 try_wait 秒级感知；
///   旧版 cat 在前台阻塞会掩盖 ssh 死亡，状态机盲停在 connected）。
/// 双向监控任一先发生均正确收敛。
const WRAPPER_SCRIPT: &str =
    "exec 3<&0; \"$@\" & p=$!; ( cat <&3 >/dev/null; kill $p ) & c=$!; wait $p; kill $c 2>/dev/null; wait $c 2>/dev/null";

/// 状态变更回调（TunnelService 落 Activity 审计；测试注入收集器）。
pub type EventSink = Arc<dyn Fn(&str) + Send + Sync>;

/// 对外状态快照（GET /api/v1/remote-access/status 响应体，camelCase）。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TunnelStatus {
    pub enabled: bool,
    pub target: String,
    /// disabled | connecting | connected | reconnecting
    pub state: String,
    pub last_error: Option<String>,
    /// 进入当前 state 的时刻（epoch ms）
    pub since: i64,
    pub pid: Option<u32>,
    /// 重连次数（本次 enabled 会话累计）
    pub restarts: u32,
}

#[derive(Debug)]
struct Current {
    state: String,
    last_error: Option<String>,
    since: i64,
    pid: Option<u32>,
    restarts: u32,
}

#[derive(Debug, Clone, PartialEq)]
struct Desired {
    enabled: bool,
    target: String,
    /// 配置代数：变化即触发子进程重启（同值 apply 不 bump，幂等）
    generation: u64,
}

struct Shared {
    desired: Mutex<Desired>,
    current: Mutex<Current>,
    /// 进程组 id（= 包裹层 sh 的 pid，process_group(0) 使其为组长）
    pgid: Mutex<Option<u32>>,
    /// socketpair 的 app 侧：持有即存活前提；drop 触发包裹层 EOF 杀链
    pipe: Mutex<Option<std::os::unix::net::UnixStream>>,
    shutdown: AtomicBool,
    stderr_ring: Mutex<Vec<u8>>,
    /// 守护的子进程程序名（生产 "ssh"；测试注入 sleep 等）
    child_program: String,
    /// 应用数据目录（净化配置写入位）；None = 不净化（测试）
    data_dir: Option<std::path::PathBuf>,
}

fn new_shared(child_program: &str) -> Arc<Shared> {
    new_shared_with_dir(child_program, None)
}

fn new_shared_with_dir(child_program: &str, data_dir: Option<std::path::PathBuf>) -> Arc<Shared> {
    Arc::new(Shared {
        desired: Mutex::new(Desired { enabled: false, target: String::new(), generation: 0 }),
        current: Mutex::new(Current {
            state: "disabled".into(),
            last_error: None,
            since: chrono::Utc::now().timestamp_millis(),
            pid: None,
            restarts: 0,
        }),
        pgid: Mutex::new(None),
        pipe: Mutex::new(None),
        shutdown: AtomicBool::new(false),
        stderr_ring: Mutex::new(Vec::new()),
        child_program: child_program.to_string(),
        data_dir,
    })
}

/// 组装 ssh 参数（不含程序名；target 恒为最后一个参数）。纯函数便于单测。
/// `config`：净化配置路径（Some 时插入 `-F <path>`，位于参数首部）。
pub fn build_ssh_args(target: &str, config: Option<&std::path::Path>) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(path) = config {
        args.push("-F".into());
        args.push(path.to_string_lossy().into_owned());
    }
    args.extend(SSH_OPTIONS.iter().map(|s| s.to_string()));
    args.push("-R".into());
    args.push(FORWARD_SPEC.into());
    args.push(target.to_string());
    args
}

/// 剔除用户 ssh 配置中的全部转发指令行（Remote/Local/Dynamic Forward，
/// 大小写不敏感、容忍前导空白）。纯函数便于单测。
fn strip_forward_lines(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let first = line.split_whitespace().next().unwrap_or("");
            !(first.eq_ignore_ascii_case("RemoteForward")
                || first.eq_ignore_ascii_case("LocalForward")
                || first.eq_ignore_ascii_case("DynamicForward"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 生成/刷新净化配置副本（每次 spawn 前调用，跟进用户编辑）：
/// ~/.ssh/config → <data_dir>/ssh-tunnel-config（0600）。
/// 无用户配置或写入失败 → None（降级直用用户配置：仅当其中无冲突转发时可用，
/// 记 warn）。Include 指令引入的转发不在此净化范围（已知边界，方案 §11）。
fn refresh_tunnel_config(data_dir: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    let dir = data_dir?;
    let home = std::env::var("HOME").ok()?;
    let source = std::path::Path::new(&home).join(".ssh").join("config");
    let text = std::fs::read_to_string(&source).ok()?;
    let dest = std::path::Path::new(dir).join("ssh-tunnel-config");
    let filtered = strip_forward_lines(&text);
    if std::fs::write(&dest, filtered).is_err() {
        tracing::warn!("[tunnel] 净化配置写入失败，降级直用用户 ssh 配置");
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600));
    }
    Some(dest)
}

// ---------------------------------------------------------------------------
// 服务门面
// ---------------------------------------------------------------------------

pub struct TunnelService {
    shared: Arc<Shared>,
    notify: Arc<Notify>,
    sink: EventSink,
    supervisor_started: AtomicBool,
}

impl TunnelService {
    /// 生产构造：状态迁移落 Activity 审计流；data_dir 用于净化 ssh 配置副本。
    pub fn new(activity: Arc<super::activity::Activity>, data_dir: std::path::PathBuf) -> Self {
        let sink: EventSink = Arc::new(move |msg: &str| {
            // 事件名仅允许字母数字与 - / : : _（Tauri 校验），用下划线风格（同 env_alive 先例）
            activity.record("info", "tunnel_state_change", None, None, msg);
        });
        TunnelService {
            shared: new_shared_with_dir("ssh", Some(data_dir)),
            notify: Arc::new(Notify::new()),
            sink,
            supervisor_started: AtomicBool::new(false),
        }
    }

    /// 注入自定义事件回调的构造（测试/诊断用：无 Activity 依赖）
    pub fn with_sink(sink: EventSink) -> Self {
        TunnelService {
            shared: new_shared("ssh"),
            notify: Arc::new(Notify::new()),
            sink,
            supervisor_started: AtomicBool::new(false),
        }
    }

    /// 应用目标配置（幂等收敛）：同值 no-op；变化 → 杀旧子进程按新配置重启。
    /// 调用点：main.rs 启动装配、api::settings PUT 成功后（全项目唯一设置写路径的薄壳编排）。
    pub fn apply(&self, enabled: bool, target: &str) {
        let wake = {
            let mut d = self.shared.desired.lock().unwrap();
            if d.enabled == enabled && d.target == target {
                false
            } else {
                d.enabled = enabled;
                d.target = target.to_string();
                d.generation += 1;
                true
            }
        };
        if enabled
            && !self.supervisor_started.swap(true, Ordering::SeqCst)
        {
            tauri::async_runtime::spawn(supervise(
                self.shared.clone(),
                self.notify.clone(),
                self.sink.clone(),
            ));
        }
        if wake {
            self.notify.notify_one();
        }
    }

    /// 状态快照（status 端点 / health 检查共用）。
    pub fn status(&self) -> TunnelStatus {
        let d = self.shared.desired.lock().unwrap().clone();
        let c = self.shared.current.lock().unwrap();
        TunnelStatus {
            enabled: d.enabled,
            target: d.target,
            state: c.state.clone(),
            last_error: c.last_error.clone(),
            since: c.since,
            pid: c.pid,
            restarts: c.restarts,
        }
    }

    /// 应用退出路径（RunEvent::ExitRequested）：置停机标记并**同步**杀进程组。
    /// 不等异步任务调度——退出窗口内监督任务可能不再被调度。
    pub fn shutdown(&self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        self.notify.notify_one();
        let pgid = *self.shared.pgid.lock().unwrap();
        if let Some(p) = pgid {
            signal_group(p as i32, libc::SIGTERM);
            std::thread::sleep(Duration::from_millis(200));
            signal_group(p as i32, libc::SIGKILL);
        }
        *self.shared.pipe.lock().unwrap() = None;
        let mut c = self.shared.current.lock().unwrap();
        c.state = "disabled".into();
        c.pid = None;
    }
}

// ---------------------------------------------------------------------------
// 监督循环
// ---------------------------------------------------------------------------

/// 监督主循环：独立于 TunnelService 生命周期（持 Shared/Notify/EventSink 克隆），
/// 测试可直接以 tokio 驱动（绕过 tauri runtime）。
async fn supervise(shared: Arc<Shared>, notify: Arc<Notify>, on_event: EventSink) {
    let mut last_gen: u64 = 0;
    // 已应用的配置代数：区分「配置驱动的主动重启」（退避清零）与
    // 「崩溃重生」（退避持续累计）——否则 3/6/12/30 序列永远停在第一档
    let mut config_seen: u64 = 0;
    let mut backoff_idx: usize = 0;
    let mut child: Option<std::process::Child> = None;

    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let desired = shared.desired.lock().unwrap().clone();

        if !desired.enabled {
            if let Some(mut c) = child.take() {
                terminate_child(&shared, &mut c);
            } else {
                // 无子进程（含从未启动）：仅收敛状态
                kill_tracked(&shared);
            }
            set_state(&shared, &on_event, "disabled", None, None, false);
            // 等待下一次配置/停机信号（Notify 许可语义：先 notify 后 await 亦即时唤醒）
            notify.notified().await;
            continue;
        }

        // 配置变化（含退出后重生）：杀旧 → 对账 → 起新
        if desired.generation != last_gen {
            if let Some(mut c) = child.take() {
                terminate_child(&shared, &mut c);
            }
            kill_tracked(&shared);
            reconcile_orphans(&shared);
            let program = shared.child_program.clone();
            // 每次 spawn 前刷新净化配置（剔除用户 config 的转发指令，见模块注释）
            let config = refresh_tunnel_config(shared.data_dir.as_deref());
            match spawn_wrapped(&shared, &program, &build_ssh_args(&desired.target, config.as_deref())) {
                Ok(c) => {
                    let pid = c.id();
                    child = Some(c);
                    set_state(&shared, &on_event, "connecting", Some(pid), None, false);
                    last_gen = desired.generation;
                    if desired.generation != config_seen {
                        config_seen = desired.generation;
                        backoff_idx = 0; // 主动（配置驱动）重启不累计退避
                    }
                }
                Err(e) => {
                    set_state(
                        &shared,
                        &on_event,
                        "reconnecting",
                        None,
                        Some(format!("ssh 启动失败: {e}")),
                        true,
                    );
                    backoff_sleep(&notify, BACKOFF_SECS[backoff_idx]).await;
                    backoff_idx = (backoff_idx + 1).min(BACKOFF_SECS.len() - 1);
                    last_gen = 0; // 退避后强制重生
                    continue;
                }
            }
        }

        // 监控 tick：退出检测 / established 判定 / 配置变化响应
        tokio::select! {
            _ = notify.notified() => { /* 回到循环顶重评估（shutdown / 配置变化） */ }
            _ = tokio::time::sleep(MONITOR_TICK) => {
                let Some(c) = child.as_mut() else { last_gen = 0; continue };
                match c.try_wait() {
                    Ok(Some(_)) => {
                        child = None;
                        kill_tracked(&shared); // 清 pgid/pipe 登记
                        let tail = take_stderr_tail(&shared);
                        let err = if tail.is_empty() {
                            format!("ssh 进程退出（目标 {}）", desired.target)
                        } else {
                            tail.clone()
                        };
                        set_state(&shared, &on_event, "reconnecting", None, Some(err), true);
                        if tail.contains(STALE_SIGNATURE) {
                            remote_stale_cleanup(&desired.target);
                        }
                        backoff_sleep(&notify, BACKOFF_SECS[backoff_idx]).await;
                        backoff_idx = (backoff_idx + 1).min(BACKOFF_SECS.len() - 1);
                        last_gen = 0; // 退避后强制重生
                    }
                    Ok(None) => {
                        // 存活：connecting → connected（存活超过 CONNECTED_GRACE_MS）
                        let mut cur = shared.current.lock().unwrap();
                        if cur.state == "connecting"
                            && chrono::Utc::now().timestamp_millis() - cur.since >= CONNECTED_GRACE_MS
                        {
                            cur.state = "connected".into();
                            cur.last_error = None;
                            cur.pid = child.as_ref().map(|c| c.id());
                            drop(cur);
                            let msg = format!(
                                "隧道已建立: {} → 本机 127.0.0.1:{LOCAL_LANDING_PORT}",
                                desired.target
                            );
                            tracing::info!("[tunnel] {msg}");
                            on_event(&msg);
                        }
                    }
                    Err(e) => {
                        child = None;
                        kill_tracked(&shared);
                        set_state(
                            &shared,
                            &on_event,
                            "reconnecting",
                            None,
                            Some(format!("ssh 状态探测失败: {e}")),
                            true,
                        );
                        backoff_sleep(&notify, BACKOFF_SECS[backoff_idx]).await;
                        backoff_idx = (backoff_idx + 1).min(BACKOFF_SECS.len() - 1);
                        last_gen = 0;
                    }
                }
            }
        }
    }

    // 退出清理：确保无残留
    if let Some(mut c) = child.take() {
        terminate_child(&shared, &mut c);
    } else {
        kill_tracked(&shared);
    }
    set_state(&shared, &on_event, "disabled", None, None, false);
}

/// 退避等待（可被配置变化/停机信号打断，避免改配置要等满 30s）。
async fn backoff_sleep(notify: &Notify, secs: u64) {
    tokio::select! {
        _ = notify.notified() => {}
        _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
    }
}

// ---------------------------------------------------------------------------
// 子进程管理（守尸包裹 / 对账 / 自愈）
// ---------------------------------------------------------------------------

/// 以守尸包裹 spawn：`sh -c WRAPPER_SCRIPT proxy <program> <args...>`，
/// stdin 为 socketpair（app 侧持有另一端）。失败时管道与 pgid 不登记。
fn spawn_wrapped(
    shared: &Arc<Shared>,
    program: &str,
    args: &[String],
) -> Result<std::process::Child, String> {
    #[cfg(unix)]
    let (app_end, child_end) =
        std::os::unix::net::UnixStream::pair().map_err(|e| format!("socketpair 失败: {e}"))?;

    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(WRAPPER_SCRIPT)
        .arg("proxy")
        .arg(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // socketpair 作为 stdin：app 死亡 → 内核关闭 app 侧 fd → 包裹层 EOF → 杀 ssh。
        // UnixStream → OwnedFd → Stdio（std 无直接 From<UnixStream>）
        let fd: std::os::fd::OwnedFd = child_end.into();
        cmd.stdin(std::process::Stdio::from(fd));
        cmd.process_group(0); // 独立进程组：TERM/KILL 整组回收
    }
    #[cfg(not(unix))]
    {
        cmd.stdin(std::process::Stdio::null());
        tracing::warn!("[tunnel] 当前平台无守尸包裹（socketpair 不可用），孤儿防护降级");
    }

    let mut child = cmd.spawn().map_err(|e| format!("spawn 失败: {e}"))?;

    let pid = child.id();
    *shared.pgid.lock().unwrap() = Some(pid);
    #[cfg(unix)]
    {
        *shared.pipe.lock().unwrap() = Some(app_end);
    }
    shared.stderr_ring.lock().unwrap().clear();

    // stderr 排水线程：环形缓冲保留最近诊断信息（bind 失败/auth 失败都在 stderr）
    if let Some(stderr) = child.stderr.take() {
        let shared = Arc::clone(shared);
        std::thread::Builder::new()
            .name("tunnel-stderr".into())
            .spawn(move || {
                use std::io::Read;
                let mut reader = stderr;
                let mut buf = [0u8; 512];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut ring = shared.stderr_ring.lock().unwrap();
                            if ring.len() + n > STDERR_RING_CAP {
                                let overflow = ring.len() + n - STDERR_RING_CAP;
                                ring.drain(..overflow);
                            }
                            ring.extend_from_slice(&buf[..n]);
                        }
                    }
                }
            })
            .map_err(|e| format!("stderr 排水线程启动失败: {e}"))?;
    }
    tracing::info!("[tunnel] ssh 已启动: pid={pid} target 见状态");
    Ok(child)
}

/// 终止受管子进程：TERM 整组 → wait 收割包裹层 → KILL 残余组员兜底 → 清登记。
fn terminate_child(shared: &Shared, child: &mut std::process::Child) {
    let pgid = *shared.pgid.lock().unwrap();
    if let Some(p) = pgid {
        signal_group(p as i32, libc::SIGTERM);
    }
    let _ = child.wait(); // TERM 后组员（cat/ssh）退出，sh 收尾随即退出
    if let Some(p) = pgid {
        signal_group(p as i32, libc::SIGKILL);
    }
    kill_tracked(shared);
}

/// 清理 pgid / pipe 登记（进程已死路径共用）。
fn kill_tracked(shared: &Shared) {
    *shared.pgid.lock().unwrap() = None;
    *shared.pipe.lock().unwrap() = None;
}

/// 取走 stderr 环形缓冲的尾部诊断文本（并清空）。
fn take_stderr_tail(shared: &Shared) -> String {
    let mut ring = shared.stderr_ring.lock().unwrap();
    let text = String::from_utf8_lossy(&ring).trim().to_string();
    // 仅保留最后 ~500 字符：诊断够用，避免 lastError 过长撑爆状态响应
    let tail = if text.len() > 500 {
        text[text.len() - 500..].to_string()
    } else {
        text
    };
    ring.clear();
    tail
}

/// 启动对账：清理特征串命中的孤儿 ssh（守尸包裹之外的人祸兜底）。
/// 逐个校验进程名为 ssh 后才杀——防 cmdline 撞车的无辜进程误杀（方案 §5.4）。
fn reconcile_orphans(shared: &Shared) {
    let _ = shared; // 当前不读 shared；保留参数为将来留痕（事件走 on_event）
    let Ok(out) = std::process::Command::new("pgrep").arg("-f").arg(FORWARD_SPEC).output() else {
        return;
    };
    if !out.status.success() {
        return; // pgrep exit 1 = 无匹配
    }
    let pids = String::from_utf8_lossy(&out.stdout);
    for pid_str in pids.split_whitespace() {
        let Ok(pid) = pid_str.parse::<u32>() else { continue };
        // 进程名校验：仅杀确证为 ssh 的进程
        let Ok(comm) = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
        else {
            continue;
        };
        if String::from_utf8_lossy(&comm.stdout).trim() != "ssh" {
            continue;
        }
        tracing::warn!("[tunnel] 对账清理孤儿 ssh: pid={pid}");
        terminate_by_pid(pid);
    }
}

/// 单进程 TERM → ≤1s 轮询 → KILL（manager.rs terminate 同款节拍）。
fn terminate_by_pid(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    for _ in 0..10 {
        if !alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status();
}

fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// stale listener 自愈（方案 §5.4，仅 bind 失败特征触发）：
/// 经一次性 ssh 会话在远端定位占用 17890 的进程，**确证为 sshd 才杀**，
/// 否则不乱动（占用者可能是目标机上的真实服务）。不引入常驻远端组件。
fn remote_stale_cleanup(target: &str) {
    let script = "pid=$(ss -tlnp 2>/dev/null | grep ':17890 ' | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2); \
                  if [ -n \"$pid\" ] && [ \"$(ps -p \"$pid\" -o comm= 2>/dev/null)\" = \"sshd\" ]; then \
                  kill \"$pid\" && echo KILLED $pid; else echo NOOP; fi";
    tracing::warn!("[tunnel] 检测到远端端口占用（bind 失败），尝试自愈清理 stale sshd: {target}");
    let out = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
        .arg(target)
        .arg(script)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            tracing::info!(
                "[tunnel] 远端自愈结果: {}",
                String::from_utf8_lossy(&o.stdout).trim()
            );
        }
        Ok(o) => {
            tracing::warn!(
                "[tunnel] 远端自愈失败: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
        }
        Err(e) => tracing::warn!("[tunnel] 远端自愈会话建立失败: {e}"),
    }
}

#[cfg(unix)]
fn signal_group(pgid: i32, sig: libc::c_int) {
    unsafe {
        libc::kill(-pgid, sig);
    }
}
#[cfg(not(unix))]
fn signal_group(_pgid: i32, _sig: i32) {}

/// 状态迁移 + 事件留痕（状态不变时不重复记录）。
fn set_state(
    shared: &Shared,
    on_event: &EventSink,
    state: &str,
    pid: Option<u32>,
    error: Option<String>,
    inc_restarts: bool,
) {
    let prev = {
        let mut c = shared.current.lock().unwrap();
        let prev = c.state.clone();
        c.state = state.to_string();
        c.since = chrono::Utc::now().timestamp_millis();
        c.pid = pid;
        c.last_error = error;
        if inc_restarts {
            c.restarts += 1;
        }
        prev
    };
    if prev != state {
        let msg = format!("隧道状态: {prev} → {state}");
        tracing::info!("[tunnel] {msg}");
        on_event(&msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_args_shape() {
        let args = build_ssh_args("vino@yun", None);
        assert_eq!(args.last().unwrap(), "vino@yun", "target 恒为末位");
        assert!(args.contains(&"-N".to_string()));
        assert!(!args.iter().any(|a| a.contains("ClearAllForwardings")),
            "ClearAllForwardings 会连带清除命令行 -R（实测踩坑 B），禁止使用");
        assert_eq!(
            args.iter().filter(|a| *a == "-R").count(),
            1,
            "仅一条转发规则"
        );
        let r = args.iter().position(|a| a == "-R").unwrap();
        assert_eq!(
            args[r + 1],
            format!("{REMOTE_PORT}:127.0.0.1:{LOCAL_LANDING_PORT}"),
            "转发规格与端口常量一致（防漂移）"
        );
        for opt in [
            "ExitOnForwardFailure=yes",
            "BatchMode=yes",
            "ConnectTimeout=10",
            "ServerAliveInterval=30",
            "ServerAliveCountMax=3",
            "StrictHostKeyChecking=accept-new",
        ] {
            assert!(args.contains(&opt.to_string()), "缺少 -o {opt}");
        }

        // 净化配置：-F 位于参数首部，路径原样透传
        let with_cfg = build_ssh_args("t", Some(std::path::Path::new("/tmp/ssh-tunnel-config")));
        assert_eq!(with_cfg[0], "-F");
        assert_eq!(with_cfg[1], "/tmp/ssh-tunnel-config");
    }

    /// 净化函数：转发指令行（三种、大小写不敏感、容忍缩进）剔除，其余原样保留
    #[test]
    fn strip_forward_lines_removes_only_forwards() {
        let src = "Host yun\n  HostName 10.0.0.1\n  RemoteForward 7890 127.0.0.1:7890\n  remoteforward 9222 x\n  LocalForward 1 2\n  DynamicForward 1080\n  IdentityFile ~/.ssh/k\n";
        let out = strip_forward_lines(src);
        assert!(out.contains("Host yun"));
        assert!(out.contains("HostName 10.0.0.1"));
        assert!(out.contains("IdentityFile"));
        assert!(!out.to_lowercase().contains("forward 7890"));
        assert!(!out.to_lowercase().contains("forward 9222"));
        assert!(!out.contains("LocalForward"));
        assert!(!out.contains("DynamicForward"));
    }

    /// 守尸机制核心验证：socketpair app 侧 drop（模拟 app 死亡）→ 包裹层秒级杀子进程。
    /// 用 sleep 替身验证包裹层语义本身，不依赖真实 ssh。
    #[test]
    fn wrapper_kills_child_on_pipe_drop() {
        let shared = new_shared("sleep");
        let mut child =
            spawn_wrapped(&shared, "sleep", &["30".to_string()]).expect("spawn 守尸包裹");
        assert!(shared.pipe.lock().unwrap().is_some(), "app 侧持有管道");
        std::thread::sleep(Duration::from_millis(300));
        assert!(child.try_wait().unwrap().is_none(), "管道在持时子进程存活");

        // 模拟 app 死亡：内核关闭 app 侧 fd → EOF → 包裹层 kill
        *shared.pipe.lock().unwrap() = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(child.try_wait().unwrap().is_some(), "EOF 后子进程应死亡（无孤儿）");
    }

    /// 监督可见性（实测踩坑回归）：子进程自然死亡 → 包裹层应随之退出，
    /// 否则 try_wait 盲区导致状态机停在 connected（旧版脚本 cat 前台阻塞的缺陷）。
    #[test]
    fn wrapper_exits_when_child_dies() {
        let shared = new_shared("sleep");
        let mut child =
            spawn_wrapped(&shared, "sleep", &["0.3".to_string()]).expect("spawn");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "子进程死亡后包裹层应退出（监督可见性）"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // app 侧管道仍在持有，但包裹层已退出——正常，EOF 观察者已被清理
    }

    /// 对账安全边界：特征串命中但进程名非 ssh → 不杀（防 cmdline 撞车误杀）。
    #[test]
    fn reconcile_skips_non_ssh_process() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 5", FORWARD_SPEC]) // 特征串在 argv → pgrep -f 命中
            .process_group(0)
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        reconcile_orphans(&new_shared("ssh"));
        let survived = child.try_wait().unwrap().is_none();
        let _ = child.kill();
        let _ = child.wait();
        assert!(survived, "非 ssh 进程（sh）不应被对账清理");
    }

    /// stderr 尾部截取：超长仅保留末段，取后清空。
    #[test]
    fn stderr_tail_truncates_and_clears() {
        let shared = new_shared("ssh");
        shared.stderr_ring.lock().unwrap().extend_from_slice(&vec![b'a'; 2000]);
        let tail = take_stderr_tail(&shared);
        assert!(tail.len() <= 500, "截取后不超过 500 字符");
        assert!(shared.stderr_ring.lock().unwrap().is_empty(), "取后清空");
    }
}
