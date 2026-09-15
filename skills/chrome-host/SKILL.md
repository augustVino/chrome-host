---
name: chrome-host
description: 通过 chrome-host CLI（chrome-host 桌面应用的官方命令行入口）编排多环境隔离 Chrome 实例——查/建环境、启停实例、开标签页、登录态快照、扩展与内核管理，并把实例的动态 CDP 端口移交给 chrome-cdp skill 做页面自动化。只要用户提到"用 XX 环境打开/排查/测试"（qa、sandbox 等环境名）、需要独立 profile / hosts 注入 / 免登录的浏览器、管理 chrome-host 的环境或实例、或任务要求先起隔离浏览器再连 CDP，就必须使用本 skill。已在本机 9222 运行的普通调试 Chrome、修改系统 hosts 不走本 skill。
---

# Chrome Host — 多环境隔离 Chrome 编排（CLI）

## 定位

Chrome Host 是桌面应用（Tauri），本 skill 通过其官方命令行入口 **chrome-host CLI** 完成全部操作。CLI 的两个契约直接影响操作方式：危险操作有确认闸门（见「危险操作与确认闸门」），退出码 0–9 语义冻结（见「退出码契约」）。

职责分工：
- **本 skill**：实例编排层——环境/实例生命周期、标签页开关、登录态、扩展、内核。
- **chrome-cdp skill**：页面自动化层——截图、eval、点击、读 DOM。本 skill 把实例"起好、开好页、登好录"，然后移交。

## 前提与自检

1. 桌面应用 Chrome Host 运行中（含托盘常驻）——CLI 是它的客户端，应用不在则一切不可用。
2. CLI 已安装：`command -v chrome-host`。缺失则告知用户安装方式（仓库内 `cargo install --path crates/cli`）。

## 第零步：健康检查

任何操作前先跑 `chrome-host status`，按**退出码**分支：

```bash
chrome-host status
```

- **exit 8**（服务不可达）→ 应用未启动 → 提示用户启动 Chrome Host。API 就在 17890，CLI 恒连此地址；连不上就是没起，**不要**猜端口、不要尝试其他地址、不要试图绕道 HTTP。
- **exit 0** → 应用在线，继续正常流程。
- **其他退出码**（多为 exit 1）→ 应用在跑但内部异常（错误行格式 `[CODE] message（HTTP status）`）。把 CODE + message 原样转述给用户——这属于应用自身故障，agent 无法修复，不要建议"重试"掩盖问题，也不要编造数据。需要细节时跑 `chrome-host doctor`（逐项 ✓/✗ + 修复建议；存在失败项时 exit 1）。
  - 特例：错误为「响应格式异常」时，典型原因是**运行中的应用是旧版本**，缺少 CLI 依赖的新端点（如 `/api/v1/health` 返回 404 空体）。此时业务命令（env/instance 等）通常仍正常，请用户更新/重启桌面应用即可，不要误判为 API 故障。

## 核心模型（10 秒）

| 概念 | 含义 | id 形态 |
|------|------|---------|
| 环境 Environment | 一份配置：名称 + 可选 hosts 配置源 + keepAlive 开关 | `env_xxx` |
| 实例 Instance | 该环境的一次隔离运行：独立 user-data-dir + 动态 CDP 端口 + 进程级 hosts 注入 + 登录态克隆 | `ins_xxx` |

- 一个环境可同时运行多个实例，互不干扰（独立 profile）。
- hosts 与登录态都在**实例启动时**固化；改了环境配置要重启实例才生效。
- hosts 注入用 `--host-resolver-rules` 实现，进程级生效，**不改系统 hosts**，应用退出即消失。

## 主工作流：用户指定环境排查问题

典型指令："用 qa27645 帮我打开 xxx 页面看看"。按以下顺序推进，每步都有明确分支：

### 1. 列环境，判断环境是否存在

```bash
chrome-host env list --json
```

- **环境存在** → 记下 `id` 与运行计数，进入第 2 步。
- **不存在** → 向用户确认是否新建，以及 hosts 配置源地址（返回 hosts 格式文本的 http(s) URL，qa 环境通常需要）：

```bash
chrome-host env create qa27645 --hosts-source-url "https://.../hosts" --quiet   # 输出 env id
```

不传 `--hosts-source-url` 则不做 hosts 注入——域名解析将与本机一致，qa 域名可能指向错误地址。**用户没提 hosts 源而要访问内部域名时，主动问一句。**

