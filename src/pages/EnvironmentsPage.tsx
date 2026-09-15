import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { api, errorText } from '../api/client';
import { useStore } from '../store';
import { confirmDeleteEnv, deleteEnv } from '../lib/deleteEnv';
import { EmptyState, Field, Modal, btnGhost, btnPrimary, inputCls } from '../components/ui';

export default function EnvironmentsPage() {
  const { envs, envsLoading, fetchEnvs, showToast } = useStore();
  const [creating, setCreating] = useState(false);
  const [form, setForm] = useState({ name: '', hostsSourceUrl: '' });
  const [submitting, setSubmitting] = useState(false);

  useEffect(() => {
    fetchEnvs();
  }, [fetchEnvs]);

  const submit = async () => {
    setSubmitting(true);
    try {
      await api.createEnvironment({
        name: form.name,
        hostsSourceUrl: form.hostsSourceUrl || null,
      });
      setCreating(false);
      setForm({ name: '', hostsSourceUrl: '' });
      showToast('ok', '环境已创建');
      fetchEnvs();
    } catch (e) {
      showToast('err', errorText(e));
    } finally {
      setSubmitting(false);
    }
  };

  const remove = async (id: string, name: string, running: number) => {
    if (!(await confirmDeleteEnv(name, running))) return;
    try {
      await deleteEnv(id, running);
      showToast('ok', '环境已删除');
    } catch (e) {
      showToast('err', errorText(e));
    }
    fetchEnvs();
  };

  return (
    <div className="mx-auto max-w-2xl">
      <div className="mb-5 flex items-center justify-between">
        <div>
          <h1 className="text-base font-semibold text-ink">Environments</h1>
          <p className="mt-0.5 text-xs text-ink-3">
            {envs.length} 个环境 ·{' '}
            {envs.reduce((acc, e) => acc + e.runningInstances, 0)} 个运行中实例
          </p>
        </div>
        <button className={btnPrimary} onClick={() => setCreating(true)}>
          + Add Environment
        </button>
      </div>

      {envsLoading && envs.length === 0 ? (
        <p className="py-20 text-center text-xs text-ink-3">加载中…</p>
      ) : envs.length === 0 ? (
        <EmptyState
          title="No environments yet"
          desc="创建你的第一个 Chrome 环境。环境可携带 hosts 配置源，实现实例级域名映射。"
          action={
            <button className={btnPrimary} onClick={() => setCreating(true)}>
              + Add Environment
            </button>
          }
        />
      ) : (
        <ul className="space-y-3">
          {envs.map((env) => (
            <li
              key={env.id}
              className="flex items-center gap-2 rounded-xl border border-hairline bg-white p-4 transition hover:border-neutral-300 hover:shadow-sm"
            >
              <Link to={`/environments/${env.id}`} className="min-w-0 flex-1">
                <p className="truncate text-sm font-medium text-ink">{env.name}</p>
                {env.hostsSourceUrl && (
                  <p className="mt-2 inline-flex rounded-md bg-neutral-50 px-1.5 py-0.5 text-[11px] text-ink-2">
                    🔗 hosts 源已配置
                  </p>
                )}
              </Link>
              <div className="flex shrink-0 items-center gap-2 text-xs">
                {/* 状态分布：有运行 → N running（+M stopped）；全停止 → N instances；真空 → No instances
                    （修复：stopped 实例曾被渲染成 "No instances"，与 popover 全量口径不一致） */}
                {env.runningInstances > 0 ? (
                  <span className="inline-flex items-center gap-1.5 text-run-text">
                    <span className="size-2 rounded-full bg-status-running" />
                    {env.runningInstances} running
                    {env.totalInstances > env.runningInstances && (
                      <span className="text-ink-3">
                        · {env.totalInstances - env.runningInstances} stopped
                      </span>
                    )}
                  </span>
                ) : env.totalInstances > 0 ? (
                  <span className="text-ink-3">○ {env.totalInstances} instances</span>
                ) : (
                  <span className="text-ink-3">○ No instances</span>
                )}
                <button
                  className="shrink-0 rounded-md px-2 py-1 text-xs text-ink-2 transition hover:bg-red-50 hover:text-status-error"
                  title="删除环境"
                  onClick={() => remove(env.id, env.name, env.runningInstances)}
                >
                  删除
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}

      <Modal open={creating} title="Add Environment" onClose={() => setCreating(false)}>
        <Field label="名称">
          <input
            className={inputCls}
            value={form.name}
            onChange={(e) => setForm({ ...form, name: e.target.value })}
            placeholder="Staging"
            autoFocus
          />
        </Field>
        <Field
          label="hosts 配置源（可选）"
          hint="返回 hosts 格式文本的 http(s) URL；实例启动时现拉最新配置并注入进程级 DNS 映射"
        >
          <input
            className={inputCls}
            value={form.hostsSourceUrl}
            onChange={(e) => setForm({ ...form, hostsSourceUrl: e.target.value })}
            placeholder="https://example.com/hosts.txt"
          />
        </Field>
        <div className="mt-4 flex justify-end gap-2">
          <button className={btnGhost} onClick={() => setCreating(false)}>
            取消
          </button>
          <button
            className={btnPrimary}
            disabled={!form.name.trim() || submitting}
            onClick={submit}
          >
            {submitting ? '创建中…' : '创建'}
          </button>
        </div>
      </Modal>
    </div>
  );
}
