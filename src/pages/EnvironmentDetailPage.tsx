import { useCallback, useEffect, useMemo, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { api, errorText, RequestError } from '../api/client';
import { copyText } from '../lib/clipboard';
import { confirm } from '../components/confirm';
import { confirmDeleteEnv, deleteEnv } from '../lib/deleteEnv';
import type { AppEvent, Instance, LoginProfileView } from '../api/types';
import { useStore } from '../store';
import { EmptyState, StatusDot, btnGhost, btnPrimary } from '../components/ui';

/** 状态已变的冲突（Chrome 已退出/实例已不在）不是用户错误：静默刷新而非报错 */
function isStaleConflict(e: unknown): boolean {
  return (
    e instanceof RequestError &&
    ['INSTANCE_NOT_RUNNING', 'INSTANCE_ALREADY_RUNNING', 'PROFILE_IN_USE'].includes(e.code)
  );
}

const PROFILE_STATUS_TEXT: Record<LoginProfileView['status'], string> = {
  not_configured: 'Not configured',
  ready: 'Ready',
  capturing: 'Capturing…',
  error: 'Error',
};

/** 实例显示名：环境内创建顺序序号（列表已按 created_at ASC 返回） */
function displayName(list: Instance[], id: string): string {
  const idx = list.findIndex((i) => i.id === id);
  return `Chrome #${idx + 1}`;
}

export default function EnvironmentDetailPage() {
  const { id = '' } = useParams();
  const navigate = useNavigate();
  const { envs, instancesByEnv, fetchEnvs, fetchInstances, showToast, settings, fetchSettings } = useStore();
  const env = envs.find((e) => e.id === id);
  const instances = instancesByEnv[id] ?? [];
  const [busyId, setBusyId] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [profile, setProfile] = useState<LoginProfileView | null>(null);
  const [profileBusy, setProfileBusy] = useState(false);
  const [activity, setActivity] = useState<AppEvent[]>([]);

  const fetchProfile = useCallback(async () => {
    try {
      setProfile((await api.getLoginProfile(id)) as LoginProfileView);
    } catch {
      /* 环境不存在等场景静默，主列表已有提示 */
    }
  }, [id]);

  const fetchActivity = useCallback(async () => {
    try {
      setActivity((await api.getActivity(id)) as AppEvent[]);
    } catch {
      /* 环境刚被删除等场景静默 */
    }
  }, [id]);

  useEffect(() => {
    fetchEnvs();
    fetchInstances(id);
    fetchProfile();
    fetchActivity();
    fetchSettings();
  }, [id, fetchEnvs, fetchInstances, fetchProfile, fetchActivity, fetchSettings]);

  // 快照状态变更事件（浏览器退出/捕获完成）→ 即时刷新；轮询兑底
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    listen('login-profile-changed', () => fetchProfile()).then((fn) => (unlisten = fn));
    const t = setInterval(fetchProfile, 3000);
    return () => {
      unlisten?.();
      clearInterval(t);
    };
  }, [fetchProfile]);

  const refresh = useCallback(() => {
    fetchEnvs();
    fetchInstances(id);
  }, [id, fetchEnvs, fetchInstances]);

  const hostsCount = useMemo(() => {
    const withRules = instances.find((i) => i.hostRules);
    if (withRules?.hostRules) return Object.keys(JSON.parse(withRules.hostRules)).length;
    return null;
  }, [instances]);

  const createInstance = async () => {
    setCreating(true);
    try {
      await api.createInstance(id);
      showToast('ok', '实例已启动');
    } catch (e) {
      showToast('err', errorText(e));
    } finally {
      setCreating(false);
      refresh();
    }
  };

  const act = async (insId: string, action: 'focus' | 'stop' | 'restart' | 'delete' | 'start') => {
    setBusyId(insId);
    try {
      if (action === 'delete') await api.deleteInstance(insId);
      else if (action === 'start') await api.startInstance(insId);
      else if (action === 'focus') await api.focusInstance(insId);
      else if (action === 'stop') await api.stopInstance(insId);
      else await api.restartInstance(insId);
      if (action === 'stop' || action === 'delete') showToast('ok', '已完成');
    } catch (e) {
      if (isStaleConflict(e)) {
        showToast('ok', '实例状态已更新');
      } else {
        showToast('err', errorText(e));
      }
    } finally {
      setBusyId(null);
      refresh();
      fetchActivity();
    }
  };

  const copyCdp = async (insId: string) => {
    try {
      const cdp = (await api.cdpEndpoint(insId)) as { httpUrl: string };
      await copyText(cdp.httpUrl);
      showToast('ok', 'CDP Endpoint 已复制');
    } catch (e) {
      if (isStaleConflict(e)) {
        showToast('ok', '实例状态已更新');
      } else {
        showToast('err', errorText(e));
      }
      refresh();
    }
  };

  /** Login Profile 操作。capture 为秒级操作：本地 optimistic capturing 态。 */
  const profileAct = async (action: 'launch' | 'capture' | 'reset') => {
    if (
      action === 'reset' &&
      !(await confirm({
        title: '重置登录快照',
        message: ['母本登录态将被清空（已创建实例不受影响）。', '确定重置？'],
        confirmText: '重置',
      }))
    ) {
      return;
    }
    setProfileBusy(true);
    if (action === 'capture') {
      setProfile((p) => (p ? { ...p, status: 'capturing' } : p));
    }
    try {
      if (action === 'launch') await api.launchLoginProfile(id);
      else if (action === 'capture') await api.captureLoginProfile(id);
      else await api.resetLoginProfile(id);
      showToast('ok', action === 'launch' ? '登录浏览器已打开' : action === 'capture' ? '快照已捕获' : '已重置');
    } catch (e) {
      if (action === 'capture' && isStaleConflict(e)) {
        showToast('ok', '登录浏览器运行中，请先关闭再捕获');
      } else {
        showToast('err', errorText(e));
      }
    } finally {
      setProfileBusy(false);
      fetchProfile();
    }
  };

  if (!env) {
    return (
      <div className="mx-auto max-w-2xl">
        <EmptyState title="环境不存在" desc="可能已被删除。" />
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-2xl">
      <Link to="/" className="text-xs text-ink-3 hover:text-ink-2">
        ← Environments
      </Link>
      <div className="mb-6 mt-2 flex items-start justify-between">
        <div>
          <h1 className="text-base font-semibold text-ink">{env.name}</h1>
        </div>
        <button
          className="rounded-md px-2 py-1 text-xs text-ink-2 transition hover:bg-red-50 hover:text-status-error dark:hover:bg-red-500/15"
          onClick={async () => {
            const running = instances.filter((i) => i.status === 'running').length;
            if (!(await confirmDeleteEnv(env.name, running))) return;
            try {
              await deleteEnv(env.id, running);
              showToast('ok', '环境已删除');
              navigate('/');
            } catch (e) {
              showToast('err', errorText(e));
              fetchInstances(id);
            }
          }}
        >
          删除环境
        </button>
      </div>

      {/* Instances */}
      <div className="mb-6">
        <div className="mb-2 flex items-center justify-between">
          <h2 className="text-sm font-medium text-ink">
            Instances
            {instances.length > 0 && (
              <span className="ml-2 text-xs font-normal text-ink-3">
                {instances.filter((i) => i.status === 'running').length} running / {instances.length}
              </span>
            )}
          </h2>
          <button className={btnPrimary} disabled={creating} onClick={createInstance}>
            {creating ? 'Creating…' : '+ New Instance'}
          </button>
        </div>

        {instances.length === 0 ? (
          <EmptyState title="No Chrome instances" desc="为此环境启动一个隔离的浏览器实例。" />
        ) : (
          <ul className="divide-y divide-hairline-soft rounded-xl border border-hairline bg-surface-card">
            {instances.map((ins) => (
              <li key={ins.id} className="flex flex-wrap items-center gap-3 px-4 py-2.5">
                <StatusDot status={ins.status} />
                <span className="w-24 truncate text-sm text-ink">
                  {displayName(instances, ins.id)}
                </span>
                <span className="font-mono text-xs text-ink-3">{ins.cdpPort ?? '—'}</span>
                {profile?.status === 'ready' && !ins.loginProfileId && (
                  <span className="text-xs text-ink-3" title="创建时快照不可用，未克隆登录态">
                    ○ 未登录
                  </span>
                )}
                {settings?.developerMode && (
                  <div className="w-full font-mono text-[10px] leading-4 text-ink-3">
                    pid {ins.pid ?? '—'} · cdp {ins.cdpPort ?? '—'} · {ins.profileDir}
                  </div>
                )}
                <div className="ml-auto flex items-center gap-1.5">
                  {ins.status === 'running' && (
                    <>
                      <button
                        className={btnGhost}
                        disabled={busyId === ins.id}
                        onClick={() => act(ins.id, 'focus')}
                      >
                        Focus
                      </button>
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
                        onClick={() => act(ins.id, 'restart')}
                      >
                        Restart
                      </button>
                      <button
                        className={btnGhost}
                        disabled={busyId === ins.id}
                        onClick={() => act(ins.id, 'stop')}
                      >
                        Stop
                      </button>
                    </>
                  )}
                  {(ins.status === 'stopped' || ins.status === 'error') && (
                    <>
                      <button
                        className={btnGhost}
                        disabled={busyId === ins.id}
                        onClick={() => act(ins.id, 'start')}
                      >
                        Start
                      </button>
                      <button
                        className={btnGhost}
                        disabled={busyId === ins.id}
                        onClick={() => act(ins.id, 'delete')}
                      >
                        Delete
                      </button>
                    </>
                  )}
                </div>
              </li>
            ))}
          </ul>
        )}
      </div>

      {/* Login Profile */}
      <div className="mb-6 rounded-xl border border-hairline bg-surface-card p-4">
        <h2 className="text-sm font-medium text-ink">🔐 {profile?.name ?? 'Login Profile'}</h2>
        {profile ? (
          <>
            <div className="mt-1.5 space-y-1 text-xs text-ink-2">
              <p>
                状态 <span className="font-medium text-ink">{PROFILE_STATUS_TEXT[profile.status]}</span>
                {' · '}
                {profile.instancesUsing} 个实例使用
                {' · '}
                最近捕获{' '}
                {profile.lastCapturedAt ? new Date(profile.lastCapturedAt).toLocaleString() : '从未'}
                {profile.snapshotVersion > 0 && ` · v${profile.snapshotVersion}`}
              </p>
              {profile.browserRunning && (
                <p className="text-amber-600">登录浏览器运行中，关闭窗口后几秒内即可捕获快照</p>
              )}
              {profile.status === 'error' && (
                <p className="text-status-error">上次捕获失败，可重试捕获或重置</p>
              )}
            </div>
            <div className="mt-3 flex items-center gap-1.5">
              <button
                className={btnGhost}
                disabled={profileBusy || profile.status === 'capturing' || profile.browserRunning}
                onClick={() => profileAct('launch')}
              >
                打开并登录
              </button>
              <button
                className={btnGhost}
                disabled={profileBusy || profile.status === 'capturing' || profile.browserRunning}
                title={profile.browserRunning ? '先关闭登录浏览器' : undefined}
                onClick={() => profileAct('capture')}
              >
                {profile.status === 'capturing' ? '捕获中…' : '捕获快照'}
              </button>
              <button
                className={btnGhost}
                disabled={profileBusy || profile.status === 'capturing'}
                onClick={() => profileAct('reset')}
              >
                重置
              </button>
            </div>
            <p className="mt-2 text-[11px] text-ink-3">
              捕获后的快照会克隆给之后创建的每个实例；快照更新不影响已运行实例
            </p>
          </>
        ) : (
          <p className="mt-1.5 text-xs text-ink-3">加载中…</p>
        )}
      </div>

      {/* Activity 流 */}
      <div className="mb-6 rounded-xl border border-hairline bg-surface-card p-4">
        <h2 className="text-sm font-medium text-ink">Activity</h2>
        {activity.length === 0 ? (
          <p className="mt-2 text-xs text-ink-3">暂无事件</p>
        ) : (
          <ul className="mt-2 max-h-60 space-y-1 overflow-y-auto">
            {activity.map((e, i) => (
              <li key={i} className="flex items-baseline gap-2 text-xs">
                <span className="shrink-0 font-mono text-[10px] text-ink-3">
                  {new Date(e.ts).toLocaleTimeString()}
                </span>
                <span
                  className={
                    e.event.includes('error') || e.event.includes('crashed')
                      ? 'text-status-error'
                      : e.event.includes('started')
                        ? 'text-run-text'
                        : 'text-ink-2'
                  }
                >
                  {e.event}
                </span>
                {e.message && <span className="truncate text-ink-3">{e.message}</span>}
              </li>
            ))}
          </ul>
        )}
      </div>

      {/* Hosts Mapping */}
      <div className="rounded-xl border border-hairline bg-surface-card p-4">
        <h2 className="text-sm font-medium text-ink">Hosts Mapping</h2>
        {env.hostsSourceUrl ? (
          <div className="mt-1.5 space-y-1 text-xs text-ink-2">
            <p>
              {hostsCount !== null ? (
                <>
                  已映射 <span className="font-medium text-ink">{hostsCount}</span> 个域名
                </>
              ) : (
                '尚未有运行实例固化映射'
              )}
              {' · 来源 '}
              <span className="font-mono text-[11px]">{env.hostsSourceUrl}</span>
            </p>
            <p className="text-[11px] text-ink-3">
              实例启动时现拉最新配置，经 --host-resolver-rules 注入，进程级隔离、不改系统 hosts
            </p>
          </div>
        ) : (
          <p className="mt-1.5 text-xs text-ink-3">未配置 hosts 源（实例使用系统 DNS）</p>
        )}
      </div>
    </div>
  );
}
