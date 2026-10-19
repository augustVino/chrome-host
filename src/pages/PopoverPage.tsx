import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { emit } from '@tauri-apps/api/event';
import { api, errorText } from '../api/client';
import type { EnvironmentSummary, Instance } from '../api/types';
import { useStore } from '../store';
import { openManager } from '../lib/windows';
import { copyText } from '../lib/clipboard';
import { deleteEnv } from '../lib/deleteEnv';
import { Icon, StatusDot, btnDanger, btnGhost, btnPrimary, btnQuietDanger } from '../components/ui';
import { GradientDefs } from '../components/icons';
import logoUrl from '../assets/logo.png';
import { Toast } from '../components/Toast';
import { envGradient } from '../lib/envColor';
import { gsap, useGSAP } from '../motion/gsap';
import { qMode } from '../motion/quality';
import { enterDelay, useGhostList, type GhostItem } from '../motion/hooks';
import { markLaunch } from '../motion/launch';

/** Popover 高度自适应：内容决定高度，540 封顶（超出后列表区内部滚动）；宽度固定由 Rust 持有 */
const POPOVER_MAX_HEIGHT = 540;
const POPOVER_MIN_HEIGHT = 280;

/** 实例显示名：环境内创建顺序序号 */
function displayName(list: Instance[], id: string): string {
  const idx = list.findIndex((i) => i.id === id);
  return `Chrome #${idx + 1}`;
}

/**
 * 托盘 Popover 内容：高频四件事——查看摘要 / New / Focus / Stop；
 * 删除为低频高危入口，采用原地两段式确认（无弹层，见 PopEnv）。
 * 数据通道不变（HTTP API + 后端事件）；窗口隐藏时 JS 持续轮询，弹出即最新。
 */
