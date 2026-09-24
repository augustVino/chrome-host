# AI Agent Skills

仓库内的 `skills/` 目录是**随仓库分发的 AI Agent 技能包**：每个 skill 一份 `SKILL.md`（教 agent 何时触发、怎么操作、有哪些坑）+ 配套脚本。支持 SKILL.md 约定的 coding agent（pi、Claude Code 等）加载后即「学会」操作 chrome-host，无需人工逐步下达命令。

## skill 内部分层

单 skill（`skills/chrome-host`）覆盖完整链路，内部分两层：

| 层 | 内容 | 形态 |
|---|---|---|
| 编排层 | 健康自检、环境模型、CLI 全流程、危险操作闸门、错误码决策表 | SKILL.md + `references/cli.md` |
| 自动化层 | 截图、可访问性树、执行 JS、点击输入等轻量 CDP 命令 | `scripts/cdp.mjs`（零依赖，Node 22+） |

两层的衔接：编排层把隔离实例「起好、开好页、登好录」，产出 `CDP_PORT`（本地直连）或 `CDP_BASE`（远程会话代理），自动化层接管做页面操作。SKILL.md 只保留每次都要用的决策主干，完整命令速查 / 退出码契约 / 错误码决策表等查表型内容在 `references/cli.md` 按需加载。

## 如何启用

把本仓库 clone 到 agent 可访问的环境，并将 skill 目录加入其技能加载路径：

- **pi**：项目级 `.pi/agents/` 或全局技能目录中挂载 / 链接；
- **Claude Code**：按其 skills 机制注册仓库内 skill；
- 其他 agent：`SKILL.md` 本身就是完整的操作手册（含触发条件、流程、命令速查、错误决策表），直接作为上下文喂给 agent 亦可。

若 agent 环境支持 MCP，更简单的路径是直接接 MCP 服务（见 [MCP 手册](/reference/mcp)），25 个工具覆盖编排层（含远程场景的 `create_cdp_session`）；skills 的价值在于附带的 `cdp.mjs` 自动化工具与更细的排障决策知识。

## cdp.mjs：人类也能直接用

`skills/chrome-host/scripts/cdp.mjs` 是零依赖的 CDP 命令行工具（Node 22+，WebSocket 直连，支持 100+ tab），**不需要 agent 也能用**：

```bash
cdp.mjs list                        # 列出打开的页面（targetId 前缀）
cdp.mjs shot <target> [file]        # 视口截图（含 DPR 与坐标换算提示）
cdp.mjs snap <target> [--compact]   # 可访问性树快照（看页面结构优先用它）
cdp.mjs eval <target> <expr>        # 执行 JS
cdp.mjs html <target> [selector]    # 页面/元素 HTML
cdp.mjs nav <target> <url>          # 导航并等待加载
cdp.mjs click <target> <selector>   # 按 CSS 选择器点击
cdp.mjs clickxy <target> <x> <y>    # 按坐标点击（CSS 像素）
cdp.mjs type <target> <text>        # 输入文本（跨域 iframe 内也可用）
cdp.mjs open [url]                  # 新开标签页
cdp.mjs stop [target]               # 停掉后台 daemon
```

连接目标默认 `127.0.0.1:9222`，用 `CDP_PORT` / `CDP_HOST` 环境变量改——接 chrome-host 实例时把 `instance cdp` 查到的端口传进来（完整闭环见[单环境闭环演练](./walkthrough)）；远程经 chrome-host 会话代理时改用 `CDP_BASE`（见[远程接入](./remote-access)）。

## 使用要点（SKILL.md 中的经验浓缩）

- `<target>` 用 `list` 输出的 **targetId 唯一前缀**，歧义前缀会被拒绝；
- **坐标**：截图按物理像素保存，CDP 点击按 CSS 像素——Retina（DPR=2）下截图坐标 ÷ 2；
- **跨域 iframe 输入**用 `type`（eval 的 JS 注入不生效）；
- **多步 eval 之间 DOM 会变**：一次性取全数据，或用稳定选择器，避免跨调用的索引错位。
