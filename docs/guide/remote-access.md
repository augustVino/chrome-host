# 远程接入（云桌面 / CI）

## 解决什么问题

云桌面 / CI 机器上的 AI Agent 需要**驱动你 Mac 上的浏览器**（复用登录态、hosts 环境、GUI 上可见的窗口），而 chrome-host 的所有端口都只绑本机回环——这是安全底线，不能破。远程接入用「SSH 隧道 + 鉴权网关」在不暴露端口的前提下打通这条路。

## 架构一览

```
┌──────── 远程机器（云桌面 / CI）────────┐
│ AI Agent（CLI / MCP / REST）           │
│   控制面：http://127.0.0.1:17890/api/v1 │← 一律携带 Bearer token
│   CDP：  /cdp/<实例>/<会话Key>/…        │← 会话 key 即凭证
└───────────────┬───────────────────────┘
                │ 唯一一条 ssh -R 隧道（app 守护，自动重连）
┌────── 本地 Mac ─▼──────────────────────┐
│ 17891 隧道落地监听器：token 强制校验     │
│ 17890 本地监听器：现状不变，免鉴权       │
│ CDP 会话代理：转发到实例真实端口          │
│ （实例 CDP 端口 9222+ 永不出本机）       │
└────────────────────────────────────────┘
```

### 为什么是两个端口

SSH 隧道转发的请求到达 app 时，来源地址恒为 `127.0.0.1`（本机 ssh 进程代连）——TCP 层无法区分「本地 GUI 的请求」和「远程经隧道的请求」，**监听端口是唯一判别器**：

- **17890（本地口）**：GUI / 本地 CLI / 本地 MCP 用，免鉴权，与现状完全一致；
- **17891（隧道落地口）**：只接隧道流量，除 CDP 会话路径外全部校验 Bearer token。

### 两级凭证

| 凭证 | 作用域 | 保护什么 |
|---|---|---|
| **访问令牌**（token） | 全部控制面 API 与 MCP | 单用户全权凭证，可随时在 Settings 页轮换（轮换立即清空全部远程 CDP 会话） |
| **会话 key**（session） | 单个实例的 CDP 代理路径，TTL 30 分钟 | WebSocket 无法携带自定义 header，故凭证嵌在 URL 路径中，作用域收到最小 |

## Mac 侧配置（一次性）

1. chrome-host → **设置 → 远程接入**；
2. 填 SSH 目标（`user@host` 或 `~/.ssh/config` 别名）→ 保存 → 打开开关；
3. 状态行变绿（约 20 秒内）→ **复制访问令牌**。

## 远程机器配置（一次性）

```bash
# ① sshd 死链有界回收（需 sudo；网络分区后隧道自动恢复的保障）
echo 'ClientAliveInterval 30\nClientAliveCountMax 3' | sudo tee /etc/ssh/sshd_config.d/90-chrome-host.conf
sudo systemctl reload sshd

# ② 安装 CLI 并注入令牌
cargo install --path crates/cli        # 或交叉编译产物 scp 过来
echo 'export CHROME_HOST_TOKEN=<Settings 页复制的令牌>' >> ~/.zshrc

# ③（可选）MCP 客户端带令牌接入
claude mcp add --transport http --header "Authorization: Bearer <token>" chrome-host http://127.0.0.1:17890/mcp
```

## 稳态工作流（远程 Agent 全自主）

```bash
chrome-host status                                    # 探活（见下方三态诊断）
ENV_ID=$(chrome-host env list --json | jq -r '.[] | select(.name=="staging") | .id')
INS_ID=$(chrome-host instance create "$ENV_ID" --quiet)

# CDP 走会话代理：cdp-session --quiet 直接输出完整代理 URL（供 CDP_BASE 直接捕获）
CDP_BASE=$(chrome-host instance cdp-session "$INS_ID" --quiet)
CDP_BASE="$CDP_BASE" node skills/chrome-host/scripts/cdp.mjs list
CDP_BASE="$CDP_BASE" node skills/chrome-host/scripts/cdp.mjs shot <target>

chrome-host instance delete "$INS_ID" --yes           # 收尾
```

令牌注入：环境变量 `CHROME_HOST_TOKEN`，或隐藏 flag `--token`（优先级更高，同 `--api-url` 模式）；本地直连两者都不需要。MCP 客户端同样可用 `create_cdp_session` 工具取会话。

会话 30 分钟过期后重新 `cdp-session` 取新会话即可（cdp.mjs 本就逐命令建连，无长连接损失）。用户在远程全程零参与；仅当环境首次登录或 Chrome 弹 "Allow debugging" 授权框时，需要**在 Mac 本地点击**。

## 诊断三态（协议语义，非启发式）

| 现象 | 含义 | 处置 |
|---|---|---|
| 连接拒绝（exit 8） | 隧道断开或 Mac 应用未运行 | 等待自动重连（退避重试），或检查 Mac 侧状态行 / `chrome-host doctor` 的 remote_access 项 |
| 401（CLI exit 10） | 隧道与应用均在线，仅凭证问题 | 检查/轮换令牌，重试无意义 |
| 200 | 正常 | — |

## API 参考

端点、字段与错误码见 [Agent API 手册 · 远程接入](/reference/agent-api#远程接入-remote-access)；CLI 侧参数见 [CLI 手册](/reference/cli)。
