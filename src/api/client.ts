import type { ApiError } from './types';

const BASE = 'http://127.0.0.1:17890/api/v1';

/** 后端统一错误→ 前端错误对象；UI 依赖 code 做文案映射 */
export class RequestError extends Error {
  readonly code: string;
  constructor(err: ApiError) {
    super(err.message);
    this.code = err.code;
  }
}

const ERROR_TEXT: Record<string, string> = {
  HOSTS_SOURCE_INVALID: 'hosts 配置源须为 http(s):// 开头的合法 URL',
  HOSTS_FETCH_FAILED: 'hosts 配置拉取失败（源不可达或服务异常）',
  HOSTS_EMPTY: 'hosts 配置为空（源无有效记录）',
  KERNEL_NOT_READY: '浏览器内核未就绪',
  INSTANCE_START_FAILED: '浏览器启动失败（CDP 未就绪）',
  INSTANCE_ALREADY_RUNNING: '实例已在运行',
  INSTANCE_NOT_RUNNING: '实例未在运行',
  ENVIRONMENT_HAS_RUNNING_INSTANCES: '环境仍有运行中的实例，请先停止',
  PROFILE_IN_USE: '登录浏览器运行中，请先关闭再操作',
  PROFILE_SNAPSHOT_FAILED: '快照捕获失败',
  EXTENSION_SYSTEM_LOCKED: '系统扩展不可修改',
  EXTENSION_NOT_FOUND: '扩展不存在',
  INVALID_REQUEST: '请求参数不合法',
  UNAUTHORIZED: '远程访问令牌缺失或不正确（Bearer token）',
  REMOTE_ACCESS_TARGET_INVALID: 'SSH 目标不能为空，且不能以 - 开头或包含空白字符',
};

export function errorText(e: unknown): string {
  if (e instanceof RequestError) {
    return ERROR_TEXT[e.code] ?? e.message;
  }
  if (e instanceof Error) return e.message;
  return String(e);
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const resp = await fetch(`${BASE}${path}`, {
    headers: { 'content-type': 'application/json' },
    ...init,
  });
  const body = await resp.json().catch((): null => null);
  if (!resp.ok) {
    const err = (body as { error?: ApiError })?.error;
    throw new RequestError(err ?? { code: 'INTERNAL_ERROR', message: `HTTP ${resp.status}` });
  }
  return body as T;
}

export const api = {
  // environments
  listEnvironments: () => request('/environments'),
  createEnvironment: (body: { name: string; hostsSourceUrl?: string | null }) =>
    request('/environments', { method: 'POST', body: JSON.stringify(body) }),
  updateEnvironment: (id: string, body: Partial<{ name: string; hostsSourceUrl: string | null; startupArgs: string | null }>) =>
    request(`/environments/${id}`, { method: 'PATCH', body: JSON.stringify(body) }),
  deleteEnvironment: (id: string) => request(`/environments/${id}`, { method: 'DELETE' }),
  envStatus: (id: string) => request(`/environments/${id}/status`),
  stopAll: (id: string) => request<{ environmentId: string; stopped: string[] }>(`/environments/${id}/stop-all`, { method: 'POST' }),

  // instances
  listInstances: (envId: string) => request(`/environments/${envId}/instances`),
  createInstance: (envId: string) => request(`/environments/${envId}/instances`, { method: 'POST' }),
  getInstance: (id: string) => request(`/instances/${id}`),
  startInstance: (id: string) => request(`/instances/${id}/start`, { method: 'POST' }),
  stopInstance: (id: string) => request(`/instances/${id}/stop`, { method: 'POST' }),
  restartInstance: (id: string) => request(`/instances/${id}/restart`, { method: 'POST' }),
  focusInstance: (id: string) => request(`/instances/${id}/focus`, { method: 'POST' }),
  cdpEndpoint: (id: string) => request(`/instances/${id}/cdp`),
  deleteInstance: (id: string) => request(`/instances/${id}`, { method: 'DELETE' }),

  // login profile
  getLoginProfile: (envId: string) => request(`/environments/${envId}/login-profile`),
  launchLoginProfile: (envId: string) =>
    request(`/environments/${envId}/login-profile/launch`, { method: 'POST' }),
  captureLoginProfile: (envId: string) =>
    request(`/environments/${envId}/login-profile/capture`, { method: 'POST' }),
  resetLoginProfile: (envId: string) =>
    request(`/environments/${envId}/login-profile/reset`, { method: 'POST' }),

  // login profiles
  listLoginProfiles: () => request('/login-profiles'),

  // settings
  getSettings: () => request('/settings'),
  updateSettings: (body: { developerMode?: boolean; envLabelPosition?: string; envLabelColor?: string; defaultStartUrl?: string; remoteAccessEnabled?: boolean; remoteAccessSshTarget?: string }) =>
    request('/settings', { method: 'PUT', body: JSON.stringify(body) }),

  // remote access
  getRemoteStatus: () => request('/remote-access/status'),
  rotateRemoteToken: () => request<{ token: string }>('/remote-access/token', { method: 'POST' }),

  // activity
  getActivity: (envId: string, limit = 50) =>
    request(`/environments/${envId}/activity?limit=${limit}`),

  // kernel
  kernelStatus: () => request('/kernel/status'),
  kernelDownload: () => request<{ ok: boolean }>('/kernel/download', { method: 'POST' }),
  kernelCancel: () => request<{ ok: boolean }>('/kernel/cancel', { method: 'POST' }),

  // extensions
  listExtensions: () => request('/extensions'),
  registerExtension: (path: string) =>
    request('/extensions', { method: 'POST', body: JSON.stringify({ path }) }),
  updateExtension: (id: string, enabled: boolean) =>
    request(`/extensions/${id}`, { method: 'PATCH', body: JSON.stringify({ enabled }) }),
  deleteExtension: (id: string) => request(`/extensions/${id}`, { method: 'DELETE' }),
};
