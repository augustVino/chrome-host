# chrome-host MCP 接入指南

> chrome-host 内嵌 **MCP（Model Context Protocol）over Streamable HTTP** server，随应用启动，与 REST API 同端口同进程。面向 AI 客户端（Claude / Cursor 等）：零 node 依赖，客户端只配一个 URL，无版本漂移。
>
> 工具与 REST API / CLI 业务语义完全一致（同一服务层），错误码同源。REST 参考见 [AGENT-API.md](AGENT-API.md)，命令行见 [CLI.md](CLI.md)。

## 目录

- [连接配置](#连接配置)
- [通用行为约定](#通用行为约定)
- [工具总览（24 个）](#工具总览24-个)
- [内核](#内核)
- [环境](#环境)
- [实例](#实例)
- [标签页](#标签页)
- [登录态](#登录态)
- [扩展](#扩展)
- [典型编排闭环](#典型编排闭环)

---

## 连接配置

**端点**：`http://127.0.0.1:17890/mcp`（Streamable HTTP；应用未启动时不可达）

Claude Code：

```bash
claude mcp add --transport http chrome-host http://127.0.0.1:17890/mcp
```

Claude Desktop（`claude_desktop_config.json`）：

```json
{
  "mcpServers": {
    "chrome-host": {
      "type": "http",
      "url": "http://127.0.0.1:17890/mcp"
    }
  }
}
```

其他支持 Streamable HTTP 的 MCP 客户端（Cursor、Cline 等）同理：类型 HTTP、地址 `http://127.0.0.1:17890/mcp`。

> 握手后 server name = `chrome-host`，版本随应用包版本。协议细节（initialize/会话/SSE/版本协商）由 rmcp 官方 SDK 处理，客户端无需关心。

## 通用行为约定

1. **业务错误不是协议错误**：tool 调用返回 `isError: true` + 结构化 JSON，携带 REST 同源的三元组——AI 可据此自行决策重试或换路径：

```json
{ "error": { "code": "INSTANCE_ALREADY_RUNNING", "message": "...", "status": 409 } }
```

2. **字段 camelCase**，与 REST 完全一致；`id` 前缀约定：环境 `env_`、实例 `ins_`、扩展 `ext_`。
3. **时间戳**为 Unix epoch 毫秒。
4. **内核未装时 `create_instance` 会阻塞于首次下载**（约 150MB）——复杂任务先调 `kernel_status` 预判。
5. 扩展启停只影响**之后启动的实例**；实例创建非幂等（每次新增一个）。

## 工具总览（24 个）

| 分组 | 工具 |
|---|---|
| 内核（1） | `kernel_status` |
| 环境（6） | `list_environments` · `create_environment` · `delete_environment` · `set_keep_alive` · `stop_all_for_environment` · `activity` |
| 实例（8） | `create_instance` · `list_instances` · `get_instance` · `start_instance` · `stop_instance` · `restart_instance` · `delete_instance` · `get_cdp` |
| 标签页（3） | `list_tabs` · `open_tab` · `navigate` |
| 登录态（1） | `login_profile`（action: get / launch / capture / reset） |
| 扩展（5） | `list_extensions` · `get_extension` · `register_extension` · `set_extension_enabled` · `delete_extension` |

## 内核

### `kernel_status`

查询浏览器内核状态（**无参数**）。

返回 `{ installed, version, pinnedVersion, upgradeAvailable, downloading, binaryPath }`。
`create_instance` 前置判断：`installed: false && downloading: false` 时创建会先触发下载（长阻塞）。

## 环境

| 工具 | 输入 | 说明 |
|---|---|---|
| `list_environments` | — | 全部环境（含运行中实例摘要 runningInstances/totalInstances） |
| `create_environment` | `name`；可选 `hostsSourceUrl`（返回 hosts 格式文本的 http(s) URL，不传则不做 hosts 注入） | 创建环境 |
| `delete_environment` | `environmentId` | 删除环境（有运行中实例 → 409，可先 `stop_all_for_environment`） |
| `set_keep_alive` | `environmentId`, `keepAlive: bool` | 实例意外退出自动重启（60s 窗口 3 次熔断） |
| `stop_all_for_environment` | `environmentId` | 停止环境全部运行中实例（删除环境前置操作），返回停止的 id 列表 |
| `activity` | `environmentId`；可选 `limit`（默认 50） | 环境最近事件流（创建/启动/停止/hosts 解析/快照等审计记录） |

## 实例

| 工具 | 输入 | 说明 |
|---|---|---|
| `create_instance` | `environmentId` | 创建并启动完全隔离的 Chrome 实例（独立 profile + hosts 注入 + 登录态克隆）。**首次可能阻塞于内核下载**；出错先查 `kernel_status` |
| `list_instances` | `environmentId` | 环境下全部实例（状态/端口/pid） |
| `get_instance` | `instanceId` | 单实例详情（状态/pid/CDP 端口/profile 目录） |
| `start_instance` | `instanceId` | 启动已停止实例（重复 start → 409 INSTANCE_ALREADY_RUNNING） |
| `stop_instance` | `instanceId` | 优雅终止 Chrome 进程 |
| `restart_instance` | `instanceId` | stop + start，hosts 重新拉取 |
| `delete_instance` | `instanceId` | 删除实例（运行中 → 409，须先停止） |
| `get_cdp` | `instanceId` | CDP endpoint（含 WebSocket URL，可直接连 Chrome DevTools Protocol 做页面自动化） |

## 标签页

| 工具 | 输入 | 说明 |
|---|---|---|
| `list_tabs` | `instanceId` | 全部页面标签（id/url/title，仅 page 类型） |
| `open_tab` | `instanceId`, `url`（须含协议） | 新开标签页并导航 |
| `navigate` | `instanceId`, `tabId`, `url` | 导航既有标签页（new+close 组合近似：丢失旧页历史） |

> MCP 不直接返回页面内容；读页面走 `get_cdp` 拿到 WebSocket URL 后由客户端 CDP 能力（如 Runtime.evaluate）完成，或结合 `list_tabs` 的 title/url 做轻量判断。

## 登录态

### `login_profile`

输入：`environmentId` + `action`（四选一）。环境登录态生命周期管理：

| action | 行为 | 前置/错误 |
|---|---|---|
| `get` | 查状态/快照版本/实例引用数 | — |
| `launch` | 打开登录浏览器（人工完成登录；响应含 `cdpPort`，自动化登录可用） | — |
| `capture` | 捕获快照（之后创建的实例克隆该登录态） | **登录浏览器须已关闭**，运行中 → 409 PROFILE_IN_USE |
| `reset` | 清空快照回到 not_configured | — |

典型闭环：`launch` →（人工/自动化登录）→ 关闭浏览器 → `capture` → `create_instance` 即带登录态。

## 扩展

| 工具 | 输入 | 说明 |
|---|---|---|
| `list_extensions` | — | 已注册扩展（status: ready/missing/invalid；type: system=内置锁定 / user=用户注册） |
| `get_extension` | `extensionId` | manifest 元数据详情 |
| `register_extension` | `path`（**绝对路径**，须含合法 manifest.json） | 注册用户扩展；安全边界：只接受显式注册，无按任意路径加载 |
| `set_extension_enabled` | `extensionId`, `enabled: bool` | 启停（system → 403 EXTENSION_SYSTEM_LOCKED；只影响之后启动的实例） |
| `delete_extension` | `extensionId` | 移除注册项（不删源文件；system → 403） |

## 典型编排闭环

应用内嵌的 server instructions 已向 AI 声明标准路径：

```text
1. kernel_status                       # 预判内核（未装则提示先下载）
2. list_environments                   # 选定/确认环境（没有则 create_environment）
3. create_instance                     # 创建并启动（返回 cdpPort 与完整实例）
4. open_tab → list_tabs                # 打开目标页 → 读 title/url 确认到达
5. 需要深度自动化：get_cdp 取 WebSocket URL，客户端接 CDP
6. delete_instance                     # 收尾清理（运行中先 stop_instance）
```

需要登录态的场景在第 3 步前插入：`login_profile(launch)` → 人工登录 → 关浏览器 → `login_profile(capture)`。

> 与 CLI 的取舍：MCP 适合 AI 多轮编排（工具发现 + 结构化错误自决策）；CLI 适合脚本一行式调用与 CI（退出码契约）。两者可混用，状态完全互通。
