# chrome-host CLI 接入指南

> `chrome-host` CLI 是 GUI 与 MCP 之外的第三个产品入口：一个不依赖 Tauri 的独立二进制，作为本机 Agent API（`127.0.0.1:17890`）的**薄客户端**，与 GUI / MCP 共享同一套服务层与业务契约。面向开发者终端、Shell 脚本、CI 流水线与 AI Agent。
>
> 本文档是命令参考与接入契约。

## 目录

- [前提条件](#前提条件)
- [安装](#安装)
- [快速上手](#快速上手)
- [全局参数](#全局参数)
- [输出模式](#输出模式)
- [Exit Code 契约](#exit-code-契约)
- [错误格式](#错误格式)
- [命令参考](#命令参考)
  - [env — 环境管理](#env--环境管理)
  - [instance — 实例管理](#instance--实例管理)
  - [extension — 扩展管理](#extension--扩展管理)
  - [login-profile — 登录态生命周期](#login-profile--登录态生命周期)
  - [settings — 全局设置](#settings--全局设置)
  - [runtime — 内核管理](#runtime--内核管理)
  - [status / doctor — 诊断](#status--doctor--诊断)
- [AI Agent / CI 接入](#ai-agent--ci-接入)
- [安全边界](#安全边界)
- [故障排查](#故障排查)
- [兼容性承诺](#兼容性承诺)

---

## 前提条件

1. **chrome-host 桌面应用正在运行**（主窗口或托盘常驻均可）。CLI 自身无业务逻辑，全部能力来自本机 Agent API；应用未运行时所有命令返回 `exit 8`。
2. Agent API 可达：`127.0.0.1:17890`（可用 `CHROME_HOST_API_URL` / `--api-url` 改指其他地址）。
3. **版本要求**：v0.1.5 以下的应用仅支持部分命令（env CRUD、实例生命周期、扩展、runtime version），其余命令会报协议/404 类错误。

## 安装

**方式一：随桌面应用（推荐）**

v0.1.7 起 CLI 随桌面应用打包：打开 **设置 → 命令行工具 → 安装**，应用会在 `/usr/local/bin` 创建指向应用内 CLI 的符号链接（需要管理员授权）。CLI 版本随应用升级自动同步；升级后若链接失效，同页一键修复。

**方式二：从源码安装（开发者）**

```bash
cargo install --path crates/cli
```

验证：

```bash
chrome-host --version
```

## 快速上手

```bash
# 1. 创建环境（环境 = hosts 配置源 + 一组隔离 Chrome 实例的容器）
chrome-host env create staging --hosts-source-url https://example.com/hosts.txt

# 2. 创建并启动一个完全隔离的 Chrome 实例（独立 profile + hosts 注入 + 登录态克隆）
chrome-host instance create <env_id> --quiet        # 输出实例 id；创建即启动
#    ⚠ 首次运行会阻塞于内核下载（Chrome for Testing ~150MB），属正常现象

# 3. 拿到 CDP 端点，即可用任意 CDP 客户端做页面自动化
chrome-host instance cdp <ins_id> --json | jq .webSocketUrl

# 4. 在实例中打开页面（等价于新开标签页并导航）
chrome-host instance open <ins_id> https://staging.example.com

# 5. 用完清理（运行中实例自动 stop → delete）
chrome-host instance delete <ins_id> --yes
chrome-host env delete <env_id> --yes
```

## 全局参数

所有命令支持以下参数（`--json` 等输出类 flag 可放在子命令之后）：

| 参数 | 说明 |
|---|---|
| `--json` | 机器可读输出：stdout 输出纯 JSON（错误体也走 stdout）。与 `--quiet` 互斥（同时使用 → exit 2） |
| `--quiet` | 安静模式：变更类命令仅输出主实体 id（每行一个），供 `$(...)` 捕获 |
| `--verbose` | 向 stderr 追加请求诊断行（`→ GET /api/v1/... (23ms)`）。不含 query 与 body，不落任何敏感数据 |
| `--no-color` | 禁用 ANSI 颜色。当前版本本就不输出颜色（纯文本 + 表格），管道/重定向下天然无转义符 |
| `--yes` | 跳过危险操作确认。**delete 类命令专属参数**；非 TTY 环境（CI/脚本）执行 delete 时缺失则 exit 2，绝不静默执行 |
| `--api-url <URL>` | Agent API 基地址（隐藏参数）；环境变量 `CHROME_HOST_API_URL` 是其回退，优先级低于 flag |
| `--token <TOKEN>` | 远程访问令牌（隐藏参数）：经 SSH 隧道接入（目标机 127.0.0.1:17890 → 本机 17891 鉴权监听器）时必需；环境变量 `CHROME_HOST_TOKEN` 是其回退。本地直连无需 |

环境变量优先级：`--api-url` > `CHROME_HOST_API_URL` > 默认 `http://127.0.0.1:17890`；
令牌同理：`--token` > `CHROME_HOST_TOKEN` > 不携带（本地直连语义）。

**TTY 语义**：stdin 为终端时，危险操作交互询问 `[y/N]`（默认 N）；stdin 非 TTY（CI、管道、定时任务）时，缺 `--yes` 直接拒绝（exit 2），保证自动化环境零意外交互。

## 输出模式

同一命令的三种输出形态（信息等价，仅格式不同）：

```bash
$ chrome-host env list                      # table：人读表格（CJK 对齐安全）
ID                                       NAME    HOSTS-SOURCE        RUNNING  TOTAL
env_d4285b49-...                         Qapub   http://...          0        1

$ chrome-host env list --json               # json：完整结构，字段冻结
[ { "id": "env_d4285b49-...", "name": "Qapub", "hostsSourceUrl": "http://...",
    "runningInstances": 0, "totalInstances": 1, ... } ]

$ chrome-host env list --quiet              # quiet：每行一个 id
env_d4285b49-...
```

**stdout / stderr 纪律**：stdout 只有业务结果（管道另一侧永远拿到纯净数据），诊断与错误文案走 stderr。因此 `chrome-host instance list --json | jq .` 永远安全。

## Exit Code 契约

退出码是 CLI 的对外契约，脚本与 Agent 应依赖退出码而非解析文案：

| Exit | 语义 | 触发示例 |
|---:|---|---|
| 0 | 成功 | — |
| 1 | 一般错误 | 响应协议破坏、未知 5xx、doctor 存在失败检查项 |
| 2 | 参数错误 | clap 解析失败、`--json --quiet` 互斥、非 TTY 危险操作缺 `--yes`、本地校验失败（如 `instance open` 的 URL 缺协议） |
| 3 | 资源不存在 | `ENVIRONMENT_NOT_FOUND` / `INSTANCE_NOT_FOUND` / `EXTENSION_NOT_FOUND` 等 404 族 |
| 4 | 资源已存在 | 预留（服务端当前无此码，映射规则先行） |
| 5 | 运行时状态冲突 / 运行时错误 | 409 族（`INSTANCE_ALREADY_RUNNING`、`INSTANCE_NOT_RUNNING`、`PROFILE_IN_USE`、`ENVIRONMENT_HAS_RUNNING_INSTANCES`）+ 500 运行时族（`KERNEL_NOT_READY`、`INSTANCE_START_FAILED`、`CDP_PORT_UNAVAILABLE`、`CDP_CONNECTION_FAILED`、`HOSTS_FETCH_FAILED`） |
| 6 | 权限拒绝 | `EXTENSION_SYSTEM_LOCKED`（内置扩展锁定） |
| 7 | 请求校验失败 | 400 族（`INVALID_REQUEST`、`HOSTS_SOURCE_INVALID` 等） |
| 8 | 服务不可达 | Agent API 连接拒绝 / 连接超时（桌面应用未运行） |
| 9 | CLI 侧等待超时（`runtime install --timeout` 轮询超时） | 下载未在 `--timeout`（缺省 900s，0 = 无限等待）内完成；CLI 停止等待，服务端下载仍在后台进行，可用 `runtime install --cancel` 中止 |
| 10 | 未授权（401 `UNAUTHORIZED`） | 远程访问令牌缺失/错误。**隧道与桌面应用均在线**（仅凭证问题）——与 exit 8（不可达）构成三态诊断；0–9 冻结契约的追加项 |

## 错误格式

```text
# 人类模式（stderr）：带上下文与动作指引
Error: 无法连接 chrome-host Agent API（...）。请确认 chrome-host 桌面应用已运行。
Error: [EXTENSION_SYSTEM_LOCKED] 系统扩展不可禁用（HTTP 403）
```

```json
// --json 模式（stdout）：machine-readable code + message，退出码非 0
{
  "error": {
    "code": "INSTANCE_NOT_FOUND",
    "message": "实例 ins_xxx 不存在"
  }
}
```

`code` 来源：服务端业务错误透传服务端错误码（`*_NOT_FOUND` / `*_LOCKED` / `INVALID_REQUEST` …）；CLI 本地错误使用 `CLI_ERROR`（本地校验/取消）、`UNREACHABLE`（连不上）、`PROTOCOL_ERROR`（响应形状异常）。

## 命令参考

### env — 环境管理

环境是核心资源：hosts 配置源 + 实例容器。删除环境会级联删除其全部实例数据。

| 命令 | 说明 |
|---|---|
| `env list` | 全部环境（含对账后的 running/total 计数） |
| `env get <ID>` | 环境详情 |
| `env create <NAME> [--hosts-source-url <URL>] [--icon <PATH>]` | 创建环境 |
| `env update <ID> [--name] [--hosts-source-url] [--icon] [--startup-args] [--keep-alive <true\|false>]` | 部分更新：只更新传入的 flag；可空字段的**空串 = 显式置空**（如 `--hosts-source-url ""`） |
| `env delete <ID> [--yes]` | 删除环境。有运行中实例时自动 stop-all 后删除（`--yes` 直接执行；交互模式会二次确认） |
| `env activity <ID> [--limit <N>]` | 环境活动事件流（TIME/LEVEL/EVENT/TARGET/MESSAGE；`--limit` 1–200，越界由服务端 clamp） |
| `env status <ID>` | 环境实例状态计数（total/running/starting/stopped/error） |

```bash
chrome-host env create qa --hosts-source-url https://cfg.example.com/hosts?env=qa
chrome-host env update <id> --name "QA 新名称" --keep-alive true
chrome-host env delete <id> --yes
```

> `hosts-source-url` 指向返回 **hosts 格式纯文本**的 http(s) 地址，实例每次启动时拉取最新内容注入。`keep-alive` 开启后实例意外退出会自动重启（60s 窗口 3 次熔断）。

### instance — 实例管理

实例 = 独立 profile + 独立 CDP 端口的 Chrome for Testing 进程。**`create` 是「创建即启动」的复合语义**（无独立的"创建不启动"模式），首次调用可能阻塞于内核下载。

| 命令 | 说明 |
|---|---|
| `instance list [--env <ENV_ID>]` | 实例列表。缺省全量（含 `environmentName` 列，需 v0.1.5+ 应用）；`--env` 只看指定环境 |
| `instance get <ID>` | 实例详情（status/pid/cdpPort/browserVersion/profileDir） |
| `instance status <ID>` | 轻量轮询视图（id/status/pid/cdpPort/browserVersion）；全量详情（含 profileDir）用 `get` |
| `instance create <ENV_ID>` | 创建并启动。读操作族用 30s 超时，本命令**无总超时**（容忍内核下载） |
| `instance start <ID>` / `instance stop <ID>` | 启动已停止实例 / 优雅停止。`start`/`restart` 同样无总超时 |
| `instance restart <ID>` | 停止再启动（hosts 重新拉取最新） |
| `instance open <ID> <URL>` | 新开标签页导航到 URL。**仅 http/https**，缺协议在本地即拒绝（exit 2，不发请求） |
| `instance tabs <ID>` | 当前标签页列表（每项 id/type/title/url，tabId 供 navigate 使用） |
| `instance navigate <ID> <TAB_ID> <URL>` | 将 `tabs` 中的指定标签页导航到新 URL（实现为新开页并关闭旧页，返回新页 target；仅 http/https） |
| `instance focus <ID>` | 聚焦实例窗口 |
| `instance cdp <ID>` | 输出 CDP 端点（host/port/httpUrl/webSocketUrl），可直接接入 Playwright / Puppeteer / puppeteer-core |
| `instance cdp-session <ID>` | 发放 CDP 会话（远程/代理访问用，30 分钟有效）。`--quiet` 输出**完整代理 URL**（新契约，供 `CDP_BASE=$(...)` 直接捕获）；`--json` 含 sessionId/baseUrl/expiresAt/fullUrl。远程调用需 `CHROME_HOST_TOKEN` |
| `instance delete <ID> [--yes]` | 删除实例（含 profile 数据）。运行中实例：`--yes` 自动 stop → delete；stop 失败则原样报错 |

实例状态机：`starting → running → stopped`，异常路径 `error / crashed`。读路径全部经过对账（以真实进程为准，不轻信数据库）。

### extension — 扩展管理

扩展是全局资源（不属于任何环境/实例），**配置变更只影响之后启动的实例**，不热加载到运行中实例。两类：

- `system`：随应用内置（目录自动发现注册），**锁定**——不可启停/移除（403 → exit 6）
- `user`：`extension add` 注册的本地目录（Direct Mode，只引用路径不复制；源目录改动后新实例即加载最新）

| 命令 | 说明 |
|---|---|
| `extension list` | 全部扩展（type: system/user，status: ready/missing/invalid） |
| `extension get <ID>` | 扩展详情（manifest 元数据） |
| `extension add <PATH>` | 注册用户扩展目录。本地先校验目录存在且含 `manifest.json`；路径会转为绝对路径存储 |
| `extension enable <ID>` / `extension disable <ID>` | 启停。仅 user 扩展可用；system 扩展 → exit 6 |
| `extension remove <ID> [--yes]` | 移除注册项（**不删除源目录文件**） |

```bash
chrome-host extension add ~/projects/my-dev-helper
chrome-host extension disable <ext_id>     # 只影响之后启动的实例
```

### login-profile — 登录态生命周期

环境级登录态快照：`launch` 打开登录浏览器 → 人工登录后关闭浏览器 → `capture` 捕获快照 → 之后该环境 `instance create` 的实例自动克隆快照。`capture` 仅在**覆盖已有快照**（snapshotVersion > 0）时要求确认，`reset` 总是确认（非 TTY 缺 `--yes` → exit 2）；登录浏览器运行中或捕获进行中时 `capture`/`launch` 返回 409 `PROFILE_IN_USE`，CLI **原样透传服务端 message**（该码含多个子场景，不做本地翻译；exit 5）。

| 命令 | 说明 |
|---|---|
| `login-profile list` | 全部环境登录态（environmentName / status / snapshotVersion / instancesUsing） |
| `login-profile get <ENV_ID>` | 单环境登录态视图（登录浏览器运行中时附 cdpPort，即自动化登录入口） |
| `login-profile launch <ENV_ID>` | 打开登录浏览器（人工登录后关闭；视图含 browserRunning / cdpPort） |
| `login-profile capture <ENV_ID> [--yes]` | 捕获登录态快照（覆盖已有快照时确认） |
| `login-profile reset <ENV_ID> [--yes]` | 清空登录态快照（总是确认） |

### settings — 全局设置

全局应用设置为单例资源（无 id 路由）。CLI 对取值**只透传不校验**：服务端是唯一校验源（position/color 白名单、start URL 须 http(s)），非法值 → 400 → exit 7。

| 命令 | 说明 |
|---|---|
| `settings get` | 查看全局设置（developerMode / envLabelPosition / envLabelColor / defaultStartUrl） |
| `settings update [--developer-mode <BOOL>] [--env-label-position <POSITION>] [--env-label-color <COLOR>] [--start-url <URL>]` | 部分更新：只更新传入的 flag（至少传一个） |

### runtime — 内核管理

内核 = Chrome for Testing，**pinned 单版本策略**（升级 = 应用发新版），不支持自定义路径/系统 Chrome。

| 命令 | 说明 |
|---|---|
| `runtime version` | 当前内核状态：pinnedVersion / 已装 version / downloading / binaryPath |
| `runtime list` | 同数据源的列表渲染（单版本策略下的兼容命令） |
| `runtime install [--timeout <SEC>]` | 安装内核：触发服务端下载并轮询至完成（`--timeout` 缺省 900s，0 = 无限等待）；**已装则幂等短路**直接输出当前状态。轮询超时 → exit 9（服务端下载继续，可 `--cancel` 中止）；观测到下载中止且未装 → exit 5（**失败或被取消两可语义**，`runtime version` 可复核）。`runtime install --cancel [--yes]` 中止进行中的下载（无下载时服务端返回 ok=false，仍 exit 0） |

内核未安装时**首次 `instance create` 会自动触发下载**（GUI 设置页或 CLI `runtime install` 亦可手动触发）。

### status / doctor — 诊断

| 命令 | 说明 | 退出码 |
|---|---|---|
| `status` | 系统状态摘要：应用版本 / 内核 / 资源计数（environments、instances、running、extensions） | 0 / 错误码 |
| `doctor` | 全项诊断：database 完整性、内核就绪、目录可写、扩展注册表健康。失败项附**建议动作**（suggestion） | 全过 → 0；存在失败项 → **1** |

```bash
$ chrome-host doctor
✓ database — SQLite 完整性校验通过（integrity_check: ok）
✓ kernel — Chrome for Testing 131.0.6778.204 就绪: ~/Library/Application Support/com.chromehost.dev/kernel/.../chrome
✓ directories — 3 个数据目录均存在且可写
✗ extensions — 共 4 个扩展注册，1 个状态非 ready
  suggestion: 运行 chrome-host extension list 查看状态异常的扩展
1 problem(s) found.
```

## AI Agent / CI 接入

CLI 从设计之初面向 Agent 与脚本。标准接入循环：

```bash
# 0. 探活：先判断服务可用性（exit 8 = 应用未运行）
chrome-host status --json >/dev/null 2>&1 || { echo "chrome-host 未运行"; exit 1; }

# 1. 找环境：按名取 id（--quiet 输出稳定可解析）
ENV_ID=$(chrome-host env list --json | jq -r '.[] | select(.name=="staging") | .id')

# 2. 创建实例并取 CDP（创建即启动；首次可能阻塞内核下载）
INS_ID=$(chrome-host instance create "$ENV_ID" --quiet)
WS=$(chrome-host instance cdp "$INS_ID" --json | jq -r .webSocketUrl)

# 2.5（可选）需要登录态的环境：launch 打开登录浏览器人工登录，
#        关闭浏览器后 capture 快照，之后 create 的实例自动克隆登录态
# chrome-host login-profile launch "$ENV_ID"
# chrome-host login-profile capture "$ENV_ID" --yes

# 3. 交给自动化（Playwright 连接 CDP）
playwright --browser "cdp://$WS" ...

# 4. 兜底清理（不管前序成败都执行；--yes 保证非交互）
chrome-host instance delete "$INS_ID" --yes || true
```

**编写脚本的规则**：

1. **依赖退出码，不解析人类文案**——这是 exit code 表存在的原因。
2. **危险操作必带 `--yes`**——CI/管道（非 TTY）缺 `--yes` 一律 exit 2，不会意外挂起等输入。
3. **`create` 不是幂等操作**——每次调用新增一个实例；重试逻辑需先 `instance list` / `get` 判重，或接受清理兜底。
4. **409 是状态冲突信号，不是错误**——`INSTANCE_ALREADY_RUNNING`（对运行中实例 start）、`INSTANCE_NOT_RUNNING`（对停止实例 open/stop）都应按状态机处理而非盲目重试。
5. **`instance create/start/restart` 不设总超时**——首次运行可能阻塞于内核下载（约 150MB，视网络分钟级）；CI 中请为这几条命令设置作业级超时，而非依赖 CLI 超时。
6. **不要解析表格输出**——脚本一律 `--json`（结构稳定）或 `--quiet`（单列 id）。

## 安全边界

- Agent API 与 CDP 均只绑定 `127.0.0.1`，CLI 不提供任何改绑公网的参数。
- CLI 不接触、不输出任何会话数据（Cookie / Token / LocalStorage / 密码）；也没有登录/凭证管理命令（认证由网站自身负责）。
- `--verbose` 的诊断行只含 method/path/耗时，**不含 query 与请求体**（URL query 可能携带敏感参数）。
- 扩展加载路径只能来自注册表；CLI/REST 均无"按任意路径加载扩展"的能力。

## 故障排查

| 现象 | exit | 处置 |
|---|---:|---|
| `无法连接 chrome-host Agent API` | 8 | 启动桌面应用（含托盘常驻）；或用 `CHROME_HOST_API_URL` 指向正确地址 |
| `非交互环境执行危险操作需要 --yes` | 2 | 脚本中补 `--yes`；交互终端中确认提示后输入 y |
| `系统扩展不可禁用/移除` | 6 | 内置扩展是应用功能的一部分，设计上锁定；如确需调整请用 GUI 意见反馈渠道 |
| `端口 xxx 已被占用，无法启动实例` | 5 | 该实例 CDP 端口被外部进程占用；释放端口或重启应用后重试 |
| `响应不是合法 JSON` / 响应格式异常 | 1 | 应用版本与 CLI 版本差异过大（如对旧版应用用 `status`）；升级应用后重试 |
| doctor 报内核未安装 | 1 | 执行任意 `instance create` 触发自动下载，或在 GUI 设置页手动下载 |

## 兼容性承诺

- **JSON 输出字段只增不删、不改名**（camelCase 稳定契约）。新增字段属于兼容变更；破坏性变更会伴随主版本升级并在本文件与 Release Notes 中公告。
- **Exit Code 表只增不改义**。预留的 4（Already Exists）启用时不会改变既有映射；9（Timeout）已由 `runtime install --timeout` 轮询超时启用。
- 服务端错误码新增时 CLI 同步映射表（有契约测试护栏）；未知错误码按兜底规则归类（未知 4xx → 7、未知 5xx → 1），不会出现未定义退出码。
- CLI 版本与桌面应用版本独立演进；接入方建议锁定 CLI 版本并跟进 Release Notes。
