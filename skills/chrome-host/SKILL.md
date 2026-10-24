---
name: chrome-host
description: chrome-host 桌面应用的完整 agent 技能：编排多环境隔离 Chrome 实例（查/建环境、启停实例、开标签页、登录态快照、hosts 注入、扩展与内核管理），并用 chrome-host page 命令族直接做页面操控（snap @eN 引用、原生点击、表单填写 fill/press、元素等待 wait、eval、坐标点击、弹窗自动接管）。只要用户提到"用 XX 环境打开/排查/测试"（qa、sandbox 等环境名）、需要独立 profile / hosts 注入 / 免登录的浏览器、在隔离浏览器里点按钮/填表单/提交/截图/读 DOM、或管理 chrome-host 的环境实例，就必须使用本 skill。与本机手动以 --remote-debugging-port 启动、与 chrome-host 无关的调试 Chrome 相关的纯页面操作不走本 skill。
---

# Chrome Host — 隔离 Chrome 编排 + 页面自动化

本 skill 通过 **chrome-host CLI**（桌面应用的官方命令行入口）完成全部工作：**instance 命令族**负责编排（隔离实例"起好、开好页、登好录"），**page 命令族**负责页面操控（snap 引用 → 点击/输入/等待，应用侧原生 CDP 通道）。

CLI 两个契约直接影响操作方式：**危险操作有确认闸门**（见「危险操作与确认闸门」），**退出码 0–10 语义冻结**（见 references/cli.md）。

## 前提自检

1. 桌面应用 Chrome Host 运行中（含托盘常驻）——CLI 是它的客户端，应用不在则一切不可用。
2. CLI 已安装：`command -v chrome-host`。缺失则引导用户在应用内安装：**设置 → 命令行工具 → 安装**（创建 /usr/local/bin 链接，需管理员授权；v0.1.7 起 CLI 随应用内置）。

## 第零步：健康检查（任何操作前）

```bash
chrome-host status
```

按退出码分支：

- **exit 0** → 应用可达且授权正常，直接开始。
- **exit 8**（不可达）→ 应用未启动，提示用户启动 Chrome Host。API 固定在 17890，连不上就是没起——不要猜端口、不要绕道 HTTP。
- **exit 10**（未授权）→ 本机是远程接入客户端（经 SSH 隧道连主机），`CHROME_HOST_TOKEN` 缺失/错误。隧道与应用均在线，只是凭证问题——勿按 exit 8 处置。核对令牌后 page/instance 命令直接用（API 经隧道可达，无需额外配置）。
- **其他**（多为 exit 1）→ 应用在跑但内部异常。错误行格式 `[CODE] message（HTTP status）`，把 CODE + message 原样转述给用户——应用自身故障 agent 修不了，不要建议"重试"掩盖问题，也不要编造数据。需要细节跑 `chrome-host doctor`。

> **判断 shell 与浏览器是否同机**（token 存在时 status 的 exit 0 区分不了）：`env -u CHROME_HOST_TOKEN chrome-host status` → exit 0 = 同机；exit 10 = 远程 shell（API 经隧道同样可达，page/instance 命令直接用，无需额外配置）。

## 核心模型

| 概念 | 含义 | id 形态 |
|------|------|---------|
| 环境 Environment | 一份配置：名称 + 可选 hosts 配置源 + keepAlive 开关 | `env_xxx` |
| 实例 Instance | 该环境的一次隔离运行：独立 user-data-dir + 动态 CDP 端口 + 进程级 hosts 注入 + 登录态克隆 | `ins_xxx` |

- 一个环境可并行多个实例，互不干扰（独立 profile）。
- hosts 与登录态都在**实例启动时**固化；改环境配置要重启实例才生效。
- hosts 注入用 `--host-resolver-rules` 进程级生效，不改系统 hosts，应用退出即消失。

## 主工作流：用户指定环境排查问题

