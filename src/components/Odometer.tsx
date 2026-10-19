import { memo, useLayoutEffect, useRef } from 'react';
import { gsap } from '../motion/gsap';
import { qMode } from '../motion/quality';

/** 数字滚轮：每位一列 0-9，值变化时各列错峰滚动到目标位（§5.3）。首次挂载静态落位。 */
export const Odometer = memo(function Odometer({ value, digits = 2, className = '' }: { value: number; digits?: number; className?: string }) {
  const cols = useRef<(HTMLSpanElement | null)[]>([]);
  const first = useRef(true);
  const text = String(Math.max(0, Math.min(value, 10 ** digits - 1))).padStart(digits, '0');

  useLayoutEffect(() => {
    const animate = !first.current && qMode() !== 'static';
    first.current = false;
    cols.current.forEach((c, i) => {
      if (!c) return;
      gsap.to(c, {
        yPercent: -10 * +text[i],
        duration: animate ? 0.95 : 0,
        delay: animate ? i * 0.05 : 0,
        ease: 'ch-out',
        overwrite: true,
      });
    });
  }, [text]);

  return (
    <span className={`odo ${className}`} aria-label={String(value)}>
      {Array.from({ length: digits }, (_, i) => (
        <span className="od" key={i} aria-hidden>
          <span className="odc" ref={(el) => (cols.current[i] = el)}>
            {[0, 1, 2, 3, 4, 5, 6, 7, 8, 9].map((n) => (
              <i key={n}>{n}</i>
            ))}
          </span>
        </span>
      ))}
    </span>
  );
});
