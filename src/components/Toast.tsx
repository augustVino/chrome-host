import { useLayoutEffect, useRef, useState } from 'react';
import { useStore } from '../store';
import { gsap } from '../motion/gsap';
import { qMode } from '../motion/quality';

type T = { kind: 'ok' | 'err'; text: string };

/**
 * Toast（§5.8）：底部弹性落入 + 倒计时进度条 + 错误态摇晃；store 清空 toast 时播放退场。
 * 数据源与时序（3.2s 自动清除）仍由 store.showToast 持有，这里只负责呈现。
 */
export function Toast({ compact = false }: { compact?: boolean }) {
  const toast = useStore((s) => s.toast);
  const last = useRef<T | null>(toast);
  const seq = useRef(0);
  const [leaving, setLeaving] = useState<T | null>(null);
  const el = useRef<HTMLDivElement>(null);

  if (toast && toast !== last.current) seq.current += 1;

  useLayoutEffect(() => {
    if (toast) {
      last.current = toast;
      setLeaving(null);
    } else if (last.current) {
      setLeaving(last.current);
      last.current = null;
    }
  }, [toast]);

  // 入场（gsap.context：StrictMode 下 revert 还原，避免 from() 把中间态当终态）
  useLayoutEffect(() => {
    const node = el.current;
    if (!toast || !node) return;
    const ctx = gsap.context(() => {
      const mode = qMode();
      const tl = gsap.timeline();
      tl.fromTo(node, { autoAlpha: 0 }, { autoAlpha: 1, duration: 0.3, ease: 'power1.out' }, 0)
        .from(node, { y: compact ? 40 : 70, duration: 1, ease: mode === 'static' ? 'power2.out' : 'ch-bounce' }, 0)
        .fromTo(node.querySelector('.tp'), { scaleX: 1 }, { scaleX: 0, duration: 3.1, ease: 'none' }, 0.1);
      if (toast.kind === 'err' && mode !== 'static') tl.fromTo(node, { x: 0 }, { x: 8, duration: 0.7, ease: 'ch-wiggle' }, 0.15);
    });
    return () => ctx.revert();
  }, [toast, compact]);

  // 退场
  useLayoutEffect(() => {
    const node = el.current;
    if (toast || !leaving || !node) return;
    const tw = gsap.to(node, { y: 20, autoAlpha: 0, duration: 0.3, ease: 'power2.in', onComplete: () => setLeaving(null) });
    return () => {
      tw.kill();
    };
  }, [toast, leaving]);

  const cur = toast ?? leaving;
  if (!cur) return null;
  return (
    <div className="toast-host" style={compact ? { bottom: 14 } : undefined}>
      <div ref={el} key={seq.current} className={`toast ${cur.kind}`} style={compact ? { fontSize: 11.5, padding: '9px 16px 11px' } : undefined}>
        <span>{cur.kind === 'err' ? '✕' : '✓'}</span>
        {cur.text}
        <i className="tp" />
      </div>
    </div>
  );
}