典型指令："用 qa12345 帮我打开 xxx 页面看看"。

### 1. 列环境

```bash
chrome-host env list --json
```

存在 → 记下 `id` 进入第 2 步。不存在 → 向用户确认新建，以及 hosts 配置源（返回 hosts 格式文本的 http(s) URL，qa 环境通常需要）：

```bash
chrome-host env create qa12345 --hosts-source-url "https://.../hosts" --quiet
```

不传 `--hosts-source-url` 则不注入 hosts，域名解析与本机一致——内部域名可能指向错误地址。**用户没提 hosts 源而要访问内部域名时，主动问一句。**

### 2. 复用或创建实例

```bash
chrome-host instance list --env <envId> --json
```

- 有 `running` → **直接复用**（登录态、已开页面都在）。
- 有 `stopped` → `chrome-host instance start <insId>`（保留 profile，登录态还在）。
- 无 → `chrome-host instance create <envId> --quiet`。

`instance create` 创建即启动、同步阻塞，CLI 对启动类长操作不设总超时——首次运行阻塞于内核下载（~200MB）是正常现象，不会误判失败。想先确认内核：`chrome-host runtime version`；未装则 `chrome-host runtime install`（默认上限 900s，超时 exit 9）。

### 3. 打开目标页面

```bash
chrome-host instance open <insId> "https://xxx.example.com/path"
```

URL 必须 http:// 或 https:// 开头（本地校验，非法 exit 2 且请求不发出）。查看已开页面：`chrome-host instance tabs <insId> --json`（返回 id/url/title）。

### 4. 需要登录 → 停下等人工

判断依据：页面跳到登录页 / 统一认证，或任务依赖登录态。

1. 明确告知用户："已在 qa12345 环境的实例中打开登录页，请在浏览器窗口完成登录后告诉我。"
2. **等待用户回复**，不轮询猜测，不自动填表单——隔离 profile 里没有用户凭据，自动登录必然失败且可能触发风控。
3. 用户确认后验证：`instance tabs` 看 URL 是否已离开登录页，或用 `page snap` 确认页面内容。

**登录态会自动保存（默认 first-login 策略）**：用户在实例里登录后，**关闭该实例**（或告诉用户直接关窗口），Cookie 保险库自动落盘；同环境的下一个新实例自动免登录，无需任何 capture 命令。要点：

- 保存发生在**实例停止时**（stop / 用户关窗口都算）；运行中的实例不会被读取。
- 查看保存状态与历史代：`chrome-host env session <envId>`（当前代 / Cookie 数 / 来源实例 / 历史代）。
- 回滚登录态到上一代：`chrome-host env session-restore <envId> <gen>`（gen 来自 env session 输出；仅回滚 Cookie 层）。
- 需要**显式固化某个实例**的登录态（如 always 策略下反复保存、或想把当前实例状态存为基准）：`chrome-host login-profile capture <envId> --from <insId> --yes`（实例须已停止）。
- 免登录判定链：新实例启动时注入保险库 Cookie（失败降级为空登录态，实例照常可用）；session cookie（关浏览器即失效那类）不在注入范围——此类系统免登录 = 复用 running 实例。
- 环境级策略可调：`env create/update --session-capture <off|first-login|always>`（默认 first-login：仅首个实例退出时保存）。

### 5. 页面操控（chrome-host page 命令族）

操控命令直连应用 API——**本地与远程接入（exit 10）均直接可用，无需拿 CDP 端口或代理凭证**。`<tab>` 是 `instance tabs` 输出的 targetId 唯一前缀（如 `6BE8AB81`）；单标签实例可省略，多标签缺省会报 `PAGE_TAB_AMBIGUOUS` 并列出全部 tab 供选择。

