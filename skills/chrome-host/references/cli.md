# chrome-host CLI 完整参考

按需查阅。命令、退出码、错误码均与 CLI 实现（`crates/cli`）冻结契约一致，勿凭记忆改写语义。

## 全局 flag

| flag | 作用 |
|------|------|
| `--json` | stdout 纯 JSON（错误体也走 stdout），配合 jq |
| `--quiet` | 仅输出主实体 id，供 `$(...)` 捕获 |
| `--verbose` | stderr 追加请求摘要（method/path/耗时，不含敏感内容） |
| `--yes` | 全局跳过确认（危险操作闸门，见 SKILL.md） |
| `--api-url` / `CHROME_HOST_API_URL` | 改基地址——**不要使用**，恒连默认本机地址（远程接入场景下默认地址恰好就是隧道入口） |
| `CHROME_HOST_TOKEN` | 远程接入令牌（环境变量；隐藏 flag `--token` 优先级更高；本地直连不需要） |

## 命令速查

### 诊断

```text
chrome-host status     健康摘要（exit 0 / 8 / 10 分支见 SKILL.md 第零步）
chrome-host doctor     逐项诊断 + 修复建议（存在失败项 → exit 1）
```

### 环境

```text
chrome-host env list                        列表（含运行摘要）
chrome-host env get <id>                    详情
chrome-host env create <name> [--hosts-source-url URL] [--icon PATH] [--session-capture off|first-login|always]
chrome-host env update <id> [--name N] [--hosts-source-url URL] [--startup-args ARGS] [--keep-alive BOOL] [--session-capture ...]
                                            只更新传入的 flag；值为空串 = 显式置空
chrome-host env session <id>                会话身份层状态（当前代/Cookie 数/历史代）
chrome-host env session-restore <id> <gen>  回滚 Cookie 保险库到历史代（仅 Cookie 层）
chrome-host env delete <id> --yes           删除（级联：运行中实例自动停止后删除）
chrome-host env status <id>                 实例状态计数
chrome-host env activity <id> [--limit N]   事件审计流（排查启动失败用）
```

### 实例与标签页

```text
chrome-host instance list [--env <envId>]   列表
chrome-host instance get <id>               详情（status/pid/cdpPort/profileDir）
chrome-host instance status <id>            轻量状态（轮询用）
chrome-host instance create <envId> [--name NAME]  创建即启动（同步阻塞；长操作无总超时）。--name 可选（1-64 字符、同环境唯一，重名 409 INSTANCE_NAME_CONFLICT）；多任务并行建议命名，后续命令按名引用（同名跨环境 → exit 2 列候选，改用 id）
chrome-host instance start|stop|restart <id>   生命周期（restart 重拉 hosts）
chrome-host instance open <id> <url>        新开标签页并导航（http/https 本地校验）
chrome-host instance tabs <id>              标签页列表（仅 page 类型；id 即 page 命令的 tab 参数）
chrome-host instance navigate <id> <tabId> <url>   导航既有标签页（丢旧页历史）
chrome-host instance focus <id>             唤出窗口到前台
chrome-host instance cdp <id>               CDP endpoint（供 Playwright/DevTools 等外部工具；page 命令族不需要它）
chrome-host instance cdp-session <id>       发放 CDP 会话 URL（外部 CDP 客户端远程接入用，30 分钟有效）
chrome-host instance console <id> [--level error] [--tab T] [--limit N]   console 环（500 条，重启即清；--tab 按 tab 过滤）
chrome-host instance network <id> [--failed-only] [--tab T]               请求环（失败/HTTP≥400 过滤；--tab 按 tab 过滤；条目带 bodyState，table 另有 BODY 列）
chrome-host instance network-body <id> <requestId> [--offset N] [--limit N]  读响应体（已脱敏；UTF-16 单位分页，truncated=true 时续读；404=不在环 409=不可读含归因）
chrome-host instance screenshot <id> [--out F]  当前页 png（缺省 <id>.png）
chrome-host instance har <id> [--out F]         请求环生成 HAR 1.2（缺省 <id>.har；body 可用条目含脱敏后 text）
chrome-host instance diagnostics <id>          页面诊断快照（错误/失败资源/LCP/FCP）
chrome-host instance delete <id> --yes      删除（运行中自动 stop→delete）
```

