/// 与后端 camelCase 序列化对齐的类型

export interface Environment {
  id: string;
  name: string;
  hostsSourceUrl: string | null;
  icon: string | null;
  startupArgs: string | null;
  keepAlive: boolean;
  createdAt: number;
  updatedAt: number;
}

export interface EnvironmentSummary extends Environment {
  runningInstances: number;
  totalInstances: number;
}

export type InstanceStatus =
  | 'created'
  | 'starting'
  | 'running'
  | 'stopping'
  | 'stopped'
  | 'error'
  | 'crashed';

export interface Instance {
  id: string;
  environmentId: string;
  loginProfileId: string | null;
  profileDir: string;
  pid: number | null;
  cdpPort: number | null;
  status: InstanceStatus;
  /** hosts 映射快照 JSON（Record<host, ip>） */
  hostRules: string | null;
  browserVersion: string | null;
  startedAt: number | null;
  stoppedAt: number | null;
  createdAt: number;
}

export interface CdpEndpoint {
  instanceId: string;
  /** CDP 绑定地址（固定 127.0.0.1；网络地址语义，与环境起始页 host 无关） */
  host: string;
  port: number;
  httpUrl: string;
  webSocketUrl: string | null;
}

/** Login Profile 视图（browserRunning 为后端派生附加字段） */
export interface LoginProfileView {
  id: string;
  name: string;
  status: 'not_configured' | 'ready' | 'capturing' | 'error';
  snapshotVersion: number;
  instancesUsing: number;
  lastCapturedAt: number | null;
  browserRunning: boolean;
}

export interface KernelStatus {
  installed: boolean;
  version: string | null;
  pinnedVersion: string;
  upgradeAvailable: boolean;
  downloading: boolean;
  binaryPath: string | null;
}

export interface EnvStatusSummary {
  environmentId: string;
  instances: { total: number; running: number; starting: number; stopped: number; error: number };
}

export interface ApiError {
  code: string;
  message: string;
}

/** 应用内事件（Activity 流） */
export interface AppEvent {
  ts: number;
  level: string;
  event: string;
  environmentId: string | null;
  targetId: string | null;
  message: string;
}

/** 应用设置（Developer Mode） */
export interface AppSettings {
  developerMode: boolean;
  envLabelPosition: 'top-left' | 'top-right' | 'bottom-left' | 'bottom-right';
  envLabelColor: 'red' | 'blue' | 'green' | 'purple';
  /** 全局默认起始页（空 = about:blank） */
  defaultStartUrl: string;
  /** 远程接入：开启 = 应用守护 SSH 隧道（仅转发 17890） */
  remoteAccessEnabled: boolean;
  /** SSH 目标（user@host 或 ~/.ssh/config 别名） */
  remoteAccessSshTarget: string;
  /** 远程访问令牌（首次开启自动生成；隧道落地监听器 Bearer 校验用） */
  remoteAccessToken: string;
}

/** 远程接入隧道状态（GET /remote-access/status） */
export interface RemoteAccessStatus {
  enabled: boolean;
  target: string;
  state: 'disabled' | 'connecting' | 'connected' | 'reconnecting';
  lastError: string | null;
  /** 进入当前状态的时刻（epoch ms） */
  since: number;
  pid: number | null;
  /** 重连次数（本次 enabled 会话累计） */
  restarts: number;
}

/** Login Profiles 页行 */
export interface LoginProfileRow {
  id: string;
  environmentId: string;
  environmentName: string;
  name: string;
  status: 'not_configured' | 'ready' | 'capturing' | 'error';
  snapshotVersion: number;
  instancesUsing: number;
  lastCapturedAt: number | null;
  browserRunning: boolean;
}

/** ：Extension 一级资源（status 由后端每次读取时惰性计算） */
export interface Extension {
  id: string;
  name: string;
  description: string | null;
  version: string | null;
  manifestVersion: number | null;
  type: 'system' | 'user';
  sourcePath: string;
  enabled: boolean;
  status: 'ready' | 'missing' | 'invalid';
  createdAt: number;
  updatedAt: number;
}
