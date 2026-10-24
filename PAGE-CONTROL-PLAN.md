# PAGE-CONTROL-PLAN — 浏览器操控能力升级方案

> 定位：chrome-host 是**双面应用**——既服务人类用户，也服务各类 agent 工具（CLI / MCP / 未来 harness）。
> 本方案将"页面操控能力"从 skill 自带脚本（cdp.mjs）升级为应用内建能力，经 CLI 与 MCP 双暴露。
> 参考项目：[Tencent/BrowserSkill](https://github.com/Tencent/BrowserSkill)（MIT）。本方案只借鉴其 L1/L2 设计，不引入其扩展接入形态。

---

## 1. 现状诊断

### 1.1 当前操控链路

```
agent
 └─ chrome-host CLI ──── 编排能力（环境/实例/hosts/登录态/扩展/内核）✅ 成熟
 └─ node skills/chrome-host/scripts/cdp.mjs ── 操控能力 ⚠️ 全部在此
      └─ per-tab daemon（Unix socket，保 CDP session + Allow-debugging 只弹一次）
           └─ CDP WebSocket 直连实例端口 / 远程走 cdp-session 代理
```

问题本质：**操控能力寄居在 skill 附带脚本里**，而非应用本体。由此派生：

| # | 问题 | 层级 | 严重度 |
|---|---|---|---|
| P-1 | `alert/confirm/beforeunload` 打开期间该 tab 所有 CDP 命令阻塞（Chrome 模态框阻塞 renderer 主线程），daemon 无处理 → 全部命令 15s 超时且无因可查 | L1 | **缺陷** |
| P-2 | `click <selector>` 用 `el.click()` 合成事件：`isTrusted=false`、不经过 hit-testing（元素被浮层遮挡也"成功"）、不触发 hover/pointer 序列 | L2 | 正确性 |
| P-3 | 元素定位靠 CSS selector 即兴拼装（CSS-in-JS hash / 动态列表易碎）；`snap` 输出的 AX 树无引用编号，agent 看得到点不到 | L2 | 易用性 |
| P-4 | 无 `fill/press/select/wait-for-element` 原语，agent 每次 eval 手写即兴代码 | L2 | 易用性 |
| P-5 | 网络观测用 `performance.getEntriesByType`（汇总行，无 body/状态码/失败原因），而 qa 排查是核心场景 | L1 | 能力缺口 |
| P-6 | cdp.mjs 错误是裸字符串，agent 无法分支处置（重试/重观察/放弃） | L2 | 可维护性 |
| P-7 | 凭证与运行时泄漏给 agent：先 `instance cdp` 拿端口 → `export CDP_PORT` → 要求环境有 Node 22+ → 远程模式还要管 `CDP_BASE` 与 404 重试 | 架构 | 拓展性 |
| P-8 | cdp.mjs 846 行单文件混杂 L1（daemon/连接）与 L2（命令语义），且仓库版与各 agent 安装版已漂移 | 可维护性 | 高 |
| P-9 | SKILL.md 第 5 步是"补偿文档"（DPR 换算、索引漂移、iframe 绕行），消耗 agent 注意力绕底层缺陷 | L3 | 易用性 |

### 1.2 已有地基（扩展而非重建）

| 模块 | 现状 | 在本方案中的角色 |
|---|---|---|
| `src-tauri/src/infrastructure/cdp/ws.rs` | WS 客户端已支持**短连接命令**（Cookie 导出/注入、diagnostics evaluate）与**常驻事件订阅**（`connect_eventing`，EventSink 透传，M6 起）两种形态 | L1 会话层的直接地基 |
| `src-tauri/src/infrastructure/cdp/proxy.rs` | HTTP/WS 帧级桥（远程 cdp-session 代理） | 远程模式继续复用 |
| `src-tauri/src/application/cdp_session.rs` | 下级凭证（单实例作用域 + 30min TTL） | 远程模式凭证不变 |
| MCP server（33 tools，编排类） | 与 REST/CLI 同服务层 | 新增 `page_*` tools 的挂载点 |
| CLI 退出码契约（0–10）与确认闸门 | 冻结契约 | 操控命令沿用同一契约体系 |

---

## 2. 参考分析：BrowserSkill 能力栈

### 2.1 它为什么用扩展——那是约束，不是优势

BrowserSkill 要接入**用户日常浏览器**（非它启动、无调试端口），唯一入口是装扩展走 `chrome.debugger`。
chrome-host 的实例是自己拉起的受控进程，**从 L0 就持有进程级控制权**（直连 CDP 端口、user-data-dir、hosts）。
因此：**不搬它的扩展接入层，只借鉴其 L1 会话管理与 L2 工具语义设计。**

### 2.2 借鉴决策矩阵

| BrowserSkill 成分 | 处置 | 理由 |
|---|---|---|
| `@browser-skill/vom` 包（VOM 语义树渲染） | **直接 vendor**（后续阶段） | 纯 TS、CDP-free、MIT；但第一阶段不需要（`getFullAXTree` 单调用够用） |
| 核心工具执行逻辑（click 坐标解析→原生 Input 链 / fill 校验 / ref 失效） | **参考重写**（Rust） | 每个 100–300 行、逻辑清晰；原样搬会拖入 sessionManager/cooperation/overlay 依赖网 |
| `__tests__` 行为测试 | **移植思路与用例** | 测试即行为规范文档，比读实现猜边界可靠 |
| 50 个工具中的 tab/window/borrow/upload/download/record/video/cooperate | **不搬** | 与扩展环境深度耦合，且 chrome-host 的实例模型已天然覆盖其隔离诉求 |
| 对话框接管（`Page.javascriptDialogOpening` → 自动 accept + 内容入结果 + 游标） | **借鉴实现** | 修复 P-1 |
| 事件缓冲（console/network 环形缓冲 + 按需取 body） | **借鉴实现** | 修复 P-5；常驻 `connect_eventing` 使其接近免费 |
| 错误语义分类（`RpcErrorReason` 60+ 细分） | **借鉴设计，收敛为 ~20** | 修复 P-6；对齐 AppError/退出码体系 |
| `effect_state`（none/committed/unknown，写超时=unknown 禁盲目重试） | **借鉴语义** | 安全底线：防"重复提交"类事故 |

---

## 3. 目标架构

### 3.1 分层

```
L3  skill（纯教学：何时用哪组命令、观察-行动循环规矩）—— 无代码、无凭证、无陷阱文档
L2  工具层（应用内 Rust）：ref 管理、原生输入、原语、等待、错误语义
L1  会话层（应用内 Rust）：连接生命周期、对话框接管、事件缓冲、超时语义
L0  编排层（已有）：环境/实例/hosts/登录态/扩展/内核
        │
        ├── CLI 暴露：chrome-host page <cmd> --instance <insId> ...（退出码契约统一）
        └── MCP 暴露：page_* tools（同一服务层，零双写）
```

### 3.2 核心决策（记录理由，防反复）

| # | 决策 | 理由 |
|---|---|---|
| D1 | **应用持有 CDP 会话**，CLI 每次调用经 17890 API 转发；不做"CLI 自带 daemon" | 应用常驻且实例本就由它启动：连接生命周期与实例生命周期天然绑定（实例 stop 即断开清理）；事件缓冲天然在应用进程；远程模式下缓冲在 Mac 端，agent 无感 |
| D2 | **ref 用 `backendNodeId`**（`Accessibility.getFullAXTree` 节点自带），不用 CSS selector 做操作锚点 | 内核对象标识 vs 字符串约定；BrowserSkill 同方案，成熟验证过 |
| D3 | **写操作全部原生 Input 事件链**，永不 `el.click()` | isTrusted / hit-testing / hover 序列三重正确性 |
| D4 | **CLI 与 MCP 是并列暴露面，同一服务层**，skill 默认教 CLI | 沿用现有原则（退出码契约与确认闸门对 agent 更安全）；MCP 面向已配置 MCP 的客户端 |
| D5 | **保留 raw 逃生舱**（`page raw` 透传任意 CDP 命令） | 排查场景偶需未封装命令；也给新能力试运行窗口 |
| D6 | **P0/P1 先在 cdp.mjs（TS）上落地，验证行为设计后再落 Rust** | TS 改造快，立即止血；Phase 1 的实现即 Phase 2 的行为规范，避免 Rust 侧返工 |

---

## 4. 详细设计

### 4.1 L1 会话层（扩展 `infrastructure/cdp/ws.rs`）

**会话管理器 `CdpSupervisor`**（`application/` 新模块）：

- 实例启动 → 应用建立 browser 级 WS 连接（复用 `connect_eventing`），维护 `tabId → page session` 表（`Target.setAutoAttach` flatten 模式）；
- 实例停止/崩溃 → 连接与会话表整体清理；重连退避由应用负责；
- 页面命令路由：CLI/MCP 调用 → `CdpSupervisor::send(insId, tabId, method, params)`，应用内部解析目标 session——**端口概念对 agent 消失**（P-7）。

**对话框接管**（修 P-1）：

- 订阅 `Page.javascriptDialogOpening` → 自动 `Page.handleJavaScriptDialog {accept:true}` → 对话框内容（type/message）写入该 tab 的对话框日志（带单调游标）；
- 工具结果中附带"上一命令期间弹过 X 对话框"提示；
- `beforeunload` 的 accept 语义 = 放行离开，在文档中明示。

**事件缓冲**（修 P-5）：

- per-tab 环形缓冲（容量与字节上限，防长页面撑爆内存）：console（`Runtime.consoleAPICalled`/`exceptionThrown`）+ network（`Network.requestWillBeSent`/`responseReceived`/`loadingFailed`，body 经 `Network.getResponseBody` 按需取）；
- `page console/network` 命令读取缓冲，支持 `--since`/过滤参数；
- 缓冲在 tab 关闭/导航时按语义清理（console 清、network 保留至 tab 关闭）。

**超时与写语义**：

- 读命令超时 → 错误 `cdp_timeout`（安全可重试）；
- 写命令（Input/导航类）超时 → `input_outcome_unknown`（**禁盲目重试**，结果中带 effect_state 语义）。

### 4.2 L2 工具层

**ref 机制**（修 P-3）：

```
snap：getFullAXTree → 可交互节点编号 @e1..@eN（输出行内联 role/name/value）
      → RefStore{ tab → {ref → backendNodeId}, generation }
操作：--ref @e3 → backendNodeId → DOM.describeNode / DOM.getContentQuads 拿坐标
失效：Page.frameNavigated / DOM.documentUpdated → 清空该 tab RefStore（报 ref_stale，强制重 snap）
作用域：per-tab、内存级、随实例生命周期；跨命令稳定，跨导航不承诺
```

**原生输入**（修 P-2，D3）：

```
click(ref|css|xy)：
  定位 → DOM.scrollIntoViewIfNeeded →（可选 hit-test 校验：DOM.getNodeForLocation
  确认目标点仍落在预期元素，遮挡时报 element_blocked）
  → Input.dispatchMouseEvent: mouseMoved → mousePressed → (50ms) → mouseReleased
fill(ref, value)：DOM.focus → 全选清空 → Input.insertText（IME 安全）→ 回读 value 校验
press(key)：Input.dispatchKeyEvent keyDown/char/keyUp 三段
select(ref, value)：DOM API 设 value + 派发 change
hover(ref) / wheel(dx,dy) / scroll-to(ref)
```

**等待原语**（修 P-4）：`wait --css <sel> --state visible|hidden|attached|detached --timeout`；`wait-nav`（lifecycle 事件）。

**错误语义**（修 P-6，收敛版）：

| 错误码 | 含义 | agent 处置 |
|---|---|---|
| `ref_not_found` / `ref_stale` | 引用不存在 / 已因导航失效 | 重新 snap |
| `element_not_visible` / `element_blocked` | 不可见 / 被遮挡 | 检查浮层后重试 |
| `input_not_ready` | disabled / 未挂载完成 | wait 后重试 |
| `input_outcome_unknown` | 写超时，效果未知 | **必须先观察再决定**，禁盲目重试 |
| `cdp_timeout` / `cdp_disconnected` | 读超时 / 会话断开 | 可安全重试 |
| `dialog_intercepted` | 命令被对话框打断（已自动处理） | 读对话框日志继续 |

并入现有 AppError 体系（`[CODE] message` 格式 + CLI 退出码映射）。

**iframe 有限读**（按需，Phase 3）：不建完整 frame graph；仅支持"attach 指定 iframe session 后 eval 读取"，交互仍走坐标（Input 事件天然穿透 iframe 边界）。

### 4.3 CLI 命令族：`chrome-host page ...`

统一 `--instance <insId>`（必填），`<tabId>` 可选（缺省 = 实例活跃 tab）。退出码沿用 0–10 契约。

```
chrome-host page list <ins>                          # tab 列表（id/url/title）
chrome-host page snap <ins> [tabId] [--compact]      # AX 树 + @eN ref
chrome-host page click <ins> --ref @e3 | --css sel | --xy x,y [--tab t]
chrome-host page fill  <ins> --ref @e3 --value "..."
chrome-host page press <ins> --key Enter [--ref @e3]
chrome-host page select <ins> --ref @e3 --value v[,v2]
chrome-host page hover / wheel / scroll-to <ins> ...
chrome-host page type  <ins> "text"                  # 焦点处输入（保留，跨 iframe）
chrome-host page shot  <ins> [tabId] [--full-page] [--out f.png]
chrome-host page html  <ins> [tabId] [--css sel]
chrome-host page eval  <ins> [tabId] "<expr>"
chrome-host page console <ins> [--since cursor] [--level error]
chrome-host page network <ins> [--fail-only] [--body <reqId>]
chrome-host page wait  <ins> --css sel --state visible --timeout 10s
chrome-host page nav   <ins> <tabId> <url>
chrome-host page raw   <ins> <tabId> <method> [json] # CDP 透传（逃生舱，D5）
```

远程模式（exit 10 场景）：CLI 内部自动经代理访问 Mac 端应用，命令面**零变化**——`CDP_PORT/CDP_BASE/Node 版本`三个概念从 agent 视野消失。

### 4.4 MCP 暴露

同服务层直接注册 `page_*` tools（`page_snap/page_click/page_fill/page_console/page_network/...`），参数与 CLI 对齐（--ref → `ref: "@e3"`），33 → ~50 tools。MCP.md / AGENT-API.md 同步补契约。

### 4.5 skill 瘦身（L3，最后执行）

- `scripts/cdp.mjs` 从 skill 目录**删除**；
- SKILL.md 第 5 步"拿凭证 + cdp.mjs 命令 + 陷阱"整段替换为 `chrome-host page` 命令速查 + 观察-行动循环规矩（snap → ref 操作 → 变化后重 snap）；
- DPR 换算 / 索引漂移 / iframe 绕行 / daemon 说明等补偿文档全部删除（机制已内化）；
- skill 只剩知识、无代码 → 应用升级即操控能力升级，无 skill 漂移。

---

## 5. 实施路线

### Phase 1 — 止血 + 行为验证（cdp.mjs / TS，1–2 天级）

改仓库内 `skills/chrome-host/scripts/cdp.mjs`（安装版随之同步）：

1. 拆文件：`cdp.mjs` → `session.mjs`（L1：daemon/连接/事件）+ `tools/*.mjs`（L2：命令）；
2. 对话框接管（P-1）；3. click(selector) 改原生事件链（P-2）；
4. snap 加 @eN + click/fill 支持 ref（P-3）；
5. 补 fill/press/wait-for-element（P-4）。

**验收**：qa 页面弹 alert 不再假死；点击被浮层遮挡可被发现；skill 陷阱段删一半。
**产物**：一份"行为规范"（含边界用例）= Phase 2 的 Rust 实现规格。

### Phase 2 — 能力落应用（Rust）

1. `CdpSupervisor`（4.1）：会话表 + 对话框 + 事件缓冲，扩展 `ws.rs` 常驻形态；
2. L2 工具层（4.2）按 Phase 1 行为规范实现；
3. REST API → CLI `page` 命令族（4.3）→ MCP `page_*`（4.4）；
4. 契约文档三份同步（AGENT-API / CLI / MCP）。

**验收**：`chrome-host page snap/click/fill/console/network` 全链路可用（本地 + 远程两种模式）；退出码契约覆盖新命令。

### Phase 3 — 切换与退役

1. skill 瘦身（4.5），cdp.mjs 退役；
2. iframe 有限读（按需）；
3. vendor `@browser-skill/vom` 评估语义树升级（snap → observe）。

**验收**：skill 目录无脚本；新 agent 零 Node 依赖接入。

---

## 6. 开放问题与风险

| # | 问题 | 状态 |
|---|---|---|
| Q1 | 实例改 `--remote-debugging-pipe`（fd 管道替代 HTTP 端口）能否同时消除 "Allow debugging" 弹窗与端口分配器？ | 待实验验证（Puppeteer 默认方案；若可行，L1 再简化） |
| Q2 | Rust 侧 `Network.getResponseBody` 的 body 上限与编码（base64）处理 | Phase 2 设计时定 |
| Q3 | 多 agent 并发操控同一实例：沿用"编排类命令无锁、page 类命令 per-tab 串行"是否足够 | Phase 2 压测后定（BrowserSkill 是 per-session 串行队列） |
| Q4 | vendor vom 包后的上游跟随策略（锁版本 + 手动升级） | Phase 3 定 |
| Q5 | 事件缓冲内存预算（每 tab 条数/字节上限，超限丢弃策略） | Phase 2 定 |

---

## 附录 A：BrowserSkill 源码锚点速查（Phase 1/2 实现时对照）

| 能力 | 文件 | 锚点 |
|---|---|---|
| 对话框接管 + attach 缓存 | `apps/extension/src/browser-driver/chromium-cdp.ts` | 文件头注释 + `dialogCursor` |
| 原生输入链 + hit-test | `apps/extension/src/tools/interaction.ts` | `Input.dispatchMouseEvent` 调用点（~L560-605）、`Runtime.evaluate` 命中校验（~L875） |
| ref 存储/失效 | `apps/extension/src/session-manager/ref-store.ts` | `replace()` 整体替换 + generation |
| AX 采集 | `apps/extension/src/tools/vom/capture-coordinator.ts` | `Accessibility.getFullAXTree` per-frame 批次 |
| 错误语义 | `apps/extension/src/transport/types.ts` | `RpcErrorReason` 枚举 |
| 会话串行队列 | daemon 侧（Rust） | `crates/bsk-cli` daemon 模块 |
| VOM 渲染 | `packages/vom/src/render.ts` | `renderVom()`（CDP-free） |
