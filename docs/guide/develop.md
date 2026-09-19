# 开发指南

## 环境要求

- Node 22+ / pnpm 10+（前端与文档站）
- Rust stable（Rust 应用与 CLI）

## 从源码运行

```bash
pnpm install
pnpm tauri dev     # beforeDevCommand 自动构建 CLI sidecar 到 src-tauri/binaries/
```

## 测试与冒烟

```bash
cd src-tauri && cargo test    # Rust 单测（服务层 / 仓储 / 进程管理 / CDP 客户端）
bash scripts/smoke.sh         # 冒烟（需应用运行中；HOSTS_SOURCE=<url> 可验证 hosts 注入）
```

## 目录速览

```
src/                  前端（React + Zustand + Tailwind，GUI 与 Popover）
src-tauri/src/
  api/                axum 路由薄壳（REST / MCP 挂载）+ 统一错误形状
  application/        服务层（环境/实例/快照/扩展/健康/活动/对账/keepAlive…）
  domain/             领域模型
  infrastructure/     db / 进程管理 / CDP 客户端 / 内核下载 / 路径 / 平台
crates/cli/           chrome-host CLI（独立 bin，纯 HTTP 薄客户端）
skills/               AI Agent 技能包（见用户文档）
docs/                 本文档站（VitePress）
```

分层纪律：路由为薄壳、业务在服务层、技术设施在 infrastructure——详见仓库内 `architecture.svg` 与 `DESIGN.md`（UI 设计 token 与规范）。

## 文档站开发

```bash
pnpm docs:dev       # 本地预览（自动从根目录同步 CLI/AGENT-API/MCP 手册）
pnpm docs:build     # 构建产物在 docs/.vitepress/dist/
```

根目录的 `CLI.md` / `AGENT-API.md` / `MCP.md` 是**单一事实源**（agent 直接读它们），文档站构建时经 `scripts/sync-docs.mjs` 复制并改写互链——修改手册内容改根目录文件，不要改 `docs/reference/`（生成物）。

## 发布

- push `v*` tag 触发 `.github/workflows/release.yml`：多平台构建 → GitHub Release（Tauri updater 产物同源）；
- 版本号：根 `package.json` / `src-tauri/tauri.conf.json` / `src-tauri/Cargo.toml` 三处同步。
