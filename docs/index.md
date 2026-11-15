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
  - icon: 🖱️
    title: 页面操控（page 命令族）
    details: snap 可访问性树快照（@eN 引用 durable、[hover] 提示）→ 点击/双击/右键、hover/滚动/拖拽/下拉/勾选/触摸、fill/press/wait 全输入面，跨域 iframe 直达；元素遮挡/可见性前置校验，对话框自动接管。
  - icon: 🎬
    title: 截图 · 录屏 · 性能取证
    details: 视口/元素/全页截图与 PDF；page record 录屏（screencast→ffmpeg，合成光标轨迹）；page profiler Chrome tracing 采样；page diff / diff-shot 快照与像素级回归比对。
  - icon: 🛡️
    title: 网络拦截与诊断
    details: 会话级 Fetch 拦截（glob 匹配 mock/改头/block、CORS 收紧、resourceType 过滤）；console/请求历史/HAR 常驻采集，响应体脱敏读取。
---
