# 浏览器操控能力评估框架（evals/）

对 chrome-host `page` 命令族（及实例编排命令）做**持续、本地、真机**行为评估的独立框架。
参考 Tencent BrowserSkill evals/browser 的思路，做 CDP 直连版：fixture 页面本地起服，
操作走 **chrome-host CLI 子进程**（评估的就是 agent 的真实接口，不直连 REST/CDP）。

- 零 npm 依赖：node ≥ 18（内建 `http`/`child_process`/`fetch`）。
- 放顶级 `evals/`，与 `skills/`（prompt 级轻量评测）互补：那套评「agent 决策路径」，这套评「产品行为本身」。

## 快速开始

```bash
# 前置 1：chrome-host 应用已运行（默认 http://127.0.0.1:17890）
# 前置 2：CLI 可发现——三选一：设 CARGO_TARGET_DIR / chrome-host 在 PATH / 传 --cli

node evals/cli.mjs list                     # 有哪些 case
node evals/cli.mjs run                      # 跑全部（真机：会建 env/实例并自动清理）
node evals/cli.mjs run --case navigate-history   # 只跑一个
node evals/cli.mjs run --api-url http://127.0.0.1:17896   # 应用不在默认端口时
node evals/cli.mjs report --json            # 重看最近一次运行的机器可读结果
```

退出码（run / report 同语义，可直接接 CI）：`0` 全过 · `1` 存在 failed · `2` 全部非
failed 但有 passed-with-unverified · `3` 前置失败（应用未运行 / CLI 未找到）· `4` case 加载失败。

## 结构

```
evals/
  cli.mjs                入口：list / run / report
  lib/
    runner.mjs           编排：fixture server → 建 env/instance → smoke 步骤 → 收证据 → 判定 → 清理
    fixture-server.mjs   本地 HTTP fixture（node:http），页面带 run 隔离的事件上报端点
    oracle.mjs           判定：三类证据源 + 三态
    case-loader.mjs      case.json 发现 + schema 校验（加载期报精确错误）
  cases/**/*.case.json   评估用例（声明式）
  fixtures/pages/*.mjs   fixture 页面（render(ctx) → HTML，或 handle(req,res,ctx) 全自控）
  .last-run.json         最近一次运行明细（gitignore，report 的数据源）
```

## case.json 结构

```jsonc
{
  "id": "page-controls-basic",           // 小写/数字/连字符，全局唯一
  "title": "表单填写与导航基础链",
  "fixture": { "startPath": "/form" },   // 可省：无页面的纯编排 case（如实例命名）
  "task": "给 agent 的任务描述（人类可读；smoke 是它的确定性实现）",
  "assertions": {
    "site":     [ { "event": "form.submitted", "where": { "data.text": "eval-{runId}" }, "minCount": 1 } ],
    "response": [ { "pattern": "Filled .*eval-\\{runId\\}" } ],
    "adapter":  [ { "evidenceKey": "navigate_back_url", "equals": "{baseUrl}/start" } ]
  },
  "smoke": [ /* 声明式步骤，见下 */ ]
}
```

### 三类证据源 + 三态（核心设计）

| 来源 | 证据是什么 | 收集时机 | 典型断言 |
|---|---|---|---|
| **site** | fixture 页 JS `fetch('/__evidence/<runId>', …)` 上报的事件流 | smoke 后经 250ms 缓冲收口 | 事件计数 ≥ minCount，`where` 按 dot path 精确匹配 |
| **response** | 全部 smoke 步骤 CLI **stdout 拼接文本** | 每步落库 | 正则命中稳定标记（如 `PAGE_NAV_NO_HISTORY`） |
| **adapter** | 步骤 `saveAs` 保存的外部事实（CLI JSON 输出的字段值） | 每步落库 | `equals` / `contains` / `exists` |

每条断言三态：**passed**（证据在且符合）/ **failed**（证据在但违约）/ **unverified**
（证据缺失——上报通道未打通、步骤未执行、保存值不存在；**绝不静默当通过**）。
case 总状态优先级：`failed > passed-with-unverified > passed`；smoke 步骤非预期失败/超时
（stepError）直接判 failed——case 没跑完是执行失败，不是「未验证」。

### smoke 步骤（声明式）

| step | 字段 | 语义 |
|---|---|---|
| `cli` | `args`（字符串数组）、`json?`（追加 --json）、`allowFailure?`、`saveAs?` + `path?`（从 JSON 输出按 dot path 取值存证）、`timeoutMs?`（默认 120s） | 跑一条真实 CLI；非零退出且未 allowFailure → stepError |
| `wait` | `args`、`until: {path, equals\|contains}`、`timeoutMs?`、`intervalMs?`、`saveAs?` | 轮询 CLI（自动 --json）直到输出字段满足条件 |
| `sleep` | `ms` | 定长等待 |
| `read` | `from`（已存 key）、`path?`、`saveAs` | 从已有证据派生新值 |

占位符 `{runId}` / `{baseUrl}` / `{envId}` / 任意 `saveAs` 键，替换发生在 args、断言值
（where 值、pattern、equals/contains）两处；未登记的占位符原样保留（拼错会显式失败）。

### 隔离与清理

- 每 case 独立 runId（`{baseRunId}-{caseId}`）：env/实例名、事件流、fixture server
  （随机端口）全部按 runId 隔离；上报端点校验 runId，串流 404 丢弃。
- 清理无条件执行：`env delete --yes`（含 stop-all），失败重试一次，仍败在结果中
  **显式报告**（cleanup: failed + 残留环境名），绝不静默。

## 扩展指南

1. **加页面**：`fixtures/pages/<name>.mjs` 导出 `render({runId, baseUrl}) → HTML`
   （路由即 `/<name>`）。需要动态路由/非 HTML 响应时导出 `handle(req, res, ctx)`。
   页内上报：`fetch('/__evidence/' + '<服务端已把 runId 注入 render ctx>' …)`。
2. **加 case**：`cases/<组>/<name>.case.json`。先 `node evals/cli.mjs list`——schema
   校验在加载期，报错行直接指到字段。
3. 断言设计原则：优先 site（行为真值）> adapter（外部事实）> response（stdout 标记
   最弱，只用于无法前两者表达的边界，如错误码路径）。

## 故障排查

| 现象 | 处置 |
|---|---|
| exit 3「应用未运行」 | 先起 app（`cargo build` 后 `chrome-host-app --hidden`）；非默认端口用 `--api-url` |
| exit 3「找不到 CLI」 | 设 `CARGO_TARGET_DIR`，或 `--cli <路径>`，或 PATH 放入 |
| 全部 site unverified | fixture 页没加载/上报被拦：先看 run 输出里 fixture URL，浏览器手动开一次 |
| cleanup: failed | 残留环境按结果里的 env id 手动 `chrome-host env delete <id> --yes` |

## 设计取舍

- **CLI 而非 REST**：评估对象是 agent 的真实接口面（含 CLI 的输出契约、退出码、
  错误渲染）——这些恰恰是最容易回归的地方。
- **串行执行**：case 间不并行（浏览器实例重、且共享同一个 app），追求确定性优先。
- **不加录制/重放**：当前 3 case 全走真机实时路径；量级上去后再考虑。
