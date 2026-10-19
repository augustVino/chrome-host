import { useCallback, useLayoutEffect, useRef, useState, type RefObject } from 'react';
import { gsap, Flip, SplitText, useGSAP } from './gsap';
import { qMode } from './quality';

/** 应用启动时刻：首屏页面/卡片的入场需等序曲把内容区显出来（≈0.95s）再开始 */
const APP_START = performance.now();
export function enterDelay(): number {
  if (qMode() === 'static') return 0;
  const age = (performance.now() - APP_START) / 1000;
  return age < 1.5 ? Math.max(0, 0.95 - age) : 0;
}

/**
 * 页面入场编排（§5.6）：标题 SplitText 行遮罩上滑 + .rv 元素错峰上浮 + .row 列表左入。
 * 约定：想参与入场的元素加 className="rv"；列表行加 "row-in"。
 */
export function usePageEnter(scope: RefObject<HTMLElement>, opts: { skipTitle?: boolean } = {}) {
  useGSAP(
    () => {
      const root = scope.current;
      if (!root) return;
      const mode = qMode();
      const tl = gsap.timeline({ delay: enterDelay() });
      const title = root.querySelector('.title');
      if (title && !opts.skipTitle && mode !== 'static') {
        const sp = SplitText.create(title, { type: 'lines', mask: 'lines' });
        tl.from(sp.lines, { yPercent: 115, duration: 0.8, stagger: 0.08, onComplete: () => sp.revert() }, 0);
      }
      const rv = root.querySelectorAll('.rv');
      if (rv.length) {
        tl.fromTo(
          rv,
          { autoAlpha: 0, y: 18 },
          { autoAlpha: 1, y: 0, duration: 0.7, stagger: 0.07, clearProps: 'transform,opacity,visibility' },
          0.08,
        );
      }
      const rows = root.querySelectorAll('.row-in');
      if (rows.length) {
        tl.from(
          rows,
          { autoAlpha: 0, x: -26, duration: 0.6, stagger: 0.05, clearProps: 'transform,opacity,visibility' },
          0.25,
        );
      }
      if (mode === 'static') tl.timeScale(4);
    },
    { scope },
  );
}

/** 3D 倾斜 + 高光跟随（仅 full 档生效；pointer 事件，无 rAF 自旋） */
export function useTilt(ref: RefObject<HTMLElement>) {
  useLayoutEffect(() => {
    const card = ref.current;
    if (!card) return;
    gsap.set(card, { transformPerspective: 900 });
    const rx = gsap.quickTo(card, 'rotationX', { duration: 0.55, ease: 'power3' });
    const ry = gsap.quickTo(card, 'rotationY', { duration: 0.55, ease: 'power3' });
    const mapX = gsap.utils.mapRange(0, 1, -7, 7);
    const mapY = gsap.utils.mapRange(0, 1, 7, -7);
    const move = (e: PointerEvent) => {
      if (qMode() !== 'full') return;
      const r = card.getBoundingClientRect();
      const px = (e.clientX - r.left) / r.width;
      const py = (e.clientY - r.top) / r.height;
      ry(mapX(px));
      rx(mapY(py));
      card.style.setProperty('--mx', `${px * 100}%`);
      card.style.setProperty('--my', `${py * 100}%`);
    };
    const enter = () => qMode() === 'full' && gsap.to(card, { z: 22, duration: 0.4, overwrite: 'auto' });
    const leave = () => {
      rx(0);
      ry(0);
      gsap.to(card, { z: 0, duration: 0.5, overwrite: 'auto' });
    };
    card.addEventListener('pointermove', move);
    card.addEventListener('pointerenter', enter);
    card.addEventListener('pointerleave', leave);
    return () => {
      card.removeEventListener('pointermove', move);
      card.removeEventListener('pointerenter', enter);
      card.removeEventListener('pointerleave', leave);
      gsap.killTweensOf(card);
    };
  }, [ref]);
}

/**
 * 子元素顺序/数量变化时用 Flip 平滑重排。
 * 渲染期（DOM 仍是旧布局）抓取状态，提交后（layout effect）播放；
 * 标记 data-leaving 的元素不参与（它们即将被移除）。
 */
export function useFlipOnChange(ref: RefObject<HTMLElement>, sig: string, selector = '[data-flip]:not([data-leaving])') {
  const committed = useRef(sig);
  const pending = useRef<ReturnType<typeof Flip.getState> | null>(null);
  if (sig !== committed.current && ref.current && !pending.current && qMode() !== 'static') {
    pending.current = Flip.getState(ref.current.querySelectorAll(selector));
  }
  useLayoutEffect(() => {
    committed.current = sig;
    const st = pending.current;
    pending.current = null;
    if (st) Flip.from(st, { duration: 0.55, ease: 'ch-out' });
  }, [sig]);
}

export interface GhostItem<T> {
  key: string;
  item: T;
  /** 已从数据源移除，正在播放退场动画（播完调用 release） */
  leaving: boolean;
  /** 入场方式：stagger = 页面首批（带序号错峰）；pop = 运行期新增；none = 不播 */
  enter: { mode: 'stagger'; i: number } | { mode: 'pop' } | { mode: 'none' };
}

/**
 * 列表"幽灵"退场：数据里消失的项继续渲染（leaving=true）直到卡片播完退场动画并 release。
 * ready=false 时（首次拉取未完成）不做入场区分；首次 ready 的那一批算"首批"。
 */
export function useGhostList<T>(items: T[], getKey: (t: T) => string, ready: boolean) {
  const ghosts = useRef(new Map<string, { item: T; index: number }>());
  const prev = useRef<{ key: string; item: T }[]>([]);
  const seen = useRef(new Set<string>());
  const initialized = useRef(false);
  const [, bump] = useState(0);

  const cur = items.map((t) => ({ key: getKey(t), item: t }));
  const curKeys = new Set(cur.map((c) => c.key));
  // 渲染期推导（幂等）：上一帧存在、这一帧没了 → 进入幽灵表；又回来了 → 移出
  prev.current.forEach((p, index) => {
    if (!curKeys.has(p.key) && !ghosts.current.has(p.key)) ghosts.current.set(p.key, { item: p.item, index });
  });
  ghosts.current.forEach((_, k) => {
    if (curKeys.has(k)) ghosts.current.delete(k);
  });
  const firstBatch = ready && !initialized.current;

  const list: GhostItem<T>[] = cur.map((c, i) => ({
    key: c.key,
    item: c.item,
    leaving: false,
    enter: !ready || seen.current.has(c.key) ? { mode: 'none' } : firstBatch ? { mode: 'stagger', i } : { mode: 'pop' },
  }));
  ghosts.current.forEach((g, key) => {
    list.splice(Math.min(g.index, list.length), 0, { key, item: g.item, leaving: true, enter: { mode: 'none' } });
  });

  useLayoutEffect(() => {
    prev.current = cur;
    if (ready) {
      initialized.current = true;
      cur.forEach((c) => seen.current.add(c.key));
    }
  });

  const release = useCallback((key: string) => {
    if (ghosts.current.delete(key)) bump((n) => n + 1);
  }, []);

  return { list, release };
}
