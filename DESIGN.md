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

所有语义色定义在 `src/main.css` 的 CSS 变量（亮/暗双值），`tailwind.config.js` 以
`rgb(var(--x) / <alpha-value>)` 引用；**必须通过语义类引用，不得写死十六进制**。

**触发机制**：跟随系统（`prefers-color-scheme: dark` media query），零 JS，主窗口与
Popover 同时生效，`color-scheme` 同步声明使原生滚动条/表单控件跟随。未来需应用内手动
切换时，变量已就位，只需换触发器（Tailwind 切回 `'class'` + `html.dark`）。

| Token | Light | Dark | Tailwind 类 | 用途 |
|---|---|---|---|---|
| accent | `#007AFF` | `#0A84FF` | `bg-accent` / `text-accent` | 主操作按钮、焦点态、进度条（macOS 系统蓝） |
| accent-hover | `#0071EB` | `#409CFF` | `hover:bg-accent-hover` | 主按钮悬停态（暗色提亮而非压暗） |
| ink | `#1D1D1F` | `#F5F5F7` | `text-ink` | 标题、强调正文 |
| ink-2 | `#6E6E73` | `#A1A1A6` | `text-ink-2` | 次要文字、ghost 按钮文字 |
| ink-3 | `#AEAEB2` | `#7C7C81` | `text-ink-3` | 辅助说明、placeholder、禁用导航 |
| hairline | `#E5E5EA` | `#38383A` | `border-hairline` / `ring-hairline` | 卡片描边、sidebar 分隔线 |
| hairline-soft | `#F5F5F7` | `#3E3E40` | `divide-hairline-soft` / `border-hairline-soft` | 卡内行间弱分隔（原 `divide-neutral-100` 归位） |
| surface | `#FFFFFF` | `#1E1E20` | `bg-surface` | 主窗口内容区底 |
| surface-sidebar | `#F5F5F7` | `#28282A` | `bg-surface-sidebar` | 侧边栏底色 |
| surface-card | `#FFFFFF` | `#2C2C2E` | `bg-surface-card` | 卡片 / Modal / Confirm 底 |
| surface-raised | `#FFFFFF` | `#3A3A3C` | `bg-surface-raised` | 浮起元素：nav active、ghost 按钮底 |
| surface-inset | `#F5F5F7` | `#3A3A3C` | `bg-surface-inset` | 卡内嵌套底（badge、代码块、进度槽） |
| toast | `#1D1D1F` | `#3A3A3C` | `bg-toast` | Toast 底（独立 token，ink 已被暗色反转为浅色） |
| status-running | `#34C759` | `#30D158` | `bg-status-running` | 运行中状态点、开启态开关 |
| status-starting | `#FF9F0A` | `#FF9F0A` | `bg-status-starting` | 启动中/停止中（配 `animate-pulse`） |
| status-stopped | `#D1D1D6` | `#636366` | `bg-status-stopped` | 已停止、开关关闭态 |
| status-error | `#FF3B30` | `#FF453A` | `bg-status-error` / `text-status-error` | 异常、危险操作、错误 Toast |
| run-text | `#248A3D` | `#30D158` | `text-run-text` | 运行中文字（亮底加深保证对比度，暗底直接用亮绿） |

**overlay 型中性色**（hover / focus ring）一律用前景色透明度：`hover:bg-ink/5`、
`focus:ring-ink/10`——一份类两个主题自动正确，禁止再写 `hover:bg-neutral-50` 一类亮色灰阶。

**使用规则**：

- 状态点一律用 `bg-status-*`；对应的**文字**用 `text-run-text`（绿字加深）或 `text-status-error`，禁止状态点与文字同色导致对比度不足。
- 危险操作（删除环境/实例）：常态 `text-ink-2`，hover 才变 `hover:bg-red-50 hover:text-status-error dark:hover:bg-red-500/15`。
- 状态徽章（`bg-red-50`/`bg-emerald-50` 等）必须配 `dark:bg-*-500/15` 半透明等价物。

### 2.2 半透明材质（仅 Popover）

Popover 宿主窗口由 Rust 侧提供原生 `NSVisualEffectView`（`Effect::Popover` + radius 12），
macOS 下**根容器必须保持透明**（不加 bg），材质感全部来自原生层；原生材质本身跟随系统
明暗，webview 侧靠 popover-* token 适配。

| 场景 | 类 |
|---|---|
| Popover 根容器（macOS） | 透明（无 bg 类）；非 macOS 回退 `bg-surface-card/95 shadow-2xl` |
| Header / Footer / 卡内分隔 | `border-popover-line`（原 `border-black/[0.06~0.08]` 两档统一为一档，亮色视觉差 2% 不可辨） |
| 环境卡 | `border-popover-line bg-popover-card backdrop-blur-sm` |
| 实例行小按钮 | `border-popover-line bg-popover-btn hover:bg-popover-btn-hover` |

**规则**：半透明材质上禁止使用不透明灰阶描边（`border-hairline` 仅用于主窗口卡片）。

