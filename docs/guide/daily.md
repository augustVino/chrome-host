# 日常使用

## 托盘与窗口

- 应用常驻**托盘**：主窗口关闭默认隐藏到托盘（不退出），Quit 在托盘菜单；
- **Popover 快捷面板**：点击托盘图标弹出，快速查看与操作环境/实例，失焦自动收起；
- 支持开机自启（自启时仅托盘常驻，不弹窗口）。

## 设置页（Settings）

| 卡片 | 说明 |
|---|---|
| 内核 | Chrome for Testing 状态、版本、下载/重装 |
| 默认起始页 | 实例与登录浏览器打开的初始页面；留空 = `about:blank`；须 `http(s)://` |
| Developer Mode | 开发者模式开关 |
| 环境标签 | 环境标识角标的位置（四角）与颜色，叠加显示在实例窗口上，多环境并行时一眼区分 |
| 命令行工具 | CLI 安装 / 修复 / 卸载（PATH 链接管理） |
| 应用更新 | 版本检查与自动更新 |

## 活动流（Activity）

环境的实例启停、崩溃自动恢复、登录态快照等关键事件均留痕：

- GUI：环境详情的 activity 列表；
- CLI：`chrome-host env activity <envId>`；
- REST / MCP 同样可查（见对应手册）。

排障时先看活动流，再跑 `chrome-host doctor`。

## 数据目录与备份

全部数据集中在应用数据目录（Tauri 标准位置，按平台）：

| 平台 | 路径 |
|---|---|
| macOS | `~/Library/Application Support/com.chromehost.dev/` |
| Linux | `~/.config/com.chromehost.dev/` |
| Windows | `%APPDATA%\com.chromehost.dev\` |

目录内容：

```
manager.db                      # 数据库（环境/实例/扩展/设置/事件）
kernel/chrome-for-testing/      # 浏览器内核
environments/<envId>/
├── login-profile/              # 登录态母本与快照
└── instances/<insId>/          # 各实例独立 profile
```

**备份 / 迁移到新机器** = 退出应用后整目录拷贝。旧版本数据迁移见下方 FAQ（`scripts/migrate-legacy.js`）。

## 已知边界（FAQ）

- **session cookie 登录态重启即失效**：Chrome 标准语义（session cookie 不落盘）。此类系统的免登录路径 = 登录浏览器登录 → 捕获快照 → 新建实例。
- **单 session 互踢**：内部系统若为单 session 策略，多个实例共用同一快照可能互相踢下线。
- **CfT 蓝条**：「仅适用于自动测试」提示为 Chrome for Testing 构建硬编码，无官方开关。
- **CLI 全部命令 exit 8**：桌面应用未运行。先启动应用（含托盘常驻），再 `chrome-host status` 探活。
- **旧数据迁移**：`node scripts/migrate-legacy.js <旧 chrome-host.db>`（按 name 幂等；hosts 映射 JSON 不迁移，实例启动时现拉）。
