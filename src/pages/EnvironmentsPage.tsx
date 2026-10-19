import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent as RKeyboardEvent, type MouseEvent as RMouseEvent } from 'react';
import { useNavigate } from 'react-router-dom';
import { api, errorText } from '../api/client';
import type { EnvironmentSummary, Instance } from '../api/types';
import { useStore } from '../store';
import { useFeed, type FeedItem } from '../feed';
import { confirmDeleteEnv, deleteEnv } from '../lib/deleteEnv';
import { envGradient, envInitial } from '../lib/envColor';
import {
  EmptyState,
  Field,
  Icon,
  Modal,
  PageHeader,
  Seg,
  StatusDot,
  btnGhost,
  btnMint,
  btnPrimary,
  btnQuietDanger,
  inputCls,
} from '../components/ui';
import { Odometer } from '../components/Odometer';
import { gsap, Flip, useGSAP } from '../motion/gsap';
import { qMode } from '../motion/quality';
import { enterDelay, useFlipOnChange, useGhostList, usePageEnter, useTilt, type GhostItem } from '../motion/hooks';
import { markLaunch, setPendingFlip } from '../motion/launch';

const COLORS: Record<FeedItem['kind'], string> = {
  ok: 'rgb(var(--mint))',
  warn: 'rgb(var(--amber))',
  err: 'rgb(var(--coral))',
  info: 'rgb(var(--brand))',
};

/* ───────────── 环境卡 ───────────── */

function InstanceChip({ ins, idx, envId, enter, leaving, onGone }: {
  ins: Instance;
  idx: number;
  envId: string;
  enter: GhostItem<Instance>['enter'];
  leaving: boolean;
  onGone: () => void;
}) {
  const ref = useRef<HTMLSpanElement>(null);
  useGSAP(
    () => {
      if (!ref.current || qMode() === 'static') return;
      if (enter.mode === 'pop') gsap.from(ref.current, { scale: 0, duration: 0.6, ease: 'back.out(2.4)' });
      else if (enter.mode === 'stagger')
        gsap.from(ref.current, { scale: 0, autoAlpha: 0, duration: 0.5, delay: enterDelay() + 0.5 + enter.i * 0.05, ease: 'back.out(2)' });
    },
    { scope: ref },
  );
  useGSAP(
    () => {
      if (!leaving || !ref.current) return;
      gsap.to(ref.current, { scale: 0, autoAlpha: 0, duration: 0.25, ease: 'power2.in', onComplete: onGone });
    },
    { dependencies: [leaving], scope: ref },
  );
  return (
    <span ref={ref} className="chip" data-leaving={leaving || undefined}>
      <StatusDot status={ins.status} label={false} insId={ins.id} flyFromEnv={enter.mode === 'pop' ? envId : undefined} />#{idx + 1}
    </span>
  );
}

