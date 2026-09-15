# Chrome Host

基于 Tauri + Rust 的 Chrome 浏览器多环境管理工具：为每个环境启动完全隔离的 Chrome 实例（独立 Profile、进程级 hosts 映射、CDP 可自动化），并支持**登录态快照**——环境捕获一次登录，之后每个新实例免登录。

## 核心能力

- **环境与实例**：环境 = 一份配置（可选 hosts 配置源 + keepAlive）；实例 = 该环境的一次隔离运行（独立 user-data-dir + CDP 端口）
- **登录态快照（Login Profile）**：用母本目录打开浏览器登录 → 停机捕获 → 之后创建的实例自动克隆登录态；快照可刷新/重置
- **hosts 注入**：实例启动时从 hosts 配置源现拉最新配置，经 `--host-resolver-rules` 进程级注入，不改系统 hosts
- **托盘常驻**（macOS 原生材质 Popover + 三平台 native 菜单）：不开主窗口完成 New Instance / Focus / Stop
- **Agent API**：`http://127.0.0.1:17890/api/v1` REST 全量管理（详见下方速查），专为 AI Agent / 自动化设计；另有内嵌 MCP server（`/mcp`，见下文）
- **Developer Mode**：设置开启后展示 PID / CDP 端口 / Profile 目录等技术字段
- **keepAlive**：环境级开关；实例异常退出自动重启，60s 窗口内连续崩溃 3 次自动熔断

## 快速开始

```bash
pnpm install
pnpm tauri dev
```

首次启动自动下载 Chrome for Testing 131（pinned，双镜像）；内核状态见 Settings 页。

## Agent API 速查

仅绑定 `127.0.0.1`。错误格式 `{ "error": { "code", "message" } }`，前端与 AI 依赖 `code` 而非 `message`。

**📖 完整接入指南（全部端点请求/响应形状 / 错误码总表 / 接入注意事项）：[AGENT-API.md](AGENT-API.md)**

```text
# Environments
GET    /api/v1/environments                          环境列表（含运行摘要）
POST   /api/v1/environments                          创建环境
GET    /api/v1/environments/:id                      详情
PATCH  /api/v1/environments/:id                      部分更新（name/hostsSourceUrl/icon/startupArgs/keepAlive）
DELETE /api/v1/environments/:id                      删除（运行中 → 409）
GET    /api/v1/environments/:id/status               运行时摘要
GET    /api/v1/environments/:id/activity?limit=50    Activity 事件流

# Login Profile（登录态快照）
GET    /api/v1/environments/:id/login-profile        视图（惰性创建）
POST   /api/v1/environments/:id/login-profile/launch 打开登录浏览器
POST   /api/v1/environments/:id/login-profile/capture 捕获快照（浏览器运行中 → 409）
POST   /api/v1/environments/:id/login-profile/reset   重置快照

# Instances
POST   /api/v1/environments/:id/instances            创建并启动
GET    /api/v1/instances/:id                         详情
DELETE /api/v1/instances/:id                         删除（运行中 → 409）
GET    /api/v1/instances/:id/status                  状态
POST   /api/v1/instances/:id/start|stop|restart      生命周期
POST   /api/v1/instances/:id/focus                   唤出窗口
GET    /api/v1/instances/:id/cdp                     CDP endpoint（含 WebSocket URL）
GET    /api/v1/instances/:id/tabs                    标签页列表（page）
POST   /api/v1/instances/:id/tabs                    新建标签页 { "url": "https://…" }
POST   /api/v1/instances/:id/navigate                导航（new+close 组合近似，丢旧页历史）

# Settings / Kernel
GET|PUT /api/v1/settings                             应用设置（developerMode）
GET    /api/v1/kernel/status                         内核状态
POST   /api/v1/kernel/download | /kernel/cancel      内核下载管理
```

应用内 Settings → Agent API 页提供可交互的 API Explorer。

## MCP（AI 客户端接入）

