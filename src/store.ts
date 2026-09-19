import { create } from 'zustand';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { api, RequestError } from './api/client';
import type {
  AppSettings,
  EnvironmentSummary,
  Extension,
  Instance,
  KernelStatus,
} from './api/types';

/**
 * 单一 store：环境 + 实例 + 内核状态。
 * 状态变化由后端事件（instance-* / environment-* / kernel-download）驱动重拉，
 * 前端不自行推断状态（对账真相在后端）。
 */

interface AppStore {
  envs: EnvironmentSummary[];
  /** 按环境 id 缓存的实例列表（含对账后状态） */
  instancesByEnv: Record<string, Instance[]>;
  envsLoading: boolean;
  /** 首拉完成标记：此后轮询静默刷新，loading 骨架不再出现（修复空环境页 3s 闪动） */
  envsLoaded: boolean;
  kernel: KernelStatus | null;
  /** kernel-download 事件进度 */
  kernelProgress: { stage: string; percent: number } | null;
  toast: { kind: 'ok' | 'err'; text: string } | null;
  /** 轮询失败去重标记：仅在“成功→失败”翻转时 toast 一次，避免 API 故障期每 3s 闪红条 */
  envsFetchFailed: boolean;
  instancesFetchFailed: boolean;
  settings: AppSettings | null;
  showToast: (kind: 'ok' | 'err', text: string) => void;
  fetchEnvs: () => Promise<void>;
  fetchInstances: (envId: string) => Promise<void>;
  fetchSettings: () => Promise<void>;
  extensions: Extension[];
  fetchExtensions: () => Promise<void>;
  updateSettings: (patch: { developerMode?: boolean; envLabelPosition?: string; envLabelColor?: string; defaultStartUrl?: string }) => Promise<void>;
  fetchKernel: () => Promise<void>;
  initEvents: () => Promise<() => void>;
}

let toastTimer: ReturnType<typeof setTimeout> | undefined;

export const useStore = create<AppStore>((set, get) => ({
  envs: [],
  instancesByEnv: {},
  envsLoading: false,
  envsLoaded: false,
  kernel: null,
  kernelProgress: null,
  toast: null,
  envsFetchFailed: false,
  instancesFetchFailed: false,
  settings: null,

  showToast: (kind, text) => {
    set({ toast: { kind, text } });
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => set({ toast: null }), 3200);
  },

  fetchEnvs: async () => {
    // loading 骨架仅首次尝试出现（成功/失败都只一次），此后 3s 轮询静默刷新
    // （修复：空环境页"骨架↔空态"周期闪动 + 失败态误导为"暂无环境"）
    const firstLoad = !get().envsLoaded && !get().envsFetchFailed;
    if (firstLoad) set({ envsLoading: true });
    try {
      const envs = (await api.listEnvironments()) as EnvironmentSummary[];
      set((s) => ({
        // 数据未变化时保持原引用，避免无谓重渲染
        ...(JSON.stringify(s.envs) === JSON.stringify(envs) ? {} : { envs }),
        envsLoading: false,
        envsLoaded: true,
        envsFetchFailed: false,
      }));
    } catch (e) {
      if (!get().envsFetchFailed) get().showToast('err', `环境列表加载失败: ${String(e)}`);
      set({ envsFetchFailed: true, envsLoading: false });
    }
  },

  fetchInstances: async (envId) => {
    try {
      const list = (await api.listInstances(envId)) as Instance[];
      set((s) => {
        if (JSON.stringify(s.instancesByEnv[envId]) === JSON.stringify(list)) {
          return { instancesFetchFailed: false };
        }
        return { instancesByEnv: { ...s.instancesByEnv, [envId]: list }, instancesFetchFailed: false };
      });
    } catch (e) {
      // 环境已删除：清掉缓存键，轮询自然停止（否则每 3s 报一次"环境不存在"）
      if (e instanceof RequestError && e.code === 'ENVIRONMENT_NOT_FOUND') {
        set((s) => {
          const next = { ...s.instancesByEnv };
          delete next[envId];
          return { instancesByEnv: next, instancesFetchFailed: false };
        });
        return;
      }
      if (!get().instancesFetchFailed) get().showToast('err', `实例列表加载失败: ${String(e)}`);
      set({ instancesFetchFailed: true });
    }
  },

  fetchSettings: async () => {
    try {
      set({ settings: (await api.getSettings()) as AppSettings });
    } catch {
      /* API 未连接时静默 */
    }
  },

  extensions: [],
  fetchExtensions: async () => {
    try {
      set({ extensions: (await api.listExtensions()) as Extension[] });
    } catch {
      /* API 未连接时静默 */
    }
  },

  updateSettings: async (patch: { developerMode?: boolean; envLabelPosition?: string; envLabelColor?: string; defaultStartUrl?: string; remoteAccessEnabled?: boolean; remoteAccessSshTarget?: string }) => {
    try {
      set({ settings: (await api.updateSettings(patch)) as AppSettings });
    } catch (e) {
      get().showToast('err', String(e));
    }
  },

  fetchKernel: async () => {
    try {
      set({ kernel: (await api.kernelStatus()) as KernelStatus, kernelProgress: null });
    } catch {
      /* API 未启动时静默，Settings 页展示禁用态 */
    }
  },

  initEvents: async () => {
    const unlistens: UnlistenFn[] = [];
    const refreshAll = () => {
      get().fetchEnvs();
      Object.keys(get().instancesByEnv).forEach((envId) => get().fetchInstances(envId));
    };
    for (const evt of [
      'instance-created',
      'instance-started',
      'instance-stopped',
      'instance-error',
      'instance-deleted',
      'instance-crashed',
      'environment-created',
      'environment-updated',
      'environment-deleted',
      'hosts-resolved',
    ]) {
      unlistens.push(await listen(evt, refreshAll));
    }
    unlistens.push(
      await listen<{ stage: string; percent: number }>('kernel-download', (e) => {
        const { stage, percent } = e.payload;
        set({ kernelProgress: { stage, percent } });
        if (stage === 'done' || stage === 'error' || stage === 'cancelled') {
          get().fetchKernel();
          setTimeout(() => set({ kernelProgress: null }), 1500);
        }
      }),
    );
    return () => unlistens.forEach((u) => u());
  },
}));
