# 远程接入（云桌面 / CI）

## 解决什么问题

云桌面 / CI 机器上的 AI Agent 需要**驱动你主机上的浏览器**（主机 = 运行 chrome-host 桌面应用的 Mac 或 Windows；复用登录态、hosts 环境、GUI 上可见的窗口），而 chrome-host 的所有端口都只绑本机回环——这是安全底线，不能破。远程接入用「SSH 隧道 + 鉴权网关」在不暴露端口的前提下打通这条路。

## 架构一览

```
┌──────── 远程机器（云桌面 / CI）────────┐
│ AI Agent（CLI / MCP / REST）           │
│   控制面：http://127.0.0.1:17890/api/v1 │← 一律携带 Bearer token
│   CDP：  /cdp/<实例>/<会话Key>/…        │← 会话 key 即凭证
└───────────────┬───────────────────────┘
                │ 唯一一条 ssh -R 隧道（app 守护，自动重连）
┌────── 本地主机（Mac / Windows）─▼─────┐
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

## 云桌面要不要装 CLI？（按接入方式选）

| 云桌面上的 Agent 形态 | 需要安装 | 说明 |
|---|---|---|
| 用 `chrome-host` CLI 命令 | **装 CLI**（Release 下载独立产物，见下方安装方法） | 与本地同构体验，`CHROME_HOST_TOKEN` 注入令牌 |
| MCP 客户端（Claude Code / Cursor 等） | **不装 CLI** | `claude mcp add --transport http --header "Authorization: Bearer <token>" chrome-host http://127.0.0.1:17890/mcp` 直接接 |
| 自研脚本 / curl 直调 REST | **什么都不装** | `curl -H "Authorization: Bearer <token>" http://127.0.0.1:17890/api/v1/...` |
| 外部 CDP 客户端（Playwright 等） | 不装 CLI | 经会话代理 URL 接入（`instance cdp-session` 需由控制面下发——REST 调用即可） |

> 三种形态的共同前提只有一条：云桌面的 `127.0.0.1:17890` 能通（隧道落地）。

## 主机侧配置（一次性）

**本地电脑清单**：

1. chrome-host 桌面应用已运行（隧道由 app 守护，app 退出隧道即断）；
2. 本机能以 SSH **主动连出**到云桌面（方向是本地 → 云桌面，`ssh -R` 反向隧道；云桌面需运行 sshd 且网络可达，见下节）；
3. 一把**可免交互使用**的 SSH 私钥（见「SSH 密钥的配置与管理」）。

**操作**：

1. chrome-host → **设置 → 远程接入**；
2. 填 SSH 目标（`user@host` 或 `~/.ssh/config` 别名）→ 保存 → 打开开关；
3. 状态行变绿（约 20 秒内）→ **复制访问令牌**。

## SSH 密钥的配置与管理

app 用系统 `ssh` 建 `-R` 隧道，密钥体系完全复用你本机的 ssh 环境——app 不存任何私钥。

**本地（发起方）**：

- 隧道以 `BatchMode=yes` 运行（禁止任何交互提示）——私钥要么**无口令**（`ssh-keygen -t ed25519 -N ""`），要么**已加入 ssh-agent**（macOS：`ssh-add --apple-use-keychain ~/.ssh/id_ed25519` 并在 `~/.ssh/config` 写 `UseKeychain yes`）。带口令且未入 agent 的密钥会让隧道静默起不来；
- 默认密钥发现顺序与 ssh 一致（`~/.ssh/id_ed25519` 等）；也可在 `~/.ssh/config` 里给目标显式指定 `IdentityFile`——app 会解析你的 config（别名 / HostName / Port / User / IdentityFile / ProxyJump 均生效），仅剔除其中的转发指令行（防止用户自有转发干扰隧道）；
- 推荐给云桌面起个别名：

```sshconfig
# ~/.ssh/config（本地）
Host yun
  HostName cloud-desktop.internal   # 云桌面地址（IP 或域名）
  Port 22
  User agent
  IdentityFile ~/.ssh/id_ed25519
```