### 2.3 字体与排印

- 字体：系统栈（`-apple-system` / SF Pro），代码与端口号用 `font-mono`。
- 字号阶梯：页面标题 `text-base(16) font-semibold` → 区块标题 `text-sm(14) font-medium` → 正文 `text-xs(12)` → 辅助 `text-[11px]` → Popover 紧凑文字 `text-[10px]`。
- 标题负字距：`tracking-[-0.2px]`（仅大标题，可选）。

## 3. 组件规范

### 3.1 按钮（定义于 `src/components/ui.tsx`）

| 层级 | 类 | 场景 |
|---|---|---|
| Primary | `btnPrimary`＝`bg-accent text-white hover:bg-accent-hover` | 每屏唯一主操作：+ Add Environment / + New Instance / Run |
| Ghost | `btnGhost`＝`border-hairline bg-surface-raised text-ink-2 hover:bg-ink/5` | 次操作：Focus / Restart / Stop / 取消 |
| 小按钮（Popover） | `text-[10px]` + `border-popover-line bg-popover-btn` | CDP / Restart / Stop / Start（半透明材质上） |
| 危险文字按钮 | `text-ink-2` + hover `bg-red-50 text-status-error dark:bg-red-500/15` | 删除环境 |
| 确认弹窗主按钮 | `bg-status-error text-white hover:brightness-90` | ConfirmDialog 的确认键（红色 = 不可逆） |

### 3.2 状态点（`StatusDot`）

`size-2 rounded-full` + 语义色映射（见 `ui.tsx`），旁配 `text-xs text-ink-2` 状态词。
七态：running / starting / stopping / stopped / created / error / crashed。

### 3.3 卡片

- 主窗口：`rounded-xl border border-hairline bg-white p-4`（环境卡、Settings 卡、详情页卡一致）。
- 列表卡（实例列表）：外框 `rounded-xl border-hairline bg-white`，行间 `divide-y`，行内 `px-4 py-2.5`。
- Popover 环境卡：`rounded-lg border-black/[0.06] bg-white/60 backdrop-blur-sm p-2.5`。

### 3.4 输入框

`inputCls`＝`rounded-lg border-hairline px-2.5 py-1.5 text-sm`，focus 态 `focus:border-ink-3 focus:ring-2 focus:ring-ink/10`。
不使用 accent 蓝做 focus（macOS 文本输入框 focus 为深灰描边，蓝色仅留给按钮与主操作）。

### 3.5 开关（Settings switch）

macOS 绿：开启 `bg-status-running`，关闭 `bg-status-stopped`；胶囊 `h-5 w-9`，旋钮固定
`bg-white`（macOS 惯例，暗色下同样保持白色旋钮）带轻阴影。

### 3.6 Modal / Confirm

遮罩 `bg-black/25 dark:bg-black/50`；容器 `rounded-xl bg-surface-card ring-1 ring-hairline`
`w-[380~420px]`，标题 `text-sm font-semibold text-ink`。

### 3.7 Toast

深色胶囊：成功 `bg-toast text-white`，错误 `bg-status-error text-white`；底部居中悬浮。

## 4. 布局骨架

- **主窗口**：左侧 Sidebar `w-44 bg-surface-sidebar`（品牌字 `text-[11px] font-semibold uppercase tracking-wide text-ink-3`；导航项 `rounded-lg px-2.5 py-1.5 text-xs`，active 态 `bg-surface-raised text-ink font-medium shadow-sm ring-1 ring-hairline`，hover `hover:bg-ink/5`）；内容区 `max-w-2xl mx-auto`。
- **Popover**：宽 380（Rust 持有），高度内容自适应 280~540（`popover-height` 事件上报）；结构＝Header（品牌 + Open Manager）→ 环境卡列表（滚动区）→ Footer（+ Add Environment）。

## 5. 禁则

1. **禁止**引入 token 之外的新色值（含 arbitrary value 如 `bg-[#xxx]`）；确需新语义色 → 先扩 `tailwind.config.js` + `main.css` 变量（亮/暗两值）并更新本文档与 .pen 设计稿。
2. 豁免灰阶（`red-50`/`neutral-*` 等 token 外既存类）**必须同时写 `dark:` 对应值**；新代码一律走语义类或前景 overlay（`ink/α`）。
3. **禁止**在 UI 样式调整中触碰功能逻辑（store 操作、API 调用、事件监听、轮询、条件渲染分支）。
4. **禁止**给 Popover 根容器加不透明背景（会挡住原生模糊材质）。
5. **禁止**一套页面一种圆角/阴影体系；圆角只有 `rounded-md`(6) / `rounded-lg`(8) / `rounded-xl`(12) 三级。
6. **禁止**在组件里手写 `prefers-color-scheme` media query 或 JS 主题切换；主题统一由 `main.css` 变量 + `dark:` 变体承载。
7. 改动 UI 后必须 `npm run build` 通过，并逐窗口人工核对主窗口 + Popover 两端观感（亮/暗两套系统外观下各过一遍）。
