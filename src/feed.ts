import { create } from 'zustand';
import { useStore } from './store';

/**
 * Live Feed：纯前端、仅会话内存。
 * 通过对 store 里"环境/实例状态"做 diff 派生事件流（不新增任何后端调用，也不改 store 逻辑）。
 */
export interface FeedItem {
  id: string;
  kind: 'ok' | 'warn' | 'err' | 'info';
  text: string;
  /** HH:MM:SS */
  t: string;
}

interface FeedStore {
  items: FeedItem[];
  push: (kind: FeedItem['kind'], text: string) => void;
}

let seq = 0;
/** 启动提示每个会话只记一次（StrictMode 开发态会重复挂载 effect，避免重复条目） */
let booted = false;
const hhmmss = () => new Date().toTimeString().slice(0, 8);

export const useFeed = create<FeedStore>((set) => ({
  items: [],
  push: (kind, text) =>
    set((s) => ({ items: [{ id: `f${++seq}`, kind, text, t: hhmmss() }, ...s.items].slice(0, 40) })),
}));

/** 启动 diff 订阅（主窗口 Shell 挂载时调用，返回取消函数） */
export function startFeed(): () => void {
  const push = useFeed.getState().push;
  if (!booted) {
    booted = true;
    push('info', 'Chrome Host 已启动 · Agent API 监听 127.0.0.1:17890');
  }

  // envId → (insId → status)；某环境首次出现只播种不出事件
  const seeded = new Map<string, Map<string, string>>();
  let envNames: Map<string, string> | null = null;

  const unsub = useStore.subscribe((s, p) => {
    if (s.envs === p.envs && s.instancesByEnv === p.instancesByEnv) return;

    // 环境增删（envsLoaded 之后才比较，避免首拉全部算"创建"）
    if (s.envsLoaded) {
      const now = new Map(s.envs.map((e) => [e.id, e.name]));
      if (envNames) {
        now.forEach((name, id) => {
          if (!envNames!.has(id)) push('info', `环境「${name}」已创建`);
        });
        envNames.forEach((name, id) => {
          if (!now.has(id)) {
            push('info', `环境「${name}」已删除`);
            seeded.delete(id);
          }
        });
      }
      envNames = now;
    }

    for (const [envId, list] of Object.entries(s.instancesByEnv)) {
      const env = s.envs.find((e) => e.id === envId);
      const name = env?.name ?? envNames?.get(envId) ?? '环境';
      const cur = new Map(list.map((i) => [i.id, i.status as string]));
      const prev = seeded.get(envId);
      if (prev) {
        list.forEach((ins, idx) => {
          const label = `${name} · Chrome #${idx + 1}`;
          const before = prev.get(ins.id);
          if (before === undefined) {
            if (ins.status === 'running') push('ok', `${label} 运行中`);
            else push('warn', `${label} 启动中`);
          } else if (before !== ins.status) {
            if (ins.status === 'running') push('ok', `${label} 运行中`);
            else if (ins.status === 'starting') push('warn', `${label} 启动中`);
            else if (ins.status === 'stopped') push('info', `${label} 已停止`);
            else if (ins.status === 'crashed') push('err', `${label} 已崩溃`);
            else if (ins.status === 'error') push('err', `${label} 异常`);
          }
        });
        prev.forEach((_, id) => {
          if (!cur.has(id)) push('info', `${name} · 实例已删除`);
        });
      }
      seeded.set(envId, cur);
    }
  });
  return unsub;
}