### 2. 判断该环境下是否已有可用实例

```bash
chrome-host instance list --env <envId> --json
```

- 有 `status: "running"` 的实例 → **直接复用**（用户在实例里的登录态、已开页面都还在），进入第 3 步。
- 有 `status: "stopped"` 的实例 → `chrome-host instance start <insId>` 复用（保留 profile，登录态还在）。
- 无实例 → 创建并启动：`chrome-host instance create <envId> --quiet`（输出 ins id）。

`instance create` 是"创建即启动"的同步阻塞命令（等 Chrome 起来 + CDP 就绪）。CLI 对启动类长操作**不设总超时**——首次运行阻塞于内核下载是正常现象，不会误判失败。想先确认内核：`chrome-host runtime version`；未安装则 `chrome-host runtime install`（CLI 自动轮询到安装完成，默认上限 900s，超时 exit 9；期间告知用户 ~200MB 下载）。

### 3. 打开目标页面

```bash
chrome-host instance open <insId> "https://xxx.example.com/path"
```

URL 必须以 http:// 或 https:// 开头——CLI 本地校验，非法输入 exit 2 且请求不发出。查看已开页面：`chrome-host instance tabs <insId> --json`（返回 id/url/title）。

### 4. 需要登录 → 提示用户，等待确认

判断依据：页面跳到登录页 / 统一认证，或用户任务本身依赖登录态。此时：

1. **停下来明确告知用户**："已在 qa27645 环境的 Chrome 实例中打开登录页，请在弹出的浏览器窗口完成登录，登录后告诉我。"
2. **等待用户回复**，不要轮询猜测，更不要尝试自动填表单——环境隔离 profile 中没有用户凭据，自动登录必然失败且可能触发风控。
3. 用户确认后，验证登录结果：`chrome-host instance tabs <insId> --json` 看 URL 是否已离开登录页，或移交 chrome-cdp 后用 `snap` 确认页面内容。

需要**长期免登录**（每次新实例都带登录态）时不用这条路径，改用「登录态快照」，见下文。

### 5. 移交 chrome-cdp 做页面自动化

```bash
chrome-host instance cdp <insId> --json | jq .port
# → JSON 含 host / port / httpUrl / webSocketUrl
```

拿到 `port` 后，按 chrome-cdp skill 的方式连接，**显式指定 `CDP_PORT`**：

```bash
CDP_PORT=<port> <chrome-cdp-skill>/scripts/cdp.mjs list
CDP_PORT=<port> <chrome-cdp-skill>/scripts/cdp.mjs shot <target>
```

衔接要点（断链几乎都出在这里）：

- 实例 CDP 端口从 **29222 起动态分配**，永远不是 chrome-cdp 默认的 9222。每次移交都现查 `instance cdp`，不要缓存旧端口——实例重启后端口可能变化。
- 实例未运行时该命令 exit 5（`INSTANCE_NOT_RUNNING`），先 `instance start` 再查。
- 每个 tab 首次被 CDP 访问时 Chrome 会弹 "Allow debugging" 授权框，需用户在本地点一次允许；后续命令走 daemon 无需再批。
- `webSocketUrl` 供需要直连 CDP WebSocket 的场景使用。

### 6. 收尾

排查完成后询问用户是否停止实例：`chrome-host instance stop <insId>`。不要默认停止——用户可能还要继续用；也不要默认保留——长期挂着的实例占内存。开启 keepAlive 的环境（实例意外退出自动重启）由应用自管，无需干预。

## CLI 命令速查

全局 flag（所有命令通用）：`--json`（stdout 纯 JSON，错误体也走 stdout，配合 jq）、`--quiet`（仅输出主实体 id，供 `$(...)` 捕获）、`--verbose`（stderr 追加请求摘要：method/path/耗时，不含 query 与敏感内容）、`--yes`（全局跳过确认）。`--api-url` 与环境变量 `CHROME_HOST_API_URL` 可改基地址——**本 skill 不要使用**，恒连默认本机地址。

### 诊断

```text
chrome-host status                    健康摘要（exit 0 / 1 / 8 分支见第零步）
chrome-host doctor                    逐项诊断 + 修复建议（存在失败项 → exit 1）
```

### 环境

