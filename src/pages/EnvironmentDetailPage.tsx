import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { api, errorText, RequestError } from '../api/client';
import { copyText } from '../lib/clipboard';
import { confirm } from '../components/confirm';
import { confirmDeleteEnv, deleteEnv } from '../lib/deleteEnv';
import type { AppEvent, Instance, LoginProfileView } from '../api/types';
import { useStore } from '../store';
import { EmptyState, StatusDot, btnGhostSm, btnPrimary, btnQuietDanger } from '../components/ui';
import { envGradient, envInitial } from '../lib/envColor';
import { Flip, useGSAP } from '../motion/gsap';
import { qMode } from '../motion/quality';
import { usePageEnter } from '../motion/hooks';
import { markLaunch, peekPendingFlip } from '../motion/launch';

/** 状态已变的冲突（Chrome 已退出/实例已不在）不是用户错误：静默刷新而非报错 */
function isStaleConflict(e: unknown): boolean {
  return (
    e instanceof RequestError &&
    ['INSTANCE_NOT_RUNNING', 'INSTANCE_ALREADY_RUNNING', 'PROFILE_IN_USE'].includes(e.code)
  );
}

/** 实例显示名：环境内创建顺序序号（列表已按 created_at ASC 返回） */
function displayName(list: Instance[], id: string): string {
  const idx = list.findIndex((i) => i.id === id);
  return `Chrome #${idx + 1}`;
}

const fmtTime = (ts: number) => new Date(ts).toLocaleTimeString();

/** 实例诊断抽屉（M7-C⑤）：console 错误 + 失败请求 + 当前页截图，
 * 数据源为三个现成常驻诊断端点，零新后端。打开时拉取一次，刷新按钮重拉
 * （环形缓冲是服务端实时态，不自动轮询——人看到问题去实例里操作才是主路径）。 */