function EnvCard({ g, instances, loaded, busy, onNew, onDelete, onRelease }: {
  g: GhostItem<EnvironmentSummary>;
  instances: Instance[];
  loaded: boolean;
  busy: boolean;
  onNew: (envId: string, el: Element) => void;
  onDelete: (env: EnvironmentSummary) => void;
  onRelease: (key: string) => void;
}) {
  const env = g.item;
  const navigate = useNavigate();
  const li = useRef<HTMLLIElement>(null);
  const card = useRef<HTMLElement>(null);
  const edge = useRef<HTMLElement>(null);
  const run = env.runningInstances;
  const bad = instances.filter((i) => i.status === 'error' || i.status === 'crashed').length;
  const gi = envGradient(env.id);
  useTilt(card);

  // 入场（首批错峰上浮 / 运行期新增弹出）
  useGSAP(
    () => {
      const el = li.current;
      if (!el || qMode() === 'static') return;
      if (g.enter.mode === 'stagger')
        gsap.from(el, { autoAlpha: 0, y: 30, scale: 0.94, duration: 0.75, delay: enterDelay() + 0.3 + g.enter.i * 0.06, clearProps: 'transform,opacity,visibility' });
      else if (g.enter.mode === 'pop')
        gsap.from(el, { autoAlpha: 0, scale: 0.8, y: 30, duration: 0.8, ease: 'back.out(1.6)', clearProps: 'transform,opacity,visibility' });
    },
    { scope: li },
  );
  // 退场（数据已删除 → 幽灵播完退场再释放）
  useGSAP(
    () => {
      const el = li.current;
      if (!g.leaving || !el) return;
      if (qMode() === 'static') return onRelease(g.key);
      gsap.to(el, { autoAlpha: 0, scale: 0.88, y: 20, rotationX: -12, duration: 0.35, ease: 'power2.in', onComplete: () => onRelease(g.key) });
    },
    { dependencies: [g.leaving], scope: li },
  );
  // 运行中：描边光环旋转
  useGSAP(
    () => {
      const c = card.current;
      const e = edge.current;
      if (!c || !e) return;
      if (run > 0) {
        gsap.to(e, { autoAlpha: 1, duration: 0.7 });
        if (qMode() !== 'static') gsap.fromTo(c, { '--ang': '0deg' }, { '--ang': '360deg', duration: 7, repeat: -1, ease: 'none' });
      } else {
        gsap.to(e, { autoAlpha: 0, duration: 0.7 });
      }
    },
    { dependencies: [run > 0], scope: card },
  );
  // 运行数徽标弹跳
  const badge = useRef<HTMLSpanElement>(null);
  const prevRun = useRef(run);
  useLayoutEffect(() => {
    if (run > prevRun.current && badge.current && qMode() !== 'static')
      gsap.fromTo(badge.current, { scale: 0.8 }, { scale: 1, duration: 0.55, ease: 'back.out(3)' });
    prevRun.current = run;
  }, [run]);

  const chips = useGhostList(instances, (i) => i.id, loaded);

  const open = (e: RMouseEvent | RKeyboardEvent) => {
    if ((e.target as HTMLElement).closest('button')) return;
    if (g.leaving) return;
    const el = card.current;
    if (el) setPendingFlip(Flip.getState(el.querySelectorAll('[data-flip-id]')));
    navigate(`/environments/${env.id}`);
  };

  const total = env.totalInstances;
  const badgeCls = bad ? 'err' : run ? 'on' : '';
  const badgeText = bad ? `${bad} error` : run ? `${run} running` : total ? 'idle' : 'empty';

  return (
    <li ref={li} className="env-li" data-flip data-leaving={g.leaving || undefined}>
      <article
        ref={card}
        className={`env-card g${gi}`}
        role="link"
        tabIndex={0}
        onClick={open}
        onKeyDown={(e) => e.key === 'Enter' && open(e)}
      >
        <i className="edge" ref={edge} />
        <i className="shine" />
        <header className="env-h">
          <span className="tile" data-flip-id={`tile-${env.id}`}>
            {envInitial(env.name)}
          </span>
          <div className="env-t">
            <h3 data-flip-id={`name-${env.id}`}>{env.name}</h3>
            <p className="meta">
              {env.hostsSourceUrl ? 'hosts 已配置 · ' : ''}
              {total} instance{total === 1 ? '' : 's'}
            </p>
          </div>
          <span ref={badge} className={`badge ${badgeCls}`}>
            {badgeText}
          </span>
        </header>
        <div className="chips">
          {chips.list.length === 0 && (loaded || total === 0) && <span className="chip-empty">还没有实例 — 点击 New 启动一个</span>}
          {chips.list.map((c, i) => (
            <InstanceChip key={c.key} ins={c.item} idx={i} envId={env.id} enter={c.enter} leaving={c.leaving} onGone={() => chips.release(c.key)} />
          ))}
        </div>
        <footer className="env-f">
          <button className={`${btnMint} btn-xs`} disabled={busy} onClick={(e) => onNew(env.id, e.currentTarget)}>
            <Icon name="plus" /> {busy ? '…' : 'New'}
          </button>
          <button className={btnQuietDanger} title="删除环境" onClick={() => onDelete(env)}>
            删除
          </button>
        </footer>
      </article>
    </li>
  );
}

/* ───────────── Deck（卡片叠放，左右翻页） ───────────── */