```text
chrome-host env list                  列表（含运行摘要）
chrome-host env get <id>              详情
chrome-host env create <name> [--hosts-source-url URL] [--icon PATH]
chrome-host env update <id> [--name N] [--hosts-source-url URL] [--startup-args ARGS] [--keep-alive BOOL]
                                      只更新传入的 flag；值为空串表示显式置空
chrome-host env delete <id> --yes     删除（级联：有运行中实例时自动停止后再删）
chrome-host env status <id>           实例状态计数
chrome-host env activity <id> [--limit N]   事件审计流（排查启动失败原因用）
```

### 实例与标签页

```text
chrome-host instance list [--env <envId>]   列表
chrome-host instance get <id>         详情（status/pid/cdpPort/profileDir）
chrome-host instance status <id>      轻量状态（轮询用）
chrome-host instance create <envId>   创建即启动（同步阻塞；长操作无总超时）
chrome-host instance start|stop|restart <id>   生命周期（restart 会重拉 hosts）
chrome-host instance open <id> <url>  新开标签页（http/https 本地校验）
chrome-host instance tabs <id>        标签页列表（仅 page 类型）
chrome-host instance navigate <id> <tabId> <url>   导航既有标签页（丢旧页历史）
chrome-host instance focus <id>       唤出窗口到前台
chrome-host instance cdp <id>         CDP endpoint（--json | jq .port 移交 chrome-cdp）
chrome-host instance delete <id> --yes   删除（运行中自动 stop→delete）
```

### 登录态快照

```text
chrome-host login-profile list        全量列表（含环境名）
chrome-host login-profile get <envId>       视图（惰性创建）
chrome-host login-profile launch <envId>    打开登录浏览器（母本）
chrome-host login-profile capture <envId> [--yes]   捕获快照（覆盖已有快照时需确认；登录浏览器须已关）
chrome-host login-profile reset <envId> --yes       清空快照（总是确认）
```

### 扩展 / 设置 / 内核

```text
chrome-host extension list|get <id>             列表 / 详情（ready/missing/invalid；system=内置锁定）
chrome-host extension add <path>                注册本地目录（须含 manifest.json）
chrome-host extension enable|disable <id>       启停（system 扩展锁定 → exit 6）
chrome-host extension remove <id> --yes         移除注册项（不删源文件）
chrome-host settings get                        应用设置
chrome-host settings update [--developer-mode BOOL] [--env-label-position P] [--env-label-color C] [--start-url URL]
chrome-host runtime version                     内核版本状态（pinned/已装/下载中）
chrome-host runtime install [--timeout SEC] [--cancel]   安装（轮询到完成，超时 exit 9）；--cancel 中止下载
```

## 危险操作与确认闸门

以下命令带确认流：`env delete`、`instance delete`、`extension remove`、`login-profile capture`（仅覆盖已有快照时）、`login-profile reset`、`runtime install --cancel`。

规则（必须遵守——闸门的价值在于强制破坏性操作先过用户授权）：

- agent 的 shell 是**非交互环境**（非 TTY），缺 `--yes` 时这些命令直接 exit 2 拒绝，**请求绝不发出**——先向用户说明后果，拿到明确同意后再带 `--yes` 执行。
- **绝不为绕过确认而静默加 `--yes`**。exit 2 的"需要 --yes"提示是闸门，不是障碍：它强制把破坏性操作先过一遍用户授权。
- `--yes` 是显式授权级联语义：`env delete --yes` 会连带停止并删除全部实例数据；`instance delete --yes` 会连带停止运行中实例。

## 退出码契约（决策依据）

0–9 冻结语义。**依据退出码决策**，勿解析文案；需要机器可读错误详情时用 `--json`（stdout 输出 `{"error":{"code","message"}}`）。

| Exit | 语义 | agent 处置 |
|------|------|-----------|
| 0 | 成功 | 继续 |
| 1 | 一般错误（协议破坏、未知 5xx、doctor 有失败项） | 转述错误行给用户，勿盲目重试 |
| 2 | 参数错误 / 危险操作缺 `--yes` / 本地校验失败 | 核对命令；若是确认闸门，先取得用户同意 |
| 3 | 资源不存在（`*_NOT_FOUND` 族） | 重新 `list` 核对 id，勿盲目重试 |
| 4 | 资源已存在（预留） | — |
| 5 | 运行时冲突 / 运行时错误（409 族、内核/启动/CDP 500 族） | 按下表 code 细分处置 |
| 6 | 权限拒绝（`EXTENSION_SYSTEM_LOCKED`） | 无解，告知用户 |
| 7 | 请求校验失败（400 族） | 修正参数后重试 |
| 8 | 服务不可达（应用未运行） | 提示用户启动应用，勿猜端口 |
| 9 | CLI 侧等待超时（`runtime install` 轮询超时） | 用 `runtime version` 查进度，必要时继续等待或 `--cancel` |

