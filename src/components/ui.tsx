import type { ReactNode } from 'react';
import { useEffect } from 'react';
import type { InstanceStatus } from '../api/types';

/** 状态点（○ 无实例 / ● 运行 / ◌ 启动中 / ⚠ 失败） */
export function StatusDot({ status, className = '' }: { status: InstanceStatus | string; className?: string }) {
  const map: Record<string, [string, string]> = {
    running: ['bg-status-running', '运行中'],
    starting: ['bg-status-starting animate-pulse', '启动中'],
    stopping: ['bg-status-starting animate-pulse', '停止中'],
    stopped: ['bg-status-stopped', '已停止'],
    created: ['bg-status-stopped', '已创建'],
    error: ['bg-status-error', '异常'],
    crashed: ['bg-status-error', '已崩溃'],
  };
  const [color, label] = map[status] ?? ['bg-status-stopped', status];
  return (
    <span className={`inline-flex items-center gap-1.5 ${className}`} title={label}>
      <span className={`inline-block size-2 rounded-full ${color}`} />
      <span className="text-xs text-ink-2">{label}</span>
    </span>
  );
}

export function Modal({
  open,
  title,
  onClose,
  children,
}: {
  open: boolean;
  title: string;
  onClose: () => void;
  children: ReactNode;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    if (open) window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [open, onClose]);

  if (!open) return null;
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/25" onClick={onClose}>
      <div
        className="w-[420px] rounded-xl bg-white p-5 shadow-xl ring-1 ring-hairline"
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="mb-4 text-sm font-semibold text-ink">{title}</h2>
        {children}
      </div>
    </div>
  );
}

export function Field({ label, children, hint }: { label: string; children: ReactNode; hint?: string }) {
  return (
    <label className="mb-3 block">
      <span className="mb-1 block text-xs font-medium text-ink-2">{label}</span>
      {children}
      {hint && <span className="mt-1 block text-[11px] leading-4 text-ink-3">{hint}</span>}
    </label>
  );
}

export const inputCls =
  'w-full rounded-lg border border-hairline px-2.5 py-1.5 text-sm outline-none transition focus:border-ink-3 focus:ring-2 focus:ring-neutral-100';

export function EmptyState({ title, desc, action }: { title: string; desc: string; action?: ReactNode }) {
  return (
    <div className="flex flex-col items-center justify-center py-20 text-center">
      <p className="text-sm font-medium text-ink">{title}</p>
      <p className="mt-1 max-w-xs text-xs text-ink-3">{desc}</p>
      {action && <div className="mt-4">{action}</div>}
    </div>
  );
}

export const btn =
  'inline-flex items-center gap-1 rounded-lg px-2.5 py-1.5 text-xs font-medium transition disabled:opacity-40 disabled:cursor-not-allowed';
export const btnPrimary = `${btn} bg-accent text-white hover:bg-accent-hover`;
export const btnGhost = `${btn} border border-hairline bg-white text-ink-2 hover:bg-neutral-50`;