应用内嵌 MCP over Streamable HTTP server（随 app 启动，与 REST 同端口）。**📖 完整接入指南（24 个工具的输入输出 / 错误语义 / 编排闭环 / 各客户端配置）：[MCP.md](MCP.md)**

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

24 个 tools：内核状态 / 环境 CRUD / keepAlive / 活动流 / 实例生命周期 / 标签页 / CDP endpoint / 登录态（get/launch/capture/reset）/ 扩展管理。业务错误以 `isError` + `{code, message, status}` 结构化透传给 AI。零 node 依赖；app 未启动时 MCP 不可达。

## CLI（命令行入口）

`chrome-host` CLI 是 GUI / MCP 之外的第三个入口：独立 binary（不依赖 Tauri），作为 Agent API 的薄客户端与 GUI/MCP 共享同一服务层，面向开发者终端与 AI Agent / CI 脚本。

**📖 完整接入指南（命令参考 / Exit Code 契约 / Agent 接入范式 / 故障排查）：[CLI.md](CLI.md)**

```bash
cargo install --path crates/cli
```

快速上手：

```bash
chrome-host env create demo                       # 创建环境，输出 id
chrome-host instance create <env_id> --quiet      # 创建即启动（首次可能阻塞内核下载），输出 id
chrome-host instance cdp <ins_id> --json | jq .port
chrome-host instance open <ins_id> https://example.com
chrome-host instance stop <ins_id>
chrome-host instance delete <ins_id> --yes        # 运行中实例自动 stop→delete
chrome-host env delete <env_id> --yes
```

Exit Code 契约（agent 依赖退出码而非解析文案）：

| Exit | 语义 |
|---|---|
| 0 | 成功 |
| 1 | 一般错误（协议破坏、未知 5xx、doctor 存在失败项） |
| 2 | 参数错误（clap 解析失败、`--json --quiet` 互斥、非 TTY 危险操作缺 `--yes`、本地校验失败） |
| 3 | 目标资源不存在（`*_NOT_FOUND`） |
| 4 | 资源已存在（预留，当前服务端无此码） |
| 5 | 运行时状态冲突 / 运行时错误（409 冲突族、内核/启动/CDP 500 族） |
| 6 | 权限拒绝（内置扩展锁定 `EXTENSION_SYSTEM_LOCKED`） |
| 7 | 请求校验失败（400 族） |
| 8 | 服务不可达（Agent API 连接拒绝 / 超时） |
| 9 | CLI 侧等待超时（预留） |

- `CHROME_HOST_API_URL`：Agent API 基地址的环境变量回退（默认 `http://127.0.0.1:17890`，优先级低于 `--api-url`），为 headless / 远程接入预留
- `--json` 输出纯 JSON（错误体 `{"error":{"code","message"}}` 也走 stdout）；`--quiet` 仅输出主实体 id 供 `$(...)` 捕获；`--verbose` 向 stderr 追加请求诊断（不含 query 与敏感内容）
- 前提：chrome-host 桌面应用需运行中（含托盘常驻），CLI 可达性与之一致；无 GUI 场景（headless daemon）见 P1 规划

## 已知边界（FAQ）

- **session cookie 登录态重启即失效**：Chrome 标准语义（session cookie 不落盘）。此类系统的免登录路径 = 登录浏览器登录 → 捕获快照 → 新建实例
- **单 session 互踢**：内部系统若为单 session 策略，多个实例共用同一快照可能互相踢下线
- **CfT 蓝条**："仅适用于自动测试"提示为 Chrome for Testing 构建硬编码，无官方开关
- **旧数据迁移**：`node scripts/migrate-legacy.js <旧 chrome-host.db>`（按 name 幂等；hosts 映射 JSON 不迁移，实例启动时现拉）

## 开发

```bash
pnpm tauri dev        # 开发（watcher 自动重编译）
cd src-tauri && cargo test   # Rust 单测
bash scripts/smoke.sh # 冒烟（需应用运行中；可选 HOSTS_SOURCE=<hosts 源 URL> 验证 hosts 注入）
```

## License

[MIT](LICENSE)
