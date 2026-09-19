# 快速开始

## 安装

**方式一：下载发布版（推荐）**

从 [GitHub Releases](https://github.com/augustVino/chrome-host/releases) 下载最新版。

**方式二：源码运行（开发者）**

```bash
pnpm install
pnpm tauri dev
```

## 首次启动

- 首次启动自动下载 **Chrome for Testing 131**（pinned 版本，双镜像）；进度与状态见 Settings 页内核卡片。
- 应用常驻托盘：关闭主窗口默认隐藏到托盘，Quit 入口在托盘菜单。

## 开启命令行工具（CLI）

CLI 内置于桌面应用（v0.1.7 起），**不随安装自动可用**，在应用内开启一次即可：

1. 打开 chrome-host → **设置 → 命令行工具 → 安装**
2. 应用在 `/usr/local/bin` 创建指向应用内 CLI 的 `chrome-host` 链接（需要管理员授权）

> **使用前提**：chrome-host 桌面应用需处于运行状态（含托盘常驻）——CLI 是 Agent API 的薄客户端，应用未运行时所有命令返回 exit 8。AI Agent / CI 接入请先跑 `chrome-host status` 探活。

## 最小使用流程

GUI 与 CLI 均可完成：

1. **创建环境**：可选填 hosts 配置源（`http(s)://` URL，实例启动时现拉）
2. **在环境中创建实例并启动**：弹出完全隔离的 Chrome 窗口（独立 Profile + 独立 CDP 端口）
3. **（可选）开启登录态快照**：环境先登录一次并捕获快照，之后创建的实例免登录，详见[核心概念](./concepts#登录态快照)
4. **CLI 编排**（示例）：

```bash
chrome-host status                 # 探活（应用运行中？）
chrome-host env list               # 环境列表
chrome-host instance create <envId> --json
chrome-host instance start <insId>
```

完整命令、`--json` / `--quiet` 输出模式与退出码契约见 [CLI 手册](/reference/cli)。

## 接入 AI Agent

REST 与 MCP 均只绑定 `127.0.0.1:17890`，随应用启动，无需额外配置：

- **Agent API（REST）**：`http://127.0.0.1:17890/api/v1` → [Agent API 手册](/reference/agent-api)（全部端点请求/响应形状、错误码总表）
- **MCP**：`http://127.0.0.1:17890/mcp` → [MCP 手册](/reference/mcp)（工具清单、各客户端配置）

Claude Code 一行接入：

```bash
claude mcp add --transport http chrome-host http://127.0.0.1:17890/mcp
```

## 下一步

- [单环境闭环演练](./walkthrough)：从建环境到 CDP 自动化收尾的完整流程
- [AI Agent Skills](./skills)：仓库自带的 agent 技能包与 cdp.mjs 工具
- [远程接入](./remote-access)：云桌面 / CI 上的 Agent 驱动本地浏览器（规划中）