function Deck({ envs }: { envs: EnvironmentSummary[] }) {
  const wrap = useRef<HTMLDivElement>(null);
  const [cur, setCur] = useState(0);
  const dir = useRef(1);
  const lastCur = useRef<number | null>(null);
  const n = envs.length;
  const idx = Math.min(cur, Math.max(0, n - 1));

  useGSAP(
    () => {
      const cards = gsap.utils.toArray<HTMLElement>('.dk', wrap.current!);
      if (!cards.length) return;
      const anim = lastCur.current !== null && lastCur.current !== idx && qMode() !== 'static';
      lastCur.current = idx;
      cards.forEach((c, i) => {
        const d = (i - idx + n) % n;
        const vars = { y: d * 17, scale: 1 - d * 0.065, autoAlpha: d > 2 ? 0 : 1 - d * 0.3, zIndex: 10 - d, rotationX: 0, x: 0, rotation: 0, duration: anim ? 0.6 : 0, ease: 'ch-out', overwrite: 'auto' as const };
        if (anim && d === (dir.current > 0 ? n - 1 : 0) && n > 1) {
          if (dir.current > 0) gsap.timeline().to(c, { x: -170, rotation: -10, autoAlpha: 0.5, duration: 0.26, ease: 'power2.in' }).set(c, { zIndex: 10 - d }).to(c, vars);
          else gsap.timeline().set(c, { x: 170, rotation: 10, zIndex: 10 }).to(c, { ...vars, autoAlpha: 1 });
        } else gsap.to(c, vars);
      });
    },
    { dependencies: [idx, n, envs.map((e) => e.id).join(',')], scope: wrap, revertOnUpdate: false },
  );

  const step = (d: number) => {
    if (n < 2) return;
    dir.current = d;
    setCur((idx + d + n) % n);
  };

  return (
    <div className="side-card rv">
      <p className="eyebrow">
        Your deck
        <span className="deck-nav">
          <button className="ib" aria-label="上一张" onClick={() => step(-1)}>
            <Icon name="back" />
          </button>
          <button className="ib" aria-label="下一张" style={{ transform: 'scaleX(-1)' }} onClick={() => step(1)}>
            <Icon name="back" />
          </button>
        </span>
      </p>
      <div className="deck-wrap" ref={wrap}>
        {envs.map((e) => (
          <div key={e.id} className={`dk g${envGradient(e.id)}`} data-env={e.id}>
            <div className="dk-top">
              <span className="dk-chip">ENVIRONMENT</span>
              <svg className="dk-logo" viewBox="0 0 24 24">
                <path d="M12 3a9 9 0 1 1 0 18a9 9 0 1 1 0-18" />
                <circle cx="12" cy="12" r="3" fill="#fff" stroke="none" />
              </svg>
            </div>
            <div className="dk-name">{e.name}</div>
            <div className="dk-row">
              <div>
                <small>RUNNING</small>
                <b>{String(e.runningInstances).padStart(2, '0')}</b>
              </div>
              <div>
                <small>TOTAL</small>
                <b>{String(e.totalInstances).padStart(2, '0')}</b>
              </div>
              <div>
                <small>HOSTS</small>
                <b>{e.hostsSourceUrl ? 'ON' : 'OFF'}</b>
              </div>
            </div>
          </div>
        ))}
      </div>
      <div className="deck-dots">
        {envs.map((e, i) => (
          <i key={e.id} className={i === idx ? 'on' : ''} />
        ))}
      </div>
    </div>
  );
}

/* ───────────── Live Feed ───────────── */

function FeedRow({ f, initial }: { f: FeedItem; initial: boolean }) {
  const ref = useRef<HTMLLIElement>(null);
  useGSAP(
    () => {
      if (!initial && ref.current && qMode() !== 'static') gsap.from(ref.current, { autoAlpha: 0, x: -24, duration: 0.6 });
    },
    { scope: ref },
  );
  return (
    <li ref={ref} data-flip>
      <i style={{ background: COLORS[f.kind] }} />
      <span>{f.text}</span>
      <time>{f.t}</time>
    </li>
  );
}