### 页面操控（page 命令族）

命令结构：`chrome-host page <子命令> <insId> [tab] ...`。`tab` 是 `instance tabs` 输出的 targetId 唯一前缀；单标签实例可省略，多标签缺省报 `PAGE_TAB_AMBIGUOUS` 并列出全部 tab。本地/远程同用，无需 CDP 端口或代理凭证。

```text
page snap    <id> [tab]                     可访问性树 + @eN 引用 + [hover]/[hover-reveal] 标记（--json 另含 hoverScan 状态）
page click   <id> <sel|@eN> [--tab t]       原生输入点击：可见性检查 + hit-test 拦截遮挡 + 三段鼠标事件
page fill    <id> <sel|@eN> <value> [--tab t]  聚焦+清空+输入+回读校验；空 value = 仅清空
page press   <id> <key> [--target <sel|@eN>] [--tab t]  键入：命名键/单字符/组合键（Control+A、Meta+V）；可先聚焦
page wait    <id> <sel> [--state visible|hidden|attached|detached] [--timeout 10s] [--tab t]  轮询等待（缺省 visible）
page navigate <id> <url> [--tab t]         页内导航（Page.navigate，保留历史；等 load 至多 25s，未证实 → loaded:false；net::ERR_* → 500 PAGE_NAV_FAILED）
page back    <id> [--tab t]                历史后退一步（边界 → 409 PAGE_NAV_NO_HISTORY）
page forward <id> [--tab t]               历史前进一步（边界 → 409 PAGE_NAV_NO_HISTORY）
page reload  <id> [--ignore-cache] [--tab t]  重载当前页（等待语义同 navigate）
page screenshot <id> [sel|@eN] [--out f] [--tab t]  png 截图：目标缺省=当前视口；元素路径先滚入视口再截包围盒（跨行取并集）
page pdf     <id> [--out f] [--landscape] [--no-print-background] [--paper-width 8.5] [--paper-height 11] [--tab t]  整页 PDF（缺省 Letter 纵向带背景）
page emulate <id> --preset NAME | --viewport WxH [--user-agent UA] [--mobile] [--touch] | --off [--tab t]  设备仿真（预设：iphone-14/iphone-se/pixel-7/galaxy-s23/ipad-air/desktop-1080p；仅作用该 tab，重复=覆盖，--off 恢复）
page eval    <id> <expr> [--tab t]          执行 JS（returnByValue+awaitPromise；优先用上面的原语）
page raw     <id> <method> [json] [--tab t] [--browser]  任意 CDP 命令透传（⚠ 最高权限逃生舱；--browser 发 browser 级）
page html    <id> [selector] [--tab t]      页面/元素 outerHTML
page clickxy <id> <x> <y> [--tab t]         视口 CSS 像素坐标点击（无 hit-test；跨域 iframe 入口）
page type    <id> <text> [--tab t]          当前焦点处输入（clickxy 点入跨域 iframe 后用它）
page loadall <id> <sel> [--interval 1.5s] [--tab t]  循环点击直到元素消失
page dialogs <id> [--tab t] [--since N]     自动接管弹窗历史（近 32 条；--since 增量拉取）
page download-wait <id> [--timeout-ms 30000] [--tab t]  等下一次下载完成（命令式：先调它，再另行 click 触发下载；落盘实例 profile/downloads/；超时 409 = 无下载发生，失败 500 = 下载了但失败）
page downloads <id> [--tab t]               下载终态记录环（近 20 条；STATE/FILE/BYTES/MATCHED；含失败与已被 download-wait 消费的）
page upload <id> <sel|@eN> <abs> [--tab t]  主机文件设入 input[type=file]（触发原生 change；必须物理绝对路径——macOS /tmp、/var 软链拒绝改传 /private/...；拒软链/目录/超100MB → 422，不存在 → 404）
```

