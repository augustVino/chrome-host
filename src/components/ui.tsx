import type { ReactNode } from 'react';
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { gsap, useGSAP } from '../motion/gsap';
import { qMode } from '../motion/quality';
import { Icon } from './icons';

export { StatusDot } from './StatusDot';

/**
 * 动画弹层壳（Modal 与 ConfirmDialog 共用，§5.8）：
 * 遮罩淡入 + 渐进模糊（--vb，仅 full 档）→ 面板 3D 翻入（back.out）→ 内容错峰；关闭时时间线倒放。
 */
export function ModalShell({
  open,
  onBackdrop,
  children,
  width,
}: {
  open: boolean;
  onBackdrop: () => void;
  children: ReactNode;
  width?: number;
}) {
  const [mounted, setMounted] = useState(open);
  const root = useRef<HTMLDivElement>(null);
  const tl = useRef<gsap.core.Timeline | null>(null);

  useEffect(() => {
    if (open) setMounted(true);
  }, [open]);

  // 入场
  useGSAP(
    () => {
      if (!open || !mounted || !root.current) return;
      const el = root.current;
      const mode = qMode();
      const t = gsap.timeline();
      t.fromTo(
        el.querySelector('.veil'),
        { autoAlpha: 0, '--vb': 0 },
        { autoAlpha: 1, '--vb': mode === 'full' ? 14 : 0, duration: 0.4, ease: 'power2.out' },
        0,
      ).fromTo(
        el.querySelector('.panel'),
        { autoAlpha: 0, y: 26, scale: 0.92, rotationX: -9, transformOrigin: '50% 100%' },
        { autoAlpha: 1, y: 0, scale: 1, rotationX: 0, duration: 0.7, ease: 'back.out(1.6)' },
        0.04,
      );
      const inner = el.querySelectorAll('.panel > *');
      if (inner.length)
        t.from(inner, { y: 12, autoAlpha: 0, duration: 0.4, stagger: 0.05, clearProps: 'transform,opacity,visibility' }, 0.18);
      const strokes = el.querySelectorAll('.m-ic path');
      if (strokes.length) t.fromTo(strokes, { drawSVG: '0%' }, { drawSVG: '100%', duration: 0.6 }, 0.25);
      if (mode === 'static') t.timeScale(4);
      tl.current = t;
    },
    { dependencies: [open, mounted], scope: root, revertOnUpdate: false },
  );

  // 退场：倒放入场时间线，结束后卸载
  useLayoutEffect(() => {
    if (open || !mounted) return;
    const t = tl.current;
    if (!t) {
      setMounted(false);
      return;
    }
    t.eventCallback('onReverseComplete', () => setMounted(false));
    t.timeScale(qMode() === 'static' ? 6 : 1.7).reverse();
  }, [open, mounted]);

  if (!mounted) return null;
  return (
    <div ref={root} className="modal" onClick={onBackdrop}>
      <div className="veil" />
      <div className="panel" style={width ? { width } : undefined} onClick={(e) => e.stopPropagation()}>
        {children}
      </div>
    </div>
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

  return (
    <ModalShell open={open} onBackdrop={onClose}>
      <h2>{title}</h2>
      <div className="mt-3">{children}</div>
    </ModalShell>
  );
}

export function Field({ label, children, hint }: { label: string; children: ReactNode; hint?: string }) {
  return (
    <label className="mb-3 block">
      <span className="mb-1.5 block text-[11.5px] font-bold text-ink-2">{label}</span>
      {children}
      {hint && <span className="mt-1.5 block text-[11px] leading-4 text-ink-3">{hint}</span>}
    </label>
  );
}

export const inputCls = 'input';

/** 空态：描边环 + 轨道光点循环（SVG 内联，无外部资源） */
export function EmptyState({ title, desc, action }: { title: string; desc: string; action?: ReactNode }) {
  const ref = useRef<SVGSVGElement>(null);
  useGSAP(
    () => {
      const svg = ref.current;
      if (!svg || qMode() === 'static') return;
      const ring = svg.querySelector('.e-ring');
      const orb = svg.querySelector('.e-orb');
      gsap.fromTo(ring, { drawSVG: '0%' }, { drawSVG: '100%', duration: 1.2, ease: 'power2.inOut' });
      gsap.to(orb, {
        motionPath: { path: ring as SVGPathElement, align: ring as SVGPathElement, alignOrigin: [0.5, 0.5] },
        duration: 6,
        repeat: -1,
        ease: 'none',
      });
    },
    { scope: ref },
  );
  return (
    <div className="empty rv">
      <svg ref={ref} viewBox="0 0 120 120" aria-hidden>
        <path className="e-ring" d="M60 20a40 40 0 1 1 0 80a40 40 0 1 1 0-80" />
        <circle cx="60" cy="60" r="14" opacity=".35" />
        <circle className="e-orb" r="4.5" cx="0" cy="0" style={{ fill: 'rgb(var(--mint))', stroke: 'none' }} />
      </svg>
      <b>{title}</b>
      <p className="mt-1.5 max-w-xs text-xs text-ink-3">{desc}</p>
      {action && <div className="mt-5">{action}</div>}
    </div>
  );
}

/** 页头：大标题（入场 SplitText 行遮罩）+ 副标题 + 右侧操作区 */
export function PageHeader({ title, sub, right }: { title: string; sub?: ReactNode; right?: ReactNode }) {
  return (
    <header className="ph rv">
      <div className="min-w-0">
        <h1 className="title">{title}</h1>
        {sub != null && <p className="sub">{sub}</p>}
      </div>
      {right && <div className="ph-r">{right}</div>}
    </header>
  );
}

/** 果冻开关：旋钮 back.out 过冲 + 挤压（static 档瞬时） */
export function Switch({
  checked,
  onClick,
  disabled,
  label,
}: {
  checked: boolean;
  onClick: () => void;
  disabled?: boolean;
  label?: string;
}) {
  const knob = useRef<HTMLElement>(null);
  const first = useRef(true);
  useLayoutEffect(() => {
    const k = knob.current;
    if (!k) return;
    const x = checked ? 18 : 0;
    if (first.current || qMode() === 'static') {
      gsap.set(k, { x });
    } else {
      gsap
        .timeline()
        .to(k, { x, duration: 0.55, ease: 'back.out(2.2)' }, 0)
        .to(k, { scaleX: 1.3, duration: 0.12, yoyo: true, repeat: 1 }, 0);
    }
    first.current = false;
  }, [checked]);
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      className={`sw ${checked ? 'on' : ''} disabled:opacity-50`}
      onClick={onClick}
    >
      <i ref={knob} />
    </button>
  );
}