function FeedPane() {
  const items = useFeed((s) => s.items);
  const ul = useRef<HTMLUListElement>(null);
  const initial = useRef(new Set(items.map((i) => i.id)));
  useFlipOnChange(ul, items[0]?.id ?? '');
  useGSAP(
    () => {
      if (qMode() === 'static') return;
      gsap.from(ul.current!.children, { autoAlpha: 0, y: 16, stagger: 0.04, duration: 0.45, clearProps: 'transform,opacity,visibility' });
    },
    { scope: ul },
  );
  return (
    <ul className="feed" ref={ul}>
      {items.map((f) => (
        <FeedRow key={f.id} f={f} initial={initial.current.has(f.id)} />
      ))}
    </ul>
  );
}

/* ───────────── 页面 ───────────── */

export default function EnvironmentsPage() {
  const { envs, envsLoading, envsLoaded, instancesByEnv, kernel, fetchEnvs, fetchInstances, showToast } = useStore();
  const [creating, setCreating] = useState(false);
  const [form, setForm] = useState({ name: '', hostsSourceUrl: '' });
  const [submitting, setSubmitting] = useState(false);
  const [tab, setTab] = useState<'envs' | 'feed'>('envs');
  const [busyEnv, setBusyEnv] = useState<string | null>(null);
  const [quickEnv, setQuickEnv] = useState('');
  const root = useRef<HTMLDivElement>(null);
  const grid = useRef<HTMLUListElement>(null);
  usePageEnter(root);

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

  /** 新建实例（与 Popover / 详情页同一接口与提示语；v2 在环境卡与 Quick Launch 提供入口） */
  const launch = async (envId: string, el: Element) => {
    markLaunch(envId, el);
    setBusyEnv(envId);
    try {
      await api.createInstance(envId);
      showToast('ok', '实例已启动');
    } catch (e) {
      showToast('err', errorText(e));
    } finally {
      setBusyEnv(null);
      fetchEnvs();
      fetchInstances(envId);
    }
  };

  // ── 汇总（只读派生） ──
  const totalRun = envs.reduce((a, e) => a + e.runningInstances, 0);
  const totalIns = envs.reduce((a, e) => a + e.totalInstances, 0);
  const bad = Object.values(instancesByEnv).reduce((a, l) => a + l.filter((i) => i.status === 'error' || i.status === 'crashed').length, 0);
  const pct = totalIns ? Math.round((totalRun / totalIns) * 100) : 0;

  useEffect(() => {
    if (!envs.some((e) => e.id === quickEnv)) setQuickEnv(envs[0]?.id ?? '');
  }, [envs, quickEnv]);

  // Hero 环与百分比
  const ringP = useRef<SVGPathElement>(null);
  const pctEl = useRef<HTMLSpanElement>(null);
  const burstEl = useRef<HTMLElement>(null);
  const sheen = useRef<HTMLElement>(null);
  const firstHero = useRef(true);
  const prevRun = useRef(0);
  useGSAP(
    () => {
      const init = firstHero.current;
      firstHero.current = false;
      gsap.to(ringP.current, { drawSVG: `0% ${pct}%`, duration: init ? 1.3 : 0.9, delay: init ? enterDelay() + 0.3 : 0, ease: 'power2.inOut', overwrite: 'auto' });
      const o = { v: Number(pctEl.current?.textContent) || 0 };
      gsap.to(o, { v: pct, duration: 0.9, delay: init ? enterDelay() + 0.3 : 0, ease: 'power2.out', onUpdate: () => pctEl.current && (pctEl.current.textContent = String(Math.round(o.v))) });
      if (!init && pct === 100 && totalIns && qMode() !== 'static') gsap.fromTo(burstEl.current, { scale: 1, autoAlpha: 0.8 }, { scale: 1.4, autoAlpha: 0, duration: 0.9, ease: 'power2.out' });
      if (!init && totalRun > prevRun.current && qMode() === 'full') gsap.fromTo(sheen.current, { x: '-160%' }, { x: '520%', duration: 1.3, ease: 'power2.inOut' });
      prevRun.current = totalRun;
    },
    { dependencies: [pct, totalRun, totalIns], scope: root, revertOnUpdate: false },
  );

  const { list, release } = useGhostList(envs, (e) => e.id, envsLoaded);
  useFlipOnChange(grid, list.map((g) => g.key + (g.leaving ? '~' : '')).join(','));

  const pillOk = kernel?.installed;
  const major = kernel?.version?.split('.')[0];

  return (
    <div ref={root} className="mx-auto max-w-[1060px]">
      <PageHeader
        title="Environments"
        sub={`${envs.length} 个环境 · ${totalRun} 个运行中实例`}
        right={
          kernel && (
            <span className={`pill ${pillOk ? 'ok' : 'err'}`}>
              <i />
              {pillOk ? `Chrome for Testing${major ? ` ${major}` : ''} · ready` : 'Chrome 内核未安装'}
            </span>
          )
        }
      />

      <div className="ov-grid">
        <div className="min-w-0">
          <div className="hero rv">
            <i className="hero-sheen" ref={sheen} />
            <div className="relative z-[2]">
              <p className="eyebrow">Running instances</p>
              <div className="odo-row">
                <Odometer value={totalRun} />
                <span className="odo-sep">/</span>
                <Odometer value={totalIns} className="dim" />
              </div>
              <p className="hero-sub">{bad ? `⚠ ${bad} 个实例异常，请检查` : totalRun ? `${totalRun} 个实例在线，CDP 端口已就绪` : '暂无运行中的实例'}</p>
              <div className="hero-chips">
                <span>
                  Environments <b>{envs.length}</b>
                </span>
                <span>
                  Agent API <b>127.0.0.1:17890</b>
                </span>
              </div>
            </div>
            <div className="hero-ring relative z-[2]">
              <svg viewBox="0 0 128 128">
                <path className="t" d="M64 10a54 54 0 1 1 0 108a54 54 0 1 1 0-108" />
                <path className="p" ref={ringP} d="M64 10a54 54 0 1 1 0 108a54 54 0 1 1 0-108" />
              </svg>
              <i className="ring-burst" ref={burstEl} />
              <div className="ring-pct">
                <span>
                  <span ref={pctEl}>0</span>
                  <small>%</small>
                </span>
              </div>
            </div>
          </div>

          <div className="tabs-row rv">
            <Seg
              value={tab}
              onChange={setTab}
              options={[
                ['envs', 'Environments'],
                ['feed', 'Live Feed'],
              ]}
            />
            <button className={`${btnPrimary} btn-lg`} onClick={() => setCreating(true)}>
              <Icon name="plus" /> Add Environment
            </button>
          </div>

          {tab === 'feed' ? (
            <FeedPane />
          ) : envsLoading && envs.length === 0 ? (
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
            <ul className="env-grid" ref={grid}>
              {list.map((g) => (
                <EnvCard
                  key={g.key}
                  g={g}
                  instances={instancesByEnv[g.key] ?? []}
                  loaded={g.key in instancesByEnv}
                  busy={busyEnv === g.key}
                  onNew={launch}
                  onDelete={(e) => remove(e.id, e.name, e.runningInstances)}
                  onRelease={release}
                />
              ))}
            </ul>
          )}
        </div>

        <div className="col-r">
          <div className="side-card rv">
            <p className="eyebrow">Quick launch</p>
            <div className="select">
              <select value={quickEnv} onChange={(e) => setQuickEnv(e.target.value)} disabled={envs.length === 0}>
                {envs.map((e) => (
                  <option key={e.id} value={e.id}>
                    {e.name}
                  </option>
                ))}
              </select>
            </div>
            <button
              className={`${btnMint} btn-lg w-full`}
              disabled={!quickEnv || busyEnv === quickEnv}
              onClick={(e) => launch(quickEnv, e.currentTarget)}
            >
              <Icon name="plus" /> New Chrome
            </button>
            <p className="hint">新实例会以光点沿曲线飞入对应环境卡，并在启动成功时迸发粒子。</p>
          </div>
          {envs.length > 0 && <Deck envs={envs} />}
        </div>
      </div>

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
        <div className="mf">
          <button className={btnGhost} onClick={() => setCreating(false)}>
            取消
          </button>
          <button className={btnPrimary} disabled={!form.name.trim() || submitting} onClick={submit}>
            {submitting ? '创建中…' : '创建'}
          </button>
        </div>
      </Modal>
    </div>
  );
}