**操作循环**：snap → 用 @eN 操作 → 页面变化后重 snap。导航后 @eN 整表失效（PAGE_REF_STALE），重新 snap，不要对旧引用重试。

**PAGE_* 错误码**（stderr 行 `[PAGE_XXX] message`，按码分支处置，不解析文案）：

| 错误码 | 处置 |
|---|---|
| PAGE_TAB_NOT_FOUND / PAGE_TAB_AMBIGUOUS | 前者：连接刚建立时重试同一命令；后者：带 tab 列表，显式选一个 |
| PAGE_REF_NOT_FOUND / PAGE_REF_STALE | 重新 snap 取新引用 |
| PAGE_ELEMENT_NOT_FOUND / PAGE_ELEMENT_NOT_VISIBLE | 核对选择器；不可见先处理可见性 |
| PAGE_ELEMENT_BLOCKED | 点击点被浮层遮挡（信息含遮挡元素）——先关浮层或改点遮挡层自身 |
| PAGE_TARGET_NOT_FILLABLE | 目标不是可输入元素（input/textarea/contenteditable 编辑根） |
| PAGE_FILL_VALUE_MISMATCH | 写入可能部分生效——先观察实际值再决定，勿盲目重试 |
| PAGE_KEY_UNKNOWN | 键名不支持，见 press --help 键表 |
| PAGE_WAIT_TIMEOUT | 超时，信息含最后观察态 |
| PAGE_NAV_NO_HISTORY | back/forward 已在历史边界（409），换方向或继续任务 |
| PAGE_NAV_FAILED | Page.navigate 返回 net::ERR_*（导航未发生）；与 loaded:false（已开始未证实）严格区分 |
| PAGE_EMULATE_UNKNOWN_PRESET | 预设名不在表内（422），message 列出全部可用预设 |
| PAGE_DISPATCH_FAILED | 附 effect_state：none=未发出可重试；unknown=已发出禁盲目重试，先观察 |
| PAGE_DOWNLOAD_TIMEOUT | download-wait 窗口耗尽且无终态（409）：确认触发动作是否执行；有下载在进行中会在 message 披露——加大 --timeout-ms 再试 |
| PAGE_DOWNLOAD_FAILED | 下载了但终态失败（interrupted/canceled/文件名不安全被拒，500）：重触发下载，勿盲目重试 |
| PAGE_UPLOAD_PATH_INVALID | 路径非绝对/经符号链接/非普通文件（422）：message 附物理路径提示，改传物理绝对路径 |
| PAGE_UPLOAD_FILE_NOT_FOUND | 主机文件不存在（404）：核对 API 侧主机路径（远程接入 = 运行 chrome-host 的机器） |
| PAGE_UPLOAD_FILE_TOO_LARGE | 超 100MB 上限（422）：拆分或换文件 |
| PAGE_TARGET_NOT_FILE_INPUT | upload 目标非 input[type=file]（422）：重 snap 核对目标元素 |
| CDP_COMMAND_FAILED（大面积超时 + dialogs 无记录） | 疑似无主对话框卡死（对话框在应用断连期间弹出，CDP 通道不注册）：用 page raw 发 Page.navigate 强制导航恢复，或让用户在浏览器点掉 |
| PAGE_EVAL_FAILED | 表达式抛异常（信息含异常文本） |

### 登录态

```text
chrome-host login-profile list              全量列表（含环境名）
chrome-host login-profile get <envId>       视图（惰性创建）
chrome-host login-profile capture <envId> [--from <insId>] [--yes]   捕获快照（--from 从已停止实例）
chrome-host login-profile reset <envId> --yes       清空快照（总是确认）
chrome-host env session <envId>             会话状态（策略/当前代/历史代）
chrome-host env session-restore <envId> <gen>       回滚 Cookie 保险库
```

常用路径是**实例即登录源**：实例里登录 → 停止实例 → 自动保存（first-login 默认策略）→
同环境新实例免登录；显式固化用 `capture --from`，状态与回滚用 `env session`。详见下文「登录态」章节。

### 扩展 / 设置 / 内核

