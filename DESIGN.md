# DESIGN.md — Chrome Host 主题风格规范（v2 · Aurora Glass）

> 本文件是 UI 的**唯一视觉规范**，也是可直接投喂给 AI 的风格 prompt。
> 适用范围：主窗口（`src/App.tsx` + `src/pages/*`）与菜单栏 Popover（`src/pages/PopoverPage.tsx`）。
> 当前为 v2（Aurora Glass）；v1（macOS HIG 克制风格）已废弃。
>
> **给 AI 的使用方式**：涉及 UI 调整时，只允许使用本文定义的 token、组件类与动效入口；
> 禁止引入 token 之外的色值；禁止因样式/动效调整改动任何功能逻辑（hooks / API 调用 / 事件 / 轮询 / 数据流）。

---

## 1. 设计基调

- **Aurora Glass**：流动的极光背景 + 整窗一张通透的玻璃面板 + 渐变高光，亮/暗双主题。侧栏与主区**不做独立的圆角分块**，只以一条细线分隔（对标 Finpay 式的大气留白）；层次只在内容卡与英雄卡上表达。
- **动效是体验的一部分，但永远可降级**：所有动效由 GSAP 驱动，受「动效档位」治理（§3），`static` 档与系统「减少动态效果」下必须完全静态可用。
- **双窗口同一套 token**：主窗口与 Popover 共享色板，仅材质不同（Popover 在 macOS 上使用原生模糊）。
- **功能优先**：视觉与动效改动不得影响数据流；动画只由已有状态的变化触发，轮询重渲染不得重放动画。

## 2. Design Tokens

定义位于 `src/main.css`（`:root` 亮色 + `@media (prefers-color-scheme: dark)` 暗色）与 `tailwind.config.js`。
**必须通过语义类 / CSS 变量引用，不得写死十六进制。**

**触发机制**：跟随系统外观（`prefers-color-scheme`，Tailwind `darkMode: 'media'`），零 JS，主窗口与 Popover 同时生效。

### 2.1 色彩

颜色变量是 **RGB 三元组**，Tailwind 以 `rgb(var(--x) / <alpha-value>)` 引用，因此支持 `/alpha` 修饰（如 `bg-mint/15`）。

| 语义 | 变量 | Tailwind 类 | 用途 |
|---|---|---|---|
| brand / brand-2 | `--brand` `--brand-2` | `bg-brand` `bg-brand-2` `text-brand` | 主操作、选中态、品牌渐变（靛 → 紫） |
| mint | `--mint` | `text-mint` `bg-mint/15` | 运行中、成功、启动按钮 |
| coral | `--coral` | `text-coral` | 异常、危险操作 |
| amber | `--amber` | `text-amber` | 启动中/停止中、警告 |
| peach / sky | `--peach` `--sky` | `text-peach` `text-sky` | 极光与装饰辅助色 |
| ink / ink-2 / ink-3 | `--ink*` | `text-ink` `text-ink-2` `text-ink-3` | 标题 / 次要 / 辅助文字 |
| run-text | `--run-text` | `text-run-text` | 运行中文字（亮底加深以保证对比度） |
| toast | `--toast` | `bg-toast` | Toast 底 |
| surface | `--bg` | `bg-surface` | 窗口底（极光之下） |

**玻璃 / 描边类变量是完整颜色值（含 alpha），不支持 `/alpha` 修饰**：

| 变量 | Tailwind 类 | 用途 |
|---|---|---|
| `--glass` | `bg-surface-sidebar` | 整窗玻璃面板底（`.layout`） |
| `--glass-2` | `bg-surface-card` `bg-surface-raised` | 卡片 / 浮起元素 |
| `--inset` | `bg-surface-inset` | 卡内嵌套底（代码块、输入框、进度槽） |
| `--line` `--line-soft` | `border-hairline` `border-hairline-soft` | 描边 / 弱分隔 |
| `--popover-*` | `bg-popover-card` `border-popover-line` 等 | Popover 半透明材质 |

**旧语义名**（`accent` `status-*` `surface-*` `hairline` `ink`）仍保留并映射到新色板，便于未改造代码继承配色；**新代码优先使用上表的新语义名**。

**环境渐变色盘**：`.g1 – .g8`（定义 `--ga` / `--gb`），由 `src/lib/envColor.ts` 的 `envGradient(id)` 按环境 id 哈希确定性取色，不落库。色盘刻意**降饱和、偏蓝紫**（靛紫 / 天蓝 / 玫瑰淡紫 / 青蓝 / 兰花粉 / 蜜桃 / 灰蓝薰衣草），**避开纯绿与纯红**（二者分别表示运行 / 异常）；`.tile` 光晕用 `color-mix(--ga 60%)`。环境首字母头像、卡片 tile、Deck 卡片、Popover 色条均使用同一渐变。新增色盘项时须遵守上述原则。