## 登录态快照（长期免登录）

实例每次启动都会克隆环境快照，所以"捕获一次、终身免登"。适合每天都要用 qa 环境的场景，流程全部可由 agent 引导：

1. `chrome-host login-profile launch <envId>` → 弹出一个独立登录浏览器（母本，不在 instances 列表）
2. 提示用户在其中完成登录（同主工作流第 4 步：等待 + 用户确认）
3. 用户登录后**让用户关闭该浏览器窗口**（必须由用户关闭或自然退出）
4. `chrome-host login-profile capture <envId> --yes` 捕获快照（覆盖已有快照，已获用户同意才加 `--yes`）
5. 之后该环境 `instance create` 自动带登录态

已知边界，引导用户时提前说明：

- **session cookie（浏览器关闭即失效的那类）无法靠快照保留**——Chrome 标准语义，不落盘。此类系统的免登录 = 每次走"launch → 登录 → capture"或者直接复用 running 实例。
- **单 session 互踢**：内部系统若单 session 策略，多实例共用同一快照会互相踢下线。此类环境一次只跑一个实例。
- 快照过期（后端 session 过期）→ 重走一次 launch/capture，或 `reset --yes` 后重来。

## 错误细分决策表（exit 5/7 时看 `[CODE]`）

CLI 错误行格式 `[CODE] message（HTTP status）`，CODE 与服务端错误码同源。exit 3/6/8 已在退出码表覆盖，此表只列需要细分的：

| code | 含义 | 处置 |
|------|------|------|
| `INSTANCE_ALREADY_RUNNING` | 重复 start | 直接复用现有实例 |
| `INSTANCE_NOT_RUNNING` | 操作需要运行中实例 | 先 `instance start`，再重试原操作 |
| `PROFILE_IN_USE` | 捕获快照时登录浏览器未关（或捕获已在进行） | 请用户关闭登录浏览器后重试 |
| `CDP_PORT_UNAVAILABLE` | 实例启动时 CDP 端口被冲突占用 | 直接重试 start（会重新分配端口） |
| `INSTANCE_START_FAILED` | Chrome 启动失败或 CDP 超时未就绪（进程已回收） | `env activity <id>` 看原因，重试一次；连续失败告知用户 |
| `HOSTS_FETCH_FAILED` / `HOSTS_EMPTY` | hosts 配置源拉取失败/内容为空，实例拒绝启动 | 检查 `hostsSourceUrl` 可达性，必要时 `env update --hosts-source-url` |
| `CDP_CONNECTION_FAILED` / `CDP_NOT_READY` | 实例 CDP 不可达（可能刚崩溃） | `instance status` 查看，必要时 `restart` |
| `KERNEL_NOT_READY` | 内核未下载 | `runtime install`，等待后重试 |

应用启动时若 17890 被其他进程占用，Agent API 会降级禁用——此时表现就是健康检查一直 exit 8。提示用户重启应用即可。

## 安全与边界

- **扩展只引用已注册资源**：`extension add` 需显式注册（服务端校验 manifest），没有任何"按任意路径加载扩展"的入口。Agent 不应尝试绕过——这是有意设计的安全边界（任意路径 = 任意代码执行）。
- 实例的 hosts 注入仅影响该实例进程，永不修改系统 hosts；应用未运行时域名解析不受任何影响。
- API 仅绑定 127.0.0.1，本机可用；不要把端口暴露到网络。

## 补充：MCP 入口

应用同时内嵌 MCP server（`http://127.0.0.1:17890/mcp`，Streamable HTTP，24 个 tools，能力为 CLI 的子集——无 settings / 内核下载 / 诊断）。已在 MCP 客户端配置过的会话可直接用 MCP tools；本 skill 默认走 CLI，因为退出码契约与确认闸门对 agent 更安全。两者调用同一个服务层，状态完全一致。
