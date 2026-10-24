# chrome-host Agent API 接入指南

> 内嵌于 chrome-host 桌面应用的 REST API（axum，随应用启动），是 GUI / MCP / CLI 三个门面共享的同一套服务层的 HTTP 视图。面向本机程序化集成：前端、脚本、Agent Runtime、CI。
>
> MCP 接入见 [MCP.md](MCP.md)；命令行见 [CLI.md](CLI.md)。三者业务语义一致，错误码同源。

## 目录

- [通用约定](#通用约定)
- [Environments — 环境](#environments--环境)
- [Login Profile — 登录态快照](#login-profile--登录态快照)
- [Instances — 实例](#instances--实例)
- [Extensions — 扩展](#extensions--扩展)
- [Settings — 应用设置](#settings--应用设置)
- [Kernel — 内核](#kernel--内核)
- [Events — 事件流（SSE）](#events--事件流sse)
- [Health — 健康检查](#health--健康检查)
- [错误码总表](#错误码总表)
- [接入注意事项](#接入注意事项)

---

## 通用约定

| 项 | 值 |
|---|---|
| Base URL | 本地 `http://127.0.0.1:17890`；远程接入另设隧道落地监听器 `127.0.0.1:17891`（同路由集 + Bearer 鉴权，见 [远程接入](#远程接入--remote-access)）。**均仅回环绑定**，无任何改绑公网的配置项 |
| 编码 | 请求/响应均为 `application/json`；字段 camelCase |
| 时间戳 | 均为 Unix epoch **毫秒**（`i64`） |
| 鉴权 | 17890 无鉴权（本机回环即信任边界）；17891 全路由要求 `Authorization: Bearer <token>`（`/cdp/` 会话路径除外——会话 key 即凭证） |
| 幂等性 | `POST .../instances` 与 `POST .../open` **非幂等**——每次调用新增一个实例 |
| 版本要求 | `GET /api/v1/instances` 与 `GET /api/v1/health` 为 v0.1.5+ 新增；其余端点历史版本可用 |

**错误格式**（所有业务错误，HTTP 状态码 + 结构化体；客户端应依赖 `code` 而非 `message`）：

```json
{ "error": { "code": "INSTANCE_NOT_RUNNING", "message": "实例 ins_xxx 未运行" } }
```

状态码语义：`400` 请求体校验失败 · `403` 权限（内置扩展锁定）· `404` 资源不存在 · `409` 运行时状态冲突 · `422` 请求语义校验失败（page 族可见性/可编辑性）· `500` 运行时错误 · `504` 工具预算超时（page 族）。完整错误码见[错误码总表](#错误码总表)。

## Environments — 环境

环境是核心资源：hosts 配置源 + 实例容器。删除环境会级联删除其全部实例记录与 profile 目录。

### `GET /api/v1/environments` — 列表（含运行摘要）

```json
[
  {
    "id": "env_d4285b49-...", "name": "Qapub",
    "hostsSourceUrl": "http://cfg.example.com/hosts?env=qa",
    "icon": null, "startupArgs": null, "keepAlive": false,
    "createdAt": 1789298923396, "updatedAt": 1789298923396,
    "runningInstances": 0, "totalInstances": 1
  }
]
```

> 列表项是 `Environment` 平铺 + `runningInstances` / `totalInstances` 两个摘要字段（已对账，以真实进程为准）。

### `POST /api/v1/environments` — 创建（`201 Created`）

```json
// 请求
{ "name": "staging", "hostsSourceUrl": "https://cfg.example.com/hosts?env=staging" }
```

`hostsSourceUrl` 可选（指向返回 **hosts 格式纯文本** 的 http(s) 地址，实例每次启动时拉取注入；非法 URL → `400 HOSTS_SOURCE_INVALID`）。

`sessionCapture`（可选，默认 `first-login`）：身份层回写策略——`off` 不保存；`first-login` 身份层为空时（**文件快照与 Cookie 保险库两层均无内容**）首个退出的实例自动保存，此后不再自动更新；`always` 每个实例退出都保存（详见下方 Login Profile 小节）。

### `GET /api/v1/environments/{id}` — 详情

返回单个 `Environment`（同上，无摘要字段）。

### `PATCH /api/v1/environments/{id}` — 部分更新

```json
// 只更新传入字段；可空字段传 null = 显式置空（与「缺省不更新」区分）
{ "name": "新名称", "hostsSourceUrl": null, "keepAlive": true }
```

| 字段 | 说明 |
|---|---|
| `name` | 改名 |
| `hostsSourceUrl` | string = 更新；`null` = 置空（不再注入 hosts） |
| `icon` / `startupArgs` | 同上置空语义；`startupArgs` 是附加 Chrome 启动参数（高级逃生口） |
| `keepAlive` | true = 该环境实例意外退出自动重启（60s 窗口 3 次熔断），开关联动运行中实例的监控注册 |
| `sessionCapture` | 身份层回写策略：`off` / `first-login` / `always` |

### `DELETE /api/v1/environments/{id}` — 删除

有运行中实例 → `409 ENVIRONMENT_HAS_RUNNING_INSTANCES`（先调 stop-all；清理途中检测到并发启动的实例同样中止并返回 409，停止后重试即收敛）。成功返回 `{ "ok": true }`。

### `GET /api/v1/environments/{id}/status` — 运行时摘要

```json
{ "environmentId": "env_...", "instances": { "total": 3, "running": 1, "starting": 0, "stopped": 2, "error": 0 } }
```

### `GET /api/v1/environments/{id}/activity?limit=50` — 审计事件流

```json
[ { "ts": 1789298923396, "level": "info", "event": "instance-created",
    "environmentId": "env_...", "targetId": "ins_...", "message": "..." } ]
```

`event` 取值：`environment-created/updated/deleted`、`instance-created/started/stopped/deleted`、hosts 解析、快照操作等；`limit` 默认 50。

### `POST /api/v1/environments/{id}/open` — 快捷创建

`create` 的别名（历史快捷端点）：同样**创建并启动一个新实例**，非"打开某 URL"。

### `POST /api/v1/environments/{id}/stop-all` — 停止全部运行中实例

```json
{ "environmentId": "env_...", "stopped": ["ins_a", "ins_b"] }
```

## Login Profile — 登录态快照

每个环境至多一个登录态母本（快照本体目录）。**主路径 = 实例即登录源**（0.2.3 起）：实例里人工登录 → 关闭实例 → SessionService 自动保存；`capture` 用于显式固化（可选 `fromInstanceId`）。母本 `launch` 登录浏览器子系统已于 0.3.0 **移除**（breaking）。**状态机**：`not_configured → ready → capturing`，失败 `error`。

| 端点 | 说明 |
|---|---|
| `GET /api/v1/login-profiles` | 全部快照列表（含环境名，供列表页） |
| `GET /api/v1/environments/{id}/login-profile` | 单环境视图（惰性建行，从未配置过也返回） |
| `POST /api/v1/environments/{id}/login-profile/capture` | 捕获快照。**登录浏览器必须已关闭**，运行中 → `409 PROFILE_IN_USE`；可选 body `{ "fromInstanceId": "ins_..." }` 从指定**已停止实例**捕获（实例即登录源，推荐路径） |
| `POST /api/v1/environments/{id}/login-profile/reset` | 清空身份层两层（Cookie 保险库含历史 + 文件快照，回到 not_configured）。遗留登录浏览器运行中 → `409 PROFILE_IN_USE`（校验先于任何清空动作） |
| `GET /api/v1/environments/{id}/session` | 会话身份层状态：策略（off/first-login/always）/当前代/来源实例/Cookie 数/历史代列表 |
| `POST /api/v1/environments/{id}/session/restore` | 回滚 Cookie 保险库到历史代：body `{ "generation": <n> }`。不存在 → `404 SESSION_GENERATION_NOT_FOUND`；仅回滚 Cookie 层，文件快照保留当前 |

视图结构：

```json
{ "id": "lp_...", "name": "...", "status": "ready", "snapshotVersion": 3,
  "instancesUsing": 2, "lastCapturedAt": 1789298923396 }
```

## Instances — 实例

实例 = 独立 profile + 独立 CDP 端口的 Chrome for Testing 进程。状态机：`starting → running → stopped`，异常 `error / crashed`。**所有读端点经过对账**（探活真实进程后回写，不轻信数据库）。

### 创建与生命周期

| 端点 | 说明 |
|---|---|
| `POST /api/v1/environments/{envId}/instances` | **创建并启动**（非幂等）。流程：分配 CDP 端口 → 克隆登录态 → hosts 拉取注入 → spawn → 等 CDP 就绪（≤10s）。首次运行可能阻塞于内核下载。`201` 返回实例 |
| `POST /api/v1/environments/{envId}/instances` + `GET` | 同路径 GET = 按环境列实例 |
| `GET /api/v1/instances` | **全量实例列表**（v0.1.5+）：`Instance` 平铺 + `environmentName` |
| `GET /api/v1/instances/{id}` | 详情 |
| `POST /api/v1/instances/{id}/start` | 启动已停止实例（重复 start → `409 INSTANCE_ALREADY_RUNNING`；hosts 重新拉取） |
| `POST /api/v1/instances/{id}/stop` | 优雅停止（SIGTERM→等待→兜底） |
| `POST /api/v1/instances/{id}/restart` | stop + start |
| `POST /api/v1/instances/{id}/focus` | 唤出窗口（CDP 激活优先，回退平台 API；未运行 → 409） |
| `DELETE /api/v1/instances/{id}` | 删除（运行中 → `409 INSTANCE_ALREADY_RUNNING`，须先 stop；清理 profile 目录） |

实例对象：

```json
{ "id": "ins_9b198afd-...", "environmentId": "env_...", "loginProfileId": null,
  "profileDir": "~/Library/.../instances/ins_...", "pid": 18231, "cdpPort": 9333,
  "status": "running", "hostRules": "...", "browserVersion": "131.0.6778.204",
  "startedAt": 1789298923396, "stoppedAt": null,
  "createdAt": 1789298923396, "updatedAt": 1789298923396 }
```

`GET /instances/{id}/status` 返回精简视图：`{ "id", "status", "pid", "cdpPort", "browserVersion" }`。

### CDP 与标签页自动化

| 端点 | 说明 |
|---|---|
| `GET /api/v1/instances/{id}/cdp` | CDP 端点。CDP 仅绑定 `127.0.0.1` |
| `GET /api/v1/instances/{id}/diagnostics` | 页面诊断快照（透传）：对第一个 http(s) 页面短连接求值 page-diagnostics 扩展挂载的 `window.__chromeHostDiagnostics`。未运行或无 http 页 → `409 DIAGNOSTICS_UNAVAILABLE` |
| `GET /api/v1/instances/{id}/console?level=&limit=` | 常驻订阅 console 环（500 条）：console API + 页面日志。level 精确匹配，limit 缺省 100 |
| `GET /api/v1/instances/{id}/network?failedOnly=&limit=` | 常驻订阅请求环（1000 条，请求完成/失败时入环）。failedOnly = 失败或 HTTP≥400 |
| `GET /api/v1/instances/{id}/screenshot` | 当前页面 png 截图（base64，复用常驻连接）。未运行/连接未就绪/无 http 页 → 409 |
| `GET /api/v1/instances/{id}/har` | 请求环生成 HAR 1.2 文档（未采集字段按规范 -1/空集占位；`_targetUrl` 标注归属页面） |
| `GET /api/v1/instances/{id}/tabs` | 页面标签列表（仅 `type: "page"`） |
| `POST /api/v1/instances/{id}/tabs` | 新开标签页并导航。请求体 `{ "url": "https://…" }`（须含协议） |
| `POST /api/v1/instances/{id}/navigate` | 导航既有标签页。请求体 `{ "tabId": "...", "url": "..." }`（new+close 组合近似，旧页历史丢失） |
| `POST /api/v1/instances/{id}/cdp/sessions` | **发放 CDP 会话**（远程/代理访问用，`201`）：`{ sessionId, baseUrl, expiresAt }`，30 分钟有效。实例须运行中（否则 409）。经 17891 访问需 Bearer token |

```json
// GET .../cdp 响应
{ "instanceId": "ins_...", "host": "127.0.0.1", "port": 9333,
  "httpUrl": "http://127.0.0.1:9333", "webSocketUrl": "ws://127.0.0.1:9333/devtools/browser/..." }

// 标签页对象
{ "id": "ABC123", "type": "page", "title": "Example Domain", "url": "https://example.com" }

// GET .../diagnostics 响应（diagnostics 为页面扩展快照透传；未注入时为 null）
{ "instanceId": "ins_...", "targetId": "ABC123", "targetUrl": "https://app.example.com/home",
  "diagnostics": {
    "startedAt": 1710000000000, "url": "https://app.example.com/home",
    "errors": [ { "ts": 1710000001000, "type": "error", "message": "boom", "source": "https://.../app.js", "line": 42 } ],
    "resources": [ { "ts": 1710000001200, "url": "https://api.example.com/x", "status": 500, "transferSize": 0, "initiatorType": "fetch", "durationMs": 12 } ],
    "lcp": 731, "fcp": 120 } }
```

diagnostics 采集规则：JS 错误（`error`/`unhandledrejection`）与失败资源（HTTP 状态 ≥400，或
`transferSize == 0`——磁盘缓存命中/被阻断的启发式信号，可能含少量噪声）各环形保留 100 条；
错误页/about:blank 等不运行 content script 的页面读不到快照。

**常驻诊断（console/network/screenshot/har）**：实例启动后 chrome-host 保持一条 browser 级
CDP 连接（`Target.setAutoAttach` flatten），**只订阅不操作**，与外部 agent 自建的 CDP 会话
互不干扰；console 环 500 条 / 请求环 1000 条，均为**进程内态，chrome-host 重启或实例重启
即清空**；请求在完成/失败时入环（in-flight 不可见）。截图取第一个 http(s) 页面 session
（隐藏页面可能拍到空白帧，与浏览器行为一致）。

拿到 `webSocketUrl` 后可直接接入 Playwright（`connectOverCDP`）/ Puppeteer / chrome-remote-interface。

### 页面控制（Page Control）

实例级常驻 CDP 连接上的页面操作面：`snap` 产生 @eN 引用，`click`/`fill`/`press`
消费引用或 CSS 选择器，`wait` 轮询等待，`dialogs` 读自动应答的对话框日志。连接
惰性建立（实例首次 page 命令时），与常驻诊断连接互不干扰；仅对运行中实例可用
（否则 `409 PAGE_SERVICE_UNAVAILABLE`）。

| 端点 | 说明 |
|---|---|
| `POST /api/v1/instances/{id}/page/snap` | AX 树快照 + @eN ref 整表替换。body `{ "tab?": ... }` |
| `POST /api/v1/instances/{id}/page/click` | 原生输入点击（三段鼠标事件，非 `el.click()`）。body `{ "target", "tab?" }` |
| `POST /api/v1/instances/{id}/page/fill` | 聚焦→全选清空→insertText→回读校验。body `{ "target", "value", "tab?" }` |
| `POST /api/v1/instances/{id}/page/press` | 键盘三段序列。body `{ "key", "target?", "tab?" }` |
| `POST /api/v1/instances/{id}/page/wait` | 轮询等待元素状态（250ms 间隔）。body `{ "selector", "state?", "timeout?", "tab?" }` |
| `GET /api/v1/instances/{id}/page/dialogs` | 已自动应答的对话框日志。query `tab?&since=` |

参数语义：

- **`tab`**：targetId 或唯一前缀（由 `GET .../tabs` 获得）；省略 = 唯一 `type:page`
  标签时直接解析，多标签 → `409 PAGE_TAB_AMBIGUOUS`；未命中 → `404 PAGE_TAB_NOT_FOUND`。
- **`target`**：`@eN` 引用（上次 snap 产生；导航/文档替换后整表失效 → `PAGE_REF_STALE`，
  需重新 snap 而非对旧引用重试）或 CSS 选择器两种形态。
- **`state`**：`visible`（缺省）/ `hidden` / `attached` / `detached`；非法 → `400 INVALID_REQUEST`。
- **`timeout`**：时长字符串（`"10s"` / `"250ms"`，裸数字 = 毫秒），缺省 10s；工具级
  预算 = timeout + 5s。
- **`since`**：对话框游标，只返回 `sequence` 严格大于 N 的条目；缓冲上限 32 条。

```json
// POST .../page/snap 响应（蛇形键对齐 TS wire 字段）
{ "instanceId": "ins_...", "targetId": "ABC123",
  "text": "[WebArea] Example Domain\n  @e1 [link] More information...",
  "refs_count": 1, "generation": 1 }

// POST .../page/click 响应（fill/press/wait 同为 result 单行形态）
{ "result": "Clicked <BUTTON> \"Save\" (native input @150,130)",
  "instanceId": "ins_...", "targetId": "ABC123" }

// 命令期间自动应答了对话框时，成功响应附 dialogs_seen（snake_case 条目，同 dialogs 端点）
{ "result": "Pressed Enter", "instanceId": "ins_...", "targetId": "ABC123",
  "dialogs_seen": [ { "tab_id": "ABC123", "type": "prompt", "message": "名称?",
    "handled": "accepted", "sequence": 1, "default_prompt": "Bob" } ] }

// GET .../page/dialogs?since=1 响应
{ "cursor": 2, "entries": [ { "tab_id": "ABC123", "type": "alert", "message": "m2",
    "handled": "accepted", "sequence": 2 } ],
  "instanceId": "ins_...", "targetId": "ABC123" }
```

对话框接管：alert/confirm/prompt/beforeunload 一律自动 `accept: true`（prompt 提交
默认文本，beforeunload = 放行离开）；应答成功才入档，缓冲按 tab 隔离、上限 32 条
FIFO。响应为服务层原样（与 console/network 等端点一致，无额外包裹层）。

## Extensions — 扩展

全局资源，不属于环境/实例。**启停/注册只影响之后启动的实例**，不热加载。两类：`system`（内置，锁定）与 `user`（注册本地目录，Direct Mode 只引用路径）。

| 端点 | 说明 |
|---|---|
| `GET /api/v1/extensions` | 列表 |
| `GET /api/v1/extensions/{id}` | 详情 |
| `POST /api/v1/extensions` | 注册：`{ "path": "/abs/path/to/ext" }`（目录须含合法 `manifest.json`，服务端读出元数据；`400` = 目录/manifest 非法） |
| `PATCH /api/v1/extensions/{id}` | `{ "enabled": false }` 启停。system → `403 EXTENSION_SYSTEM_LOCKED` |
| `DELETE /api/v1/extensions/{id}` | 移除注册项（**不删源文件**）。system → 403 |
| `GET /api/v1/extensions/{id}/config?environmentId=` | 读运行时配置 → `{ environmentId, config }`（未配置 → `config: null`；省略 query = 全局默认） |
| `PUT /api/v1/extensions/{id}/config?environmentId=` | 写运行时配置（body 即 JSON 对象，整体覆盖；仅列表中 `configurable: true` 的 system 扩展如 net-rules；体 ≤64KB；未知 environmentId → 404）。**对新启动实例生效** |

```json
{ "id": "ext_...", "name": "My Dev Helper", "description": "...", "version": "0.3.1",
  "manifestVersion": 3, "type": "user", "sourcePath": "/Users/x/projects/my-dev-helper",
  "enabled": true, "status": "ready", "configurable": false, "createdAt": ..., "updatedAt": ... }
```

`status` 是读取时按文件系统现状惰性计算的展示值：`ready` / `missing`（目录没了）/ `invalid`（manifest 损坏）；`configurable: true` 仅限声明了 configFile 的内置物化扩展（当前为 net-rules），这类扩展可经 config 端点按环境写入运行时配置。

**net-rules 配置形状**（写入时同步校验可编译，非法 → 400 `EXTENSION_CONFIG_INVALID`）：

```json
{ "block": ["*.google-analytics.com"],
  "redirect": [{ "from": "https://api.foo.com/*", "to": "http://localhost:3000/$1" }],
  "headers": [{ "url": "https://app.foo.com/*", "set": { "X-Debug-Uid": "42" } }] }
```

- `block`/`headers[].url`：域锚定匹配，可选 scheme 前缀与路径通配（`*.` 与裸域名均匹配域及全部子域）；
- `redirect[].from`：全量 glob，`*` 依序对应 `$1`、`$2`…（支持 `$1`–`$9`）；`to` 中的 `$N` 展开为第 N 个捕获组（无数字后缀的 `$` 为字面量），**引用不存在的组 → 写入/编译期 400 拒绝**；
- `headers[].set`：注入请求头（服务端可见）；
- 同一 URL 同时命中 `block` 与 `redirect` 时 **block 确定性优先**（更严格的动作胜出）；
- 规则在浏览器层（DNR）生效，service worker 休眠不影响拦截。

## Settings — 应用设置

| 端点 | 说明 |
|---|---|
| `GET /api/v1/settings` | 读取 |
| `PUT /api/v1/settings` | 部分更新（只更新传入字段） |

```json
{ "developerMode": true, "envLabelPosition": "top-right",
  "envLabelColor": "#FF4D4F", "defaultStartUrl": "https://example.com",
  "remoteAccessEnabled": false, "remoteAccessSshTarget": "", "remoteAccessToken": "" }
```

远程接入三字段（v0.2+）：`remoteAccessEnabled` 开关（开启 ⇒ `remoteAccessSshTarget` 恒非空，服务端校验）；`remoteAccessToken` 只读呈现（首次开启自动生成，轮换走 `POST /api/v1/remote-access/token`，不接受 PUT 写入）。

## Kernel — 内核

Chrome for Testing，**pinned 单版本策略**（不支持自定义路径/系统 Chrome）。

| 端点 | 说明 |
|---|---|
| `GET /api/v1/kernel/status` | 内核状态 |
| `POST /api/v1/kernel/download` | 触发后台下载（已在下载中返回 `ok: false`） |
| `POST /api/v1/kernel/cancel` | 取消下载 |

```json
{ "installed": true, "version": "131.0.6778.204", "pinnedVersion": "131.0.6778.204",
  "upgradeAvailable": false, "downloading": false,
  "binaryPath": "~/Library/.../kernel/chrome-for-testing/131.../chrome" }
```

首次 `POST .../instances` 若内核未装会**同步阻塞于下载**（集成方应先查 kernel/status 或设作业级超时）。

## Events — 事件流（SSE）

`GET /api/v1/events`（`text/event-stream`）：服务端推送业务事件——与 `GET /environments/{id}/activity`
同源（`Activity::record` 落库行），但为**订阅**形态：事件发生即推送，无需轮询。

| 端点 | 说明 |
|---|---|
| `GET /api/v1/events` | 事件流。`event:` 为事件名（下表），`data:` 为落库行同款富 JSON（比 Tauri 事件多 `level`/`ts`/`targetId`） |

```json
// data 载荷（AppEvent 行）
{ "ts": 1710000000000, "level": "info", "event": "instance-started",
  "environmentId": "env_...", "targetId": "ins_...", "message": "" }
```

```bash
curl -N http://127.0.0.1:17890/api/v1/events      # 挂住订阅（-N 禁用缓冲）
# 另一终端 create instance 后依次收到：
# event: instance-created
# data: {"ts":...,"level":"info","event":"instance-created",...}
# event: instance-started
# data: {...}
```

事件清单（`event:` 值）：

| 类别 | 事件 |
|---|---|
| 环境 | `environment-created` · `environment-updated` · `environment-deleted` · `hosts-resolved` |
| 实例 | `instance-created` · `instance-started` · `instance-stopped` · `instance-error` · `instance-deleted` · `instance-crashed`（keep-alive 捕获） |
| 登录态 | `login-profile-changed` · `session_cookies_exported`（实例停止时导出 N 条） · `session_captured_from_instance`（按策略/显式固化新代） · `session_promote_failed`（自动升级失败，warn） · `session_restored`（回滚到历史代） · `session_reset`（清空身份层） |
| 远程接入 | `tunnel_state_change` · `remote_access_token_rotated`（令牌轮换） |
| CDP 会话 | `cdp_session_created` · `cdp_session_expired` |

行为语义：

- **心跳**：空闲时每 15s 发 `: ping` 注释行（SSH 隧道/代理空闲掐连的兜底）。
- **滞后**：订阅者消费过慢（服务端环形缓冲 256 条被冲掉）时收到 `: lagged n` 注释行，
  丢弃的只是该订阅者的追赶窗口，不影响其他订阅者与事件落库。
- **断开**：客户端断开即自动退订（无泄漏）；重连后从当下开始接收，**历史不回放**
  （需要历史走 activity 查询）。
- **鉴权**：17890 免鉴权；17891 与其余 API 相同的 Bearer token 中间件覆盖。

## Health — 健康检查

`GET /api/v1/health`（v0.1.5+）：探活 + 诊断数据源。

```json
{
  "version": "0.1.5", "ok": true,
  "checks": [
    { "name": "database", "ok": true, "detail": "SQLite 完整性校验通过（integrity_check: ok）", "suggestion": null },
    { "name": "kernel", "ok": true, "detail": "Chrome for Testing 131.0.6778.204 就绪: ...", "suggestion": null },
    { "name": "directories", "ok": true, "detail": "3 个数据目录均存在且可写", "suggestion": null },
    { "name": "extensions", "ok": true, "detail": "共 4 个扩展注册，0 个状态非 ready", "suggestion": null }
  ],
  "counts": { "environments": 2, "instances": 1, "running": 0, "extensions": 4 }
}
```

各项检查独立执行不短路；失败项 `ok: false` 且 `suggestion` 给出建议动作。

## 远程接入（Remote Access）

面向云桌面 / 远程机器上的 AI Agent：应用守护一条 SSH 反向隧道（唯一转发
`-R 17890 → 本机 17891`），远程机器上的客户端访问**自己的** `127.0.0.1:17890`
即等价于访问本机 API。凭证两级：

| 层 | 凭证 | 作用域 |
|---|---|---|
| 控制面（REST / MCP） | Bearer token（Settings 页生成/轮换） | 全部 API |
| 数据面（CDP 代理 `/cdp/*`） | 会话 key（URL 路径内） | 单实例 + 30 分钟 |

| 端点 | 说明 |
|---|---|
| `GET /api/v1/remote-access/status` | 隧道实时状态：`{ enabled, target, state: disabled/connecting/connected/reconnecting, lastError, since, pid, restarts }` |
| `POST /api/v1/remote-access/token` | 轮换访问令牌（旧令牌立即失效；同时清空全部活跃 CDP 会话） |
| `POST /api/v1/instances/{id}/cdp/sessions` | 发放 CDP 会话（见 Instances 节） |
| `GET /cdp/{ins}/{sid}/json*`、`WS /cdp/{ins}/{sid}/devtools/*` | CDP 会话代理（白名单端点；变更类不代理） |

远程探活三态（CLI 退出码）：连接拒绝 = 隧道断或应用停（exit 8）；`401 UNAUTHORIZED` = **隧道与应用均在线**、仅凭证问题（CLI exit 10）；正常 = exit 0。

典型远程闭环（目标机器上）：

```bash
export CHROME_HOST_TOKEN=<Settings 页令牌>
chrome-host status                                          # exit 0 = 隧道+应用在线
chrome-host instance cdp-session <ins_id> --quiet           # 输出代理 URL
CDP_BASE=$(chrome-host instance cdp-session <ins_id> --quiet) cdp.mjs shot <target>
```

## 错误码总表

| HTTP | code | 场景 |
|---|---|---|
| 400 | `INVALID_REQUEST` | 请求体校验失败（含非代理端点访问 `/cdp/*`） |
| 400 | `HOSTS_SOURCE_INVALID` | hosts 配置源 URL 非法 |
| 400 | `REMOTE_ACCESS_TARGET_INVALID` | 开启远程接入时 SSH 目标缺失/非法（含空白、`-` 开头、超长；已开启时置空） |
| 400 | `EXTENSION_CONFIG_INVALID` | 扩展配置写入非法（非 configFile 扩展 / 非 JSON 对象 / >64KB / net-rules 规则不可编译） |
| 401 | `UNAUTHORIZED` | 17891 访问缺少/错误 Bearer token（隧道与应用均在线的信号） |
| 404 | `SESSION_NOT_FOUND` | CDP 会话不存在或已过期（30 分钟），或实例与会话不匹配 |
| 403 | `EXTENSION_SYSTEM_LOCKED` | 内置扩展不可启停/移除 |
| 404 | `ENVIRONMENT_NOT_FOUND` / `INSTANCE_NOT_FOUND` / `EXTENSION_NOT_FOUND` / `LOGIN_PROFILE_NOT_FOUND` | 资源不存在 |
| 409 | `ENVIRONMENT_HAS_RUNNING_INSTANCES` | 删环境前须 stop-all |
| 409 | `INSTANCE_ALREADY_RUNNING` | 重复 start；删除运行中实例 |
| 409 | `INSTANCE_NOT_RUNNING` | 对停止实例 open/stop/焦点操作 |
| 409 | `DIAGNOSTICS_UNAVAILABLE` | 诊断查询（快照/console/network/screenshot/har）：实例未运行、常驻连接未就绪，或无 http(s) 页面目标 |
| 409 | `PROFILE_IN_USE` | 快照操作进行中，或遗留登录浏览器（旧版本 launch 遗留）运行中 |
| 409 | `SESSION_CAPTURE_FAILED` | 从实例捕获登录态失败（实例目录无导出文件，或导出 Cookie 为空） |
| 404 | `SESSION_GENERATION_NOT_FOUND` | session/restore 目标历史代不存在 |
| 400 | `PAGE_KEY_UNKNOWN` | page press 键名不认识（支持命名键/单字符/组合键） |
| 404 | `PAGE_TAB_NOT_FOUND` / `PAGE_REF_NOT_FOUND` / `PAGE_ELEMENT_NOT_FOUND` | page 族：tab 路由未命中 / @eN 引用不在上次 snap 表 / 选择器未命中 |
| 409 | `PAGE_TAB_AMBIGUOUS` / `PAGE_SERVICE_UNAVAILABLE` | page 族：tab 前缀多匹配（含省略 tab 且多页面标签）/ 实例未运行或无 CDP 端口 |
| 409 | `PAGE_REF_STALE` / `PAGE_TARGET_MOVED` | @eN 引用已因导航/文档替换失效（重 snap）/ 引用节点不再可解析 |
| 409 | `PAGE_ELEMENT_BLOCKED` / `PAGE_FILL_VALUE_MISMATCH` / `PAGE_WAIT_TIMEOUT` | 点击点被遮挡 / fill 回读不一致（写入可能已部分生效，先观察再决定）/ 等待超时 |
| 409 | `PAGE_FOCUS_FAILED` / `PAGE_FILL_FOCUS_LOST` | page press/fill 焦点失败 / 页面在 fill 过程中抢走焦点 |
| 422 | `PAGE_ELEMENT_NOT_VISIBLE` / `PAGE_TARGET_NOT_FILLABLE` | 元素不可见 / fill 目标不可编辑 |
| 500 | `PAGE_DISPATCH_FAILED` | Input 派发失败（message 附 effect_state：`none`=未发出可重试 / `unknown`=已发出禁盲目重试） |
| 500 | `PAGE_EVAL_FAILED` / `CDP_COMMAND_FAILED` | page 工具页内 eval 失败 / CDP 发送失败 |
| 504 | `PAGE_TOOL_TIMEOUT` | page 工具超出预算（常规 30s；wait = 用户 timeout + 5s） |
| 409 | `CDP_PORT_UNAVAILABLE` | 实例 start 时 CDP 端口被外部进程占用 |
| 500 | `CDP_PORT_UNAVAILABLE` | 创建实例时端口窗口耗尽（9222–9421 全部不可用） |
| 500 | `KERNEL_NOT_READY` | 内核未就绪（未安装/下载中） |
| 500 | `INSTANCE_START_FAILED` / `CDP_CONNECTION_FAILED` / `HOSTS_FETCH_FAILED` | 启动/CDP/hosts 拉取失败 |
| 500 | `INTERNAL_ERROR` | 未分类（附 message） |

## 接入注意事项

1. **依赖 `code` 不解析 `message`**——message 是给人看的，措辞可能调整；code 与 HTTP 状态码是稳定契约。
2. **`POST .../instances` 非幂等**——重试前先 `GET /instances/{id}` 或按环境列表判重，否则会堆积实例。
3. **409 是状态机信号**——按语义处理（先 stop 再删、先关浏览器再 capture），不是可盲目重试的错误。
4. **创建实例可能长阻塞**——内核下载（首次约 150MB）+ CDP 就绪等待（≤10s）；集成方设置作业级超时。
5. **CORS 为 Origin 白名单**（仅 GUI webview 与本地 dev vite 源）——本机网页不再能跨源读此 API；无 Origin 头的客户端（curl/CLI/MCP）不受影响。仍请勿将 17890/17891 端口做端口转发/容器映射暴露到本机之外；远程接入只经应用自身守护的 SSH 隧道（目标机器回环 ≡ 本机回环，与手工隧道同语义）。
6. **前端即第一接入方**——chrome-host GUI 自身全部走这套 API（Tauri IPC 只做窗口/托盘壳），遇到行为疑问可直接参照前端 `src/api/` 的调用方式。
