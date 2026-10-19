import { useRef } from 'react';
import { NavLink } from 'react-router-dom';
import { useStore } from '../store';
import { AMB, qMode } from '../motion/quality';
import { gsap, useGSAP } from '../motion/gsap';

/**
 * 侧栏下方的欢迎卡：轻量插图 + 欢迎语 + 状态摘要 + 引导入口（纯展示，数据只读自 store）。
 * 插图的漂浮动画挂在 AMB 常驻时间线上（失焦 / static 自动暂停或跳过）。
 */
export function SideHello() {
  const root = useRef<HTMLDivElement>(null);
  const running = useStore((s) => s.envs.reduce((a, e) => a + e.runningInstances, 0));

  useGSAP(
    () => {
      if (qMode() === 'static') return;
      AMB.to('.sh-win', { y: -5, duration: 3.2, ease: 'sine.inOut', yoyo: true, repeat: -1 }, 0);
      AMB.to('.sh-o1', { y: 6, x: -2, duration: 2.6, ease: 'sine.inOut', yoyo: true, repeat: -1 }, 0.4);
      AMB.to('.sh-o2', { y: -6, x: 2, duration: 3.6, ease: 'sine.inOut', yoyo: true, repeat: -1 }, 0.9);
      gsap.to('.sh-ring', { rotation: 360, svgOrigin: '80 62', duration: 14, ease: 'none', repeat: -1 });
    },
    { scope: root },
  );

  return (
    <div className="side-hello" ref={root}>
      <svg viewBox="0 0 160 112" aria-hidden>
        <defs>
          <linearGradient id="sh-g" x1="0" y1="0" x2="1" y2="1">
            <stop offset="0" style={{ stopColor: 'rgb(var(--brand))' }} />
            <stop offset="1" style={{ stopColor: 'rgb(var(--brand-2))' }} />
          </linearGradient>
          <linearGradient id="sh-m" x1="0" y1="0" x2="1" y2="1">
            <stop offset="0" style={{ stopColor: 'rgb(var(--mint))' }} />
            <stop offset="1" style={{ stopColor: 'rgb(var(--sky))' }} />
          </linearGradient>
        </defs>
        <ellipse cx="80" cy="102" rx="46" ry="5" style={{ fill: 'rgb(var(--brand) / 0.16)' }} />
        <g className="sh-win">
          <rect x="28" y="16" width="104" height="76" rx="14" style={{ fill: 'var(--glass-2)', stroke: 'rgb(var(--brand) / 0.35)' }} strokeWidth="1.2" />
          <circle cx="42" cy="29" r="2.6" style={{ fill: 'rgb(var(--coral))' }} />
          <circle cx="51" cy="29" r="2.6" style={{ fill: 'rgb(var(--amber))' }} />
          <circle cx="60" cy="29" r="2.6" style={{ fill: 'rgb(var(--mint))' }} />
          <line x1="28" y1="40" x2="132" y2="40" style={{ stroke: 'rgb(var(--brand) / 0.18)' }} strokeWidth="1" />
          <circle cx="80" cy="62" r="15" fill="none" style={{ stroke: 'rgb(var(--brand) / 0.18)' }} strokeWidth="5" />
          <path
            className="sh-ring"
            d="M80 47a15 15 0 0 1 15 15"
            fill="none"
            stroke="url(#sh-g)"
            strokeWidth="5"
            strokeLinecap="round"
          />
          <circle cx="80" cy="62" r="4" fill="url(#sh-m)" />
          <rect x="44" y="80" width="30" height="4" rx="2" style={{ fill: 'rgb(var(--brand) / 0.22)' }} />
          <rect x="82" y="80" width="34" height="4" rx="2" style={{ fill: 'rgb(var(--brand) / 0.12)' }} />
        </g>
        <circle className="sh-o1" cx="138" cy="26" r="8" fill="url(#sh-m)" />
        <circle className="sh-o2" cx="22" cy="78" r="6" style={{ fill: 'rgb(var(--peach))' }} />
        <path d="M144 70l2.2 5 5 2.2-5 2.2-2.2 5-2.2-5-5-2.2 5-2.2z" style={{ fill: 'rgb(var(--brand-2) / 0.8)' }} />
      </svg>
      <b>Welcome back!</b>
      <p>{running > 0 ? `${running} 个实例正在运行，一切就绪` : '还没有运行中的实例，点 New 即可启动'}</p>
      <NavLink to="/agent-api" className="btn btn-ghost btn-xs">
        连接 AI Agent
      </NavLink>
    </div>
  );
}
