---
name: chrome-host
description: chrome-host 桌面应用的完整 agent 技能：编排多环境隔离 Chrome 实例（查/建环境、启停实例、开标签页、登录态快照、hosts 注入、扩展与内核管理），并用内置 cdp.mjs 直接做页面自动化（截图、可访问性树、eval、点击、输入）。只要用户提到"用 XX 环境打开/排查/测试"（qa、sandbox 等环境名）、需要独立 profile / hosts 注入 / 免登录的浏览器、管理 chrome-host 的环境或实例、或要求在隔离浏览器里截图/操作页面/读 DOM，就必须使用本 skill。与本机手动以 --remote-debugging-port 启动、与 chrome-host 无关的调试 Chrome 相关的纯页面操作不走本 skill。
---

# Chrome Host — 隔离 Chrome 编排 + 页面自动化

本 skill 通过 **chrome-host CLI**（桌面应用的官方命令行入口）完成编排，通过内置 **`scripts/cdp.mjs`**（零依赖 CDP CLI，Node 22+）完成页面自动化。两层一条链路：CLI 把隔离实例"起好、开好页、登好录"，cdp.mjs 接管 CDP 做截图 / 交互 / 读 DOM。

CLI 两个契约直接影响操作方式：**危险操作有确认闸门**（见「危险操作与确认闸门」），**退出码 0–10 语义冻结**（见 references/cli.md）。

## 前提自检

1. 桌面应用 Chrome Host 运行中（含托盘常驻）——CLI 是它的客户端，应用不在则一切不可用。
2. CLI 已安装：`command -v chrome-host`。缺失则引导用户在应用内安装：**设置 → 命令行工具 → 安装**（创建 /usr/local/bin 链接，需管理员授权；v0.1.7 起 CLI 随应用内置）。

## 第零步：健康检查（任何操作前）

```bash
chrome-host status
```

按退出码分支：

- **exit 0** → 在线，走主工作流。
- **exit 8**（不可达）→ 应用未启动，提示用户启动 Chrome Host。API 固定在 17890，连不上就是没起——不要猜端口、不要绕道 HTTP。
- **exit 10**（未授权）→ 本机是远程接入客户端（经 SSH 隧道连 Mac），`CHROME_HOST_TOKEN` 缺失/错误。隧道与应用均在线，只是凭证问题——勿按 exit 8 处置。此后走远程路径：CDP 自动化用 `cdp-session` 会话代理（见第 5 步），不要直连实例端口。
- **其他**（多为 exit 1）→ 应用在跑但内部异常。错误行格式 `[CODE] message（HTTP status）`，把 CODE + message 原样转述给用户——应用自身故障 agent 修不了，不要建议"重试"掩盖问题，也不要编造数据。需要细节跑 `chrome-host doctor`。

> 不确定在本机还是远程？`env -u CHROME_HOST_TOKEN chrome-host status`：exit 0 = Mac 本地（直连路径）；exit 10 = 远程机器（会话代理路径）。

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
3. 用户确认后验证：`instance tabs` 看 URL 是否已离开登录页，或用 cdp.mjs `snap` 确认页面内容。

只需本次登录 → 到此为止。需要**长期免登录**（每个新实例都带登录态）→ 走登录态快照流程（launch → 人工登录 → 关浏览器 → capture），完整步骤与已知边界见 references/cli.md「登录态快照」。

### 5. 页面自动化（cdp.mjs）

先拿连接凭证——**每次现查，不缓存端口**（实例重启后端口可能变化；分配器从 9222 起动态分配，被占自动顺延）：

```bash
# Mac 本地（第零步 exit 0）：直连实例端口
CDP_PORT=$(chrome-host instance cdp <insId> --json | jq .port)

# 远程机器（第零步 exit 10）：会话代理，30 分钟有效
CDP_BASE=$(chrome-host instance cdp-session <insId> --quiet)
```

实例未运行时上面两条 exit 5（`INSTANCE_NOT_RUNNING`），先 `instance start` 再查。

cdp.mjs 全部命令（`<target>` 是 `list` 输出中的 targetId 唯一前缀，如 `6BE827FA`，模糊前缀会被拒绝）：

```bash
node scripts/cdp.mjs list                       # 列页面
node scripts/cdp.mjs shot  <target> [file]      # 视口截图（仅视口，长页先滚动）
node scripts/cdp.mjs snap  <target> [--compact] # 可访问性树（看结构优先用它）
node scripts/cdp.mjs eval  <target> <expr>      # 执行 JS
node scripts/cdp.mjs html  <target> [selector]  # 页面/元素 HTML
node scripts/cdp.mjs nav   <target> <url>       # 导航并等加载
node scripts/cdp.mjs click <target> <selector>  # 按 CSS 选择器点击
node scripts/cdp.mjs clickxy <target> <x> <y>   # 按坐标点击（CSS 像素）
node scripts/cdp.mjs type  <target> <text>      # 输入文本（当前焦点处）
node scripts/cdp.mjs loadall <target> <sel> [ms]  # 反复点"加载更多"直到消失
node scripts/cdp.mjs evalraw <target> <method> [json]  # 原始 CDP 命令透传
node scripts/cdp.mjs open   [url]               # 新开标签页
node scripts/cdp.mjs stop   [target]            # 停后台 daemon
```

关键陷阱（断链几乎都出在这里）：

- **坐标换算**：截图按原生分辨率保存（图像像素 = CSS 像素 × DPR），CDP 点击吃 CSS 像素。`shot` 输出会带当前页 DPR，Retina（DPR=2）除以 2 再点击。
- **eval 索引漂移**：多次 `eval` 之间 DOM 会变（如点掉一个卡片后全体系号位移），跨调用别用 `querySelectorAll(...)[i]`——一次 eval 收齐数据，或用稳定选择器。
- **跨域 iframe 输入**：`eval` 进不去跨域 iframe，用 `click`/`clickxy` 聚焦后 `type`（Input.insertText，不受同源限制）。
- **"Allow debugging" 授权框**：每个 tab 首次被 CDP 访问 Chrome 会弹一次，需用户在本地点允许；cdp.mjs 的后台 daemon 保住会话，后续命令不再弹，daemon 空闲 20 分钟自动退出。
- **会话过期**（仅 CDP_BASE 模式，404 `SESSION_NOT_FOUND`）→ 重新 `cdp-session` 即可，无需重建实例。

### 6. 收尾

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
| 登录态快照完整流程与边界（session cookie / 单 session 互踢） | references/cli.md「登录态快照」 |
| 远程接入原理与隧道排障 | references/cli.md「远程接入」 |
| MCP 入口（25 个 tools）与安全边界 | references/cli.md 末尾两章 |
