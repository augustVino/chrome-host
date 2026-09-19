# 单环境闭环演练

从创建环境到自动化收尾的**完整闭环**，全程 CLI 可复制；每步附 GUI 等价操作。以「在隔离环境里排查 staging 站点」为场景：hosts 指向测试环境、需要登录态、最后用 CDP 自动化。

## 0. 前置检查

```bash
chrome-host status    # exit 0 = 应用运行中；exit 8 = 先启动 chrome-host（含托盘常驻）
chrome-host doctor    # 可选：数据库 / 内核 / 目录 / 扩展全量体检
```

## 1. 创建环境

环境 = hosts 配置源 + 一组隔离实例的容器。hosts 源可选（不填则不注入）：

```bash
chrome-host env create staging --hosts-source-url https://example.com/hosts.txt
ENV_ID=$(chrome-host env list --json | jq -r '.[] | select(.name=="staging") | .id')
```

> GUI 等价：环境页 → 新建环境 → 填名称与 hosts 配置源。

## 2. 登录态快照（可选，推荐）

需要免登录的环境走这一步；跳过则每个实例从全新登录开始：

```bash
chrome-host login-profile launch "$ENV_ID"    # 打开「母本登录浏览器」
# → 在弹出的浏览器中人工完成登录，然后关闭该浏览器
chrome-host login-profile capture "$ENV_ID" --yes
chrome-host login-profile get "$ENV_ID"       # 查看 status / snapshotVersion
```

之后该环境**每次 `instance create` 的实例自动克隆登录态**。快照过期重新走 launch → capture（覆盖已有快照需 `--yes`）。

> GUI 等价：环境详情 → 登录态 → 打开登录浏览器 / 捕获快照。

## 3. 创建并启动实例

```bash
INS_ID=$(chrome-host instance create "$ENV_ID" --quiet)    # 创建即启动
```

- 首次运行会阻塞于内核下载（Chrome for Testing 约 150MB），属正常现象；
- 弹出的 Chrome 窗口即隔离实例：独立 Profile + hosts 注入 + 登录态克隆 + 环境标签角标（内置扩展渲染，一眼区分环境）。

> GUI 等价：环境详情 → 新建实例。

## 4. 打开页面与取 CDP 端点

```bash
chrome-host instance open "$INS_ID" https://staging.example.com
chrome-host instance cdp "$INS_ID" --json | jq '{port, httpUrl, webSocketUrl}'
```

> 端口每次现查：实例重启后 CDP 端口可能变化，不要缓存。

## 5. 自动化（三种方式任选）

**方式 A：任意 CDP 客户端直连**（Playwright / Puppeteer / 自研客户端）：

```bash
WS=$(chrome-host instance cdp "$INS_ID" --json | jq -r .webSocketUrl)
playwright --browser "cdp://$WS" ...
```

**方式 B：仓库自带的轻量 CDP 工具**（无需任何依赖，Node 22+ 即可）：

```bash
PORT=$(chrome-host instance cdp "$INS_ID" --json | jq -r .port)
node skills/chrome-cdp/scripts/cdp.mjs list                    # 列出页面，取 targetId 前缀
CDP_PORT=$PORT node skills/chrome-cdp/scripts/cdp.mjs shot 6BE827FA   # 截图
CDP_PORT=$PORT node skills/chrome-cdp/scripts/cdp.mjs snap 6BE827FA   # 可访问性树
CDP_PORT=$PORT node skills/chrome-cdp/scripts/cdp.mjs eval 6BE827FA 'document.title'
```

完整命令见 [AI Agent Skills](./skills)。

**方式 C：交给 AI Agent**：通过 MCP 或加载仓库 skills 接入，见 [AI Agent Skills](./skills)。

> **首次访问提示**：Chrome 新版每个 tab 首次被 CDP 访问时会弹 "Allow debugging" 授权框，需人工点一次允许；cdp.mjs 的后台 daemon 会保住会话，后续命令不再弹。

## 6. 收尾清理

```bash
chrome-host instance delete "$INS_ID" --yes    # 运行中实例自动 stop → delete
chrome-host env delete "$ENV_ID" --yes         # 环境仍有运行实例时会被拒绝（先停/删实例）
```

危险操作在非 TTY（脚本/CI）环境缺 `--yes` 一律 exit 2，绝不静默执行；保留环境只删实例也完全可以——环境数据（登录态快照、配置）留在环境上。

## 常见岔路

| 现象 | 处置 |
|---|---|
| `exit 8` | 应用未运行，先启动 chrome-host |
| `exit 5` + `INSTANCE_ALREADY_RUNNING` 等 409 族 | 状态冲突不是错误：按状态机处理（先 stop 或直接复用运行中实例），不要盲目重试 |
| `instance create` 卡住 | 首次内核下载属正常；CI 里为 create/start/restart 设置作业级超时 |
| 快照登录态失效 | session cookie 类站点重启即失效（Chrome 标准语义），重新 launch → capture |