**使用规则**：
- **主题色是 brand（靛 → 紫渐变）**：主操作按钮、导航选中态、开关开启态、分段控件选中态一律用 brand。mint 仅用于「运行 / 成功 / 启动」语义（运行状态点、running 徽章、启动按钮）；coral 仅用于异常与危险操作；amber 仅用于启动中与警告。**禁止把 mint 用在通用控件上。**
- 状态点用 `StatusDot`；对应文字用 `text-run-text`（运行）或 `text-coral`（异常），禁止状态点与文字同色导致对比度不足。
- 徽章背景用语义色半透明（`bg-mint/15`、`bg-coral/15`、`bg-amber/15`），无需再写 `dark:` 对应值。
- hover / focus 的中性 overlay 用前景透明度：`hover:bg-ink/5`。

### 2.2 材质

| 类 | 说明 |
|---|---|
| `.layout` | 整窗玻璃面板：`position:absolute; inset:0`，背景 `--glass`，**无外边距、无圆角、无投影、无 `backdrop-filter`**（极光本身已是柔和渐变，模糊无视觉收益且耗性能） |
| `.gcard` / `.env-card` / `.side-card` | 20–24px 圆角内容卡：`--glass-2` 底 + 投影。**亮色无描边**，靠 `--card-ambient`（环绕淡影）+ 下方大投影浮起；**暗色**投影不可见，保留 1px `--card-line` 描边。描边/投影统一通过 `--card-line` / `--card-ambient` 变量控制，不要在卡片上单独写 `ring` / `border`。**不叠 `backdrop-filter`** |
| `.aurora` / `.blob` / `.grain` | 窗口底层的极光 blob 与颗粒噪点；由 `Aurora.tsx` 驱动 |
| `.inset` 背景 | `bg-surface-inset`，嵌套内容底 |

**性能规则**：
- 主窗口不使用 `backdrop-filter`；如确需（如浮层），同屏 blur 元素 **≤ 4 个**，且不得嵌套。
- `static` 档下 `.layout` 自动换为 `--glass-solid` 不透明底，`.pop` 去掉 blur。
- Popover：macOS 根容器**保持透明**（材质来自原生 `NSVisualEffectView`）；非 macOS 使用 `.no-vibrancy`（`--glass-solid` 底 + 投影）。

### 2.3 字体与排印

- 正文：`Plus Jakarta Sans Variable`，回退 `PingFang SC` / `Microsoft YaHei` / `system-ui`（`--ff`，`font-sans`）。
- 等宽：`JetBrains Mono Variable`，回退 `ui-monospace` / `SFMono-Regular` / `Menlo`（`--mono`，`font-mono`），用于端口、版本号、代码、方法标签。
- 字体来自 npm 包 `@fontsource-variable/*`，在 `src/main.tsx` 引入，随应用打包，离线可用。
- 字号阶梯：页面标题（`.title`）→ 区块标题 `text-sm font-medium` → 正文 `text-xs` → 辅助 `text-[11px]` → Popover 紧凑文字 `text-[10px]`；眉标用 `.eyebrow`（大写、字距）。

## 3. 动效体系（GSAP）

### 3.1 统一入口

- GSAP 及全部插件**只在 `src/motion/gsap.ts` 注册一次**（Flip / SplitText / ScrambleText / DrawSVG / MorphSVG / MotionPath / Physics2D / ScrollTo / CustomEase / CustomBounce / CustomWiggle / `useGSAP`）。其它文件一律 `import { gsap, ... } from '../motion/gsap'`，禁止直接 `from 'gsap'`。
- 全局默认：`duration .56`，`ease: 'ch-out'`（自定义缓动）；弹性用 `ch-bounce`，摆动用 `ch-wiggle`。
- React 内动画一律用 `useGSAP`（或 `gsap.context`）并设 `scope`，保证 StrictMode 双挂载与卸载时可 revert；`from()` 类 tween 尤其如此。

### 3.2 动效档位（`src/motion/quality.ts`）

| 档位 | 行为 |
|---|---|
| `full` | 全部动效：极光漂移、视差、tilt、磁吸、粒子、光点飞行 |
| `lite` | 极光减速（`AMB.timeScale(.5)`），关闭视差 / 倾斜 / 粒子，保留基础转场 |
| `static` | 无动画，直接到终态（含 `backdrop-filter` 关闭） |