```bash
chrome-host page snap    <id> [tab]                    # 可访问性树 + @eN 引用（操作循环起点）
chrome-host page click   <id> <sel|@eN> [--tab t]      # 原生输入点击（非 el.click()）
chrome-host page fill    <id> <sel|@eN> <value> [--tab t]  # 聚焦+清空+输入+回读校验（表单用它，别 eval 手写）
chrome-host page press   <id> <key> [--target <sel|@eN>] [--tab t]  # Enter/Tab/箭头/F1-F12/Control+A 组合键
chrome-host page wait    <id> <sel> [--state visible|hidden|attached|detached] [--timeout 10s] [--tab t]  # SPA 渲染等待用它，别 sleep
chrome-host page eval    <id> <expr> [--tab t]         # 执行 JS（优先用上面的原语，eval 是逃生舱）
chrome-host page raw     <id> <method> [json] [--tab t] [--browser]  # 任意 CDP 命令透传（最高权限逃生舱，未覆盖场景用）
chrome-host page html    <id> [selector] [--tab t]     # 页面/元素 HTML
chrome-host page clickxy <id> <x> <y> [--tab t]        # 视口 CSS 像素坐标点击（跨域 iframe 用它+type）
chrome-host page type    <id> <text> [--tab t]         # 焦点处输入（clickxy 点入跨域 iframe 后用它）
chrome-host page loadall <id> <sel> [--interval 1.5s] [--tab t]  # 反复点"加载更多"直到消失
chrome-host page dialogs <id> [--tab t] [--since N]    # 自动接管的弹窗历史（近 32 条）
```

操作循环：**`snap → 用 @eN 操作 → 页面变化后重 snap`**。

关键约定（断链几乎都出在这里）：

- **元素定位优先 @eN 引用**：snap 给可交互节点（button/link/textbox/checkbox…）编号输出 `@e3 [button] 提交`，click/fill/press 直接吃引用，比 CSS 选择器稳（不受 CSS-in-JS hash、动态列表位移影响）。导航后引用整表失效（报 `PAGE_REF_STALE`）——这是特性：重新 snap 再操作，不要对旧引用重试。
- **弹窗已自动接管**：页面 alert/confirm/prompt/beforeunload 由应用自动 accept（beforeunload=放行离开，prompt 提交默认文本），不阻塞命令；命令期间弹过的窗以 `dialogs_seen` 附在输出里，`page dialogs` 查历史。
- **错误看 [PAGE_*] 前缀分支处置**，不解析文案：`PAGE_ELEMENT_BLOCKED`（被浮层遮挡，先处理遮挡）/ `PAGE_ELEMENT_NOT_VISIBLE` / `PAGE_REF_NOT_FOUND`·`PAGE_REF_STALE`（重 snap）/ `PAGE_TARGET_NOT_FILLABLE` / `PAGE_FILL_VALUE_MISMATCH`（写入可能部分生效，先观察再重试）/ `PAGE_WAIT_TIMEOUT` / `PAGE_DISPATCH_FAILED`（附 effect_state：none=未发出可重试，unknown=已发出禁盲目重试）/ `PAGE_TAB_AMBIGUOUS`（多 tab，带列表重选）。
- **跨域 iframe**：eval/fill 的 selector 均不可达；用 `clickxy`（视口坐标，事件穿透 iframe 边界）点入目标获得焦点后 `type` 输入。
- **无主对话框卡死（罕见）**：对话框在应用无连接期间弹出（如应用重启前埋的 setTimeout alert）时，CDP 通道不认识它——命令大面积超时但 `page dialogs` 无记录。恢复：`chrome-host page raw <id> "Page.navigate" '{"url":"<当前页>"}' --tab <t>`（强制导航丢弃对话框），或让用户在浏览器窗口点掉。
- **首条命令偶发 PAGE_TAB_NOT_FOUND**：连接刚建立的 attach 竞态，提示语含 retry——直接重试同一命令即可。
- **多实例调试互不干扰**：page 命令经应用持有的独立 CDP 通道，与其他工具（DevTools、Playwright 直连）并存无冲突。

### 6. 页面排障：先查历史，不要 reload 重放