```text
chrome-host extension list|get <id>         列表 / 详情（ready/missing/invalid；system=内置锁定）
chrome-host extension add <path>            注册本地目录（须含 manifest.json）
chrome-host extension enable|disable <id>   启停（system 扩展锁定 → exit 6）
chrome-host extension remove <id> --yes     移除注册项（不删源文件）
chrome-host settings get                    应用设置
chrome-host settings update [--developer-mode BOOL] [--env-label-position P] [--env-label-color C] [--start-url URL]
chrome-host runtime version                 内核版本状态（pinned/已装/下载中）
chrome-host runtime list                    版本列表（单 pinned 策略）
chrome-host runtime install [--timeout SEC] [--cancel]   安装（轮询到完成，超时 exit 9）；--cancel 中止下载
```

## 退出码契约（0–10 冻结）

依据退出码决策，勿解析错误文案；机器可读错误详情用 `--json`（`{"error":{"code","message"}}`）。

| Exit | 语义 | agent 处置 |
|------|------|-----------|
| 0 | 成功 | 继续 |
| 1 | 一般错误（协议破坏、未知 5xx、doctor 有失败项） | 转述错误行给用户，勿盲目重试 |
| 2 | 参数错误 / 危险操作缺 `--yes` / 本地校验失败 | 核对命令；若是确认闸门，先取得用户同意 |
| 3 | 资源不存在（`*_NOT_FOUND` 族） | 重新 `list` 核对 id，勿盲目重试 |
| 4 | 资源已存在（预留） | — |
| 5 | 运行时冲突 / 运行时错误（409 族、内核/启动/CDP 500 族） | 按下方 code 细分处置 |
| 6 | 权限拒绝（`EXTENSION_SYSTEM_LOCKED`） | 无解，告知用户 |
| 7 | 请求校验失败（400 族） | 修正参数后重试 |
| 8 | 服务不可达（应用未运行） | 提示用户启动应用，勿猜端口 |
| 9 | CLI 侧等待超时（`runtime install` 轮询超时） | `runtime version` 查进度，必要时继续等或 `--cancel` |
| 10 | 未授权（401，远程令牌缺失/错误） | 核对 `CHROME_HOST_TOKEN`；隧道与应用均在线，勿按 exit 8 处置 |

## 错误码细分决策表（exit 5/7 时看 `[CODE]`）

CLI 错误行格式 `[CODE] message（HTTP status）`。exit 3/6/8 已被退出码表覆盖，此表只列需细分的：

| code | 含义 | 处置 |
|------|------|------|
| `INSTANCE_ALREADY_RUNNING` | 重复 start | 直接复用现有实例 |
| `INSTANCE_NOT_RUNNING` | 操作需要运行中实例 | 先 `instance start` 再重试 |
| `PROFILE_IN_USE` | 捕获时登录浏览器未关（或捕获进行中） | 请用户关闭登录浏览器后重试 |
| `CDP_PORT_UNAVAILABLE` | 实例启动时 CDP 端口被占 | 直接重试 start（会重新分配端口） |
| `INSTANCE_START_FAILED` | Chrome 启动失败或 CDP 超时（进程已回收） | `env activity <id>` 看原因，重试一次；连续失败告知用户 |
| `HOSTS_FETCH_FAILED` / `HOSTS_EMPTY` | hosts 源拉取失败/内容为空，实例拒绝启动 | 检查 `hostsSourceUrl` 可达性，必要时 `env update --hosts-source-url` |
| `CDP_CONNECTION_FAILED` / `CDP_NOT_READY` | 实例 CDP 不可达（可能刚崩溃） | `instance status` 查看，必要时 `restart` |
| `KERNEL_NOT_READY` | 内核未下载 | `runtime install`，等待后重试 |

补充：应用启动时若 17890 被占，Agent API 降级禁用——表现为健康检查持续 exit 8，提示用户重启应用。错误为「响应格式异常」时，典型原因是**运行中的应用是旧版本**（缺 CLI 依赖的新端点），业务命令通常仍正常，请用户更新应用即可，勿误判为 API 故障。

## 登录态（实例即登录源）

默认工作流（环境策略 `first-login`，无需任何 capture 命令）：

