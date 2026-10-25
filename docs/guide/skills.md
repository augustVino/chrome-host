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

若 agent 环境支持 MCP，更简单的路径是直接接 MCP 服务（见 [MCP 手册](/reference/mcp)），48 个工具覆盖编排与操控（含 `page_*` 十五族）；skills 的价值在于更细的排障决策知识与工作流经验。

## page 命令族：人类也能直接用

`chrome-host page` 是内置的页面操控命令族（无需 agent、无需任何脚本依赖），完整命令见 [CLI 手册 · 页面操控](/reference/cli#页面操控page-命令族)：

```bash
chrome-host page snap    <insId> [tab]        # 可访问性树 + @eN 引用
chrome-host page click   <insId> <sel|@eN>    # 原生输入点击（非 el.click()）
chrome-host page fill    <insId> @e1 "文本"   # 聚焦+输入+回读校验
chrome-host page press   <insId> Enter        # 按键（支持 Control+A 组合）
chrome-host page wait    <insId> ".result"    # 等元素出现（SPA 场景）
chrome-host page dialogs <insId>              # 查自动接管的弹窗历史
chrome-host page screenshot <insId> [@eN]     # 元素/视口 png 截图（--out 写文件）
chrome-host page pdf       <insId>            # 整页 PDF（缺省 <insId>.pdf）
chrome-host page emulate   <insId> --preset iphone-14  # 设备仿真（--off 恢复）
```

本地与远程接入（SSH 隧道 + 令牌）用法完全一致；外部 CDP 客户端（Playwright / DevTools）接入走 `instance cdp` / `cdp-session`（见[单环境闭环演练](./walkthrough)与[远程接入](./remote-access)）。

## 使用要点（SKILL.md 中的经验浓缩）

- `<tab>` 用 `instance tabs` 输出的 **targetId 唯一前缀**，单标签实例可省略；
- **元素定位优先 @eN 引用**（snap 产出），比 CSS 选择器稳；导航后引用整表失效，重新 snap；
- **错误看 `[PAGE_*]` 前缀**分支处置（完整表见 `references/cli.md`），不解析文案；
- **跨域 iframe** 用 `clickxy`（视口坐标）点入后 `type` 输入。
