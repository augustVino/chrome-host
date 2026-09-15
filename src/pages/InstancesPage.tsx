import { useCallback, useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { api, errorText } from '../api/client';
import { copyText } from '../lib/clipboard';
import { useStore } from '../store';
import { EmptyState, StatusDot, btnGhost } from '../components/ui';

/** 全局实例视图（轻量列表，非重表格） */
export default function InstancesPage() {
  const { envs, instancesByEnv, fetchEnvs, fetchInstances, showToast } = useStore();
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [busyId, setBusyId] = useState<string | null>(null);

  const refresh = useCallback(() => {
    fetchEnvs();
    useStore.getState().envs.forEach((e) => fetchInstances(e.id));
  }, [fetchEnvs, fetchInstances]);

  const act = useCallback(
    async (insId: string, fn: () => Promise<unknown>, okText?: string) => {
      setBusyId(insId);
      try {
        await fn();
        if (okText) showToast('ok', okText);
      } catch (e) {
        showToast('err', errorText(e));
      } finally {
        setBusyId(null);
        refresh();
      }
    },
    [showToast, refresh],
  );

  const copyCdp = useCallback(
    async (insId: string) => {
      setBusyId(insId);
      try {
        const cdp = (await api.cdpEndpoint(insId)) as { httpUrl: string };
        await copyText(cdp.httpUrl);
        showToast('ok', 'CDP Endpoint 已复制');
      } catch (e) {
        showToast('err', errorText(e));
      } finally {
        setBusyId(null);
      }
    },
    [showToast],
  );

  useEffect(() => {
    fetchEnvs().then(() => {
      useStore.getState().envs.forEach((e) => fetchInstances(e.id));
    });
  }, [fetchEnvs, fetchInstances]);

  const rows = envs.flatMap((env) =>
    (instancesByEnv[env.id] ?? []).map((ins) => ({ env, ins })),
  );

  const toggle = (envId: string) => setExpanded((m) => ({ ...m, [envId]: !m[envId] }));

  return (
    <div className="mx-auto max-w-2xl">
      <div className="mb-5">
        <h1 className="text-base font-semibold text-ink">All Instances</h1>
        <p className="mt-0.5 text-xs text-ink-3">
          {rows.length} 个实例 · {rows.filter((r) => r.ins.status === 'running').length} running
        </p>
      </div>

      {rows.length === 0 ? (
        <EmptyState title="No instances" desc="还没有任何 Chrome 实例。前往 Environments 创建。" />
      ) : (
        <ul className="space-y-1">
          {envs.map((env) => {
            const list = instancesByEnv[env.id] ?? [];
            if (list.length === 0) return null;
            const open = expanded[env.id] ?? true;
            return (
              <li key={env.id} className="rounded-xl border border-hairline bg-white">
                <button
                  className="flex w-full items-center gap-2 px-4 py-2.5 text-left"
                  onClick={() => toggle(env.id)}
                >
                  <span className={`text-[10px] text-ink-3 transition ${open ? 'rotate-90' : ''}`}>▶</span>
                  <span className="text-sm font-medium text-ink">{env.name}</span>
                  <span className="text-xs text-ink-3">{list.length} instances</span>
                </button>
                {open && (
                  <ul className="border-t border-neutral-100">
                    {list.map((ins, idx) => (
                      <li
                        key={ins.id}
                        className="flex items-center gap-3 border-b border-neutral-50 px-4 py-2 last:border-0"
                      >
                        <StatusDot status={ins.status} className="w-24" />
                        <Link
                          to={`/environments/${env.id}`}
                          className="w-24 truncate text-sm text-ink hover:underline"
                        >
                          Chrome #{idx + 1}
                        </Link>
                        <span className="text-xs text-ink-3">{env.name}</span>
                        <span className="ml-auto font-mono text-xs text-ink-3">
                          {ins.cdpPort ?? '—'}
                        </span>
                        <span className="flex items-center gap-1">
                          {ins.status === 'running' && (
                            <>
                              <button
                                className={btnGhost}
                                disabled={busyId === ins.id}
                                onClick={() => copyCdp(ins.id)}
                              >
                                Copy CDP
                              </button>
                              <button
                                className={btnGhost}
                                disabled={busyId === ins.id}
                                onClick={() => act(ins.id, () => api.restartInstance(ins.id))}
                              >
                                Restart
                              </button>
                              <button
                                className={btnGhost}
                                disabled={busyId === ins.id}
                                onClick={() => act(ins.id, () => api.stopInstance(ins.id))}
                              >
                                Stop
                              </button>
                            </>
                          )}
                          {(ins.status === 'stopped' || ins.status === 'error') && (
                            <button
                              className={btnGhost}
                              disabled={busyId === ins.id}
                              onClick={() => act(ins.id, () => api.startInstance(ins.id), '实例已启动')}
                            >
                              Start
                            </button>
                          )}
                        </span>
                      </li>
                    ))}
                  </ul>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
