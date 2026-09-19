# REMOTE-ACCESS-PLAN.md — 「远程接入」实施方案（v2，网关架构）

> 状态：**方案已确认，待实施**。本版按最终确认的「鉴权网关 + CDP 会话代理」架构整体重写，
> 取代早期草案（端口整段枚举转发 / 动态按端口转发两个方向的讨论仅保留为决策记录，见 §2）。
> 撰写基准：v0.1.x 实际代码 + 本机 OpenSSH_10.0p2 实测（见附录 C）。

---

## 目录

1. [目标与方案结论](#1-目标与方案结论)
2. [市场验证与决策记录](#2-市场验证与决策记录)
3. [架构与信任模型](#3-架构与信任模型)
4. [变更清单（按代码分层）](#4-变更清单按代码分层)
5. [TunnelService 规格（隧道层）](#5-tunnelservice-规格隧道层)
6. [鉴权网关规格（token 层）](#6-鉴权网关规格token-层)
7. [CDP 会话代理规格（数据面）](#7-cdp-会话代理规格数据面)
8. [API / CLI / MCP 契约变更](#8-api--cli--mcp-契约变更)
9. [UI 规格](#9-ui-规格)
10. [目标机器一次性配置](#10-目标机器一次性配置)
11. [风险与已知边界](#11-风险与已知边界)
12. [非目标（V1 明确不做）](#12-非目标v1-明确不做)
13. [测试与验收](#13-测试与验收)
14. [实施顺序](#14-实施顺序)

---

## 1. 目标与方案结论

**采用「单端口 SSH 隧道 + 鉴权网关 + CDP 会话代理」三层架构。**

```
┌────────────────── 远程机器（云桌面 yun）──────────────────┐
│ AI Agent（CLI / MCP / REST 客户端）                        │
│   控制面：http://127.0.0.1:17890/api/v1 + Bearer token     │
│   MCP：  http://127.0.0.1:17890/mcp   + Bearer token       │
│   CDP：  ws://127.0.0.1:17890/cdp/<ins>/<会话Key>/devtools/…│
└───────────────────────────┬───────────────────────────────┘
                            │ 唯一一条 ssh -R（app 守护，恒静态）
┌────────────── 本地 Mac ───▼───────────────────────────────┐
│ 17891 隧道落地监听器：同一 axum 应用 + token 中间件（全路由）│
│ 17890 本地监听器：  现状不变，GUI / 本地 CLI / 本地 MCP 免鉴权│
│ CDP 会话代理：/cdp/{ins}/{sid}/json*（重写 WS URL）          │
│              /cdp/{ins}/{sid}/devtools/*（WS 桥接实例端口）  │
└───────────────────────────────────────────────────────────┘
```

核心性质：

- **唯一转发端口**：隧道只含 `-R 17890:127.0.0.1:17891` 一条静态规则，无任何动态端口状态；
- **CDP 端口零转发**：数据面是 17890 上的反向代理，实例的真实端口不出本机；
- **鉴权是围栏不是门禁**：远程请求全部过凭证校验——REST/MCP 验 Bearer token，
  CDP 代理路径（`/cdp/*`）验 URL 内会话 key（WS 升级请求无法携带自定义 header，
  会话即该层凭证，作用域 = 单实例 + TTL）；
- **本地零回归**：17890 监听器与四个门面（GUI/REST/MCP/CLI）的本地用法完全不变。

稳态工作流（远程 agent 全自主；用户仅在 Mac 本地点击登录框与 Chrome 授权框）：

```
chrome-host status（token）→ env list/create → instance create →
instance cdp-session <id> → CDP_BASE=<baseUrl> cdp.mjs list/shot/eval → 收尾清理
```

---

## 2. 市场验证与决策记录

### 2.1 市场同类产品的收敛模式

「远程程序接管浏览器」在 agent 基础设施领域已是成熟赛道，模式高度一致：

| 产品 | 形态 | 远程 CDP 暴露方式 |
|---|---|---|
| Playwright `launchServer` | 自托管 browser server | 单一 WS 端点，路径内嵌随机 GUID（不可猜测即凭证） |
| Browserbase | 云端浏览器平台 | CDP WebSocket 走其网关，API key 鉴权 |
| Steel | 自托管 agent 浏览器平台 | `cdpUrl` 指向自身 API 网关，API key 鉴权 |
| browserless | 浏览器即服务 | 连接 URL 内嵌 token |
| Selenium Grid 4 | 自托管网格 | 一切经 Hub 的 HTTP/WS 端点，可开 Basic auth |

共同点：**单一鉴权网关，凭证校验在网关层，没有任何一家「鉴权后开放裸端口」**。
原因：CDP 端口 = 浏览器完整控制权（任意 JS 执行、以浏览器身份访问全部已登录站点），
凭证必须管住每一次连接而非端口的诞生。Chrome 官方同向收紧（M136 起默认 profile 拒绝
`--remote-debugging-port`、新版首次 CDP 访问弹授权框）——裸 CDP 端口已被定性为不可信攻击面。

### 2.2 决策记录（被否决的路线，实施时不重议）

| 路线 | 否决理由 |
|---|---|
| **A. 手工 LaunchAgent + 静态转发** | 用户需手工维护、易误杀；隧道生命周期与应用脱节 |
| **B. 端口分配域全量枚举转发**（200/400 个 `-R`） | 实测可行（附录 C）但本质是「无差别暴露」：无鉴权、全有或全无 bind、与安全目标冲突；被网关架构整体取代 |
| **C. 鉴权后动态转发该实例端口**（ControlMaster + `ssh -O forward/cancel`） | 实测可行（附录 C）但存在安全破绽：鉴权只把守**转发的建立**，端口一旦 bind 到目标机 loopback，当地任何进程无需凭证即可连（门禁非围栏）；且要求实例生命周期与隧道跨服务耦合（订阅/重放机制），为代码库引入新的耦合范式 |
| **D. 自建反向连接 + 目标机常驻 relay**（rathole/frp 模式） | 结构上最彻底（心跳级回收），但违反「不引入常驻远端组件」约束，个人工具撑不起自研协议的维护成本 |
| **E. Tailscale 等 Overlay 网络** | 依赖公司网络策略放行；已有 SSH 通道（mutagen 先例）时无增量价值 |

本方案 = 路线 A 的 app 内化（隧道守护）+ 市场收敛的网关模式（B/C 的安全反面教材作为对照保留）。

---

## 3. 架构与信任模型

```
信任边界                        凭证               能力
─────────────────────────────────────────────────────────
Mac 本机进程（GUI/CLI/MCP）  →  无（现状）    →  完整 API + 直连 CDP 端口
持有 token 的远程进程        →  Bearer token  →  完整 API + 会话代理 CDP
目标机上其它进程             →  无            →  只能看到"17890 有人监听"，
                                                   任何请求均 401
```

- 17891 仅绑定 `127.0.0.1`（与 17890 同一纪律），隧道本身受 SSH 密钥认证保护；
- token 是单用户粒度的全权凭证（无分级），见 §12 非目标；
- 会话 key（session）是 token 的下级凭证：仅能访问**单一实例**的 CDP 代理路径、有 TTL；
- 两条既有不变量保持：API 只绑回环；「不要把端口暴露到网络」仍然成立。

关键机制说明（为什么需要第二个监听器）：SSH 转发的请求到达 app 时来源地址是
`127.0.0.1`（本机 ssh 进程代连），TCP 层无法区分本地请求与隧道请求——**监听端口是唯一
判别器**。故 17890（本地，免鉴权）与 17891（隧道落地，强制 token）分流。

---

## 4. 变更清单（按代码分层）

| 新组件 | 落点 | 挂靠的既有模式 |
|---|---|---|
| TunnelService | `application/tunnel.rs`（新） | keep_alive 的监督模式（状态机+退避）；spawn 技术样板取 `process/manager.rs` 平台实现段（process_group(0)、stderr 排水线程、TERM→KILL 升级，~L126-246）；**不走 ProcessManager trait**——该 trait 为 Chrome 专属（`LaunchSpec` 携带 user-data-dir/cdp-port，探活以 profile_dir 防伪）。cli_tool.rs 仅证明「infrastructure 直接 std::process 合法」，但它是一次性同步调用（osascript），非长驻监督先例，勿作样板抄 |
| token 中间件 | `api/auth.rs`（新） | axum middleware layer，叠加方式同现有 CorsLayer；token 存取走 SettingsService |
| 双监听器 | `api/server.rs`（改） | 同一 Router 派生两个（带/不带 auth layer）；`ApiState` 增加两个字段 |
| CDP 会话服务 | `application/cdp_session.rs`（新） | 业务逻辑归服务层；过期/校验/审计是业务语义 |
| WS/HTTP 桥接 | `infrastructure/cdp/proxy.rs`（新） | WS **帧级桥**：axum ws（server 侧）↔ tokio-tungstenite `connect_async`（client 侧），见 §7.2；与既有 `cdp/client.rs` 同属「与实例 CDP 端口对话」的技术设施 |
| 依赖变更 | `src-tauri/Cargo.toml`（改） | axum 增 `features = ["ws"]`；新增直接依赖 `tokio-tungstenite`——当前依赖图零 WS 能力（附录 D.1），两者是帧级桥的前置条件 |
| 代理路由 | `api/cdp_proxy.rs`（新） | 「路由为薄壳」既规 |
| 设置存储 | `application/settings_service.rs`（改） | `app_settings` KV 表新增 3 key，复用 `get_bool/get_str_with_default` |
| 健康检查 | `application/health_service.rs`（改） | `snapshot()` 第 5 项检查 |
| 装配/退出 | `main.rs`（改） | 服务构造顺序 + `RunEvent::ExitRequested` 钩子 |
| CLI | `crates/cli`（改） | token 注入参照 `--api-url` 的 flag>env 优先级模式 |
| 前端 | `src/api/client.ts` / `src/api/types.ts` / `src/store.ts` / `src/pages/SettingsPage.tsx` | 设置草稿模式（startUrl 先例）、开关模式（developerMode 先例） |

**域模型与 DB schema 零改动**：会话是运行态（进程内内存 Map），token/配置进现有 KV 表，无迁移。

---

## 5. TunnelService 规格（隧道层）

### 5.1 ssh 命令规格

```
ssh -N \
  -o ExitOnForwardFailure=yes \
  -o BatchMode=yes \
  -o ConnectTimeout=10 \
  -o ServerAliveInterval=30 -o ServerAliveCountMax=3 \
  -o StrictHostKeyChecking=accept-new \
  -R 17890:127.0.0.1:17891 \
  <remote_access_ssh_target>
```

| 参数 | why |
|---|---|
| `-N` | 纯转发无会话 |
| `ExitOnForwardFailure=yes` | 17890 bind 失败即整体退出，杜绝「进程活着端口半通」僵尸态；退出是监督器重连信号 |
| `BatchMode=yes` | 禁交互（密码/确认挂起），密钥不可用快速失败 |
| `ConnectTimeout=10` | TCP 层快速失败 |
| `ServerAliveInterval/CountMax` | 客户端 90s 判死链并退出 → 触发重连；同时本地 TCP 关闭使 sshd 正常回收 |
| `accept-new` | 首连 TOFU 自动接受新指纹；已变更指纹仍拒绝——个人设备工具链合理默认 |
| `-R 17890:127.0.0.1:17891` | **唯一一条转发**。远端端口保持 17890（远程 CLI/MCP 零配置直用默认地址）；本地落地 17891（鉴权监听器）。⚠️ 不能用 `ClearAllForwardings` 隔离用户配置——它连命令行 `-R` 一并清除（实测踩坑 E.1），隧道会零转发零报错地假活 |
| `-F <净化配置>` | 隔离用户 `~/.ssh/config` 的转发指令（如代理转发 7890），避免其 bind 失败在 `ExitOnForwardFailure` 下连带杀死本隧道（E.1 实验 A）：每次 spawn 前把用户配置剔除 RemoteForward/LocalForward/DynamicForward 行写入 `<app_data>/ssh-tunnel-config`（0600），别名/密钥/跳板解析完整保留。已知边界：Include 引入的转发不在净化范围 |

**「已连接」判定**：`BatchMode` 排除交互挂起 + `ConnectTimeout` 排除 TCP 挂起 +
`ExitOnForwardFailure` 排除半通 ⇒ 子进程存活超过 20s（2×ConnectTimeout 裕量）即判
established，状态 `connecting → connected`。

### 5.2 守尸包裹（孤儿 ssh 的内核级根治）

ssh 以守尸包裹层 spawn，利用内核不变量「**进程无论怎么死（含 SIGKILL），内核关闭其
全部 fd；管道对端读到 EOF**」实现孤儿防护，与信号、清理代码无关。脚本为两次实测
修正后的终版（踩坑记录见附录 E）：

```rust
Command::new("/bin/sh")
    .arg("-c").arg(WRAPPER_SCRIPT)  // 见下
    .arg("proxy")            // $0；$@ = ssh 及其参数（测试可注入 sleep）
    .stdin(pipe_read_end)    // app 持有 write_end；app 死 → 内核关 fd → EOF
    .stderr(piped())         // last_error 诊断环（排水线程 + ~4KB 环形缓冲）
    .process_group(0)        // unix：独立进程组，便于整组回收

// WRAPPER_SCRIPT =
// exec 3<&0; "$@" & p=$!; ( cat <&3 >/dev/null; kill $p ) & c=$!; wait $p; \\
//   kill $c 2>/dev/null; wait $c 2>/dev/null
```

双向监控语义（任一先发生均正确收敛）：

| 事件 | 链路 | 保障 |
|---|---|---|
| ssh 死亡（任何原因） | `wait $p` 返回 → sh 退出 → 监督器 try_wait 秒级感知 → 重连 | **监督可见性**（早期版本 `cat` 前台阻塞会掩盖 ssh 死亡，状态机盲停 connected——实测踩坑 E.2，已修） |
| app 死亡 | 内核关 pipe → EOF → 后台 `cat` 退出 → `kill $p` 杀 ssh | **孤儿防护**（内核 fd 语义） |

两个非显而易见的坑（附录 E.2 有完整实验记录）：

1. `exec 3<&0` 必须在先：POSIX sh 对后台任务把 stdin 重定向为 /dev/null，
   EOF 观察子壳必须显式读 fd3 备份，否则秒读 EOF 误杀 ssh；
2. `wait $p` 必须在前台：它是监督器感知 ssh 死亡的唯一通道。

残余仅「人为单独杀 sh 但放过 ssh」的人祸路径，由启动对账兜底（§5.4）。

### 5.3 状态机与生命周期

```
                 apply(true, t)
  ── disabled ─────────────────▶ connecting ──(存活>20s)──▶ connected
        ▲                            │                        │
        │ apply(false,*) / shutdown()│ 子进程退出(任何原因)     │ 子进程退出
        ▼                            ▼                        ▼
        └────────────────── reconnecting（保留 lastError，退避后 respawn → connecting）
```

- 退避：3s → 6s → 12s → 30s（封顶），无限重试；`restarts` 计数入状态。
- `apply(enabled, target)` 幂等收敛：同配置 no-op；变化先杀旧进程组再走启动路径。
- 状态迁移写 `tracing` + `Activity::record("info", "tunnel.state_change", None, None, msg)`
  （`app_events.environment_id` 可空，schema 已允许）。
- `RunEvent::ExitRequested`（含托盘 Quit）→ `tunnel.shutdown()`：TERM → 1s → KILL 整进程组。

### 5.4 启动对账 + stale 自愈

- **启动对账**（每次 spawn 前）：`pgrep -f` 匹配特征串 `-R 17890:127.0.0.1:17891`，
  **校验进程名为 ssh 后** kill——覆盖守尸包裹也被绕过的人祸残留。与 `Reconciler`/
  `startup_self_heal` 同哲学。
- **stale 自愈**（仅特定错误触发）：分区导致目标机 sshd 滞留死 listener 时（sshd 无
  `ClientAliveInterval` 则 TCP 半开可挂 2h+），重连识别 stderr 中
  `remote port forwarding failed for listen port` 特征 → **经一条一次性 ssh 会话**在远端
  `ss -tlnp` 定位占用 17890 的 stale sshd pid（用户态进程可杀）→ kill → 立即重试。
  占用者非 sshd 则不乱杀、退回退避 + 状态页呈现手工清理命令。不引入常驻远端组件。
- 目标机 sshd 配置 `ClientAliveInterval 30 / ClientAliveCountMax 3`（§10，一次性）后，
  stale 窗口有界 90s，自愈降级为最后防线。

---

## 6. 鉴权网关规格（token 层）

- **生成**：首次开启远程接入时自动生成（复用现有 `uuid` 依赖，v4，122bit 熵——不引新依赖）；
  存 `app_settings`（key: `remote_access_token`）。
- **校验**：`api/auth.rs` middleware 于 17891 Router 校验 `Authorization: Bearer <token>`
  （覆盖 `/api/v1/*`、`/mcp`、status/token 端点）；**`/cdp/` 前缀路径豁免 Bearer 校验**——
  URL 内会话 key 即该层凭证（WS 升级请求无法携带自定义 header；与 §7.2、附录 A 同源）。
  两种凭证各守其域：token（控制面）/ session（数据面，单实例作用域 + TTL）。
  **常数时间比较**（手写 ct_eq 或 subtle，实施择一）；失败 →
  `401 {"error":{"code":"UNAUTHORIZED",...}}`（**必须走既有统一错误形状**，
  旧 CLI 降级语义依赖它，见 §8）。17890 Router 不挂此层。
- **轮换**：`POST /api/v1/remote-access/token`（17891 需旧 token，17890 本地直接可调）→
  生成新 token、**同时清空全部活跃 CDP 会话**、写审计事件。Settings 页提供按钮。
- **呈现**：`GET /api/v1/settings` 的 view 含 `remoteAccessToken`（本地 GUI 显示用；
  远程持 token 者本就持有，无额外泄露面）。
- 诊断语义（CLI exit 契约联动，见 §8）：`ECONNREFUSED` = 隧道断/应用停；**401 = 隧道+应用
  均在线仅凭证问题**；200 = 正常。三态可判，取代早期 refused/RST 启发式。

---

## 7. CDP 会话代理规格（数据面）

### 7.1 会话生命周期

```
POST /api/v1/instances/{id}/cdp/sessions          （需 token）
  → 201 { "sessionId": "<uuid>", "baseUrl": "/cdp/<ins>/<sid>", "expiresAt": <ms> }
```

- TTL **30 分钟**固定，到期作废，客户端重取（无刷新端点、无吊销端点——YAGNI，见 §12）；
- 会话表 = 进程内 `Mutex<HashMap<sid, Session>>`，值含 instanceId 与过期时刻；app 重启即清空；
- 发放时实例须 running（否则 409 `INSTANCE_NOT_RUNNING`）；
- 审计：`cdp_session.created / expired` 写 `app_events`（target_id=实例 id，
  `Activity::resolve_environment_id` 自动归到环境）；
- 清理机制：访问时惰性校验（正确性保障）+ 后台 sweeper（tokio interval 60s）统一删除
  过期项并记 `cdp_session.expired` 审计事件（清理与审计责任归属 sweeper）；
  轮换 token 时随 `POST /remote-access/token` 清空全表。

### 7.2 代理路径与改写规则

挂在**两个**监听器上（17890 免鉴权——本地客户端可选用，获得统一接口；17891 过 token）：

```
GET  /cdp/{ins}/{sid}/json | /json/list | /json/version
     → 校验会话（ins 匹配 + 未过期，否则 404 SESSION_NOT_FOUND）
     → 请求实例真实端口的同路径（复用 infrastructure/cdp 的 HTTP 客户端模式）
     → 改写响应中的 webSocketDebuggerUrl（及 /json/version 的 browser 级 WS URL）：
        ws://127.0.0.1:<port>/devtools/... ⇒ ws://<请求Host头>/cdp/{ins}/{sid}/devtools/...
     → devtoolsFrontendUrl 不迁移（客户端不用，保持原样即可）

WS   /cdp/{ins}/{sid}/devtools/page/{targetId} | /devtools/browser
     → 会话校验 + 实例 running 校验（停了 → 409，桥随目标 TCP 死亡自动断）
     → WS 帧级桥：axum ws（server 侧，握手完成后得 Message 帧）↔
        tokio-tungstenite connect_async(ws://127.0.0.1:<实例真实端口>/devtools/...)
        （client 侧向 Chrome 完成握手），Message 级转发（Text↔Text、Binary↔Binary、
        Close 双向传播）
     ⚠️ 不可「桥到裸 TcpStream」：axum 已消费握手字节，而 Chrome 端口说 WS 协议，
        桥对端必须完成客户端握手。备选的 hyper upgrade 字节级透传（劫持连接后原样
        转发握手请求、改写 path，零新直接依赖）因需手写 upgrade 且绕过 axum ws 而否决
```

要点：

- **WS 客户端无法自定义 header**，故会话凭证走 URL 路径（与 Playwright GUID 路径、
  browserless URL token 同构）；`Host 头改写`使客户端拿到的 URL 自动指向自己可达的地址；
- **零生命周期耦合**：代理按请求现查实例端口（"实例↔端口"映射本就在 DB）；实例停止 →
  目标 TCP 死 → 桥自动关；实例重启换端口 → 下一请求自然解析——**不订阅任何实例事件**，
  不为代码库引入新的服务间耦合范式；
- `get_cdp` / `instance cdp` **语义不变**（仍返回直连 URL，本地场景继续直连）；
  远程可达路径由新命令 `cdp-session` 提供（§8）；
- **代理端点覆盖（显式边界）**：仅 `GET /json`、`GET /json/list`、`GET /json/version`
  与 `WS /devtools/*`（browser / page 目标级）；变更类 HTTP 端点（`/json/new`、
  `/json/activate/{tid}`、`/json/close/{tid}`）**不代理**——cdp.mjs 仅用 `/json/version`
  + browser 级 WS（Target 域完成 list/attach/open，附录 D.2），远程开新页统一走 REST
  `open_tab`，防止远程 agent 误用未经审计的变更端点。

---

## 8. API / CLI / MCP 契约变更

### REST（AGENT-API.md 同步项）

| 项 | 内容 |
|---|---|
| `GET/PUT /api/v1/settings` | view/请求体新增 `remoteAccessEnabled: bool`、`remoteAccessSshTarget: string`、`remoteAccessToken: string`（只读呈现；PUT 不接受写 token，轮换走专用端点） |
| `GET /api/v1/remote-access/status` | `{ enabled, target, state: disabled\|connecting\|connected\|reconnecting, lastError, since, pid, restarts }` |
| `POST /api/v1/remote-access/token` | 轮换 token（清空会话），返回新 token |
| `POST /api/v1/instances/{id}/cdp/sessions` | 发放 CDP 会话（§7.1） |
| `/cdp/{ins}/{sid}/...` | 代理路径（§7.2，非 `/api/v1` 前缀——URL 更短、与 WS 路径一致） |
| 错误码 | **新增** `UNAUTHORIZED`（401）、`SESSION_NOT_FOUND`（404）；既有 `INSTANCE_NOT_RUNNING` 复用 |
| `/api/v1/health` | checks 新增 `remote_access`：disabled → ok「未启用」；connected → ok（含 target）；其余 → fail + suggestion（`chrome-host doctor` 可报隧道故障） |

### CLI（CLI.md 同步项）

- `CHROME_HOST_TOKEN` 环境变量（隐藏 `--token` flag 优先级更高，对齐 `--api-url` 模式）。
  **注意**：CLI 的 send() 请求构造目前无任何 header 注入先例（client.rs 只设
  method/URL/body）——Authorization 注入是全新代码路径，在 reqwest builder 处实现；
- 新子命令 `chrome-host instance cdp-session <id>`（`--quiet` 输出 baseUrl 供 `$(...)` 捕获，
  `--json` 输出全量）。`--quiet` 输出 baseUrl 是**新契约**：`render_kv` 的 quiet 对单对象
  无冻结语义（output.rs 注释明说），按变更类命令 print_id 先例定义，CLI.md 写明；
- **退出码契约扩展：exit 10 = 未授权**（401；应用与隧道均在线，仅凭证问题）——
  0–9 冻结语义不变，纯追加；实施须同步 crates/cli **exit.rs 映射表**
  （`UNAUTHORIZED`→10；client.rs 头注释明示新码必须同步，否则落兜底分支——
  现状 401 落 VALIDATION(7) 兜底）；
- 兼容性：旧 CLI 收到 401 按既有 4xx 族映射 exit 7 + `UNAUTHORIZED` 错误码，
  优雅降级。**前提**：401 响应体严格是 `{"error":{code,message}}` 标准形状
  （否则落 Protocol → exit 1）——§13.1-3 的形状断言在实施中不可省。

### MCP（MCP.md 同步项）

- `/mcp` 同时挂在两监听器；17891 上过 token 中间件（rmcp 在 axum 路由之下，协议层无感）；
- 远程客户端接入示例（Claude Code）：
  `claude mcp add --transport http --header "Authorization: Bearer <token>" chrome-host http://127.0.0.1:17890/mcp`；
- 新增第 25 个工具 `create_cdp_session(instanceId)` → `{ sessionId, baseUrl, expiresAt }`；
  `get_cdp` 保持原语义；
- Streamable HTTP/SSE 经 ssh TCP 转发透明（冒烟验证一次）。

### cdp.mjs（repo skill `skills/chrome-cdp`）

- 新增 `CDP_BASE` 环境变量：设置后所有 `/json*` 与 WS 连接改走 `<CDP_BASE>/...`，
  未设置时维持 `CDP_HOST/CDP_PORT` 直连行为（本地零回归）。

---

## 9. UI 规格（严格对齐 DESIGN.md token）

Settings 页新 section「远程接入（云桌面 / CI）」，插在 Developer Mode 卡之后：

```
┌─ 远程接入（云桌面 / CI）────────────────────────────────┐
│ 说明（text-xs text-ink-3）：开启后仅转发 Agent API 端口到 │
│ 目标机器，远程访问需持有访问令牌；实例 CDP 由会话代理提供，│
│ 不开放任何额外端口。                                       │
│ [SSH 目标 inputCls flex-1]（placeholder: user@host 或别名）[保存 btnPrimary]
│ 访问令牌：<font-mono 令牌> [复制 btnGhost] [轮换 btnGhost] │
│ 状态行：● 已连接 vino@yun    （StatusDot + text-run-text） │
│ [开关：与 Developer Mode 同款胶囊]                         │
└──────────────────────────────────────────────────────────┘
```

- 目标输入框走 startUrl 草稿模式（加载同步、点保存才 PUT）；保存仅写 target；
- 开关点击 PUT `{remoteAccessEnabled}`；目标为空时 400 → Toast 引导先填目标；
- 状态行挂载期间每 5s 轮询 `/remote-access/status`（setInterval + cleanup）：
  connected → `bg-status-running` + `text-run-text`「已连接 <target>」；
  connecting/reconnecting → `bg-status-starting` + `animate-pulse` + `text-ink-2`
  （reconnecting 附 lastError，`text-status-error` 截断）；
  disabled → `bg-status-stopped` + `text-ink-3`「未启用」；
- 轮换按钮弹 confirm（提示会使现有远程会话全部失效）；
- `pnpm build` 通过 + 主窗口/Popover 两端人工核对（DESIGN.md 禁则 5）。

---

## 10. 目标机器一次性配置

```bash
# ① sshd 死链有界回收（需 sudo；对走该 sshd 的所有连接生效，含 mutagen）
echo 'ClientAliveInterval 30\nClientAliveCountMax 3' | sudo tee /etc/ssh/sshd_config.d/90-chrome-host.conf
sudo systemctl reload sshd        # 或发行版等价命令

# ② CLI（repo 经 mutagen 同步后；独立二进制，纯 HTTP 薄客户端）
#    前置：yun 需有 Rust 工具链（个人工具可接受）；无工具链则用交叉编译产物 scp
#    （cargo build --release --target x86_64-unknown-linux-gnu 后 scp 到 yun）
cargo install --path crates/cli
echo 'export CHROME_HOST_TOKEN=<Settings 页复制的令牌>' >> ~/.zshrc

# ③ （可选）MCP 客户端 header 配置，见 §8
```

前置校验：目标机 17890 端口未被占用（`ss -ltn | grep 17890`）；`ss` 命令存在（自愈依赖，
缺失则自愈降级为退避重试，功能不受损）。

---

## 11. 风险与已知边界

| 风险/边界 | 定性 | 处置 |
|---|---|---|
| 孤儿 ssh | 已根治（内核级） | pipe-EOF 守尸包裹（§5.2）；人祸残余由启动对账兜底 |
| 分区 stale listener | 有界化 | 目标机 `ClientAliveInterval`（90s）为主，bind 失败自愈（§5.4）为辅，启动对账兜底 |
| app 重启断远程 CDP 会话 | **显式接受的语义变化** | 桥与会话表在进程内；客户端重取会话即恢复（cdp.mjs 本就逐命令建连）；本地直连不受影响（现状 app 重启本地 CDP 连接存活） |
| token = 全权凭证 | 设计粒度 | 单用户工具的正确粒度；泄露面 = Settings 页与目标机 shell 配置；轮换按钮一键止血（清会话） |
| CDP 流量经 app 中继 | 性能边界 | axum WS 桥 + 单条 ssh；截图/快照级流量冒烟验证；超预期再议直通优化（非目标） |
| 首连 TOFU（accept-new） | 弱于严格校验 | 个人设备工具链取舍；首次连接时 UI 状态行可见连接过程 |
| 远程 agent 探活三态 | 已消解旧歧义 | refused（隧道/应用断）/ 401（在线凭证错）/ 200——协议语义而非启发式 |
| 目标机占用 17890 | 低概率 | `ExitOnForwardFailure` 整体失败并报错；状态页呈现具体 bind 失败信息 |
| 平台 | Windows 未验证 | V1 仅验证 macOS→Linux（实际场景）；不做功能级 gate，平台 API（如 unix-only 的 `process_group(0)`）按既有 cfg 门控模式处理（manager.rs 先例） |

---

## 12. 非目标（V1 明确不做）

- 多 token / 权限分级 / 配额（市场产品的多租户复杂度，单用户不买）
- 非 SSH 传输（Tailscale / frp / 自研 relay）
- CDP 会话的刷新与主动吊销端点（TTL + 轮换清空已覆盖需求）
- 登录浏览器（login-profile launch 的 cdpPort）的代理覆盖——仅代理 instances 表内实例
- 远程 GUI / 远程托盘 / 每环境开关
- MCP token 之外的凭证形态（mTLS 等）

---

## 13. 测试与验收

### 13.1 Rust 单测（`cargo test`，随行提交）

1. **settings**：新字段 roundtrip；enable 且目标空 → 400 `REMOTE_ACCESS_TARGET_INVALID`
   （含"先 enable 后清空目标"路径拒绝）；target 含空白/`-` 开头 → 400；token 首次 enable
   自动生成且非空。
2. **build_ssh_args**：含全部 `-o` 项与**唯一** `-R 17890:127.0.0.1:17891`；target 末位；
   守尸包裹 argv 结构（`sh -c <script> proxy ssh ...`）。
3. **token 中间件**：无/错 token → 401 `UNAUTHORIZED`（响应形状断言）；正确 token 放行；
   常数时间比较函数直测。
4. **会话服务**：发放/过期（TTL 边界）/实例停止拒绝发放；sid 与 ins 不匹配 → 404；
   sweeper 删除过期项并记 `cdp_session.expired`（mock 时钟断言）。
5. **URL 改写**（纯函数）：`webSocketDebuggerUrl` 按 Host 头正确重写（含 browser 级）；
   非实例端口来源字符串不动。
6. **守尸包裹**（真实进程，参照 `process/manager.rs` 测试先例）：以 `sleep` 替身 spawn 包裹，
   持 pipe 写端的测试进程 drop 写端 → 断言子进程在秒级内死亡且无残留。
7. **监督器**：外部 kill 子进程 → reconnecting + respawn +1；`apply(false)` → 进程组清空；
   target 变更 → 旧进程死、新 argv 含新 target。
8. **对账**：预 spawn 命令行含特征串的替身进程 → 清理后确认被杀；进程名非 ssh 的同特征串
   进程不被杀。

### 13.2 代理集成测试（进程内，不依赖真实 Chrome）

起一个假 CDP 服务（axum test server 模拟 `/json` 返回固定 targets + WS echo）：
会话发放 → 经代理 `/json` 拿到改写后 URL → 经代理 WS 与假服务完成 echo 往返 →
会话过期后再访问 → 404。覆盖：改写正确性、桥双向、close 传播、过期拒绝。

### 13.3 冒烟（真实 yun；smoke.sh 可选段，`REMOTE_SSH_TARGET` 存在才执行）

1. Settings 填 target 开启 → 20s 内状态行「已连接」；目标机 `nc -z 127.0.0.1 17890` 通。
2. 无 token 探活 → 401（CLI exit 10）；带 token `chrome-host status` → exit 0。
3. **全链路**：`instance create` → `cdp-session <id> --quiet` →
   `CDP_BASE=<url> cdp.mjs list/shot`（远程截图落盘）；`env activity` 可见 cdp_session 事件。
4. 韧性：断网 60s 恢复 → 状态自动回 connected；会话过期后重取会话可用。
5. 生命周期：app 正常退出 → 目标机 17890 连接拒绝 + 本机 `pgrep -f 17890:127.0.0.1:17891`
   为空；force-quit 后重启 → 对账生效、隧道恢复。
6. `chrome-host doctor`：connected → remote_access ✓；改错 target → ✗ 且 suggestion 准确。
7. MCP：目标机 Claude Code 带 header 接入 → 25 工具可列举调用。

### 13.4 前端

`pnpm build` 通过；Settings 新卡主窗口核对；开关/保存/轮换 confirm/状态轮询/错误 Toast
按设计稿 token 呈现。

---

## 14. 实施顺序（建议 PR 拆分）

1. **PR-1 隧道与鉴权地基**：settings 三字段 + 校验 + token 生成/轮换 → 17891 监听器与
   auth 中间件 → `application/tunnel.rs`（守尸包裹/对账/自愈/退避）→ status 端点 →
   health 检查 → main.rs 装配与 ExitRequested 钩子 → Settings UI 卡 → CLI token 与
   exit 10。**验收**：目标机带 token 的 CLI 全命令经隧道可用，401/exit-10 语义正确。
2. **PR-2 CDP 会话代理**：`application/cdp_session.rs` + `infrastructure/cdp/proxy.rs` +
   `api/cdp_proxy.rs`（路径/改写/WS 桥）→ CLI `cdp-session` + MCP `create_cdp_session` +
   cdp.mjs `CDP_BASE` → 审计事件。**验收**：目标机完整自动化闭环（建实例→开会话→截图→清理）。
3. **PR-3 文档与生态**：AGENT-API.md / CLI.md / MCP.md / README 同步（§8 清单）；
   repo 两份 skill 增远程章节；smoke 可选段。
4. （**独立小修，与本方案解耦**：`skills/chrome-host/SKILL.md:110` 承诺端口基数 29222 与
   代码 `port_allocator.rs:6` 的 9222 不一致——早期方案曾联动此问题，网关架构下已无关，
   按普通契约 bug 修复即可。）

---

## 附录 A：稳态数据流示例（远程截图一次的完整链路）

```
yun: chrome-host instance cdp-session ins_xxx --quiet
 → POST http://127.0.0.1:17890/api/v1/instances/ins_xxx/cdp/sessions
   [经隧道 → Mac 17891 → token 中间件 → 会话服务]
 ← { baseUrl: "/cdp/ins_xxx/<sid>" }

yun: CDP_BASE=http://127.0.0.1:17890/cdp/ins_xxx/<sid> cdp.mjs shot <target>
 → GET /cdp/ins_xxx/<sid>/json/list          [token 层不拦——会话即凭证]
   [代理 → 实例真实端口 /json/list → Host 头改写 webSocketDebuggerUrl]
 → WS  /cdp/ins_xxx/<sid>/devtools/page/<tid> [代理 ↔ 127.0.0.1:<实例端口> 双向桥]
 ← Page.captureScreenshot 帧
```

## 附录 B：与早期草案（v1）的差异摘要

| 维度 | v1 草案 | v2（本方案） |
|---|---|---|
| 转发端口 | 17890 + CDP 分配域枚举（200/400 个 `-R`） | 仅 17890 一条 |
| CDP 数据面 | 实例端口同号直通 | 会话代理（零端口暴露） |
| 鉴权 | 无（回环信任延伸） | token 网关 + 会话凭证 |
| START_PORT 变更 | 需联动（远程撞车规避） | 不需要（CDP 端口不出本机） |
| 动态转发（ControlMaster） | 曾列为备选 | 否决（安全破绽 + 生命周期耦合，§2.2-C） |
| 远程探活歧义 | refused/RST 启发式 | refused / 401 / 200 三态协议语义 |

## 附录 C：实测验证记录（2025-09，本机 OpenSSH_10.0p2 + 本地 sshd）

1. **OpenSSH 无区间转发语法**：`-R 29990-29991:...` 与 `-L` 区间均报
   `Bad forwarding specification`（man page 无此特性）——v1 草案的端口段方案据此改为
   枚举，v2 整体不再依赖。
2. **逐端口枚举可行**：200 个（单段）与 400 个（双段）`-R` 全部 bind 成功、数据往返
   HTTP 200、ssh 退出零残留（该路线已否决，记录备查）。
3. **ControlMaster 动态增删可行**：`-O forward` / `-O cancel` 增删单端口转发，master
   不断线、既有连接不受扰（该路线因安全与耦合否决，记录备查）。
4. **验证边界**：1–3 均在本机 sshd 验证，覆盖 OpenSSH 协议行为；真实 yun（含其 sshd
   版本/配置）的最终确认在 §13.3 冒烟执行。

## 附录 D：v2.1 修订验证记录（2025-09，静态代码核实）

1. **依赖图零 WS 能力**：`src-tauri/Cargo.toml` `axum = "0.8"` 无 feature，
   Cargo.lock 全文 tungstenite 出现 0 次 → 帧级桥需 `features=["ws"]` + 新增直接依赖
   tokio-tungstenite（§4 变更清单已列）。
2. **cdp.mjs 端点面**：仅 `fetch /json/version` + browser 级 WebSocket
   （Target.getTargets / attachToTarget / createTarget 完成 list/attach/open）；
   `/json/new` 等变更类 HTTP 端点不使用——代理覆盖边界据此收紧（§7.2）。
3. **cli_tool.rs 定位**：一次性同步 osascript 调用（elevate_mkdir / elevate_remove），
   非长驻监督先例；监督样板在 process/manager.rs 平台实现段（§4 已改参照）。
4. **output.rs quiet 语义**：render_kv 注释明说「quiet 对单对象无冻结语义」→
   `cdp-session --quiet` 输出 baseUrl 以新契约写入 CLI.md（§8 已列）。
5. **exit.rs 映射**：按 HTTP status match（403/404/409 显式，其余落 VALIDATION 兜底）
   → 401 现落 exit 7；`UNAUTHORIZED`→exit 10 需显式新增映射项。
6. **本附录为静态核实**（代码阅读 + 依赖图检查）；未实际运行隧道/代理，
   附录 C 的 OpenSSH 实测结论未复测（与已知行为一致）。

## 附录 E：实施期实测踩坑记录（2025-09，PR-1 真实环境验证）

### E.1 `ClearAllForwardings` 连命令行 `-R` 一并清除 / 用户 config 转发污染

试图隔离用户 config 转发时发现的实验矩阵（真实 yun）：

| 实验 | 配置 | 结果 |
|---|---|---|
| A | `-R 17890` + 用户 config 的 7890 转发 + `ExitOnForwardFailure=yes` | ssh 退出：「remote port forwarding failed for listen port **7890**」——用户无关转发失败杀死本隧道 |
| B | 同 A 再加 `ClearAllForwardings=yes` | 零转发零报错假活（连接/认证成功但无任何转发请求），yun 无监听 |
| C | `-F 净化配置`（剔除转发行）+ `-R 17890` | **17890 正常 bind，用户 7890 转发互不影响，退出后监听即时回收**（采纳） |

结论：OpenSSH 的 `ClearAllForwardings` 语义是「清除配置文件**与命令行**的全部转发」，
与参数顺序无关；配置隔离唯一可靠路径是净化配置文件。

### E.2 POSIX sh 后台任务的 stdin 语义（守尸脚本两处修正）

1. **后台 stdin = /dev/null**：`( cat >/dev/null; kill $p ) &` 的 cat 刚启动即读到
   /dev/null 的 EOF → 约 3ms 内误杀 ssh。修法：`exec 3<&0` 先备份真实 stdin，
   观察子壳显式 `cat <&3`；
2. **监督盲区**：早期版本 `cat` 在前台阻塞，ssh 死后 sh 不退出（`wait $p` 排在 cat 后），
   监督器 try_wait 永远看不到退出，状态机盲停 connected（真实环境复现：隧道假活
   connected 而远端无监听）。修法：`wait $p` 前台化，sh 随 ssh 死亡即刻退出。
   修复后实测：杀 ssh → 3s 内状态转 reconnecting、restarts+1 → 退避后自动重连成功。

### E.3 端到端验收结果（真实 yun）

- 启动自动收敛（持久化 enabled）→ connecting → connected（20s 判活）；
- yun 侧 `ss` 可见 17890 监听；无 token → 401（标准 ErrorBody 形状）、带 token → 200；
- CLI 三态：连接拒绝 → exit 8；401 → **exit 10**（`UNAUTHORIZED`）；正常 → exit 0；
- 杀 ssh 断链 → 3s 感知 → 退避重连 → yun 复验 401/200 恢复；
- app 正常退出 → 隧道进程零残留（EOF 守尸 + 退出钩子双路径验证）。

### E.4 PR-2（CDP 会话代理）端到端验收结果（2025-09，真实 yun）

本地闭环（经 17890 代理面）：
- `cdp-session --quiet` 输出完整代理 URL（`CDP_BASE=$(...)` 直接可用，新契约）；
- `CDP_BASE=<url> cdp.mjs list/eval/shot` 全通：/json/version 改写、browser 级 WS
  （Target.getTargets）、page 级 attach + Runtime.evaluate + Page.captureScreenshot
  均过帧级桥（本地截图落盘真 PNG 2400×1532）；
- 负路径：伪造会话 → 404 `SESSION_NOT_FOUND`；`/json/new` → 400（非代理端点）；
  实例停止后发放 → 409 `INSTANCE_NOT_RUNNING`。

远程闭环（yun，经隧道 + Bearer token）：
- yun 侧 curl（token）POST sessions 发放成功 → `CDP_BASE=http://127.0.0.1:17890/cdp/...`
  `node cdp.mjs list` 与 `shot` 全通，截图在 yun 落盘（PNG 2400×1532，与本地同源同尺寸）。

实施备注：
- axum 0.8 移除了 `Option<T>` 万能提取器（WebSocketUpgrade 未实现
  OptionalFromRequestParts）→ 自写 `MaybeUpgrade` 提取器（cdp_proxy.rs）；
- tungstenite 0.26 的 Text/Binary 载荷与 axum ws 同源（Bytes 直传、Utf8Bytes 经 &str 转换）；
- 观察到既有问题（与本方案无关，待单独处理）：`open_tab` 的 `PUT /json/new?url=`
  在 CfT 131 下创建 tab 但不导航（停在 about:blank）——远程测试改用既有 tab 完成。