/** 分段控件：滑动指示器 */
export function Seg<T extends string>({
  value,
  options,
  onChange,
  className = '',
}: {
  value: T;
  options: readonly (readonly [T, string])[];
  onChange: (v: T) => void;
  className?: string;
}) {
  const wrap = useRef<HTMLDivElement>(null);
  const first = useRef(true);
  const move = useCallback(() => {
    const w = wrap.current;
    const on = w?.querySelector<HTMLElement>('button.on');
    const ind = w?.querySelector('.seg-ind');
    if (!on || !ind) return;
    gsap.to(ind, {
      x: on.offsetLeft,
      width: on.offsetWidth,
      duration: first.current || qMode() === 'static' ? 0 : 0.55,
      ease: 'back.out(1.4)',
      overwrite: 'auto',
    });
    first.current = false;
  }, []);
  useLayoutEffect(move, [value, move, options]);
  useEffect(() => {
    // 字体加载完成后宽度会变化，重新对齐
    document.fonts?.ready.then(() => {
      first.current = true;
      move();
    });
  }, [move]);
  return (
    <div className={`seg ${className}`} ref={wrap}>
      <i className="seg-ind" />
      {options.map(([v, label]) => (
        <button key={v} type="button" className={v === value ? 'on' : ''} onClick={() => onChange(v)}>
          {label}
        </button>
      ))}
    </div>
  );
}

export { Icon };

export const btn = 'btn';
export const btnPrimary = 'btn btn-brand';
/** 行内小号幽灵按钮（列表行操作） */
export const btnGhostSm = 'btn btn-ghost btn-xs';
export const btnGhost = 'btn btn-ghost';
export const btnMint = 'btn btn-mint';
export const btnDanger = 'btn btn-danger';
/** 低强调破坏性文字按钮（删除环境/移除扩展等） */
export const btnQuietDanger = 'btn btn-quiet-danger btn-xs';
