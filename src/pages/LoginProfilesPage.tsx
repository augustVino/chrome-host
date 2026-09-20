import { useCallback, useEffect, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { Link } from 'react-router-dom';
import { api, errorText } from '../api/client';
import type { LoginProfileRow } from '../api/types';
import { useStore } from '../store';
import { ConfirmDialog, confirm } from '../components/confirm';
import { btnGhost } from '../components/ui';

const STATUS_TEXT: Record<LoginProfileRow['status'], string> = {
  not_configured: 'Not configured',
  ready: 'Ready',
  capturing: 'Capturing…',
  error: 'Error',
};

/** Login Profiles 页：全部环境的登录快照总览与快捷操作 */
export default function LoginProfilesPage() {
  const { showToast } = useStore();
  const [rows, setRows] = useState<LoginProfileRow[] | null>(null);
  const [busyKey, setBusyKey] = useState<string | null>(null);

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
    <div className="mx-auto max-w-2xl">
      <h1 className="mb-1 text-base font-semibold text-ink">Login Profiles</h1>
      <p className="mb-5 text-xs text-ink-3">
        登录浏览器登录 → 捕获快照 → 之后创建的实例自动免登录
      </p>

      {rows === null ? (
        <p className="text-xs text-ink-3">加载中…</p>
      ) : rows.length === 0 ? (
        <p className="py-16 text-center text-xs text-ink-3">
          暂无登录快照。先在环境中点击「打开并登录」，再捕获快照。
        </p>
      ) : (
        <ul className="space-y-3">
          {rows.map((p) => (
            <li key={p.id} className="rounded-xl border border-hairline bg-surface-card p-4">
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
                  className={`shrink-0 rounded px-1.5 py-0.5 text-[10px] ${
                    p.status === 'ready'
                      ? 'bg-emerald-50 text-run-text dark:bg-emerald-500/15'
                      : p.status === 'error'
                        ? 'bg-red-50 text-status-error dark:bg-red-500/15'
                        : 'bg-surface-inset text-ink-2'
                  }`}
                >
                  {STATUS_TEXT[p.status]}
                </span>
              </div>

              {p.browserRunning && (
                <p className="mt-2 text-xs text-amber-600">登录浏览器运行中，关闭窗口后即可捕获</p>
              )}

              <div className="mt-3 flex items-center gap-1.5">
                <button
                  className={btnGhost}
                  disabled={busyKey !== null || p.status === 'capturing' || p.browserRunning}
                  onClick={() =>
                    act(`launch:${p.id}`, () => api.launchLoginProfile(p.environmentId), '登录浏览器已打开')
                  }
                >
                  打开并登录
                </button>
                <button
                  className={btnGhost}
                  disabled={busyKey !== null || p.status === 'capturing' || p.browserRunning}
                  title={p.browserRunning ? '先关闭登录浏览器' : undefined}
                  onClick={() =>
                    act(`capture:${p.id}`, () => api.captureLoginProfile(p.environmentId), '快照已捕获')
                  }
                >
                  {p.status === 'capturing' ? '捕获中…' : '捕获快照'}
                </button>
                <button
                  className={btnGhost}
                  disabled={busyKey !== null || p.status === 'capturing'}
                  onClick={() => reset(p.name, () => api.resetLoginProfile(p.environmentId))}
                >
                  重置
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}

      <ConfirmDialog />
    </div>
  );
}