1. 实例里人工完成登录（同 SKILL.md 主工作流第 4 步原则：停下等用户）
2. **停止该实例**（`instance stop`，或用户直接关窗口——关窗即保存）
3. Cookie 保险库自动落盘；`env session <envId>` 可查当前代 / Cookie 数 / 来源实例
4. 同环境 `instance create` 的新实例启动时自动注入，免登录（注入失败自动降级为空登录态，实例照常可用）

变体与工具：

- **显式固化某实例**：`login-profile capture <envId> --from <insId> --yes`（实例须已停止；覆盖已有快照需确认）
- **每次实例退出都保存**：`env update <envId> --session-capture always`；关闭保存用 `--session-capture off`
- **回滚登录态**：`env session-restore <envId> <gen>`（gen 来自 env session；仅回滚 Cookie 层，文件快照保留当前）
- `login-profile launch`（独立登录浏览器母本）已于 0.3.0 移除（breaking）——登录一律走实例即登录源

已知边界，引导用户时提前说明：

- **session cookie（关浏览器即失效那类）不落盘**——Chrome 标准语义。此类系统免登录 = 复用 running 实例。
- **单 session 互踢**：单 session 策略的内部系统，多实例共用同一快照会互相踢下线。此类环境一次只跑一个实例。
- 快照过期（后端 session 过期）→ 实例里重新登录并关闭（自动保存新代），或 `reset --yes` 后重来。

## 远程接入（云桌面 / 远程机器）

应用内置「远程接入」：守护一条 SSH 反向隧道（仅转发 17890 → 主机端 17891 鉴权监听器；主机 = 运行桌面应用的 Mac 或 Windows）。远程机器访问**自己的** `127.0.0.1:17890` 即等价于访问主机上应用的 API，CLI 默认地址零改动。

远程探活三态：

- 连接拒绝 → exit 8：隧道断或应用停，请用户到主机端检查（agent 无法远程修隧道）
- exit 10 → 仅令牌问题：核对 `CHROME_HOST_TOKEN`，或请用户在主机端 Settings 页轮换后重发；这同时证明隧道与应用都在线
- exit 0 → 一切正常

**exit 0 的二义性**：token 存在时它只证明「隧道 + 授权正常」。同机判别：`env -u CHROME_HOST_TOKEN chrome-host status` → exit 0 = 同机；exit 10 = 远程 shell。page/instance 命令远程同用（API 经隧道可达），无需区分处置。

要点：

1. **令牌**：远程调用需 `export CHROME_HOST_TOKEN=<令牌>`（主机端 Settings 页「远程接入」复制）。
2. **页面操控远程直用**：`chrome-host page <命令> <insId> ...` 经 API 通道，本地/远程行为一致，无需会话代理或直连端口。仅当外部工具（Playwright/DevTools）需要直接接入实例 CDP 时才用会话代理：

   ```bash
   CDP_BASE=$(chrome-host instance cdp-session <insId> --quiet)   # 30 分钟有效，供外部 CDP 客户端
   ```

   会话过期（404 `SESSION_NOT_FOUND`）→ 重新 `cdp-session`，无需重建实例。
3. 隧道状态在主机端查：`curl 127.0.0.1:17890/api/v1/remote-access/status` 或 `chrome-host doctor`（remote_access 检查项）。

## MCP 入口

应用内嵌 MCP server（`http://127.0.0.1:17890/mcp`，Streamable HTTP，53 个 tools，能力为 CLI 子集——无 settings / 内核下载）。已配置 MCP 的会话可直接用 tools；本 skill 默认走 CLI，因为退出码契约与确认闸门对 agent 更安全。两者调用同一服务层，状态一致。

## 安全与边界

- **扩展只引用已注册资源**：`extension add` 需显式注册（服务端校验 manifest），没有"按任意路径加载扩展"的入口——这是有意设计的安全边界（任意路径 = 任意代码执行），不要尝试绕过。
- hosts 注入仅影响实例进程（`--host-resolver-rules`），永不修改系统 hosts。
- API 仅绑定 127.0.0.1；不要把端口暴露到网络。