function DiagnosticsPanel({ insId, onClose }: { insId: string; onClose: () => void }) {
  const [loading, setLoading] = useState(true);
  const [errors, setErrors] = useState<{ ts: number; text: string; source: string }[]>([]);
  const [failed, setFailed] = useState<{ ts: number; method: string; url: string; status: number | null; error: string | null }[]>([]);
  const [shot, setShot] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setErr(null);
    try {
      // 截图失败（无 http 页/连接未就绪）不拖垮面板：静默置空
      const [c, n, s] = await Promise.all([
        api.getInstanceConsole(insId, 'error', 50),
        api.getInstanceNetwork(insId, true, 50),
        api.getInstanceScreenshot(insId).catch((): null => null),
      ]);
      setErrors(c.entries ?? []);
      setFailed(n.entries ?? []);
      setShot(s?.data ?? null);
    } catch (e) {
      setErr(errorText(e));
    } finally {
      setLoading(false);
    }
  }, [insId]);

  useEffect(() => {
    load();
  }, [load]);

  return (
    <div className="mt-2 w-full rounded-2xl bg-surface-inset p-3">
      <div className="mb-2 flex items-center gap-2">
        <span className="text-xs font-medium text-ink">诊断</span>
        <span className="text-[11px] text-ink-3">console 错误 / 失败请求 / 当前页截图（进程内态，实例重启即清）</span>
        <div className="ml-auto flex gap-1.5">
          <button className={btnGhostSm} disabled={loading} onClick={load}>
            刷新
          </button>
          <button className={btnGhostSm} onClick={onClose}>
            收起
          </button>
        </div>
      </div>
      {err && <p className="text-xs text-status-error">{err}</p>}
      {loading && <p className="text-xs text-ink-3">读取中…</p>}
      {!loading && !err && (
        <div className="flex flex-wrap items-start gap-3">
          {shot ? (
            <img
              src={`data:image/png;base64,${shot}`}
              alt="实例当前页面截图"
              className="w-64 shrink-0 rounded-xl ring-1 ring-hairline"
            />
          ) : (
            <div className="flex w-64 shrink-0 items-center justify-center rounded-xl bg-surface-card px-2 py-8 text-center text-[11px] text-ink-3 ring-1 ring-hairline">
              截图不可用（无 http 页或连接未就绪）
            </div>
          )}
          <div className="min-w-0 flex-1 space-y-2">
            <div>
              <p className="text-[11px] font-medium text-ink-2">console 错误（最近 {errors.length} 条）</p>
              {errors.length === 0 ? (
                <p className="text-[11px] text-ink-3">无</p>
              ) : (
                <ul className="max-h-28 space-y-0.5 overflow-y-auto font-mono text-[11px] leading-4 text-ink-2">
                  {errors.map((e, i) => (
                    <li key={i} className="truncate" title={`${e.source}: ${e.text}`}>
                      {fmtTime(e.ts)} [{e.source}] {e.text}
                    </li>
                  ))}
                </ul>
              )}
            </div>
            <div>
              <p className="text-[11px] font-medium text-ink-2">失败请求（失败或 HTTP≥400，最近 {failed.length} 条）</p>
              {failed.length === 0 ? (
                <p className="text-[11px] text-ink-3">无</p>
              ) : (
                <ul className="max-h-28 space-y-0.5 overflow-y-auto font-mono text-[11px] leading-4 text-ink-2">
                  {failed.map((f, i) => (
                    <li key={i} className="truncate" title={f.url}>
                      {fmtTime(f.ts)} {f.method} {f.status ?? `FAIL(${f.error ?? '?'})`} {f.url}
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

export default function EnvironmentDetailPage() {
  const { id = '' } = useParams();
  const navigate = useNavigate();
  const { envs, instancesByEnv, fetchEnvs, fetchInstances, showToast, settings, fetchSettings } = useStore();
  const env = envs.find((e) => e.id === id);
  const instances = instancesByEnv[id] ?? [];
  const [busyId, setBusyId] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [diagId, setDiagId] = useState<string | null>(null);
  const [profile, setProfile] = useState<LoginProfileView | null>(null);
  const [profileBusy, setProfileBusy] = useState(false);
  const [activity, setActivity] = useState<AppEvent[]>([]);
  const root = useRef<HTMLDivElement>(null);
  // 卡片 → 详情共享元素（tile / 名称）：有 Flip 状态时标题跳过 SplitText 入场
  const hadFlip = useRef(peekPendingFlip() !== null);
  usePageEnter(root, { skipTitle: hadFlip.current });
  useGSAP(
    () => {
      const st = peekPendingFlip<ReturnType<typeof Flip.getState>>();
      if (st && qMode() !== 'static') Flip.from(st, { duration: 0.9, ease: 'ch-out', scale: true });
    },
    { scope: root },
  );

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

  /** Login Profile 重置（实例即登录源后，保存路径已自动化，仅剩重置入口） */
  const profileAct = async () => {
    if (
      !(await confirm({
        title: '重置登录态',
        message: ['环境登录态将被清空（Cookie 保险库 + 文件快照）。', '已创建实例不受影响。确定重置？'],
        confirmText: '重置',
      }))
    ) {
      return;
    }
    setProfileBusy(true);
    try {
      await api.resetLoginProfile(id);
      showToast('ok', '已重置');
    } catch (e) {
      showToast('err', errorText(e));
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
    <div ref={root} className="mx-auto max-w-3xl">
      <Link to="/" className="rv inline-flex items-center gap-1 text-xs font-semibold text-ink-3 transition hover:text-ink-2">
        ← Environments
      </Link>
      <header className="ph rv mb-6 mt-3">
        <div className="flex min-w-0 items-center gap-4">
          <span className={`tile g${envGradient(env.id)}`} style={{ width: 52, height: 52, fontSize: 22 }} data-flip-id={`tile-${env.id}`}>
            {envInitial(env.name)}
          </span>
          <div className="min-w-0">
            <h1 className="title truncate" style={{ fontSize: 26 }} data-flip-id={`name-${env.id}`}>
              {env.name}
            </h1>
            <p className="sub">
              {instances.filter((i) => i.status === 'running').length} running / {instances.length} instances
            </p>
          </div>
        </div>
        <button
          className={btnQuietDanger}
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
      </header>

      {/* Instances */}
      <div className="rv mb-6">
        <div className="mb-3 flex items-center justify-between">
          <h2 className="text-sm font-medium text-ink">
            Instances
            {instances.length > 0 && (
              <span className="ml-2 text-xs font-normal text-ink-3">
                {instances.filter((i) => i.status === 'running').length} running / {instances.length}
              </span>
            )}
          </h2>
          <button
            className={btnPrimary}
            disabled={creating}
            onClick={(e) => {
              markLaunch(id, e.currentTarget);
              void createInstance();
            }}
          >
            {creating ? 'Creating…' : '+ New Instance'}
          </button>
        </div>

        {instances.length === 0 ? (
          <EmptyState title="No Chrome instances" desc="为此环境启动一个隔离的浏览器实例。" />
        ) : (
          <ul className="gcard divide-y divide-hairline-soft">
            {instances.map((ins) => (
              <li key={ins.id} className="flex flex-wrap items-center gap-3 px-4 py-2.5">
                <StatusDot status={ins.status} insId={ins.id} flyFromEnv={id} />
                <span className="w-24 truncate text-sm text-ink">
                  {displayName(instances, ins.id)}
                </span>
                <span className="font-mono text-xs text-ink-3">{ins.cdpPort ?? '—'}</span>
                {profile?.status === 'ready' && !ins.loginProfileId && (
                  <span className="text-xs text-ink-3" title="创建时快照不可用，未克隆登录态">
                    ○ 未登录
                  </span>
                )}
                {diagId === ins.id && (
                  <DiagnosticsPanel insId={ins.id} onClose={() => setDiagId(null)} />
                )}
                {settings?.developerMode && (
                  <div className="w-full font-mono text-[10px] leading-4 text-ink-3">
                    pid {ins.pid ?? '—'} · cdp {ins.cdpPort ?? '—'} · {ins.profileDir}
                  </div>
                )}
                <div className="ml-auto flex items-center gap-1.5">
                  {ins.status === 'running' && (
                    <button
                      className={btnGhostSm}
                      onClick={() => setDiagId(diagId === ins.id ? null : ins.id)}
                    >
                      {diagId === ins.id ? '收起诊断' : '诊断'}
                    </button>
                  )}
                  {ins.status === 'running' && (
                    <>
                      <button
                        className={btnGhostSm}
                        disabled={busyId === ins.id}
                        onClick={() => act(ins.id, 'focus')}
                      >
                        Focus
                      </button>
                      <button
                        className={btnGhostSm}
                        disabled={busyId === ins.id}
                        onClick={() => copyCdp(ins.id)}
                      >
                        Copy CDP
                      </button>
                      <button
                        className={btnGhostSm}
                        disabled={busyId === ins.id}
                        onClick={() => act(ins.id, 'restart')}
                      >
                        Restart
                      </button>
                      <button
                        className={btnGhostSm}
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
                        className={btnGhostSm}
                        disabled={busyId === ins.id}
                        onClick={() => act(ins.id, 'start')}
                      >
                        Start
                      </button>
                      <button
                        className={btnGhostSm}
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

      {/* Login Profile（实例即登录源：在实例里登录 → 关闭，登录态自动保存） */}
      <div className="gcard rv mb-6 p-5">
        <h2 className="text-sm font-medium text-ink">🔐 {profile?.name ?? 'Login Profile'}</h2>
        {profile ? (
          <>
            <div className="mt-1.5 space-y-1 text-xs text-ink-2">
              <p>
                {profile.snapshotVersion > 0 ? (
                  <>
                    已登录{' '}
                    <span className="font-medium text-run-text">v{profile.snapshotVersion}</span>
                    {' · '}
                    {profile.lastCapturedAt ? new Date(profile.lastCapturedAt).toLocaleString() : '-'}
                    {' · '}
                    {profile.instancesUsing} 个实例使用
                  </>
                ) : (
                  <>未登录：在任一实例中登录后关闭它，登录态自动保存</>
                )}
              </p>
              {profile.status === 'error' && (
                <p className="text-status-error">上次登录态保存失败，重新登录一次即可恢复</p>
              )}
            </div>
            <div className="mt-3 flex items-center gap-1.5">
              <button
                className={btnGhostSm}
                disabled={profileBusy || profile.status === 'capturing'}
                onClick={() => profileAct()}
              >
                重置登录态
              </button>
            </div>
            <p className="mt-2 text-[11px] text-ink-3">
              在实例里登录 → 关闭实例 → 之后创建的实例自动免登录（含 Cookie 与本地存储）；
              也可用 CLI <code className="font-mono">login-profile capture --from</code> 从指定实例固化
            </p>
          </>
        ) : (
          <p className="mt-1.5 text-xs text-ink-3">加载中…</p>
        )}
      </div>

      {/* Activity 流 */}
      <div className="gcard rv mb-6 p-5">
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
      <div className="gcard rv p-5">
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