- 用户偏好 `auto | full | lite | static` 存于 `localStorage['ch-motion-pref']`，通过 `html[data-q]` 暴露给 CSS；设置页「动效档位」可切换。
- `auto`：系统「减少动态效果」→ `static`；FPS 治理器连续 3 次（1.2s 窗口）平均帧 > 27ms → 自动降到 `lite` 并 Toast 提示。
- 窗口失焦 / 页面隐藏时，常驻时间线 `AMB`（极光等）自动暂停。
- **新增任何持续性或大面积动效，必须读 `qMode()` 并在 `static` 下跳过；持续性动画必须挂在 `AMB` 上或自行在失焦时暂停。**

### 3.3 约定

- **只由状态变化触发**：`StatusDot` 通过「上一状态 → 当前状态」的 diff 播放生命周期 FX；首次挂载与轮询导致的重渲染只做静态绘制，不得重放。
- **页面入场**：各页面用 `usePageEnter`（标题遮罩 + `.rv` 错峰 + `.row-in`）；应用冷启动时通过 `enterDelay()` 等待开场序曲结束。
- **列表增删**：用 `useGhostList`（退场幽灵项）+ `useFlipOnChange`（Flip 位移补间，自动排除 `[data-leaving]`）。
- **共享元素**：环境卡 → 详情页用 `data-flip-id` + `setPendingFlip / peekPendingFlip`（1.5s 非破坏性窗口）。
- **启动光点**：New 按钮 → 环境卡的光点飞行通过 `markLaunch / takeLaunch` 侧信道传递起点。
- **MotionPath** 的 `path` 必须是 `<path>` 元素（`<circle>` 等不受支持，会抛错）。
- **开场序曲**（`Shell`，约 0.95s 完成主体）：极光渐显 → 侧栏弹入 → Logo 弹入 + 品牌名逐字 → 导航图标描边 → 内容上浮；点击任意处可跳过，`static` 档不播放。
- 过场：路由切换由 `Shell` 做「旧页 200ms 淡出 → 换页 → 新页入场」，不要在页面里重复实现。

## 4. 组件规范

### 4.1 按钮（常量定义于 `src/components/ui.tsx`，样式在 `main.css` `.btn*`）

| 层级 | 常量 / 类 | 场景 |
|---|---|---|
| Primary | `btnPrimary`（`btn btn-brand`） | 每屏唯一主操作：Add Environment / Run |
| Mint | `btnMint`（`btn btn-mint`） | 启动类操作：New Chrome / New。浅 mint 渐变底 + **深绿字**（`#053b2e`，对比度 >8:1）；浅底配白字对比度不足（<2:1），不要改成白字 |
| Ghost | `btnGhost` / `btnGhostSm`（`btn-ghost`，`-xs` 小号） | 次操作：Focus / Restart / Stop / 取消 |
| 危险 | `btnDanger`（实心）/ `btnQuietDanger`（文字，hover 才变色） | 删除 / 确认不可逆操作 |
| 图标按钮 | `.icon-btn` / `.ib` + `data-tip` | Popover 与卡内紧凑操作 |

按钮带 `::after` 扫光；尺寸修饰 `btn-lg` / `btn-xs`。

### 4.2 状态点 `StatusDot`（`src/components/StatusDot.tsx`）

七态：running / starting / stopping / stopped / created / error / crashed。
内含生命周期 FX（starting 轨道旋转、running 粒子爆发、crashed 抖动等），props：`status` `className` `label` `insId` `flyFromEnv`。**不要在页面里自绘状态点。**

### 4.3 其它通用组件（`src/components/`）

| 组件 | 说明 |
|---|---|
| `PageHeader` | 页头：`.title`（SplitText 入场）+ `.sub` + 右侧操作区。**一级页面（Environments / Instances / Login Profiles / Extensions / Agent API / Settings）必须统一使用，必须带一行 `sub` 描述；右侧只放状态或主操作，不得在页头下另放说明框** |
| `Seg` | 分段控件，带滑动指示器（`.seg` `.seg-ind`） |
| `Switch` | 开关，开启态走 brand 渐变（与主题色一致，**不用 mint**），旋钮弹性动画 |
| `ModalShell` / `Modal` / `Field` | 带进出场动画的弹窗骨架；`ConfirmDialog` 复用 `ModalShell`（`confirm.tsx` 的 store API 不变） |
| `EmptyState` | 空态：描边环 + 轨道光点（`<path>` + MotionPath） |
| `Toast` | GSAP 驱动；`compact` 用于 Popover |
| `Odometer` | 数字滚动计数（概览英雄区） |
| `Aurora` | 极光背景层（挂在 `.shell` 下，驱动 `--a-mint` / `--a-coral`） |
| `Icon` / `GradientDefs`（`icons.tsx`） | 统一线性图标（24 栅格、1.8 描边）；SVG 渐变 `#ch-lg` |

