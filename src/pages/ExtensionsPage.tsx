import { useCallback, useEffect } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import { revealItemInDir } from '@tauri-apps/plugin-opener';
import { api, errorText } from '../api/client';
import { useStore } from '../store';
import { confirm } from '../components/confirm';
import { btnGhost, btnPrimary } from '../components/ui';
import type { Extension } from '../api/types';

/** 展示层状态 → 语义色（DESIGN.md：状态点 bg-status-*，异常文字加深） */
function StatusBadge({ status }: { status: Extension['status'] }) {
  const map = {
    ready: { dot: 'bg-status-running', text: 'text-ink-2', label: '正常' },
    missing: { dot: 'bg-status-stopped', text: 'text-ink-2', label: '源目录缺失' },
    invalid: { dot: 'bg-status-error', text: 'text-status-error', label: 'manifest 无效' },
  } as const;
  const s = map[status];
  return (
    <span className="flex shrink-0 items-center gap-1.5">
      <span className={`size-2 rounded-full ${s.dot}`} />
      <span className={`text-xs ${s.text}`}>{s.label}</span>
    </span>
  );
}

function ExtensionRow({ ext }: { ext: Extension }) {
  const { showToast, fetchExtensions } = useStore();

  const toggle = useCallback(async () => {
    try {
      await api.updateExtension(ext.id, !ext.enabled);
      await fetchExtensions();
    } catch (e) {
      showToast('err', errorText(e));
    }
  }, [ext, fetchExtensions, showToast]);

  const remove = useCallback(async () => {
    const ok = await confirm({
      title: '移除扩展',
      message: [`将从 chrome-host 移除「${ext.name}」的注册。`, '已启动的实例不受影响；源文件不会被删除。'],
      confirmText: '移除',
    });
    if (!ok) return;
    try {
      await api.deleteExtension(ext.id);
      await fetchExtensions();
    } catch (e) {
      showToast('err', errorText(e));
    }
  }, [ext.name, ext.id, fetchExtensions, showToast]);

  const openFolder = useCallback(async () => {
    try {
      await revealItemInDir(ext.sourcePath);
    } catch {
      showToast('err', '无法打开目录（可能已被移动或删除）');
    }
  }, [ext.sourcePath, showToast]);

  return (
    <div className="flex items-start justify-between gap-3 px-4 py-3">
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <span className="truncate text-sm font-medium text-ink">{ext.name}</span>
          <StatusBadge status={ext.status} />
          {ext.version && <span className="shrink-0 font-mono text-[11px] text-ink-3">v{ext.version}</span>}
        </div>
        {ext.description && <p className="mt-0.5 truncate text-xs text-ink-2">{ext.description}</p>}
        <p className="mt-1 truncate font-mono text-[11px] text-ink-3" title={ext.sourcePath}>
          {ext.sourcePath}
        </p>
      </div>
      {ext.type === 'user' && (
        <div className="flex shrink-0 items-center gap-1.5 pt-0.5">
          <button className={btnGhost} onClick={toggle}>
            {ext.enabled ? '禁用' : '启用'}
          </button>
          <button className={btnGhost} onClick={openFolder}>
            Open Folder
          </button>
          <button
            className="rounded-lg px-2.5 py-1.5 text-xs text-ink-2 transition hover:bg-red-50 hover:text-status-error dark:hover:bg-red-500/15"
            onClick={remove}
          >
            移除
          </button>
        </div>
      )}
    </div>
  );
}

function GroupCard({ title, items }: { title: string; items: Extension[] }) {
  if (items.length === 0) return null;
  return (
    <section className="mb-4 rounded-xl border border-hairline bg-surface-card">
      <div className="flex items-center justify-between px-4 pt-4 pb-2">
        <h2 className="text-sm font-medium text-ink">{title}</h2>
        <span className="text-[11px] text-ink-3">{items.length} 项</span>
      </div>
      <div className="divide-y divide-hairline-soft border-t border-hairline-soft pb-1">
        {items.map((ext) => (
          <ExtensionRow key={ext.id} ext={ext} />
        ))}
      </div>
    </section>
  );
}

export default function ExtensionsPage() {
  const { extensions, fetchExtensions, showToast } = useStore();

  useEffect(() => {
    fetchExtensions();
  }, [fetchExtensions]);

  const add = useCallback(async () => {
    const selected = await open({ directory: true, multiple: false, title: '选择扩展目录（含 manifest.json）' });
    if (typeof selected !== 'string') return;
    try {
      await api.registerExtension(selected);
      showToast('ok', '扩展已注册，对新启动的实例生效');
      await fetchExtensions();
    } catch (e) {
      showToast('err', errorText(e));
    }
  }, [fetchExtensions, showToast]);

  const system = extensions.filter((e) => e.type === 'system');
  const user = extensions.filter((e) => e.type === 'user');

  return (
    <div className="mx-auto max-w-2xl">
      <div className="mb-5 flex items-center justify-between">
        <h1 className="text-base font-semibold text-ink">Extensions</h1>
        <button className={btnPrimary} onClick={add}>
          + Add Extension
        </button>
      </div>

      <p className="mb-4 rounded-lg border border-hairline bg-surface-inset px-3 py-2 text-xs text-ink-2">
        启用的扩展会自动加载进每个新启动的 Chrome 实例；配置变化只影响之后启动的实例。
      </p>

      <GroupCard title="System" items={system} />
      <GroupCard title="User" items={user} />
    </div>
  );
}
