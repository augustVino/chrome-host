/** 粒子 / 光点工具（规范见 DESIGN.md §3）。粒子层是挂在 body 上的 fixed 容器，不参与业务布局。 */
import { gsap } from './gsap';
import { qMode } from './quality';

const rnd = gsap.utils.random;

function layer(): HTMLElement {
  let el = document.getElementById('ch-fx');
  if (!el) {
    el = document.createElement('div');
    el.id = 'ch-fx';
    document.body.appendChild(el);
  }
  return el;
}

export const MINT = 'rgb(46,204,168)';
export const CORAL = 'rgb(238,108,112)';
export const GREY = 'rgb(163,166,186)';

export function centerOf(el: Element) {
  const r = el.getBoundingClientRect();
  return { x: r.left + r.width / 2, y: r.top + r.height / 2, w: r.width };
}

/** Physics2D 粒子迸发：仅 full 档；窗口不可见时跳过 */
export function burst(
  x: number,
  y: number,
  color: string,
  { n = 14, gravity = 520, up = true, size = [3, 6] as [number, number] } = {},
) {
  if (qMode() !== 'full' || document.hidden) return;
  const host = layer();
  for (let i = 0; i < n; i++) {
    const p = document.createElement('i');
    p.className = 'ch-pt';
    const s = rnd(size[0], size[1]);
    Object.assign(p.style, {
      width: `${s}px`,
      height: `${s}px`,
      background: color,
      boxShadow: `0 0 8px ${color}`,
    });
    host.append(p);
    gsap.set(p, { x, y });
    gsap.to(p, {
      duration: 1.1,
      physics2D: { velocity: rnd(110, 270), angle: up ? rnd(-155, -25) : rnd(60, 120), gravity },
      ease: 'none',
    });
    gsap.to(p, {
      autoAlpha: 0,
      scale: 0.2,
      duration: 0.55,
      delay: 0.45,
      ease: 'power1.in',
      onComplete: () => p.remove(),
    });
  }
}

/** 光点沿贝塞尔曲线从 from 飞向 to（New → 新实例状态点） */
export function flyOrb(fromEl: Element | null, getTarget: () => Element | null) {
  if (qMode() === 'static' || !fromEl || document.hidden) return;
  // 等一帧：新实例的 DOM 在 React 提交后才存在
  let tries = 0;
  const go = () => {
    const to = getTarget();
    if (!to || !to.getBoundingClientRect().width) {
      if (++tries < 40) requestAnimationFrame(go);
      return;
    }
    const a = centerOf(fromEl);
    const b = centerOf(to);
    const o = document.createElement('i');
    o.className = 'ch-orb';
    layer().append(o);
    gsap.set(o, { x: a.x - 7, y: a.y - 7 });
    const dx = b.x - a.x;
    const dy = b.y - a.y;
    gsap.fromTo(o, { scale: 0.4 }, { scale: 1, duration: 0.25, ease: 'back.out(3)' });
    gsap.to(o, {
      motionPath: {
        path: [
          { x: a.x - 7, y: a.y - 7 },
          { x: a.x - 7 + dx * 0.35, y: a.y - 7 + Math.min(dy, 0) - 110 },
          { x: b.x - 7, y: b.y - 7 },
        ],
        curviness: 1.5,
      },
      duration: 0.95,
      ease: 'power2.inOut',
      onComplete: () => {
        burst(b.x, b.y, MINT, { n: 8 });
        gsap.to(o, { scale: 2.6, autoAlpha: 0, duration: 0.35, onComplete: () => o.remove() });
      },
    });
  };
  requestAnimationFrame(go);
}
