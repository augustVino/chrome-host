---
layout: home

hero:
  name: Chrome Host
  text: Chrome 浏览器多环境管理工具
  tagline: 每个环境一份完全隔离的 Chrome —— 独立 Profile、进程级 hosts 注入、CDP 可自动化，登录一次、处处免登。
  actions:
    - theme: brand
      text: 快速开始
      link: /guide/quickstart
    - theme: alt
      text: CLI 手册
      link: /reference/cli
    - theme: alt
      text: GitHub
      link: https://github.com/augustVino/chrome-host

features:
  - icon: 🗂️
    title: 环境与实例
    details: 环境 = 一份配置（hosts 源 / keepAlive / 启动参数）；实例 = 独立 user-data-dir + CDP 端口的一次隔离运行，互不串扰。
  - icon: 🔑
    title: 登录态快照
    details: 母本浏览器登录一次、捕获快照，之后创建的每个实例自动克隆登录态；快照可刷新、可重置。
  - icon: 🌐
    title: hosts 注入
    details: 实例启动时从 hosts 配置源现拉最新配置，经 --host-resolver-rules 进程级注入，不改系统 hosts。
  - icon: 🤖
    title: 四入口同契约
    details: GUI / Agent API（REST）/ MCP / CLI 共享同一服务层与错误码体系，为 AI Agent 与自动化而设计。
  - icon: ♻️
    title: keepAlive
    details: 实例异常退出自动重启；60 秒窗口内连续崩溃 3 次自动熔断，防重启风暴。
  - icon: 🧩
    title: 内核自管理
    details: 首次启动自动下载 pinned 版 Chrome for Testing（双镜像），随应用管理，无需手工维护浏览器。
---