export default function PopoverPage() {
  const { envs, envsLoaded, instancesByEnv, fetchEnvs, initEvents, showToast } = useStore();
  const [busyId, setBusyId] = useState<string | null>(null);
  /** 根容器显式像素高度：不依赖 100vh（程序化 resize 后 vh 不可靠） */
  const [contentH, setContentH] = useState<number | null>(null);
  const headerRef = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const footerRef = useRef<HTMLDivElement>(null);

  /** 高度自适应：实测 header + 列表内容 + footer 的自然高度，上报给 Rust 执行 setSize。
   *  内容 ≤ max 时窗口收缩到内容高度；超出时窗口封顶（clamp 在 Rust 侧），列表区内部滚动。 */
  const syncHeight = useCallback(async () => {
    const { header, content, footer } = {
      header: headerRef.current,
      content: contentRef.current,
      footer: footerRef.current,
    };
    if (!header || !content || !footer) return;
    // 注意：不能测 list.scrollHeight——滚动容器被 flex 约束后 scrollHeight ≥ 自身高度，
    // 测量值被"当前窗口高度"污染，自适应死循环失效；content 包装器高度才是自然内容高度
    const natural = header.offsetHeight + content.offsetHeight + footer.offsetHeight;
    const clamped = Math.round(Math.min(Math.max(natural, POPOVER_MIN_HEIGHT), POPOVER_MAX_HEIGHT));
    setContentH((prev) => (prev === clamped ? prev : clamped));
    try {
      await emit('popover-height', { height: clamped });
    } catch {
      /* 事件通道不可用时静默：窗口保持创建尺寸 */
    }
  }, []);

  // 内容尺寸变化（环境/实例增减、首次数据加载）→ 重算窗口高度
  useEffect(() => {
    const content = contentRef.current;
    if (!content) return;
    let raf = 0;
    const ro = new ResizeObserver(() => {
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(() => void syncHeight());
    });
    ro.observe(content);
    return () => {
      ro.disconnect();
      cancelAnimationFrame(raf);
    };
  }, [syncHeight]);

  const refresh = useCallback(async () => {
    const s = useStore.getState();
    await s.fetchEnvs();
    s.envs.forEach((e) => s.fetchInstances(e.id));
  }, [fetchEnvs]);

  useEffect(() => {
    refresh();
    initEvents();
    const t = setInterval(refresh, 2000);
    void syncHeight();
    return () => clearInterval(t);
  }, [refresh, initEvents, syncHeight]);

  const act = useCallback(
    async (fn: () => Promise<unknown>, key: string) => {
      setBusyId(key);
      try {
        await fn();
      } catch (e) {
        showToast('err', errorText(e));
      } finally {
        setBusyId(null);
        refresh();
      }
    },
    [showToast, refresh],
  );

  const removeEnv = useCallback(
    async (envId: string, running: number) => {
      setBusyId(envId);
      try {
        await deleteEnv(envId, running);
        showToast('ok', '环境已删除');
      } catch (e) {
        showToast('err', errorText(e));
      } finally {
        setBusyId(null);
      }
      refresh();
    },
    [showToast, refresh],
  );

  const isMac = navigator.userAgent.includes('Mac');
  const rootRef = useRef<HTMLDivElement>(null);

  // 弹出重放入场：窗口每次获得焦点（托盘点击唤出）重放一次，让 Popover 每次出现都有"活"感
  useEffect(() => {
    const replay = () => {
      const root = rootRef.current;
      if (!root) return;
      const quick = qMode() === 'static';
      const tl = gsap.timeline();
      tl.fromTo(root, { autoAlpha: 0, y: -12, scale: 0.97, transformOrigin: '88% 0%' }, { autoAlpha: 1, y: 0, scale: 1, duration: quick ? 0.15 : 0.4, clearProps: 'transform,opacity,visibility' });
      if (!quick) {
        tl.fromTo(root.querySelectorAll('.pop-env'), { autoAlpha: 0, y: 14 }, { autoAlpha: 1, y: 0, duration: 0.5, stagger: 0.06, clearProps: 'transform,opacity,visibility' }, 0.08);
      }
    };
    window.addEventListener('focus', replay);
    return () => window.removeEventListener('focus', replay);
  }, []);

  const envList = useGhostList(envs, (e) => e.id, envsLoaded);

  return (
    <div
      ref={rootRef}
      className={`pop-root ${isMac ? '' : 'no-vibrancy'}`}
      style={{ height: contentH ? `${contentH}px` : '100vh' }}
    >
      <GradientDefs />
      <div className="pop-glow" />
      <div ref={headerRef} className="pop-h">
        <span className="pop-brand">
          <img src={logoUrl} alt="" draggable={false} />
          Chrome Host
        </span>
        <button className={`${btnGhost} btn-xs`} onClick={() => openManager()}>
          Open Manager
        </button>
      </div>

      <div ref={listRef} className="relative min-h-0 flex-1 overflow-y-auto" style={{ scrollbarWidth: 'none' }}>
        <div ref={contentRef} className="flex flex-col gap-2.5 p-3">
          {envs.length === 0 && (
            <p className="py-10 text-center text-xs text-ink-3">暂无环境，去主窗口创建</p>
          )}
          {envList.list.map((g) => (
            <PopEnv
              key={g.key}
              g={g}
              instances={instancesByEnv[g.key] ?? []}
              loaded={g.key in instancesByEnv}
              busyId={busyId}
              onRelease={envList.release}
              onNew={(env, el) => {
                markLaunch(env.id, el);
                act(async () => {
                  await api.createInstance(env.id);
                  showToast('ok', '实例已启动');
                }, env.id);
              }}
              onDelete={(env, instances) =>
                removeEnv(env.id, instances.filter((i) => i.status === 'running').length)
              }
              act={act}
            />
          ))}
        </div>
      </div>

      <div ref={footerRef} className="relative border-t border-popover-line px-3.5 py-2.5">
        <button className={`${btnGhost} btn-xs w-full`} onClick={() => openManager()}>
          <Icon name="plus" /> Add Environment
        </button>
      </div>

      <Toast compact />
    </div>
  );
}