设置页的 SSH 目标直接填 `yun` 即可。

**云桌面（sshd 侧）**：

- 把本地公钥加入 `~/.ssh/authorized_keys`（`ssh-copy-id agent@cloud-desktop.internal` 一步完成）；
- 保持 sshd 可达 + 允许 TCP 转发（`AllowTcpForwarding yes`，多数发行版默认开）。

**主机密钥（中间人防护）**：

- 隧道用 `StrictHostKeyChecking=accept-new`：**首次**连接自动信任并记录云桌面主机密钥，之后密钥变化会拒绝连接（防中间人）。建议先手工 `ssh yun` 一次完成首次确认与指纹留档；
- 轮换：换私钥后重跑 `ssh-copy-id` 并在设置页关闭再打开远程接入开关即可，隧道自动以新密钥重建；访问令牌的轮换在 Settings 页独立进行（轮换令牌会即时清空全部远程 CDP 会话，隧道不受影响）。

**故障对照**：状态行/`doctor` 的 remote_access 项 + stderr 环形缓冲会给出 `Permission denied (publickey)`（密钥/口令问题）、`remote port forwarding failed`（云桌面 17891 被 stale 进程占用，app 会自动一次性清理）等原文特征。

### CLI 独立安装（无桌面 app 的机器）

Release 附带独立 CLI 产物（v0.7.5 起，每个版本三平台）：

| 产物 | 适用 |
|---|---|
| `chrome-host-cli-<版本>.x86_64-unknown-linux-musl.tar.gz` | **任意 Linux x86_64**（静态链接，无 glibc 版本依赖，Ubuntu/CentOS/Alpine 通吃） |
| `chrome-host-cli-<版本>.aarch64-apple-darwin.tar.gz` | macOS Apple Silicon |
| `chrome-host-cli-<版本>.x86_64-apple-darwin.tar.gz` | macOS Intel |

```bash
# Linux 云桌面为例（VERSION 替换为实际版本，如 0.7.5）
curl -LO https://github.com/augustVino/chrome-host/releases/latest/download/chrome-host-cli-<VERSION>.x86_64-unknown-linux-musl.tar.gz
tar -xzf chrome-host-cli-*.tar.gz
install -m 755 chrome-host ~/.cargo/bin/   # 或任意 PATH 目录
chrome-host --version                       # 验证
```

> macOS 产物同样解包后放 PATH 即可。源码编译（`cargo install --path crates/cli`）仍可用，但 Release 产物免工具链。

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

# 页面操控：page 命令族远程直用（API 经隧道，与编排命令同路，无需凭证准备）
chrome-host page snap "$INS_ID"                       # 可访问性树 + @eN 引用
chrome-host page click "$INS_ID" @e1                 # 原生输入点击

chrome-host instance delete "$INS_ID" --yes           # 收尾
```

令牌注入：环境变量 `CHROME_HOST_TOKEN`，或隐藏 flag `--token`（优先级更高，同 `--api-url` 模式）；本地直连两者都不需要。MCP 客户端同样可用 `page_*` 工具族直接操控。

外部 CDP 客户端（Playwright / DevTools）仍经会话代理接入：`chrome-host instance cdp-session "$INS_ID" --quiet` 输出代理 URL（30 分钟有效，过期重取）；page 命令族不需要它。用户在远程全程零参与；仅当环境首次登录时需要**在主机本地点击**。

## 诊断三态（协议语义，非启发式）

| 现象 | 含义 | 处置 |
|---|---|---|
| 连接拒绝（exit 8） | 隧道断开或主机应用未运行 | 等待自动重连（退避重试），或检查主机侧状态行 / `chrome-host doctor` 的 remote_access 项 |
| 401（CLI exit 10） | 隧道与应用均在线，仅凭证问题 | 检查/轮换令牌，重试无意义 |
| 200 | 正常 | — |

## API 参考

端点、字段与错误码见 [Agent API 手册 · 远程接入](/reference/agent-api#远程接入-remote-access)；CLI 侧参数见 [CLI 手册](/reference/cli)。
