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
chrome-host env create <name> [--hosts-source-url URL] [--icon PATH]
chrome-host env update <id> [--name N] [--hosts-source-url URL] [--startup-args ARGS] [--keep-alive BOOL]
                                            只更新传入的 flag；值为空串 = 显式置空
chrome-host env delete <id> --yes           删除（级联：运行中实例自动停止后删除）
chrome-host env status <id>                 实例状态计数
chrome-host env activity <id> [--limit N]   事件审计流（排查启动失败用）
```

### 实例与标签页

```text
chrome-host instance list [--env <envId>]   列表
chrome-host instance get <id>               详情（status/pid/cdpPort/profileDir）
chrome-host instance status <id>            轻量状态（轮询用）
chrome-host instance create <envId>         创建即启动（同步阻塞；长操作无总超时）
chrome-host instance start|stop|restart <id>   生命周期（restart 重拉 hosts）
chrome-host instance open <id> <url>        新开标签页（http/https 本地校验）
chrome-host instance tabs <id>              标签页列表（仅 page 类型）
chrome-host instance navigate <id> <tabId> <url>   导航既有标签页（丢旧页历史）
chrome-host instance focus <id>             唤出窗口到前台
chrome-host instance cdp <id>               CDP endpoint（--json 后 jq .port）
chrome-host instance cdp-session <id>       发放 CDP 会话 URL（远程自动化用，30 分钟有效）
chrome-host instance delete <id> --yes      删除（运行中自动 stop→delete）
```

### 登录态快照

```text
chrome-host login-profile list              全量列表（含环境名）
chrome-host login-profile get <envId>       视图（惰性创建）
chrome-host login-profile launch <envId>    打开登录浏览器（母本）
chrome-host login-profile capture <envId> [--yes]   捕获快照（覆盖已有快照需确认；登录浏览器须已关）
chrome-host login-profile reset <envId> --yes       清空快照（总是确认）
```

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

## 登录态快照（长期免登录）

实例每次启动克隆环境快照，"捕获一次、终身免登"。适合每天都要用的环境：

1. `login-profile launch <envId>` → 弹出独立登录浏览器（母本，不在 instances 列表）
2. 提示用户完成登录（等待人工，同 SKILL.md 主工作流第 4 步原则）
3. 用户登录后**让用户关闭该浏览器**（必须由用户关闭或自然退出）
4. `login-profile capture <envId> --yes`（已获用户同意才加 `--yes`）
5. 之后该环境 `instance create` 自动带登录态

已知边界，引导用户时提前说明：

- **session cookie（关浏览器即失效那类）无法靠快照保留**——Chrome 标准语义，不落盘。此类系统免登录 = 复用 running 实例，或每次走 launch → 登录 → capture。
- **单 session 互踢**：单 session 策略的内部系统，多实例共用同一快照会互相踢下线。此类环境一次只跑一个实例。
- 快照过期（后端 session 过期）→ 重走 launch/capture，或 `reset --yes` 后重来。

## 远程接入（云桌面 / 远程机器）

应用内置「远程接入」：守护一条 SSH 反向隧道（仅转发 17890 → Mac 端 17891 鉴权监听器）。远程机器访问**自己的** `127.0.0.1:17890` 即等价于访问 Mac 的 API，CLI 默认地址零改动。

远程探活三态：

- 连接拒绝 → exit 8：隧道断或应用停，请用户到 Mac 端检查（agent 无法远程修隧道）
- exit 10 → 仅令牌问题：核对 `CHROME_HOST_TOKEN`，或请用户在 Mac 端 Settings 页轮换后重发；这同时证明隧道与应用都在线
- exit 0 → 一切正常

要点：

1. **令牌**：远程调用需 `export CHROME_HOST_TOKEN=<令牌>`（Mac 端 Settings 页「远程接入」复制）。
2. **CDP 自动化走会话代理**（远程拿不到实例真实端口，`instance cdp` 的直连端口仅 Mac 本地有效）：

   ```bash
   CDP_BASE=$(chrome-host instance cdp-session <insId> --quiet)   # 30 分钟有效
   CDP_BASE=$CDP_BASE node scripts/cdp.mjs list
   ```

   会话过期（404 `SESSION_NOT_FOUND`）→ 重新 `cdp-session`，无需重建实例。
3. 隧道状态在 Mac 端查：`curl 127.0.0.1:17890/api/v1/remote-access/status` 或 `chrome-host doctor`（remote_access 检查项）。

## MCP 入口

应用内嵌 MCP server（`http://127.0.0.1:17890/mcp`，Streamable HTTP，25 个 tools，能力为 CLI 子集——无 settings / 内核下载 / 诊断）。已配置 MCP 的会话可直接用 tools；本 skill 默认走 CLI，因为退出码契约与确认闸门对 agent 更安全。两者调用同一服务层，状态一致。

## 安全与边界

- **扩展只引用已注册资源**：`extension add` 需显式注册（服务端校验 manifest），没有"按任意路径加载扩展"的入口——这是有意设计的安全边界（任意路径 = 任意代码执行），不要尝试绕过。
- hosts 注入仅影响实例进程（`--host-resolver-rules`），永不修改系统 hosts。
- API 仅绑定 127.0.0.1；不要把端口暴露到网络。