const PLAY = 'M8 5.5v13l11-6.5z';
const STOP = 'M6.5 6.5h11v11h-11z';

/** 启动/停止切换图标：播放三角 ⇄ 停止方块（MorphSVG） */
function ToggleIcon({ mode }: { mode: 'play' | 'stop' }) {
  const p = useRef<SVGPathElement>(null);
  const first = useRef(true);
  useLayoutEffect(() => {
    const to = mode === 'stop' ? STOP : PLAY;
    if (first.current || qMode() === 'static') p.current?.setAttribute('d', to);
    else gsap.to(p.current, { morphSVG: to, duration: 0.4, ease: 'power2.inOut' });
    first.current = false;
  }, [mode]);
  return (
    <svg viewBox="0 0 24 24">
      <path className="mp" ref={p} />
    </svg>
  );
}

function PopRow({ ins, name, envId, busy, g, onRelease, act }: {
  ins: Instance;
  name: string;
  envId: string;
  busy: boolean;
  g: GhostItem<Instance>;
  onRelease: (key: string) => void;
  act: (fn: () => Promise<unknown>, key: string) => Promise<void>;
}) {
  const { showToast } = useStore();
  const li = useRef<HTMLLIElement>(null);
  useGSAP(
    () => {
      if (!li.current || qMode() === 'static') return;
      if (g.enter.mode !== 'none') gsap.from(li.current, { autoAlpha: 0, x: -16, duration: 0.5, delay: g.enter.mode === 'stagger' ? enterDelay() : 0, clearProps: 'transform,opacity,visibility' });
    },
    { scope: li },
  );
  useGSAP(
    () => {
      if (!g.leaving || !li.current) return;
      if (qMode() === 'static') return onRelease(g.key);
      gsap.to(li.current, { autoAlpha: 0, x: 16, duration: 0.25, ease: 'power2.in', onComplete: () => onRelease(g.key) });
    },
    { dependencies: [g.leaving], scope: li },
  );
  const running = ins.status === 'running';
  const canToggle = running || ins.status === 'stopped' || ins.status === 'error';
  return (
    <li ref={li} className="flex items-center gap-2">
      <StatusDot status={ins.status} label={false} insId={ins.id} flyFromEnv={g.enter.mode === 'pop' ? envId : undefined} />
      <span className="ml-[-2px] flex-1 text-[11.5px] font-semibold text-ink-2">{name}</span>
      <span className="inline-flex gap-1.5">
        {running && (
          <button
            key="cdp"
            className="ib"
            data-tip="复制 CDP"
            disabled={busy}
            onClick={() =>
              act(async () => {
                const cdp = (await api.cdpEndpoint(ins.id)) as { httpUrl: string };
                await copyText(cdp.httpUrl);
                showToast('ok', 'CDP 已复制');
              }, ins.id)
            }
          >
            <Icon name="copy" />
          </button>
        )}
        {running && (
          <button
            key="restart"
            className="ib"
            data-tip="重启"
            disabled={busy}
            onClick={(e) => {
              const svg = e.currentTarget.querySelector('svg');
              if (svg && qMode() !== 'static') gsap.fromTo(svg, { rotation: 0 }, { rotation: 360, duration: 0.7, ease: 'power2.inOut' });
              void act(() => api.restartInstance(ins.id), ins.id);
            }}
          >
            <Icon name="restart" />
          </button>
        )}
        {canToggle && (
          <button
            key="toggle"
            className="ib"
            data-tip={running ? '停止' : '启动'}
            disabled={busy}
            onClick={() => act(() => (running ? api.stopInstance(ins.id) : api.startInstance(ins.id)), ins.id)}
          >
            <ToggleIcon mode={running ? 'stop' : 'play'} />
          </button>
        )}
      </span>
    </li>
  );
}

/** 删除按钮 armed 态自动复位时限（ms）：超时未确认回到"删除" */
const ARM_TIMEOUT = 3000;

