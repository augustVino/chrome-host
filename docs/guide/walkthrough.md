# 单环境闭环演练

从创建环境到自动化收尾的**完整闭环**，全程 CLI 可复制；每步附 GUI 等价操作。以「在隔离环境里排查 staging 站点」为场景：hosts 指向测试环境、需要登录态、最后用 CDP 自动化。

## 0. 前置检查

```bash
chrome-host status    # exit 0 = 应用运行中；exit 8 = 先启动 chrome-host（含托盘常驻）
chrome-host doctor    # 可选：数据库 / 内核 / 目录 / 扩展全量体检
```

## 1. 创建环境

环境 = hosts 配置源 + 一组隔离实例的容器。hosts 源可选（不填则不注入）：

```bash
chrome-host env create staging --hosts-source-url https://example.com/hosts.txt
ENV_ID=$(chrome-host env list --json | jq -r '.[] | select(.name=="staging") | .id')
```

> GUI 等价：环境页 → 新建环境 → 填名称与 hosts 配置源。

## 2. 登录态快照（可选，推荐）

需要免登录的环境走这一步；跳过则每个实例从全新登录开始：

```bash
chrome-host instance create "$ENV_ID" --quiet  # 打开实例，在弹出的 Chrome 里人工登录
# → 登录完成后关闭实例（或告诉用户直接关窗口）
chrome-host instance stop "$INS_ID"
chrome-host env session "$ENV_ID"              # 当前代 / Cookie 数 / 历史代
```

之后该环境**每次 `instance create` 的实例自动克隆登录态**（first-login 默认策略，首个实例退出时自动保存）。快照过期：实例里重新登录并关闭即保存新代；回滚历史代用 `env session-restore`。

> GUI 等价：环境详情 → 新建实例 → 登录 → 关闭；登录态卡片自动回正。显式固化某实例用 `login-profile capture --from`（0.3.0 起母本 launch 登录浏览器已移除）。

## 3. 创建并启动实例

```bash
INS_ID=$(chrome-host instance create "$ENV_ID" --quiet)    # 创建即启动
```

- 首次运行会阻塞于内核下载（Chrome for Testing 约 150MB），属正常现象；
- 弹出的 Chrome 窗口即隔离实例：独立 Profile + hosts 注入 + 登录态克隆 + 环境标签角标（内置扩展渲染，一眼区分环境）。

> GUI 等价：环境详情 → 新建实例。

## 4. 打开页面

```bash
chrome-host instance open "$INS_ID" https://staging.example.com
chrome-host instance tabs "$INS_ID" --json    # 查看已开页面（id 即 page 命令的 tab 参数）
```

## 5. 自动化（三种方式任选）

**方式 A：内置 page 命令族**（推荐，无需任何依赖，本地/远程同用）：

```bash
chrome-host page snap "$INS_ID"                          # 可访问性树 + @eN 引用
chrome-host page click "$INS_ID" @e1                     # 原生输入点击
chrome-host page fill "$INS_ID" "#kw" "搜索词"          # 表单填写（回读校验）
chrome-host instance screenshot "$INS_ID" --out now.png # 当前页截图
chrome-host page screenshot "$INS_ID" @e1 --out btn.png # 元素级截图（先 snap 拿引用）
chrome-host page screenshot "$INS_ID" --full            # 全页长截图（截发起时刻整文档，缺省 <id>-full.png）
chrome-host page pdf "$INS_ID"                            # 整页 PDF（缺省 <id>.pdf）
chrome-host page emulate "$INS_ID" --preset iphone-14   # 设备仿真（--off 恢复）
chrome-host page download-wait "$INS_ID"               # 挂住等下一次下载（先调它，再另行 click 下载链接）
chrome-host page upload "$INS_ID" 'input[type=file]' /private/tmp/a.png  # 文件设入上传框（物理绝对路径）
```

