# AI Agent Skills

仓库内的 `skills/` 目录是**随仓库分发的 AI Agent 技能包**：每个 skill 一份 `SKILL.md`（教 agent 何时触发、怎么操作、有哪些坑）+ 参考文档。支持 SKILL.md 约定的 coding agent（pi、Claude Code 等）加载后即「学会」操作 chrome-host，无需人工逐步下达命令。

## skill 内部分层

单 skill（`skills/chrome-host`）覆盖完整链路，内部分两层：

| 层 | 内容 | 形态 |
|---|---|---|
| 编排层 | 健康自检、环境模型、CLI 全流程、危险操作闸门、错误码决策表 | SKILL.md + `references/cli.md` |
| 操控层 | 页面操控命令族：snap @eN 引用、原生点击、表单填写、按键、元素等待、弹窗自动接管 | `chrome-host page` 命令（应用内置，本地/远程同用） |

两层的衔接：编排层把隔离实例「起好、开好页、登好录」，操控层直接对实例页面操作（`page snap → @eN 操作 → 变化后重 snap` 循环；跨域 iframe 内容在 snap 的 `[frame]` 段内，帧内 @eN 可直接 click/fill/press）。**agent 全程不接触 CDP 端口或代理凭证**——连接由应用内部管理。SKILL.md 只保留每次都要用的决策主干，完整命令速查（含 page 命令族与 PAGE_* 错误码表）/ 退出码契约等查表型内容在 `references/cli.md` 按需加载。

## 如何启用

把本仓库 clone 到 agent 可访问的环境，并将 skill 目录加入其技能加载路径：

- **pi**：项目级 `.pi/agents/` 或全局技能目录中挂载 / 链接；
- **Claude Code**：按其 skills 机制注册仓库内 skill；
- 其他 agent：`SKILL.md` 本身就是完整的操作手册（含触发条件、流程、命令速查、错误决策表），直接作为上下文喂给 agent 亦可。

若 agent 环境支持 MCP，更简单的路径是直接接 MCP 服务（见 [MCP 手册](/reference/mcp)），56 个工具覆盖编排与操控（含 `page_*` 命令族）；skills 的价值在于更细的排障决策知识与工作流经验。

## page 命令族：人类也能直接用

