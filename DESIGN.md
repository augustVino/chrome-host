# DESIGN.md — Chrome Host 主题风格规范

> 本文件是 UI 的**唯一视觉规范**，也是可直接投喂给 AI 的风格 prompt。
> 视觉源头：`chrome-host-ui.pen`（设计稿，pencil 格式，经 pencil MCP 查看）。
> 适用范围：主窗口（`src/App.tsx` + `src/pages/*`）与菜单栏 Popover（`src/pages/PopoverPage.tsx`）。
>
> **给 AI 的使用方式**：涉及 UI 调整时，只允许使用本文定义的 token 与组件类；
> 禁止引入 token 之外的色值；禁止因样式调整改动任何功能逻辑（hooks / API 调用 / 事件 / 数据流）。

---

## 1. 设计基调

- **macOS HIG 原生感**：贴近系统 app（Finder / System Settings）的材质与层次，不做重度品牌化装饰。
- **克制**：无边框嵌套卡片、无大面积渐变；层级靠「灰阶 + hairline + 半透明」表达，不靠重阴影。
- **双窗口同一套 token**：Popover 与主窗口共享色彩体系，仅材质（透明模糊）不同。

## 2. Design Tokens（Tailwind 语义类）

定义位于 `tailwind.config.js`，**必须通过语义类引用，不得写死十六进制**。

### 2.1 色彩

| Token | 值 | Tailwind 类 | 用途 |
|---|---|---|---|
| accent | `#007AFF` | `bg-accent` / `text-accent` | 主操作按钮、焦点态、进度条（macOS 系统蓝） |
| accent-hover | `#0071EB` | `hover:bg-accent-hover` | 主按钮悬停态 |
| ink | `#1D1D1F` | `text-ink` | 标题、强调正文、深色 Toast 底 |
| ink-2 | `#6E6E73` | `text-ink-2` | 次要文字、ghost 按钮文字 |
| ink-3 | `#AEAEB2` | `text-ink-3` | 辅助说明、placeholder、禁用导航 |
| hairline | `#E5E5EA` | `border-hairline` / `ring-hairline` | 卡片描边、sidebar 分隔线 |
| surface-sidebar | `#F5F5F7` | `bg-surface-sidebar` | 侧边栏底色 |
| surface-inset | `#F5F5F7` | `bg-surface-inset` | 卡内嵌套底（badge、代码底） |
| status-running | `#34C759` | `bg-status-running` | 运行中状态点、开启态开关 |
| status-starting | `#FF9F0A` | `bg-status-starting` | 启动中/停止中（配 `animate-pulse`） |
| status-stopped | `#D1D1D6` | `bg-status-stopped` | 已停止、开关关闭态 |
| status-error | `#FF3B30` | `bg-status-error` / `text-status-error` | 异常、危险操作、错误 Toast |
| run-text | `#248A3D` | `text-run-text` | 运行中文字（深绿，保证白底对比度） |

**使用规则**：

- 状态点一律用 `bg-status-*`；对应的**文字**用 `text-run-text`（绿字加深）或 `text-status-error`，禁止状态点与文字同色导致对比度不足。
- 危险操作（删除环境/实例）：常态 `text-ink-2`，hover 才变 `hover:bg-red-50 hover:text-status-error`。
- 交互 hover 的中性底统一 `hover:bg-neutral-50`（token 外的既存中性灰阶仅允许 hover 场景沿用）。
- 卡片内行分隔线 `divide-neutral-100` / `border-neutral-100` 沿用（白底上极浅分隔）。

### 2.2 半透明材质（仅 Popover）

Popover 宿主窗口由 Rust 侧提供原生 `NSVisualEffectView`（`Effect::Popover` + radius 12），
macOS 下**根容器必须保持透明**（不加 bg），材质感全部来自原生层：

| 场景 | 类 |
|---|---|
| Popover 根容器（macOS） | 透明（无 bg 类）；非 macOS 回退 `bg-white/95 shadow-2xl` |
| Header / Footer 分隔 | `border-black/[0.08]` |
| 环境卡 | `border border-black/[0.06] bg-white/60 backdrop-blur-sm` |
| 实例行小按钮 | `border-black/[0.08] bg-white/70 hover:bg-white` |
| 卡内实例列表分隔 | `border-black/[0.06]` |

**规则**：半透明材质上禁止使用不透明灰阶描边（`border-hairline` 仅用于主窗口白底卡片）。

