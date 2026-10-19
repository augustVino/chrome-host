import { useCallback, useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { Link } from 'react-router-dom';
import { api, errorText } from '../api/client';
import type { LoginProfileRow } from '../api/types';
import { useStore } from '../store';
import { confirm } from '../components/confirm';
import { PageHeader, btnGhostSm } from '../components/ui';
import { usePageEnter } from '../motion/hooks';

/** Login Profiles 页：全部环境登录态总览（实例即登录源：在实例里登录 → 关闭即保存） */
export default function LoginProfilesPage() {
  const { showToast } = useStore();
  const [rows, setRows] = useState<LoginProfileRow[] | null>(null);
  const [busyKey, setBusyKey] = useState<string | null>(null);
  const root = useRef<HTMLDivElement>(null);
  usePageEnter(root);

  const fetchRows = useCallback(async () => {
    try {
      setRows((await api.listLoginProfiles()) as LoginProfileRow[]);
    } catch {
      /* 静默 */
    }
  }, []);

  useEffect(() => {
    fetchRows();
    const unlisten = listen('login-profile-changed', () => fetchRows());
    const t = setInterval(fetchRows, 3000);
    return () => {
      unlisten.then((fn) => fn());
      clearInterval(t);
    };
  }, [fetchRows]);

  const act = useCallback(
    async (key: string, fn: () => Promise<unknown>, okText: string) => {
      setBusyKey(key);
      try {
        await fn();
        showToast('ok', okText);
      } catch (e) {
        showToast('err', errorText(e));
      } finally {
        setBusyKey(null);
        fetchRows();
      }
    },
    [showToast, fetchRows],
  );

  const reset = useCallback(
    async (name: string, doReset: () => Promise<unknown>) => {
      if (
        !(await confirm({
          title: '重置登录快照',
          message: [`「${name}」的母本登录态将被清空（已创建实例不受影响）。`, '确定重置？'],
          confirmText: '重置',
        }))
      ) {
        return;
      }
      act(`reset`, doReset, '已重置');
    },
    [act],
  );

  return (
    <div ref={root} className="mx-auto max-w-4xl">
      <PageHeader title="Login Profiles" sub="登录浏览器登录 → 捕获快照 → 之后创建的实例自动免登录" />

      {rows === null ? (
        <p className="text-xs text-ink-3">加载中…</p>
      ) : rows.length === 0 ? (
        <p className="py-16 text-center text-xs text-ink-3">
          暂无登录快照。先在环境中点击「打开并登录」，再捕获快照。
        </p>
      ) : (
        <ul className="space-y-3">
          {rows.map((p) => (
            <li key={p.id} className="gcard row-in p-4">
              <div className="flex items-start justify-between gap-3">
                <div className="min-w-0">
                  <p className="truncate text-sm font-medium text-ink">🔐 {p.name}</p>
                  <p className="mt-0.5 truncate text-xs text-ink-3">
                    <Link to={`/environments/${p.environmentId}`} className="hover:text-ink-2">
                      {p.environmentName}
                    </Link>
                    {' · '}
                    {p.instancesUsing} 个实例使用
                    {' · '}
                    {p.lastCapturedAt
                      ? `最近捕获 ${new Date(p.lastCapturedAt).toLocaleString()}`
                      : '从未捕获'}
                    {p.snapshotVersion > 0 && ` · v${p.snapshotVersion}`}
                  </p>
                </div>
                <span
                  className={`shrink-0 rounded-full px-2.5 py-0.5 text-[10.5px] font-bold ${
                    p.snapshotVersion > 0
                      ? 'bg-mint/15 text-run-text'
                      : p.status === 'error'
                        ? 'bg-coral/15 text-status-error'
                        : 'bg-surface-inset text-ink-2'
                  }`}
                >
                  {p.snapshotVersion > 0 ? `已登录 v${p.snapshotVersion}` : p.status === 'error' ? 'Error' : '未登录'}
                </span>
              </div>

              <div className="mt-3 flex items-center gap-1.5">
                <button
                  className={btnGhostSm}
                  disabled={busyKey !== null || p.status === 'capturing'}
                  onClick={() => reset(p.name, () => api.resetLoginProfile(p.environmentId))}
                >
                  重置登录态
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
