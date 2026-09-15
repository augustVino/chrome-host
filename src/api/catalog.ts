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
      { method: 'POST', path: '/environments/:id/login-profile/launch', desc: '打开登录浏览器' },
      { method: 'POST', path: '/environments/:id/login-profile/capture', desc: '捕获快照（运行中 → 409）' },
      { method: 'POST', path: '/environments/:id/login-profile/reset', desc: '重置快照' },
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
    ],
  },
];
