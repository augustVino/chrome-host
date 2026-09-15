# Chrome Host

基于 Tauri + Rust 的 Chrome 浏览器多环境管理工具：为每个环境启动完全隔离的 Chrome 实例（独立 Profile、进程级 hosts 映射、CDP 可自动化），并支持**登录态快照**——环境捕获一次登录，之后每个新实例免登录。

## 核心能力

- **环境与实例**：环境 = 一份配置（可选 hosts 配置源 + keepAlive）；实例 = 该环境的一次隔离运行（独立 user-data-dir + CDP 端口）
- **登录态快照（Login Profile）**：母本浏览器登录 → 捕获快照 → 之后创建的实例自动克隆登录态；快照可刷新/重置
- **hosts 注入**：实例启动时从 hosts 配置源现拉最新配置，经 `--host-resolver-rules` 进程级注入，不改系统 hosts
- **多入口共享服务层**：GUI / Agent API（REST）/ MCP / CLI 四个入口，同一套业务契约，专为 AI Agent 与自动化设计
- **keepAlive**：环境级开关；实例异常退出自动重启，60s 窗口内连续崩溃 3 次自动熔断

## 快速开始

```bash
pnpm install
pnpm tauri dev
```

首次启动自动下载 Chrome for Testing 131（pinned，双镜像）；内核状态见 Settings 页。

## 命令行工具（CLI）

CLI 内置于桌面应用（v0.1.7 起），**不随安装自动可用，需在应用内开启一次**：

1. 打开 chrome-host → **设置 → 命令行工具 → 安装**
2. 应用在 `/usr/local/bin` 创建指向应用内 CLI 的 `chrome-host` 链接（需要管理员授权）

> 使用前提：chrome-host 桌面应用需处于运行状态（含托盘常驻）——CLI 是 Agent API 的薄客户端，应用未运行时所有命令返回 exit 8。AI Agent / CI 接入请先跑 `chrome-host status` 探活。

开发者从源码安装：`cargo install --path crates/cli`

**📖 完整接入指南（命令参考 / Exit Code 契约 / AI Agent 与 CI 接入范式 / 故障排查）：[CLI.md](CLI.md)**

## AI 接入（Agent API / MCP）

REST 与 MCP 均只绑定 `127.0.0.1:17890`，随应用启动：

- **Agent API**：`http://127.0.0.1:17890/api/v1` → **📖 [AGENT-API.md](AGENT-API.md)**（全部端点请求/响应形状 / 错误码总表）
- **MCP**：`http://127.0.0.1:17890/mcp` → **📖 [MCP.md](MCP.md)**（工具清单 / 各客户端配置），Claude Code 一行接入：

```bash
claude mcp add --transport http chrome-host http://127.0.0.1:17890/mcp
```

## 已知边界（FAQ）

- **session cookie 登录态重启即失效**：Chrome 标准语义（session cookie 不落盘）。此类系统的免登录路径 = 登录浏览器登录 → 捕获快照 → 新建实例
- **单 session 互踢**：内部系统若为单 session 策略，多个实例共用同一快照可能互相踢下线
- **CfT 蓝条**："仅适用于自动测试"提示为 Chrome for Testing 构建硬编码，无官方开关
- **旧数据迁移**：`node scripts/migrate-legacy.js <旧 chrome-host.db>`（按 name 幂等；hosts 映射 JSON 不迁移，实例启动时现拉）

## 开发

```bash
pnpm tauri dev                # 开发（beforeDevCommand 自动构建 CLI sidecar 到 src-tauri/binaries/）
cd src-tauri && cargo test    # Rust 单测
bash scripts/smoke.sh         # 冒烟（需应用运行中；可选 HOSTS_SOURCE=<hosts 源 URL> 验证 hosts 注入）
```

## License

[MIT](LICENSE)
