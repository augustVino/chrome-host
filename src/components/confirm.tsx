import { create } from 'zustand';
import { btnGhost } from './ui';

/**
 * 应用内确认对话框：Tauri WKWebView 不支持 window.confirm/alert（点击无反应的根因），
 * 全部原生确认流改走本组件。Promise 风格：await confirm(opts) → true/false。
 * 每个窗口（main / popover）各自渲染一个 <ConfirmDialog /> 实例。
 */

interface ConfirmOptions {
  title: string;
  /** 多行说明文字 */
  message: string[];
  /** 确认按钮文案，默认"确定" */
  confirmText?: string;
}

interface ConfirmState {
  options: ConfirmOptions | null;
  resolve: ((v: boolean) => void) | null;
}

interface ConfirmStore {
  st: ConfirmState | null;
  confirm: (opts: ConfirmOptions) => Promise<boolean>;
  answer: (v: boolean) => void;
}

export const useConfirmStore = create<ConfirmStore>((set, get) => ({
  st: null,
  confirm: (opts) =>
    new Promise<boolean>((resolve) => {
      // 已有未决对话框时先取消，保证同时只有一个
      get().st?.resolve?.(false);
      set({ st: { options: opts, resolve } });
    }),
  answer: (v) => {
    get().st?.resolve?.(v);
    set({ st: null });
  },
}));

/** 命令式确认入口：const ok = await confirm({ title, message }) */
export function confirm(opts: ConfirmOptions): Promise<boolean> {
  return useConfirmStore.getState().confirm(opts);
}

export function ConfirmDialog() {
  const { st, answer } = useConfirmStore();
  if (!st) return null;
  const { options, resolve } = st;
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/25 p-4 dark:bg-black/50"
      onClick={() => resolve?.(false)}
    >
      <div
        className="w-[380px] rounded-xl bg-surface-card p-5 shadow-xl ring-1 ring-hairline"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="mb-2 text-sm font-semibold text-ink">{options.title}</h2>
        <div className="space-y-0.5 text-xs leading-5 text-ink-2">
          {options.message.map((m, i) => (
            <p key={i}>{m}</p>
          ))}
        </div>
        <div className="mt-4 flex justify-end gap-2">
          <button className={btnGhost} onClick={() => answer(false)}>
            取消
          </button>
          <button
            className="inline-flex items-center gap-1 rounded-lg bg-status-error px-2.5 py-1.5 text-xs font-medium text-white transition hover:brightness-90"
            onClick={() => answer(true)}
          >
            {options.confirmText ?? '确定'}
          </button>
        </div>
      </div>
    </div>
  );
}

/** 便捷 hook（组件内解构使用） */
export function useConfirm() {
  return useConfirmStore((s) => s.confirm);
}