function PopEnv({ g, instances, loaded, busyId, onRelease, onNew, onDelete, act }: {
  g: GhostItem<EnvironmentSummary>;
  instances: Instance[];
  loaded: boolean;
  busyId: string | null;
  onRelease: (key: string) => void;
  onNew: (env: EnvironmentSummary, el: Element) => void;
  onDelete: (env: EnvironmentSummary, instances: Instance[]) => void;
  act: (fn: () => Promise<unknown>, key: string) => Promise<void>;
}) {
  const env = g.item;
  const el = useRef<HTMLDivElement>(null);
  const running = instances.filter((i) => i.status === 'running').length;
  const rows = useGhostList(instances, (i) => i.id, loaded);
  /** 两段式删除：首次点击 arm（实心 danger 确认态），再次点击执行；超时/失焦/窗口隐藏自动复位 */
  const [armed, setArmed] = useState(false);
  const armTimer = useRef(0);
  const disarm = useCallback(() => {
    clearTimeout(armTimer.current);
    setArmed(false);
  }, []);
  const arm = () => {
    clearTimeout(armTimer.current);
    armTimer.current = window.setTimeout(disarm, ARM_TIMEOUT);
    setArmed(true);
  };
  useEffect(() => {
    // popover 隐藏/切窗会触发 blur，重新唤起会触发 focus：两时机都复位，
    // 防止确认态残留到下次弹出（叠加 3s 超时兑底）
    window.addEventListener('blur', disarm);
    window.addEventListener('focus', disarm);
    return () => {
      window.removeEventListener('blur', disarm);
      window.removeEventListener('focus', disarm);
      clearTimeout(armTimer.current);
    };
  }, [disarm]);

  useGSAP(
    () => {
      if (!el.current || qMode() === 'static' || g.enter.mode !== 'pop') return;
      gsap.from(el.current, { autoAlpha: 0, y: 14, scale: 0.96, duration: 0.5, clearProps: 'transform,opacity,visibility' });
    },
    { scope: el },
  );
  useGSAP(
    () => {
      if (!g.leaving || !el.current) return;
      if (qMode() === 'static') return onRelease(g.key);
      gsap.to(el.current, { autoAlpha: 0, scale: 0.94, y: 10, duration: 0.3, ease: 'power2.in', onComplete: () => onRelease(g.key) });
    },
    { dependencies: [g.leaving], scope: el },
  );

  return (
    <div ref={el} className={`pop-env g${envGradient(env.id)} ${running > 0 ? 'live' : ''}`}>
      <i className="bar" />
      <div className="flex items-center gap-2">
        <b className="min-w-0 flex-1 truncate text-[13px] font-bold">{env.name}</b>
        <small className={`text-[10.5px] font-bold ${running ? 'text-run-text' : 'text-ink-3'}`}>
          {running ? `● ${running} running` : instances.length ? '○ idle' : '○ empty'}
        </small>
        <button
          className={armed ? `${btnDanger} btn-xs` : btnQuietDanger}
          title={armed ? '将删除登录快照与全部实例数据，不可恢复' : '删除环境'}
          disabled={busyId === env.id}
          onBlur={disarm}
          onClick={() => (armed ? (disarm(), onDelete(env, instances)) : arm())}
        >
          {busyId === env.id ? '…' : armed ? (running ? '停止并删除' : '确认删除') : '删除'}
        </button>
        <button className={`${btnPrimary} btn-xs`} disabled={busyId === env.id} onClick={(e) => onNew(env, e.currentTarget)}>
          {busyId === env.id ? '…' : '+ New'}
        </button>
      </div>
      {rows.list.length > 0 && (
        <ul className="pop-ins">
          {rows.list.map((r, idx) => (
            <PopRow
              key={r.key}
              ins={r.item}
              name={r.leaving ? `Chrome #${idx + 1}` : displayName(instances, r.key)}
              envId={env.id}
              busy={busyId === r.key}
              g={r}
              onRelease={rows.release}
              act={act}
            />
          ))}
        </ul>
      )}
    </div>
  );
}
