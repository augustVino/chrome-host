/**
 * 画质档位 / 自适应治理（规范见 DESIGN.md §3.2）。
 *  - full   全部动效（极光漂移、鼠标视差、3D 倾斜、粒子、模糊遮罩）
 *  - lite   极光减速、关闭视差/倾斜/粒子，保留基础转场
 *  - static 无环境动画、无 backdrop-filter，转场瞬时（prefers-reduced-motion 默认落在此档）
 * 偏好：auto（默认，按帧率自动降级）/ full / lite / static，持久化到 localStorage（纯前端偏好）。
 */
import { create } from 'zustand';
import { gsap } from './gsap';

export type QualityMode = 'full' | 'lite' | 'static';
export type QualityPref = 'auto' | QualityMode;

const KEY = 'ch-motion-pref';

const reduced = () =>
  typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;

function readPref(): QualityPref {
  try {
    const v = localStorage.getItem(KEY);
    if (v === 'auto' || v === 'full' || v === 'lite' || v === 'static') return v;
  } catch {
    /* localStorage 不可用时回落默认 */
  }
  return 'auto';
}

interface QualityStore {
  pref: QualityPref;
  mode: QualityMode;
  setPref: (p: QualityPref) => void;
  /** 治理器内部使用：自动降级到某档（不改偏好） */
  degrade: (m: QualityMode) => void;
}

const initialPref = readPref();
const initialMode: QualityMode = reduced() ? 'static' : initialPref === 'auto' ? 'full' : initialPref;

export const useQuality = create<QualityStore>((set) => ({
  pref: initialPref,
  mode: initialMode,
  setPref: (pref) => {
    try {
      localStorage.setItem(KEY, pref);
    } catch {
      /* 忽略 */
    }
    set({ pref, mode: reduced() ? 'static' : pref === 'auto' ? 'full' : pref });
  },
  degrade: (mode) => set({ mode }),
}));

/** 非 React 场景读取当前档位（事件回调 / 粒子工具里用） */
export const qMode = (): QualityMode => useQuality.getState().mode;

/** 氛围层（极光漂移）统一时间线：失焦 / 隐藏 / static 时暂停，省电 */
export const AMB = gsap.timeline();

function applyMode(m: QualityMode) {
  document.documentElement.dataset.q = m;
  if (m === 'static') AMB.pause();
  else if (document.hasFocus() && !document.hidden) AMB.resume();
  AMB.timeScale(m === 'lite' ? 0.5 : 1);
}

let started = false;

/** 应用入口调用一次：绑定档位 → DOM、失焦暂停、FPS 监测与自动降级 */
export function startQualityGovernor(): void {
  if (started) return;
  started = true;
  applyMode(useQuality.getState().mode);
  useQuality.subscribe((s, p) => {
    if (s.mode !== p.mode) applyMode(s.mode);
  });

  const pause = () => AMB.pause();
  const resume = () => {
    if (qMode() !== 'static') AMB.resume();
  };
  addEventListener('blur', pause);
  addEventListener('focus', resume);
  document.addEventListener('visibilitychange', () => (document.hidden ? pause() : resume()));
  matchMedia?.('(prefers-reduced-motion: reduce)').addEventListener?.('change', () =>
    useQuality.getState().setPref(useQuality.getState().pref),
  );

  // FPS 监测：full 档连续 3 次（每次 1.2s）均帧间隔 > 27ms（<37fps）→ 降到 lite
  const ft: number[] = [];
  let last = performance.now();
  let bad = 0;
  gsap.ticker.add(() => {
    const n = performance.now();
    const dt = n - last;
    last = n;
    if (document.hidden || dt > 250) return;
    ft.push(dt);
    if (ft.length > 90) ft.shift();
  });
  setInterval(() => {
    const { pref, mode, degrade } = useQuality.getState();
    if (!ft.length || pref !== 'auto' || mode !== 'full' || !document.hasFocus()) {
      bad = 0;
      return;
    }
    const avg = ft.reduce((a, b) => a + b, 0) / ft.length;
    if (avg > 27) {
      if (++bad >= 3) degrade('lite');
    } else bad = 0;
  }, 1200);
}