`chrome-host page` 是内置的页面操控命令族（无需 agent、无需任何脚本依赖），完整命令见 [CLI 手册 · 页面操控](/reference/cli#页面操控page-命令族)：

```bash
chrome-host page snap    <insId> [tab]        # 可访问性树 + @eN 引用
chrome-host page click   <insId> <sel|@eN>    # 原生输入点击（非 el.click()）
chrome-host page fill    <insId> @e1 "文本"   # 聚焦+输入+回读校验
chrome-host page press   <insId> Enter        # 按键（支持 Control+A 组合）
chrome-host page wait    <insId> ".result"    # 等元素出现（SPA 场景）
chrome-host page dialogs <insId>              # 查自动接管的弹窗历史
chrome-host page screenshot <insId> [@eN|--full]  # 元素/视口/全页 png 截图（--out 写文件；--full = 整文档长截图）
chrome-host page pdf       <insId>            # 整页 PDF（缺省 <insId>.pdf）
chrome-host page emulate   <insId> --preset iphone-14  # 设备仿真（--off 恢复）
chrome-host page navigate  <insId> <url>       # 页内导航（保留历史；等 load 至多 25s）
chrome-host page back|forward|reload <insId>   # 历史回退 / 前进 / 重载（边界 → 409）
chrome-host page extract   <insId> [sel|@eN]   # 表格 → 结构化 JSON（缺省 = 最大表格自动发现）
chrome-host page download-wait <insId>         # 等下一次下载完成（命令式：先调它再 click 触发）
chrome-host page upload <insId> <sel|@eN> /private/tmp/f.png  # 文件设入 input[type=file]
```

本地与远程接入（SSH 隧道 + 令牌）用法完全一致；外部 CDP 客户端（Playwright / DevTools）接入走 `instance cdp` / `cdp-session`（见[单环境闭环演练](./walkthrough)与[远程接入](./remote-access)）。

## 使用要点（SKILL.md 中的经验浓缩）

- `<tab>` 用 `instance tabs` 输出的 **targetId 唯一前缀**，单标签实例可省略；
- **元素定位优先 @eN 引用**（snap 产出），比 CSS 选择器稳；导航后引用整表失效，重新 snap；
- **错误看 `[PAGE_*]` 前缀**分支处置（完整表见 `references/cli.md`），不解析文案；
- **跨域 iframe（OOPIF）**：snap 以 `[frame]` 段内嵌帧内容，@eN 跨帧连续编号，帧内引用直接 click/fill/press（点击走坐标投影、输入走帧会话）；CSS 选择器仍只查主 frame，帧内定位一律用 @eN；`clickxy` 点入 + `type` 输入仍是坐标级兜底；
- **表格/查询结果导出**用 `page extract`（不要 eval 手拼 innerText）：合并单元格被覆盖位是 `null`（空串才是真实空单元格）；`coverage.datasetComplete` 只有 unknown/incomplete——看到 incomplete 与 reasons 先处理归因（虚拟化未加载完就先 `page wait` 行出现），绝不当作取全。

## hover 提示：snap 的 `[hover]` / `[hover-reveal]` 标记

`page snap` 会静态扫描页面 CSS 的 `:hover` 规则，在节点行内打两类标记：

- `@e1 [button] [hover] 菜单` —— **悬停它会有反应**（下拉/悬浮菜单的触发器）；
- `@e2 [link] [hover-reveal] 子项` —— 它**藏在某个 hover 展开区内**，触发器悬停前不可见/不可点，直接操作会报 `PAGE_ELEMENT_NOT_VISIBLE`。

操作节奏：**悬停触发器 → 重新 snap → 用展开项的新 @eN 操作**。悬停没有专用命令：点击触发器（多数 hover 菜单点击即展开）即可，需要纯悬停时用 `page raw` 发 `Input.dispatchMouseEvent`（`mouseMoved`）。

诚实边界：标记来自静态扫描，**没有标记 ≠ 确认无 hover**。`--json` 输出的 `hoverScan` 字段披露扫描完整性（四态）：

| hoverScan | 含义 |
|---|---|
| `ok` | 预算内完整扫描 |
| `truncated` | 预算截断（32 表 / 单表 2MiB / 256 规则），**可能漏看** |
| `skipped` | 无可扫的 hover 规则（或该帧未扫——帧内不扫） |
| `error` | 扫描通道故障（「扫不了」与「没扫出」严格区分） |

`truncated` 时对菜单类页面保持怀疑，用交互试探补证。

## 响应体读取：network → network-body

`instance network` 的 table 自带 **BODY 列**（= bodyState：`available` / `partial` / `evicted` / `unavailable`，unavailable 附归因）。要看接口真实返回时的排障链：

```bash
# ① 找目标请求，BODY 列确认响应体可读
chrome-host instance network <insId> --failed-only
# ② 取 requestId（table 不含；--quiet 每行 status<TAB>url<TAB>requestId）
RID=$(chrome-host instance network <insId> --quiet | grep 'api.foo.com/v1/order' | cut -f3)
# ③ 分页读响应体（UTF-16 单位；缺省 8192 / 上限 65536）
chrome-host instance network-body <insId> "$RID"
chrome-host instance network-body <insId> "$RID" --offset 8192   # truncated=true 时续读
```

要点：

- 仅 `bodyState = available / partial` 可读；`404`（exit 3）= 请求不在环，`409`（exit 5）= 状态不可读（message 含归因）——都不必盲目重试，换证据源；
- body 中敏感 query/字段值已替换为 `[redacted]`——这是**落环前脱敏的安全特性，不是数据损坏**；
- `--quiet` 只输出 body 原文（可管道），`--json` 含 bodyState/totalLength/truncated 完整元信息。

## 会话级拦截：Fetch 层动态规则（REST）

`POST /api/v1/instances/{id}/page/intercept` 在任务内**即时**改写当前实例的网络：规则集**整表替换**（不是追加）、本地决策不逐请求往返、连接重建自动失效、未命中/规则失败一律放行（不会挂起页面）。三种 action：

```bash
API=http://127.0.0.1:17890/api/v1

# mock：订单接口换成假响应（status 须 2xx 或 4xx-599；delayMs 0-10000）
curl -s "$API/instances/$INS/page/intercept" -H 'content-type: application/json' -d '{
  "rules": [{ "id": "r1", "match": { "urlPattern": "*://api.foo.com/v1/orders*" },
    "action": { "type": "mock", "status": 200, "body": "{\"ok\":true}" }, "times": 0 }]}'

# setHeaders：给全部 api 请求注入调试头
curl -s "$API/instances/$INS/page/intercept" -H 'content-type: application/json' -d '{
  "rules": [{ "id": "h1", "match": { "urlPattern": "*://api.foo.com/*" },
    "action": { "type": "setHeaders", "headers": { "X-Debug-Uid": "42" } } }]}'

# block：临时屏蔽埋点上报
curl -s "$API/instances/$INS/page/intercept" -H 'content-type: application/json' -d '{
  "rules": [{ "id": "b1", "match": { "urlPattern": "*://*.google-analytics.com/*" },
    "action": { "type": "block" }, "times": 0 }]}'
```

要点：`times` 为命中次数上限（缺省 1 次即失效，`0` = 不限）；`urlPattern` 是 `*` 通配 glob（须带 scheme 或 `*://` 开头）；匹配 XHR/fetch 时**省略 `match.resourceTypes`**（Fetch 域枚举坑：合法集 Document/Stylesheet/Image/Media/Font/Script/XHR/Fetch/Ping/Other，WebSocket/Manifest 等会被 CfT 131 拒绝）；`GET` 同路径查规则命中计数与干预记录环（200 条，url 已脱敏）。

与 **net-rules 扩展（持久层）**构成两层模型，按场景选层：

| | net-rules 扩展（持久层） | page/intercept（会话层） |
|---|---|---|
| 生效时机 | 对**新启动**的实例（配置不热加载） | 即时（当前运行实例） |
| 生命周期 | 环境级配置，跨会话存活 | 任务内；连接重建自动失效 |
| 能力 | block / redirect / headers（浏览器层 DNR） | mock（改响应）/ setHeaders / block（Fetch 域） |
| 配置入口 | `extension config set`（CLI / MCP / GUI，全局或 per-env） | REST `POST /instances/{id}/page/intercept` |
| 典型场景 | 长期屏蔽埋点、api 指到本地 mock 服务 | 测一次异常分支、注入调试头、临时屏蔽 |

响应体改写（mock）是会话层独有能力——DNR 层做不了 body 改写；两层可叠加使用。
