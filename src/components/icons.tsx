import type { SVGProps } from 'react';

/** 统一线性图标（24 栅格、1.8 描边）；状态点/导航 drawSVG 依赖 path 子元素 */
const IC = {
  envs: (
    <>
      <rect x="3" y="3" width="7.5" height="7.5" rx="2.2" />
      <rect x="13.5" y="3" width="7.5" height="7.5" rx="2.2" />
      <rect x="3" y="13.5" width="7.5" height="7.5" rx="2.2" />
      <rect x="13.5" y="13.5" width="7.5" height="7.5" rx="2.2" />
    </>
  ),
  instances: (
    <>
      <rect x="3" y="4" width="18" height="16" rx="3.5" />
      <path d="M3 9.5h18" />
    </>
  ),
  profiles: (
    <>
      <circle cx="12" cy="8" r="4" />
      <path d="M4.5 20.5c0-3.8 3.4-6 7.5-6s7.5 2.2 7.5 6" />
    </>
  ),
  extensions: <path d="M12 3l2.1 5.4L19.5 10.5l-5.4 2.1L12 18l-2.1-5.4L4.5 10.5l5.4-2.1z" />,
  api: (
    <>
      <path d="M5 7l5.5 5L5 17" />
      <path d="M13 18h6" />
    </>
  ),
  settings: (
    <>
      <path d="M4 7h9M17 7h3M4 17h3M11 17h9" />
      <circle cx="15" cy="7" r="2.2" />
      <circle cx="9" cy="17" r="2.2" />
    </>
  ),
  copy: (
    <>
      <rect x="9" y="9" width="11" height="11" rx="2.6" />
      <path d="M5 15V6.5A2.5 2.5 0 0 1 7.5 4H15" />
    </>
  ),
  restart: (
    <>
      <path d="M20 11.5a8 8 0 1 0-2.4 5.8" />
      <path d="M20.5 4.5v7h-7" />
    </>
  ),
  plus: <path d="M12 5v14M5 12h14" />,
  trash: <path d="M4 7h16M9.5 7V4.5h5V7M6.5 7l.9 12.5h9.2L17.5 7" />,
  back: <path d="M15 5l-7 7 7 7" />,
  warn: (
    <>
      <path d="M12 3.5l9.5 16.5h-19z" />
      <path d="M12 10v4.5M12 17.4v.1" />
    </>
  ),
  check: <path d="M5 12.5l4.5 4.5L19 7.5" />,
  eye: (
    <>
      <path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12z" />
      <circle cx="12" cy="12" r="2.8" />
    </>
  ),
  focus: (
    <>
      <path d="M4 9V5.5A1.5 1.5 0 0 1 5.5 4H9M15 4h3.5A1.5 1.5 0 0 1 20 5.5V9M20 15v3.5a1.5 1.5 0 0 1-1.5 1.5H15M9 20H5.5A1.5 1.5 0 0 1 4 18.5V15" />
      <circle cx="12" cy="12" r="2.4" />
    </>
  ),
  pulse: <path d="M3 12h4l2.5-6 4 12L16 12h5" />,
} as const;

export type IconName = keyof typeof IC;

export function Icon({ name, ...rest }: { name: IconName } & SVGProps<SVGSVGElement>) {
  return (
    <svg viewBox="0 0 24 24" {...rest}>
      {IC[name]}
    </svg>
  );
}

/** 品牌渐变定义（全局一份：logo / popover 品牌环 / 设置页进度环引用） */
export function GradientDefs() {
  return (
    <svg width="0" height="0" style={{ position: 'absolute' }} aria-hidden>
      <defs>
        <linearGradient id="ch-lg" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stopColor="#4f5be0" />
          <stop offset="1" stopColor="#2ecca8" />
        </linearGradient>
      </defs>
    </svg>
  );
}