### 2.3 字体与排印

- 字体：系统栈（`-apple-system` / SF Pro），代码与端口号用 `font-mono`。
- 字号阶梯：页面标题 `text-base(16) font-semibold` → 区块标题 `text-sm(14) font-medium` → 正文 `text-xs(12)` → 辅助 `text-[11px]` → Popover 紧凑文字 `text-[10px]`。
- 标题负字距：`tracking-[-0.2px]`（仅大标题，可选）。

## 3. 组件规范

### 3.1 按钮（定义于 `src/components/ui.tsx`）

| 层级 | 类 | 场景 |
|---|---|---|
| Primary | `btnPrimary`＝`bg-accent text-white hover:bg-accent-hover` | 每屏唯一主操作：+ Add Environment / + New Instance / Run |
| Ghost | `btnGhost`＝白底 `border-hairline` `text-ink-2` | 次操作：Focus / Restart / Stop / 取消 |
| 小按钮（Popover） | `text-[10px]` + `border-black/[0.08] bg-white/70` | CDP / Restart / Stop / Start（半透明材质上） |
| 危险文字按钮 | `text-ink-2` + hover `bg-red-50 text-status-error` | 删除环境 |
| 确认弹窗主按钮 | `bg-status-error text-white hover:brightness-90` | ConfirmDialog 的确认键（红色 = 不可逆） |

### 3.2 状态点（`StatusDot`）

`size-2 rounded-full` + 语义色映射（见 `ui.tsx`），旁配 `text-xs text-ink-2` 状态词。
七态：running / starting / stopping / stopped / created / error / crashed。

### 3.3 卡片

- 主窗口：`rounded-xl border border-hairline bg-white p-4`（环境卡、Settings 卡、详情页卡一致）。
- 列表卡（实例列表）：外框 `rounded-xl border-hairline bg-white`，行间 `divide-y`，行内 `px-4 py-2.5`。
- Popover 环境卡：`rounded-lg border-black/[0.06] bg-white/60 backdrop-blur-sm p-2.5`。

### 3.4 输入框

`inputCls`＝`rounded-lg border-hairline px-2.5 py-1.5 text-sm`，focus 态 `focus:border-ink-3 focus:ring-2 focus:ring-neutral-100`。
不使用 accent 蓝做 focus（macOS 文本输入框 focus 为深灰描边，蓝色仅留给按钮与主操作）。

### 3.5 开关（Settings switch）

macOS 绿：开启 `bg-status-running`，关闭 `bg-status-stopped`；胶囊 `h-5 w-9`，白色旋钮带轻阴影。

### 3.6 Modal / Confirm

遮罩 `bg-black/25`；容器 `rounded-xl bg-white ring-1 ring-hairline` `w-[380~420px]`，标题 `text-sm font-semibold text-ink`。

### 3.7 Toast

深色胶囊：成功 `bg-ink text-white`，错误 `bg-status-error text-white`；底部居中悬浮。

## 4. 布局骨架

- **主窗口**：左侧 Sidebar `w-44 bg-surface-sidebar`（品牌字 `text-[11px] font-semibold uppercase tracking-wide text-ink-3`；导航项 `rounded-lg px-2.5 py-1.5 text-xs`，active 态 `bg-white text-ink font-medium shadow-sm ring-1 ring-hairline`）；内容区 `max-w-2xl mx-auto`。
- **Popover**：宽 380（Rust 持有），高度内容自适应 280~540（`popover-height` 事件上报）；结构＝Header（品牌 + Open Manager）→ 环境卡列表（滚动区）→ Footer（+ Add Environment）。

## 5. 禁则

1. **禁止**引入 token 之外的新色值（含 arbitrary value 如 `bg-[#xxx]`）；确需新语义色 → 先扩 `tailwind.config.js` 并更新本文档与 .pen 设计稿。
2. **禁止**在 UI 样式调整中触碰功能逻辑（store 操作、API 调用、事件监听、轮询、条件渲染分支）。
3. **禁止**给 Popover 根容器加不透明背景（会挡住原生模糊材质）。
4. **禁止**一套页面一种圆角/阴影体系；圆角只有 `rounded-md`(6) / `rounded-lg`(8) / `rounded-xl`(12) 三级。
5. 改动 UI 后必须 `npm run build` 通过，并逐窗口人工核对主窗口 + Popover 两端观感。
