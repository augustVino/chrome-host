import { useLayoutEffect, useRef } from 'react';
import type { InstanceStatus } from '../api/types';
import { gsap } from '../motion/gsap';
import { qMode } from '../motion/quality';
import { burst, centerOf, flyOrb, CORAL, GREY, MINT } from '../motion/fx';
import { takeLaunch } from '../motion/launch';

const RING = 'M11 2a9 9 0 1 1 0 18a9 9 0 1 1 0-18';

const LABEL: Record<string, string> = {
  running: '运行中',
  starting: '启动中',
  stopping: '停止中',
  stopped: '已停止',
  created: '已创建',
  error: '异常',
  crashed: '已崩溃',
};

type Parts = {
  st: HTMLElement;
  ring: SVGPathElement;
  track: SVGPathElement;
  chk: SVGPathElement;
  core: SVGCircleElement;
  orb: SVGCircleElement;
  pulse: HTMLElement;
  label: HTMLElement | null;
};

/**
 * 实例状态点（§5.5 生命周期编排）。
 * 仅在"上一状态 → 当前状态"确实变化时播放转场动画；首次挂载/轮询重渲染只做静态落位，
 * 因此 3s/2s 轮询不会重复触发特效。动效细节：
 *  starting  DrawSVG 弧线旋转 + 光点沿环公转
 *  running   环闭合 + 对勾描绘 + 薄荷粒子迸发 + 标签 Scramble
 *  stopped   环回缩 + 灰色粒子下坠
 *  error     摇晃 + 珊瑚脉冲 + 粒子
 */
export function StatusDot({
  status,
  className = '',
  label = true,
  insId,
  flyFromEnv,
}: {
  status: InstanceStatus | string;
  className?: string;
  /** 是否显示文字标签 */
  label?: boolean;
  insId?: string;
  /** 新实例挂载时若该环境刚点过 New，光点从按钮飞入此状态点 */
  flyFromEnv?: string;
}) {
  const norm = status in LABEL ? status : 'stopped';
  const root = useRef<HTMLSpanElement>(null);
  const prev = useRef<string | null>(null);
  const tl = useRef<gsap.core.Timeline | null>(null);
  const spinTl = useRef<gsap.core.Timeline | null>(null);
  const flew = useRef(false);

  useLayoutEffect(() => {
    const st = root.current;
    if (!st) return;
    const parts: Parts = {
      st,
      ring: st.querySelector('.r-ring')!,
      track: st.querySelector('.r-track')!,
      chk: st.querySelector('.r-check')!,
      core: st.querySelector('.r-core')!,
      orb: st.querySelector('.orb')!,
      pulse: st.querySelector('.sd-pulse')!,
      label: st.querySelector('.st-label'),
    };
    const from = prev.current;
    prev.current = norm;
    paint(parts, from, norm, tl, spinTl);

    // 新实例首次挂载为 starting：若刚点过 New，光点飞入
    if (from === null && norm === 'starting' && flyFromEnv && !flew.current) {
      flew.current = true;
      const origin = takeLaunch(flyFromEnv);
      if (origin) flyOrb(origin, () => st.querySelector('.sdot'));
    }
  }, [norm, flyFromEnv]);

  useLayoutEffect(
    () => () => {
      tl.current?.kill();
      spinTl.current?.kill();
    },
    [],
  );

  return (
    <span
      ref={root}
      className={`st s-${norm} ${className}`}
      data-ins={insId}
      data-status={norm}
      title={LABEL[norm]}
    >
      <span className="sdot">
        <svg viewBox="0 0 22 22">
          <path className="r-track" d={RING} />
          <path className="r-ring" d={RING} />
          <path className="r-check" d="M7 11.5l3 3 5-6" />
          <circle className="r-core" cx="11" cy="11" r="3" />
          <circle className="orb" r="1.7" cx="0" cy="0" />
        </svg>
        <i className="sd-pulse" />
      </span>
      {label && <span className="st-label" />}
    </span>
  );
}