页面报错/白屏/接口失败时，**实例在后台持续记录 console 与全部请求**（环形缓冲，进程内态）——直接查历史即可，重新加载页面会破坏现场（spa 状态丢失、错误可能不再复现）：

```bash
chrome-host instance console  <insId> --level error        # JS 错误（含 unhandledrejection）
chrome-host instance network  <insId> --failed-only        # 失败请求或 HTTP ≥400
chrome-host instance screenshot <insId> --out /tmp/now.png # 不动页面看当前现场（png）
chrome-host instance har      <insId> --out /tmp/trace.har # 完整时序导出（HAR 1.2，可直接给 LLM 分析）
chrome-host instance diagnostics <insId>                    # 当前页快照：JS 错误/失败资源/LCP/FCP
```

选择顺序：console/network 先定位直接证据 → screenshot 看视觉现场 → diagnostics 拿性能指标 → har 导出做完整时序分析（无请求/响应头、分阶段耗时以 -1 占位）。注意：缓冲随实例重启清空，重启后查不到重启前的记录。

### 7. 拦截/改写网络（net-rules 扩展，按环境配置）

需要屏蔽埋点、或把前端接口指到本地 mock 服务时，不必改代码/代理——写环境级网络规则（对新启动实例生效）：

```bash
NR=$(chrome-host extension list --json | jq -r '.[] | select(.name=="Net Rules") | .id')
# 屏蔽埋点域（域及全部子域；对新启动的实例生效）
chrome-host extension config set $NR '{"block":["*.google-analytics.com"]}'
# api 指到本地 mock（仅该环境）：https://api.foo.com/v1/x → http://localhost:3000/v1/x（* 依序对应 $1 $2…）
chrome-host extension config set $NR '{"redirect":[{"from":"https://api.foo.com/*","to":"http://localhost:3000/$1"}]}' --env <envId>
```

要点：省略 `--env` 写全局默认，`--env` 写环境覆盖；headers 注入用 `[{"url":"...","set":{"X-Debug-Uid":"42"}}]`；语法参考 `chrome-host extension get $NR` 输出的 configDocsUrl。

### 8. 收尾

排查完成后**询问用户**是否停止实例：`chrome-host instance stop <insId>`。不默认停止（用户可能还要用），也不默认保留（长期挂着占内存）。开启 keepAlive 的环境由应用自管，无需干预。

## 危险操作与确认闸门

带确认流的命令：`env delete`、`instance delete`、`extension remove`、`login-profile capture`（仅覆盖已有快照时）、`login-profile reset`、`runtime install --cancel`。

agent 的 shell 是非交互环境（非 TTY），缺 `--yes` 时这些命令直接 exit 2 拒绝，请求不发出。规则：

- 先向用户说明后果，拿到明确同意后再带 `--yes` 执行。exit 2 的"需要 --yes"提示是闸门不是障碍——它强制破坏性操作先过用户授权。
- **绝不为绕过确认而静默加 `--yes`**。
- `--yes` 是显式授权级联语义：`env delete --yes` 连带停止并删除全部实例数据；`instance delete --yes` 连带停止运行中实例。

## 深入细节（references/cli.md）

| 场景 | 章节 |
|------|------|
| 完整命令与全局 flag 速查 | references/cli.md「命令速查」 |
| exit 0–10 全表与处置 | references/cli.md「退出码契约」 |
| exit 5/7 时按 `[CODE]` 细分 | references/cli.md「错误码细分决策表」 |
| page 命令族全量速查与 PAGE_* 错误码表 | references/cli.md「页面操控（page 命令族）」 |
| 登录态自动保存 / 显式固化 / 回滚 | references/cli.md「登录态」 |
| 远程接入原理与隧道排障 | references/cli.md「远程接入」 |
| MCP 入口（44 个 tools，含 page_* 十一族）与安全边界 | references/cli.md 末尾两章 |