### 4.4 输入、卡片、徽章

- 输入框：`inputCls`（`.input`），focus 为 brand 描边 + 柔光。
- 内容卡：`.gcard`（可叠加 `row-in` 入场）；列表行 `.row`；空态区块 `.empty`。
- 徽章 `.badge(.on/.err/.warn)`、标签 `.chip`、方法标签 `.mtag-*`、代码块 `.code`。

### 4.5 品牌 Logo

使用软件图标 `src/assets/logo.png`（取自 `src-tauri/icons/128x128@2x.png`），主窗口侧栏 30px / Popover 头部 20px，`border-radius` 8 / 6。**不要再用自绘的渐变圆环**；图标更新时同步替换该文件。

## 5. 布局骨架

- **主窗口**：`.shell` → `Aurora`（底）→ `.layout`（整窗玻璃面板，无外边距/间距）→ 左 `.sidebar`（宽 214，右侧 1px `--line` 分隔线）+ 右 `.content`（`.scroller` 滚动，内边距 30px）。**禁止给侧栏/主区加圆角、投影或独立背景块。**
- **侧栏**：Logo + 品牌名 → 导航（`.nav-item`，滑动指示器 `.nav-ind`，Instances 项带运行数徽章）→ 底部区 `.side-bottom`：欢迎卡 `SideHello`（轻量 SVG 插图 + "Welcome back!" + 运行状态摘要 + 「连接 AI Agent」入口；插图漂浮动画挂在 `AMB` 上；窗口高度 < 680px 时自动隐藏）+ 一行版本号 `.side-foot`（`v{app}`）。**内核信息不放侧栏**，仅出现在 Environments 页的内核 pill 与 Settings。
- **macOS 标题栏**：`tauri.conf.json` 使用 `titleBarStyle: "Overlay"` + `hiddenTitle` + `trafficLightPosition: { x: 22, y: 22 }`，让红绿灯与侧栏品牌区左对齐；顶部 26px 为拖拽区（`.drag-region`，`data-tauri-drag-region`，仅 Mac 渲染），侧栏顶部留白 50px。窗口拖拽需 capability `core:window:allow-start-dragging`。
- **Popover**：宽 380（Rust 持有），高度由内容自适应 280~540（`ResizeObserver` → `popover-height` 事件，该逻辑不得改动）；结构＝Header（Logo + 品牌 + Open Manager）→ 环境卡列表（滚动区）→ Footer（+ Add Environment）；窗口 `focus` 时重放入场动画。

## 6. 禁则

1. **禁止**引入 token 之外的新色值（含 `bg-[#xxx]` 等 arbitrary value）；确需新语义色 → 先扩 `tailwind.config.js` + `main.css` 变量（亮/暗两值）并更新本文档。
2. **禁止**在 UI 样式/动效调整中触碰功能逻辑（store 操作、API 调用、事件监听、轮询、条件渲染分支）。
3. **禁止**给 Popover 根容器（macOS）加不透明背景（会挡住原生模糊材质）。
4. **禁止**把侧栏 / 主区做成独立的圆角块（含投影、rim 描边）；**禁止**嵌套 `backdrop-filter`，同屏 blur 元素不得超过 4 个。
5. **禁止**直接 `import from 'gsap'` 或重复注册插件；一律经 `src/motion/gsap.ts`。
6. **禁止**新增不尊重 `qMode()` / 减少动态效果的动画；持续性动画必须可暂停。
7. **禁止**让轮询导致的重渲染重放动画；动画触发条件必须是明确的状态变化。
8. **禁止**在组件里手写 `prefers-color-scheme` media query 或 JS 主题切换；主题统一由 `main.css` 变量承载。
9. 玻璃 / 描边 / popover 类 token 是完整颜色值，**不得**加 `/alpha` 修饰。
10. 改动 UI 后必须 `pnpm exec tsc --noEmit` 与 `pnpm build` 通过，并逐窗口人工核对主窗口 + Popover（亮/暗两套系统外观，`full` 与 `static` 两个动效档位各过一遍）。