function spin(p: Parts, spinTl: React.MutableRefObject<gsap.core.Timeline | null>) {
  const t = gsap.timeline();
  spinTl.current = t;
  t.to(p.ring, { rotation: '+=360', duration: 0.95, repeat: -1, ease: 'none', transformOrigin: '50% 50%' }, 0);
  t.to(p.core, { scale: 0.55, duration: 0.5, repeat: -1, yoyo: true, ease: 'sine.inOut', transformOrigin: '50% 50%' }, 0);
  if (qMode() !== 'static') {
    gsap.set(p.orb, { autoAlpha: 1 });
    t.to(p.orb, { motionPath: { path: p.track, align: p.track, alignOrigin: [0.5, 0.5] }, duration: 1.5, repeat: -1, ease: 'none' }, 0);
  }
}

function paint(
  p: Parts,
  from: string | null,
  to: string,
  tlRef: React.MutableRefObject<gsap.core.Timeline | null>,
  spinRef: React.MutableRefObject<gsap.core.Timeline | null>,
) {
  const { ring, chk, core, orb, pulse, label, st } = p;
  const r0 = (gsap.getProperty(ring, 'rotation') as number) || 0;
  tlRef.current?.kill();
  spinRef.current?.kill();
  tlRef.current = spinRef.current = null;

  const full = to === 'running' || to === 'error' || to === 'crashed';
  const busy = to === 'starting' || to === 'stopping';
  const live = from !== null && from !== to && qMode() !== 'static' && st.getBoundingClientRect().width > 0;

  gsap.set(ring, { transformOrigin: '50% 50%' });
  gsap.set(core, { transformOrigin: '50% 50%' });

  const text = LABEL[to];
  if (!live) {
    gsap.set(ring, { rotation: 0, drawSVG: full ? '0% 100%' : busy ? '0% 28%' : '0%' });
    gsap.set(core, { scale: 1 });
    gsap.set(chk, { autoAlpha: 0 });
    gsap.set(orb, { autoAlpha: 0 });
    if (label) label.textContent = text;
    if (busy) spin(p, spinRef);
    return;
  }

  if (label) gsap.to(label, { duration: 0.7, scrambleText: { text, chars: 'lowerCase', speed: 0.8 } });
  const t = gsap.timeline();
  tlRef.current = t;
  const { x, y } = centerOf(p.st.querySelector('.sdot')!);
  gsap.set(orb, { autoAlpha: 0 });

  if (busy) {
    t.to(ring, { drawSVG: '0% 28%', duration: 0.35 }, 0);
    t.add(() => spin(p, spinRef), 0.1);
  } else if (to === 'running') {
    t.to(ring, { rotation: Math.ceil(r0 / 360) * 360, duration: 0.32, ease: 'power2.out' }, 0)
      .to(ring, { drawSVG: '0% 100%', duration: 0.55, ease: 'power2.inOut' }, 0)
      .set(ring, { rotation: 0 })
      .fromTo(chk, { drawSVG: '0%', autoAlpha: 1 }, { drawSVG: '100%', duration: 0.32, ease: 'power2.out' }, 0.5)
      .to(chk, { autoAlpha: 0, duration: 0.35, delay: 0.45 }, '>')
      .fromTo(core, { scale: 0 }, { scale: 1, duration: 0.55, ease: 'back.out(3)' }, 0.5)
      .fromTo(pulse, { scale: 1, autoAlpha: 0.8 }, { scale: 2.8, autoAlpha: 0, duration: 0.85, ease: 'power2.out' }, 0.5)
      .add(() => burst(x, y, MINT, { n: 14 }), 0.5);
  } else if (to === 'stopped' || to === 'created') {
    t.to(ring, { drawSVG: '0%', duration: 0.5, ease: 'power2.inOut' }, 0)
      .fromTo(core, { scale: 1.6 }, { scale: 1, duration: 0.5 }, 0)
      .add(() => burst(x, y, GREY, { n: 6, gravity: 760, up: false, size: [2, 4] }), 0.1);
  } else {
    // error / crashed
    t.to(ring, { drawSVG: '0% 100%', duration: 0.4 }, 0)
      .fromTo(st, { x: 0 }, { x: 9, duration: 0.8, ease: 'ch-wiggle', clearProps: 'transform' }, 0)
      .fromTo(pulse, { scale: 1, autoAlpha: 0.9 }, { scale: 3, autoAlpha: 0, duration: 0.9, ease: 'power2.out' }, 0)
      .add(() => burst(x, y, CORAL, { n: 10, gravity: 380 }), 0.05);
  }
}
