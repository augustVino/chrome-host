import { useRef } from 'react';
import { gsap, useGSAP } from '../motion/gsap';
import { AMB, qMode } from '../motion/quality';
import { useStore } from '../store';

/**
 * 极光背景：三层 blob（漂移走统一 AMB 时间线，失焦/隐藏/static 时暂停）+ 鼠标视差 + 噪点。
 * 数据驱动：运行实例占比 → 薄荷光强度；存在异常实例 → 珊瑚红光（经 CSS 变量作用于父 .shell）。
 * 纯展示层：只读 store，不改任何数据。
 */
export function Aurora() {
  const ref = useRef<HTMLDivElement>(null);
  const envs = useStore((s) => s.envs);
  const instancesByEnv = useStore((s) => s.instancesByEnv);

  useGSAP(
    () => {
      const root = ref.current;
      if (!root) return;
      const rnd = gsap.utils.random;
      root.querySelectorAll('.blob').forEach((b) => {
        AMB.to(
          b,
          {
            x: () => rnd(-60, 60),
            y: () => rnd(-50, 50),
            scale: () => rnd(0.92, 1.12),
            duration: rnd(14, 26),
            ease: 'sine.inOut',
            repeat: -1,
            yoyo: true,
            repeatRefresh: true,
          },
          rnd(0, 6),
        );
      });
      const layers = (['#L1', '#L2', '#L3'] as const).map((sel, i) => {
        const el = root.querySelector(sel);
        const k = [0.012, 0.022, 0.034][i];
        return {
          x: gsap.quickTo(el, 'x', { duration: 1.2, ease: 'power3' }),
          y: gsap.quickTo(el, 'y', { duration: 1.2, ease: 'power3' }),
          k,
        };
      });
      const move = (e: PointerEvent) => {
        if (qMode() !== 'full') return;
        const nx = e.clientX - innerWidth / 2;
        const ny = e.clientY - innerHeight / 2;
        layers.forEach((l) => {
          l.x(nx * l.k);
          l.y(ny * l.k);
        });
      };
      addEventListener('pointermove', move);
      return () => removeEventListener('pointermove', move);
    },
    { scope: ref },
  );

  // 数据驱动：作用于 .shell（父元素）上的 --a-mint / --a-coral
  useGSAP(
    () => {
      const shell = ref.current?.parentElement;
      if (!shell) return;
      const total = envs.reduce((a, e) => a + e.totalInstances, 0);
      const run = envs.reduce((a, e) => a + e.runningInstances, 0);
      const bad = Object.values(instancesByEnv).some((l) => l.some((i) => i.status === 'error' || i.status === 'crashed'));
      const ratio = total ? run / total : 0;
      gsap.to(shell, {
        '--a-mint': 0.18 + 0.72 * ratio,
        '--a-coral': bad ? 0.75 : 0,
        duration: 1.2,
        ease: 'power2.out',
        overwrite: 'auto',
      });
    },
    { dependencies: [envs, instancesByEnv] },
  );

  return (
    <>
      <div className="aurora" ref={ref} data-aurora>
        <div className="aur-layer" id="L1">
          <i className="blob b-brand" />
          <i className="blob b-sky" />
        </div>
        <div className="aur-layer" id="L2">
          <i className="blob b-peach" />
          <i className="blob b-mint" />
        </div>
        <div className="aur-layer" id="L3">
          <i className="blob b-brand2" />
          <i className="blob b-coral" />
        </div>
      </div>
      <div className="grain" />
    </>
  );
}
