# chrome-host MCP 接入指南

> chrome-host 内嵌 **MCP（Model Context Protocol）over Streamable HTTP** server，随应用启动，与 REST API 同端口同进程。面向 AI 客户端（Claude / Cursor 等）：零 node 依赖，客户端只配一个 URL，无版本漂移。
>
> 工具与 REST API / CLI 业务语义完全一致（同一服务层），错误码同源。REST 参考见 [AGENT-API.md](AGENT-API.md)，命令行见 [CLI.md](CLI.md)。

## 目录

- [连接配置](#连接配置)
- [通用行为约定](#通用行为约定)
- [工具总览（57 个）](#工具总览57-个)
- [内核](#内核)
- [环境](#环境)
- [实例](#实例)
- [标签页](#标签页)
- [页面控制](#页面控制)
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

远程接入（云桌面，经 SSH 隧道）：客户端需携带访问令牌——

```bash
claude mcp add --transport http --header "Authorization: Bearer <token>" chrome-host http://127.0.0.1:17890/mcp
```

> 握手后 server name = `chrome-host`，版本随应用包版本。协议细节（initialize/会话/SSE/版本协商）由 rmcp 官方 SDK 处理，客户端无需关心。

## 通用行为约定

1. **业务错误不是协议错误**：tool 调用返回 `isError: true` + 结构化 JSON，携带 REST 同源的三元组——AI 可据此自行决策重试或换路径：

```json
{ "error": { "code": "INSTANCE_ALREADY_RUNNING", "message": "...", "status": 409 } }
```

2. **字段 camelCase**，与 REST 完全一致；`id` 前缀约定：环境 `env_`、实例 `ins_`、扩展 `ext_`。
3. **时间戳**为 Unix epoch 毫秒。
4. **内核未装时 `create_instance` 会阻塞于首次下载**（约 150MB）——复杂任务先调 `kernel_status` 预判。
5. 扩展启停只影响**之后启动的实例**；实例创建非幂等（每次新增一个）。可选 `name` 在同环境内唯一——重名创建会得到 409，可先 `list_instances` 查名再建。

- [工具总览（57 个）](#工具总览57-个)

| 分组 | 工具 |
|---|---|
| 内核（1） | `kernel_status` |
| 环境（6） | `list_environments` · `create_environment` · `delete_environment` · `set_keep_alive` · `stop_all_for_environment` · `activity` |
| 实例（9） | `create_instance` · `list_instances` · `get_instance` · `start_instance` · `stop_instance` · `restart_instance` · `delete_instance` · `get_cdp` · `create_cdp_session` |
| 常驻诊断（6） | `get_diagnostics` · `get_console` · `get_network` · `get_network_body` · `get_screenshot` · `get_har` |
| 标签页（3） | `list_tabs` · `open_tab` · `navigate` |
- [工具总览（57 个）](#工具总览57-个)
| 页面控制（23） | `page_snap` · `page_click` · `page_fill` · `page_press` · `page_wait` · `page_dialogs` · `page_eval` · `page_raw` · `page_html` · `page_clickxy` · `page_loadall` · `page_type` · `page_navigate` · `page_back` · `page_forward` · `page_reload` · `page_screenshot` · `page_pdf` · `page_emulate` · `page_extract` · `page_download_wait` · `page_downloads` · `page_upload` |
| 登录态（2） | `login_profile`（action: get / capture / reset） · `session_status` |
| 扩展（7） | `list_extensions` · `get_extension` · `register_extension` · `set_extension_enabled` · `delete_extension` · `get_extension_config` · `set_extension_config` |

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
| `create_instance` | `environmentId`；可选 `name`（1-64 字符、非纯空白；同环境唯一，重名 → 409 `INSTANCE_NAME_CONFLICT`） | 创建并启动完全隔离的 Chrome 实例（独立 profile + hosts 注入 + 登录态克隆）。**首次可能阻塞于内核下载**；出错先查 `kernel_status`。多任务并行时建议命名，便于按任务锚定（返回体与 list 均携带 name） |
| `list_instances` | `environmentId` | 环境下全部实例（name/状态/端口/pid） |
| `get_instance` | `instanceId` | 单实例详情（name/状态/pid/CDP 端口/profile 目录） |
| `start_instance` | `instanceId` | 启动已停止实例（重复 start → 409 INSTANCE_ALREADY_RUNNING） |
| `stop_instance` | `instanceId` | 优雅终止 Chrome 进程 |
| `restart_instance` | `instanceId` | stop + start，hosts 重新拉取 |
| `delete_instance` | `instanceId` | 删除实例（运行中 → 409，须先停止） |
| `create_cdp_session` | `instanceId` | 发放 CDP 会话（远程/代理访问用，30 分钟有效）→ `{ sessionId, baseUrl, expiresAt }`。远程客户端以 baseUrl 代理访问 CDP（`/json/version` 与 `/devtools/*` WS，webSocketDebuggerUrl 已改写）；本地直连场景优先 `get_cdp` |
| `get_cdp` | `instanceId` | CDP endpoint（含 WebSocket URL，可直接连 Chrome DevTools Protocol 做页面自动化） |
| `get_console` | `instanceId`；可选 `level`、`limit` | 常驻 console 环（500 条，进程内态重启即清）：console API + 页面日志。响应 meta 带证据边界（dropped/capacity/since/live） |
| `get_network` | `instanceId`；可选 `failedOnly`、`limit` | 常驻请求环（1000 条，完成/失败时入环，进行中不可见——meta `scope:"completed"`）。完整时序用 `get_har` |
| `get_network_body` | `instanceId`, `requestId`；可选 `offset`、`limit` | 分页读响应体（脱敏后；UTF-16 单位计量不劈代理对，limit 缺省 8192/上限 65536，truncated=true 时 offset+=returned 续读）。仅 bodyState available/partial 可读：404 = requestId 不在环（进程内态重启即清）；409 = 状态不可读（message 携带 bodyState/bodyReason，如 mime/timeout/no-data/not-text/not-finished/redact-failed/evicted） |
| `get_screenshot` | `instanceId` | 当前页面 png 截图（base64，复用常驻连接；未运行/未就绪/无 http 页 → 409） |
| `get_har` | `instanceId` | 请求环生成 HAR 1.2 文档（`_targetUrl` 标注归属页面；未采集字段按规范占位，语义见顶层 `_meta.uncollected`） |
| `get_diagnostics` | `instanceId` | 当前页面诊断快照（JS 错误/失败资源各环形 100 条 + LCP/FCP；对第一个 http(s) 页面短连接求值页面扩展）。未运行或无 http 页 → 409 `DIAGNOSTICS_UNAVAILABLE`；`diagnostics: null` = 页面未注入 |

## 标签页

| 工具 | 输入 | 说明 |
|---|---|---|
| `list_tabs` | `instanceId` | 全部页面标签（id/url/title，仅 page 类型） |
| `open_tab` | `instanceId`, `url`（须含协议） | 新开标签页并导航 |
| `navigate` | `instanceId`, `tabId`, `url` | 导航既有标签页（new+close 组合近似：丢失旧页历史；**推荐改用 `page_navigate`**——同 tab 内导航保留历史，本工具保留给兼容） |

> MCP 读页面内容优先 `page_snap`（AX 文本树 + @eN 引用）；页内求值/读 HTML
> 已内建为 `page_eval` / `page_html`（实例级常驻连接，无需自建 WS）；需要完全
> 自由的 CDP 会话（Network 域等）时走 `get_cdp` 拿 WebSocket URL 由客户端连接，
> 或结合 `list_tabs` 的 title/url 做轻量判断。

## 页面控制

实例级常驻连接上的页面操作面（与 CLI `page` 命令族、REST `/instances/{id}/page/*`
同源）。操作循环：`page_snap` 拿 @eN 引用 → `page_click`/`page_fill`/`page_press`
用引用操作 → 页面变化后重新 `page_snap`（导航后引用整表失效 → 409 `PAGE_REF_STALE`，
重 snap 而非对旧引用重试）。**跨域 iframe（OOPIF）**：snap 的 `[frame]` 段内嵌
iframe 内可交互元素（@eN 跨 frame 连续编号），click 走坐标投影、fill/press 走
帧会话——帧内 @eN 直接可用；单帧导航/移除只失效该帧内引用（主 frame 引用
不受影响）；CSS 选择器仍只查主 frame；帧内 hover 标记不适用（迭代边界）。
对话框（alert/confirm/prompt/beforeunload）自动
accept，命令期间新弹的附在成功响应 `dialogs_seen`，历史查 `page_dialogs`。
`page_eval`/`page_html`/`page_clickxy`/`page_loadall`/`page_type` 为 cdp.mjs
退役补齐的兑底五件：eval 是高权限原语（可执行任意 JS，与退役前 cdp.mjs 同
权限——仅在 page 原语不够用时使用）；跨域 iframe（OOPIF）内元素已支持：snap 以 `[frame]` 段内嵌帧内容、@eN 跨 frame 连续编号，click（坐标投影）/fill/press（帧会话）对帧内 @eN 直接可用；CSS 选择器仍只查主 frame，帧内坐标级兜底仍可用（`page_clickxy` 点入获得焦点 → `page_type` 输入）。
`page_navigate`/`page_back`/`page_forward`/`page_reload` 为导航四件套：同一
标签的 page session 内导航/历史回退/重载，**保留历史**（实例级 `navigate` 的
new+close 组合近似会丢历史）；等待 load 至多 25s，未证实 → 响应 `loaded:false`
（已开始而非失败，用 `page_snap`/`page_wait` 观察）。SPA 路由切换（pushState）
不触发 load，同文档历史条目以历史索引 + readyState 为完成信号。

| 工具 | 输入 | 说明 |
|---|---|---|
| `page_snap` | `instanceId`；可选 `tab` | AX 树快照：精简文本树（@e1..@eN 内联）+ `refs_count` + `generation` + hover 感知（行内 `[hover]` = hover 触发器 / `[hover-reveal]` = hover 后才变化；meta `hoverScan`：ok/truncated(预算截断 32 表/2MiB/256 规则)/skipped(无 hover 规则)/error） |
| `page_click` | `instanceId`, `target`；可选 `tab` | 原生输入点击；target 支持 @eN 引用（导航后失效需重新 snap）与 CSS 选择器 |
| `page_fill` | `instanceId`, `target`, `value`；可选 `tab` | 聚焦→清空→输入→回读校验；空 value = 仅清空 |
| `page_press` | `instanceId`, `key`；可选 `target`, `tab` | 三段键盘事件（组合键如 Control+A）；target 先聚焦 |
| `page_wait` | `instanceId`, `selector`；可选 `state`, `timeout`, `tab` | 轮询等待（250ms）；timeout 如 `"10s"`（裸数字 = ms），缺省 10s |
| `page_dialogs` | `instanceId`；可选 `tab`, `since` | 已自动应答的对话框日志（cursor + entries；since 游标增量拉取，缓冲 32 条） |
| `page_eval` | `instanceId`, `expression`；可选 `tab` | ⚠ 高权限：任意 JS 求值，仅在 page 原语不够用时用；对象结果 = pretty JSON 字符串；页面抛错 → 500 `PAGE_EVAL_FAILED` |
| `page_raw` | `instanceId`, `method`；可选 `params`（对象，缺省 {}）、`tab`、`browser`（bool，true = browser 级） | 任意 CDP 命令透传（⚠ 最高权限逃生舱）：返回 CDP 原始 result。仅 page 原语不够用时使用 |
| `page_html` | `instanceId`；可选 `selector`, `tab` | 读 outerHTML（selector 缺省 = 整文档；未命中 → 404）；大页面响应很大，优先 snap 定位后缩小范围 |
| `page_clickxy` | `instanceId`, `x`, `y`；可选 `tab` | 视口坐标点击（CSS 像素；无定位/遮挡校验，坐标即意图）；跨域 iframe 元素兜底 |
| `page_loadall` | `instanceId`, `selector`；可选 `interval`, `maxClicks`, `tab` | 循环点击直到消失（interval 缺省 `"1500ms"`，maxClicks 缺省 200）；达上限仍未消失时结果明示 still present |
| `page_type` | `instanceId`, `text`；可选 `tab` | 向当前焦点元素直接输入（无定位无校验）；跨域 iframe 内坐标级兜底（@eN 已支持帧直达，selector 不可达帧内时先 `page_clickxy` 点入）；空文本 → 400 |
- [工具总览（57 个）](#工具总览57-个)
| `page_download_wait` | `instanceId`；可选 `timeoutMs`（缺省 30000）、`tab` | 等待**下一次**下载完成（命令式：先调本工具再另行 `page_click` 触发；等待期不阻塞其他命令）。返回 filePath/fileName/bytes（落盘在实例 profile 的 downloads/）；失败/取消 → 500 PAGE_DOWNLOAD_FAILED；超时 → 409 PAGE_DOWNLOAD_TIMEOUT |
| `page_downloads` | `instanceId`；可选 `tab` | 下载终态记录环（最近 20 条，含已消费——matched 区分；并发下载未被等待者拿走的在此可查） |
| `page_upload` | `instanceId`, `target`, `filePath`；可选 `tab` | 主机文件设入 input[type=file]（触发原生 change；React 受控表单同常收到）。filePath 绝对路径且拒符号链接（422，macOS /tmp 类软链拒绝）、不存在 → 404、超 100MB → 422；非 file input → 422 PAGE_TARGET_NOT_FILE_INPUT |
| `page_extract` | `instanceId`；可选 `target`（@eN / CSS，缺省 = 最大表格自动发现）、`tab` | 表格 → 结构化 JSON：columns（name/path/source）/rows（null = 被合并覆盖，"" = 真实空单元格）/meta（spans 锚点）/coverage（datasetComplete 只有 unknown/incomplete，永不装作取全）。非表格目标 → 422 PAGE_EXTRACT_NOT_A_TABLE |

`tab` = targetId 或唯一前缀（`list_tabs` 获得）；省略 = 唯一页面标签自动选中，
多标签 409 并在 message 列出全部 targetId。错误处置看 code 分支：`PAGE_REF_STALE`/
`PAGE_REF_NOT_FOUND` 重 snap；`PAGE_ELEMENT_BLOCKED` 先处理遮挡；
`PAGE_FILL_VALUE_MISMATCH` 写入可能已部分生效，先观察再决定；
`PAGE_DISPATCH_FAILED` 附 effect_state（none 可重试 / unknown 禁盲目重试）。

## 登录态

### `login_profile`

输入：`environmentId` + `action`（四选一）。环境登录态生命周期管理：

| action | 行为 | 前置/错误 |
|---|---|---|
| `get` | 查状态/快照版本/实例引用数 | — |
| `capture` | 显式固化快照（主路径「实例里登录→关闭→自动保存」无需 capture；可选 `fromInstanceId` 从已停止实例捕获） | 快照操作进行中或遗留浏览器运行中 → 409 PROFILE_IN_USE |
| `reset` | 清空快照回到 not_configured | — |

> 母本 `launch` 登录浏览器 action 已于 0.3.0 移除（breaking；登录主路径 = 实例即登录源）。

典型闭环：`create_instance` →（实例里人工登录）→ 关闭实例（自动保存）→ 新 `create_instance` 即带登录态；需要固化基准时 `capture(fromInstanceId)`。

### `session_status`

输入：`environmentId`。查环境会话身份层：当前策略（off / first-login / always）、当前代、
来源实例、Cookie 数、历史代列表（回滚观测面）。

## 扩展

| 工具 | 输入 | 说明 |
|---|---|---|
| `list_extensions` | — | 已注册扩展（status: ready/missing/invalid；type: system=内置锁定 / user=用户注册） |
| `get_extension` | `extensionId` | manifest 元数据详情 |
| `register_extension` | `path`（**绝对路径**，须含合法 manifest.json） | 注册用户扩展；安全边界：只接受显式注册，无按任意路径加载 |
| `set_extension_enabled` | `extensionId`, `enabled: bool` | 启停（system → 403 EXTENSION_SYSTEM_LOCKED；只影响之后启动的实例） |
| `delete_extension` | `extensionId` | 移除注册项（不删源文件；system → 403） |
| `get_extension_config` | `extensionId`；可选 `environmentId` | 读运行时配置（未配置 → `config: null`；省略 `environmentId` = 全局默认） |
| `set_extension_config` | `extensionId`, `config`（JSON 对象）；可选 `environmentId` | 写运行时配置（整体覆盖，对新启动实例生效）。仅 `configurable: true` 的 system 扩展（如 net-rules）；形状：`{"block":["*.google-analytics.com"],"redirect":[{"from":"https://api.foo.com/*","to":"http://localhost:3000/$1"}],"headers":[{"url":"...","set":{"X-Debug-Uid":"42"}}]}`；非法 → 400 `EXTENSION_CONFIG_INVALID` |

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

需要登录态的场景在第 4 步后：实例里人工登录 → 关闭实例（Cookie 自动保存，first-login 默认策略）→ 新实例免登录；需固化基准时 `login_profile(capture, fromInstanceId)`。

> 与 CLI 的取舍：MCP 适合 AI 多轮编排（工具发现 + 结构化错误自决策）；CLI 适合脚本一行式调用与 CI（退出码契约）。两者可混用，状态完全互通。
