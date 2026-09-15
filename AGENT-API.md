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
- [Health — 健康检查](#health--健康检查)
- [错误码总表](#错误码总表)
- [接入注意事项](#接入注意事项)

---

## 通用约定

| 项 | 值 |
|---|---|
| Base URL | `http://127.0.0.1:17890`（**仅回环绑定**，无任何改绑公网的配置项） |
| 编码 | 请求/响应均为 `application/json`；字段 camelCase |
| 时间戳 | 均为 Unix epoch **毫秒**（`i64`） |
| 鉴权 | 无（回环地址即信任边界；不要将端口暴露到网络） |
| 幂等性 | `POST .../instances` 与 `POST .../open` **非幂等**——每次调用新增一个实例 |
| 版本要求 | `GET /api/v1/instances` 与 `GET /api/v1/health` 为 v0.1.5+ 新增；其余端点历史版本可用 |

**错误格式**（所有业务错误，HTTP 状态码 + 结构化体；客户端应依赖 `code` 而非 `message`）：

```json
{ "error": { "code": "INSTANCE_NOT_RUNNING", "message": "实例 ins_xxx 未运行" } }
```

状态码语义：`400` 请求体校验失败 · `403` 权限（内置扩展锁定）· `404` 资源不存在 · `409` 运行时状态冲突 · `500` 运行时错误。完整错误码见[错误码总表](#错误码总表)。

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

### `DELETE /api/v1/environments/{id}` — 删除

有运行中实例 → `409 ENVIRONMENT_HAS_RUNNING_INSTANCES`（先调 stop-all）。成功返回 `{ "ok": true }`。

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

每个环境至多一个登录态母本：`launch` 打开独立登录浏览器（人工完成登录）→ 关闭浏览器 → `capture` 捕获快照 → 之后创建的实例克隆该快照。**状态机**：`not_configured → ready → capturing`，失败 `error`。

| 端点 | 说明 |
|---|---|
| `GET /api/v1/login-profiles` | 全部快照列表（含环境名，供列表页） |
| `GET /api/v1/environments/{id}/login-profile` | 单环境视图（惰性建行，从未配置过也返回） |
| `POST /api/v1/environments/{id}/login-profile/launch` | 打开登录浏览器（人工登录）。响应含 `cdpPort`，自动化登录可用 |
| `POST /api/v1/environments/{id}/login-profile/capture` | 捕获快照。**登录浏览器必须已关闭**，运行中 → `409 PROFILE_IN_USE` |
| `POST /api/v1/environments/{id}/login-profile/reset` | 清空快照回到 not_configured |

视图结构：

```json
{ "id": "lp_...", "name": "...", "status": "ready", "snapshotVersion": 3,
  "instancesUsing": 2, "lastCapturedAt": 1789298923396,
  "browserRunning": false, "cdpPort": null }
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
| `GET /api/v1/instances/{id}/tabs` | 页面标签列表（仅 `type: "page"`） |
| `POST /api/v1/instances/{id}/tabs` | 新开标签页并导航。请求体 `{ "url": "https://…" }`（须含协议） |
| `POST /api/v1/instances/{id}/navigate` | 导航既有标签页。请求体 `{ "tabId": "...", "url": "..." }`（new+close 组合近似，旧页历史丢失） |

```json
// GET .../cdp 响应
{ "instanceId": "ins_...", "host": "127.0.0.1", "port": 9333,
  "httpUrl": "http://127.0.0.1:9333", "webSocketUrl": "ws://127.0.0.1:9333/devtools/browser/..." }

// 标签页对象
{ "id": "ABC123", "type": "page", "title": "Example Domain", "url": "https://example.com" }
```

拿到 `webSocketUrl` 后可直接接入 Playwright（`connectOverCDP`）/ Puppeteer / chrome-remote-interface。

## Extensions — 扩展

全局资源，不属于环境/实例。**启停/注册只影响之后启动的实例**，不热加载。两类：`system`（内置，锁定）与 `user`（注册本地目录，Direct Mode 只引用路径）。

| 端点 | 说明 |
|---|---|
| `GET /api/v1/extensions` | 列表 |
| `GET /api/v1/extensions/{id}` | 详情 |
| `POST /api/v1/extensions` | 注册：`{ "path": "/abs/path/to/ext" }`（目录须含合法 `manifest.json`，服务端读出元数据；`400` = 目录/manifest 非法） |
| `PATCH /api/v1/extensions/{id}` | `{ "enabled": false }` 启停。system → `403 EXTENSION_SYSTEM_LOCKED` |
| `DELETE /api/v1/extensions/{id}` | 移除注册项（**不删源文件**）。system → 403 |

```json
{ "id": "ext_...", "name": "My Dev Helper", "description": "...", "version": "0.3.1",
  "manifestVersion": 3, "type": "user", "sourcePath": "/Users/x/projects/my-dev-helper",
  "enabled": true, "status": "ready", "createdAt": ..., "updatedAt": ... }
```

`status` 是读取时按文件系统现状惰性计算的展示值：`ready` / `missing`（目录没了）/ `invalid`（manifest 损坏）。

## Settings — 应用设置

| 端点 | 说明 |
|---|---|
| `GET /api/v1/settings` | 读取 |
| `PUT /api/v1/settings` | 部分更新（只更新传入字段） |

```json
{ "developerMode": true, "envLabelPosition": "top-right",
  "envLabelColor": "#FF4D4F", "defaultStartUrl": "https://example.com" }
```

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

## 错误码总表

| HTTP | code | 场景 |
|---|---|---|
| 400 | `INVALID_REQUEST` | 请求体校验失败 |
| 400 | `HOSTS_SOURCE_INVALID` | hosts 配置源 URL 非法 |
| 403 | `EXTENSION_SYSTEM_LOCKED` | 内置扩展不可启停/移除 |
| 404 | `ENVIRONMENT_NOT_FOUND` / `INSTANCE_NOT_FOUND` / `EXTENSION_NOT_FOUND` / `LOGIN_PROFILE_NOT_FOUND` | 资源不存在 |
| 409 | `ENVIRONMENT_HAS_RUNNING_INSTANCES` | 删环境前须 stop-all |
| 409 | `INSTANCE_ALREADY_RUNNING` | 重复 start；删除运行中实例 |
| 409 | `INSTANCE_NOT_RUNNING` | 对停止实例 open/stop/焦点操作 |
| 409 | `PROFILE_IN_USE` | 登录浏览器未关闭即 capture |
| 409 | `CDP_PORT_UNAVAILABLE` | 实例 CDP 端口被外部进程占用 |
| 500 | `KERNEL_NOT_READY` | 内核未就绪（未安装/下载中） |
| 500 | `INSTANCE_START_FAILED` / `CDP_CONNECTION_FAILED` / `HOSTS_FETCH_FAILED` | 启动/CDP/hosts 拉取失败 |
| 500 | `INTERNAL_ERROR` | 未分类（附 message） |

## 接入注意事项

1. **依赖 `code` 不解析 `message`**——message 是给人看的，措辞可能调整；code 与 HTTP 状态码是稳定契约。
2. **`POST .../instances` 非幂等**——重试前先 `GET /instances/{id}` 或按环境列表判重，否则会堆积实例。
3. **409 是状态机信号**——按语义处理（先 stop 再删、先关浏览器再 capture），不是可盲目重试的错误。
4. **创建实例可能长阻塞**——内核下载（首次约 150MB）+ CDP 就绪等待（≤10s）；集成方设置作业级超时。
5. **CORS 现状为放行任意 Origin**——本机任意网页在浏览器内可访问此 API（CSRF 面）。请勿将 17890 端口做端口转发/容器映射暴露到本机之外。
6. **前端即第一接入方**——chrome-host GUI 自身全部走这套 API（Tauri IPC 只做窗口/托盘壳），遇到行为疑问可直接参照前端 `src/api/` 的调用方式。
