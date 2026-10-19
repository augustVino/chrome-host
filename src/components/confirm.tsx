import { useRef } from 'react';
import { create } from 'zustand';
import { ModalShell, btnDanger, btnGhost } from './ui';

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
  // 退场动画期间 st 已为 null：保留最后一次内容，直到弹层卸载
  const last = useRef<ConfirmOptions | null>(null);
  if (st) last.current = st.options;
  const options = last.current;
  if (!options) return null;
  return (
    <ModalShell open={!!st} onBackdrop={() => st?.resolve?.(false)} width={400}>
      <svg className="m-ic" viewBox="0 0 24 24" aria-hidden>
        <path d="M12 3.5l9.5 16.5h-19z" />
        <path d="M12 10v4.5M12 17.4v.1" />
      </svg>
      <h2>{options.title}</h2>
      <div className="space-y-0.5">
        {options.message.map((m, i) => (
          <p key={i}>{m}</p>
        ))}
      </div>
      <div className="mf">
        <button className={btnGhost} onClick={() => answer(false)}>
          取消
        </button>
        <button className={btnDanger} onClick={() => answer(true)}>
          {options.confirmText ?? '确定'}
        </button>
      </div>
    </ModalShell>
  );
}

/** 便捷 hook（组件内解构使用） */
export function useConfirm() {
  return useConfirmStore((s) => s.confirm);
}