完整命令与 PAGE_* 错误码见 [CLI 手册 · 页面操控](/reference/cli#页面操控page-命令族)；操作循环经验见 [AI Agent Skills](./skills)。

**方式 B：外部 CDP 客户端直连**（Playwright / Puppeteer / DevTools）：

```bash
WS=$(chrome-host instance cdp "$INS_ID" --json | jq -r .webSocketUrl)
playwright --browser "cdp://$WS" ...
```

> 端口每次现查：实例重启后 CDP 端口可能变化，不要缓存。远程机器经会话代理接入（`instance cdp-session`）。

**方式 C：交给 AI Agent**：通过 MCP（含 `page_*` 工具族）或加载仓库 skills 接入，见 [AI Agent Skills](./skills)。

## 6. 场景：跨域 iframe（OOPIF）内的表单

第三方登录框、嵌入的支付/验证码组件常是跨域 iframe。`page snap` 会把帧内容以 `[frame]` 段内嵌输出，@eN **跨帧连续编号**：

```text
@e3 [textbox] 邮箱
[frame] https://accounts.foo.com/embed
  @e7 [textbox] 手机号
  @e8 [button] 获取验证码
```

帧内元素与主文档同套操作：`page fill "$INS_ID" @e7 "13800138000"`、`page click "$INS_ID" @e8` 直接可用（点击自动做帧内坐标投影，输入走帧会话）——**不再需要 clickxy 点入 + type 的坐标兜底**（兜底路径仍可用）。CSS 选择器仍只查主 frame，帧内定位一律用 @eN。

循环节奏：帧内导航/单帧重载只失效**该帧**的引用（报 `PAGE_REF_STALE`）——重新 snap 后继续，主文档引用照常可用；帧整体移除则该段消失，按新快照重定锚。

## 7. 场景：表格数据提取（订单列表 → JSON）

后台列表页拿结构化数据用 `page extract`，不要 `page eval` 手拼 innerText：

```bash
chrome-host page extract "$INS_ID"                    # 缺省 = 主文档最大表格自动发现
chrome-host page extract "$INS_ID" @e12              # 用 snap 到的表格引用（更稳）
chrome-host page extract "$INS_ID" '#orders table' --out orders.json   # 全量 JSON 写文件
```

输出 `{ columns, rows, coverage, meta }`，两个关键语义：

- **单元格 `null` vs `""`**：`null` = 被合并单元格覆盖/空位（rowspan/colspan 占位），`""` = 真实空单元格——下游处理别把两者混为一谈；
- **coverage 诚实标记**：`datasetComplete` 只有 `unknown` / `incomplete`，**永不装作取全**。虚拟化表格（DOM 只挂可见行）会标 `incomplete` 并附 `reasons`（如 aria-rowcount 超出可见行、预算 10000 单元格截断、空 tbody）——先 `page wait` 行出现或滚动加载后重取，绝不把 incomplete 当全量入库；`unknown` = 页面未声明总行数，无法判定。

目标必须是 `table` / `[role=table|grid|treegrid]`，否则 422（exit 7）。

## 8. 收尾清理

```bash
chrome-host instance delete "$INS_ID" --yes    # 运行中实例自动 stop → delete
chrome-host env delete "$ENV_ID" --yes         # 环境仍有运行实例时会被拒绝（先停/删实例）
```

危险操作在非 TTY（脚本/CI）环境缺 `--yes` 一律 exit 2，绝不静默执行；保留环境只删实例也完全可以——环境数据（登录态快照、配置）留在环境上。

## 常见岔路

| 现象 | 处置 |
|---|---|
| `exit 8` | 应用未运行，先启动 chrome-host |
| `exit 5` + `INSTANCE_ALREADY_RUNNING` 等 409 族 | 状态冲突不是错误：按状态机处理（先 stop 或直接复用运行中实例），不要盲目重试 |
| `instance create` 卡住 | 首次内核下载属正常；CI 里为 create/start/restart 设置作业级超时 |
| 快照登录态失效 | session cookie 类站点重启即失效（Chrome 标准语义），实例里重新登录并关闭即保存新代 |

同一闭环在云桌面 / CI 上远程驱动本地浏览器的方式，见[远程接入](./remote-access)。
