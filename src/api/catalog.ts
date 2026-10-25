/**
 * Agent API 目录：单一数据源。
 * REST 端点变更只需改这里；AgentApiPage 据此渲染清单与 Explorer。
 */

export interface CatalogItem {
  method: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';
  path: string;
  desc: string;
  /** 示例请求体（JSON 字符串），无请求体则省略 */
  body?: string;
  /** 语义备注（如 navigate 的组合近似） */
  note?: string;
}

export interface CatalogGroup {
  group: string;
  items: CatalogItem[];
}

export const API_CATALOG: CatalogGroup[] = [
  {
    group: 'Environments',
    items: [
      { method: 'GET', path: '/environments', desc: '环境列表（含运行摘要）' },
      {
        method: 'POST',
        path: '/environments',
        desc: '创建环境（起始页缺省走 Settings 默认起始页，再退回 about:blank）',
        body: '{ "name": "Staging", "hostsSourceUrl": null }',
      },
      { method: 'GET', path: '/environments/:id', desc: '环境详情' },
      {
        method: 'PATCH',
        path: '/environments/:id',
        desc: '部分更新环境',
        body: '{ "name": "新名称", "keepAlive": true }',
      },
      { method: 'DELETE', path: '/environments/:id', desc: '删除环境（运行中 → 409）' },
      { method: 'GET', path: '/environments/:id/status', desc: '运行时摘要' },
      { method: 'GET', path: '/environments/:id/activity', desc: 'Activity 流（?limit=50）' },
      { method: 'POST', path: '/environments/:id/instances', desc: '创建并启动 Instance' },
      { method: 'POST', path: '/environments/:id/stop-all', desc: '停止该环境全部实例' },
      {
        method: 'GET',
        path: '/environments/:id/login-profile',
        desc: 'Login Profile 视图（惰性创建）',
      },
      {
        method: 'POST',
        path: '/environments/:id/login-profile/capture',
        desc: '捕获快照（运行中 → 409）',
        body: '{ "fromInstanceId": "ins_..." }',
        note: 'body 可选；fromInstanceId 从指定已停止实例捕获（实例即登录源，推荐路径）',
      },
      { method: 'POST', path: '/environments/:id/login-profile/reset', desc: '重置快照' },
      {
        method: 'GET',
        path: '/environments/:id/session',
        desc: '会话身份层状态',
        note: '策略（off/first-login/always）/当前代/来源实例/Cookie 数/历史代；restore 的回滚观测面',
      },
      {
        method: 'POST',
        path: '/environments/:id/session/restore',
        desc: '回滚 Cookie 保险库到历史代',
        body: '{ "generation": 2 }',
        note: '仅回滚 Cookie 层，文件快照保留当前；代号不存在 → 404 SESSION_GENERATION_NOT_FOUND',
      },
    ],
  },
  {
    group: 'Instances',
    items: [
      { method: 'GET', path: '/instances/:id', desc: '实例详情' },
      { method: 'DELETE', path: '/instances/:id', desc: '删除实例（运行中 → 409）' },
      { method: 'GET', path: '/instances/:id/status', desc: '状态摘要' },
      { method: 'POST', path: '/instances/:id/start', desc: '启动' },
      { method: 'POST', path: '/instances/:id/stop', desc: '停止' },
      { method: 'POST', path: '/instances/:id/restart', desc: '重启（stop + start）' },
      { method: 'POST', path: '/instances/:id/focus', desc: '唤出窗口' },
      { method: 'GET', path: '/instances/:id/cdp', desc: 'CDP endpoint（含 WebSocket URL）' },
      {
        method: 'GET',
        path: '/instances/:id/tabs',
        desc: '标签页列表（仅 page）',
        note: '新建页初始 url 可能显示 about:blank（导航未提交）',
      },
      {
        method: 'POST',
        path: '/instances/:id/tabs',
        desc: '新建标签页',
        body: '{ "url": "https://example.com" }',
      },
      {
        method: 'POST',
        path: '/instances/:id/navigate',
        desc: '导航（new + close 组合近似）',
        body: '{ "tabId": "<旧 tab id>", "url": "https://example.org" }',
        note: 'MVP 为组合近似：丢失旧页历史；Phase 2 WebSocket Page.navigate 兜底',
      },
      {
        method: 'GET',
        path: '/instances/:id/diagnostics',
        desc: '页面诊断快照（M5）',
        note: '页面扩展采集的即时快照（JS 错误/失败资源/LCP/FCP）；对第一个 http(s) 页短连接求值，与常驻诊断互补',
      },
      {
        method: 'GET',
        path: '/instances/:id/console',
        desc: 'console 环（?level=&limit=&tab=）',
        note: '常驻订阅的环形缓冲（500 条，重启即清），非 Chrome 查询接口；level 精确匹配，tab 按 targetId 归属过滤；响应 meta 恒带 dropped/capacity/since/live（证据边界契约：溢出与采集窗口显式可见）',
      },
      {
        method: 'GET',
        path: '/instances/:id/network',
        desc: '请求环（?failedOnly=&limit=&tab=）',
        note: '常驻订阅，请求完成/失败时入环（1000 条）；failedOnly = 失败或 HTTP≥400；meta 另带 scope:"completed"（进行中的请求不可见）',
      },
      {
        method: 'GET',
        path: '/instances/:id/screenshot',
        desc: '当前页面 png 截图',
        note: 'base64，复用常驻诊断连接；未运行/未就绪/无 http 页 → 409',
      },
      {
        method: 'GET',
        path: '/instances/:id/har',
        desc: '请求环生成 HAR 1.2',
        note: '未采集字段按规范 -1/空集占位（无请求/响应头、分阶段耗时）；_targetUrl 标注归属页面；顶层 _meta 扩展键声明占位语义与证据边界（dropped/capacity/since/live/scope）',
      },
    ],
  },
  {
    group: 'Page Control（页面操控）',
    items: [
      {
        method: 'POST',
        path: '/instances/:id/page/snap',
        desc: '可访问性树 + @eN 引用（操作循环起点）；响应含 text/refs_count/generation',
        body: '{ "tab": "<targetId 前缀，单标签可省>" }',
        note: '导航后 @eN 整表失效（PAGE_REF_STALE），重新 snap',
      },
      {
        method: 'POST',
        path: '/instances/:id/page/click',
        desc: '原生输入点击：<sel|@eN>（可见性检查 + 遮挡拦截 + 三段鼠标事件）',
        body: '{ "target": "@e1", "tab": null }',
      },
      {
        method: 'POST',
        path: '/instances/:id/page/fill',
        desc: '表单填写：聚焦+清空+输入+回读校验；空 value = 仅清空',
        body: '{ "target": "@e2", "value": "文本", "tab": null }',
      },
      {
        method: 'POST',
        path: '/instances/:id/page/press',
        desc: '按键：命名键/单字符/组合键（Control+A）；target 先聚焦',
        body: '{ "key": "Enter", "target": "@e1", "tab": null }',
      },
      {
        method: 'POST',
        path: '/instances/:id/page/wait',
        desc: '轮询等待元素状态：visible|hidden|attached|detached',
        body: '{ "selector": ".result", "state": "visible", "timeoutMs": 10000, "tab": null }',
      },
      {
        method: 'GET',
        path: '/instances/:id/page/dialogs',
        desc: '自动接管的弹窗历史（近 32 条；?tab=&since= 增量）',
        note: '缓冲 FIFO 逐出累计于 meta.dropped（诚实证据）；since 游标增量拉取，幸存首条 sequence > since+1 即中间有逐出',
      },
      {
        method: 'POST',
        path: '/instances/:id/page/eval',
        desc: '任意 JS 求值（⚠ 高权限：优先 page 原语）',
        body: '{ "expression": "document.title", "tab": null }',
      },
      {
        method: 'POST',
        path: '/instances/:id/page/raw',
        desc: '任意 CDP 命令透传（⚠ 最高权限逃生舱）',
        body: '{ "method": "Target.getTargets", "params": {}, "tab": null, "browser": true }',
        note: 'browser=true 发 browser 级；缺省发 tab 的 page session',
      },
      {
        method: 'POST',
        path: '/instances/:id/page/html',
        desc: '页面/元素 outerHTML（selector 缺省 = 整文档）',
        body: '{ "selector": "#main", "tab": null }',
      },
      {
        method: 'POST',
        path: '/instances/:id/page/clickxy',
        desc: '视口 CSS 像素坐标点击（无 hit-test；跨域 iframe 入口）',
        body: '{ "x": 100, "y": 200, "tab": null }',
      },
      {
        method: 'POST',
        path: '/instances/:id/page/type',
        desc: '当前焦点处输入（clickxy 点入跨域 iframe 后用它）',
        body: '{ "text": "输入内容", "tab": null }',
      },
      {
        method: 'POST',
        path: '/instances/:id/page/loadall',
        desc: '循环点击"加载更多"直到元素消失',
        body: '{ "selector": ".more", "intervalMs": 1500, "tab": null }',
      },
    ],
  },
  {
    group: 'Settings',
    items: [
      { method: 'GET', path: '/settings', desc: '应用设置' },
      {
        method: 'PUT',
        path: '/settings',
        desc: '更新设置（部分更新）',
        body: '{ "developerMode": true }',
      },
    ],
  },
  {
    group: 'Events',
    items: [
      {
        method: 'GET',
        path: '/events',
        desc: '业务事件 SSE 流（text/event-stream）',
        note: 'event: 为事件名（instance-started 等），data: 为落库行同款富 JSON；15s 心跳注释行；断开自动退订、历史不回放（走 activity 查询）',
      },
    ],
  },
  {
    group: 'Kernel',
    items: [
      { method: 'GET', path: '/kernel/status', desc: '内核状态（pinned/本地版本/下载进度）' },
      { method: 'POST', path: '/kernel/download', desc: '下载/升级内核' },
      { method: 'POST', path: '/kernel/cancel', desc: '取消下载' },
    ],
  },
  {
    group: 'Extensions',
    items: [
      { method: 'GET', path: '/extensions', desc: '扩展列表（含状态）' },
      {
        method: 'POST',
        path: '/extensions',
        desc: '注册扩展（服务端读 manifest 提取元数据）',
        body: '{ "path": "/absolute/path/to/extension-dir" }',
        note: '注册后新实例自动加载；Agent 只能引用已注册资源，无按路径加载',
      },
      { method: 'GET', path: '/extensions/:id', desc: '扩展详情（manifest 元数据）' },
      {
        method: 'PATCH',
        path: '/extensions/:id',
        desc: '启用/禁用（系统扩展 → 403）',
        body: '{ "enabled": false }',
        note: '配置变化只影响之后启动的实例（Chromium 机制）',
      },
      { method: 'DELETE', path: '/extensions/:id', desc: '移除注册项（不删源文件；系统扩展 → 403）' },
      {
        method: 'GET',
        path: '/extensions/:id/config',
        desc: '读扩展运行时配置（?environmentId=）',
        note: '省略 query = 全局默认；未配置 → config: null；仅 configurable 扩展（如 net-rules）',
      },
      {
        method: 'PUT',
        path: '/extensions/:id/config',
        desc: '写扩展运行时配置（?environmentId=）',
        body: '{ "block": ["*.google-analytics.com"], "redirect": [{ "from": "https://api.foo.com/*", "to": "http://localhost:3000/$1" }] }',
        note: 'body 即 JSON 对象（≤64KB，非法 → 400）；per-env 覆盖全局默认；对新启动实例生效；语法参考见 extension get 的 configDocsUrl',
      },
    ],
  },
];
