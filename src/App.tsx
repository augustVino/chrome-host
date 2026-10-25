import { useEffect, useLayoutEffect, useRef, useState, type PointerEvent as RPointerEvent } from 'react';
import { HashRouter, NavLink, Route, Routes, useLocation } from 'react-router-dom';
import { getVersion } from '@tauri-apps/api/app';
import { openUrl } from '@tauri-apps/plugin-opener';
import { useStore } from './store';
import { startFeed } from './feed';
import { ConfirmDialog } from './components/confirm';
import { Toast } from './components/Toast';
import { SideHello } from './components/SideHello';
import { Aurora } from './components/Aurora';
import { GradientDefs, Icon, type IconName } from './components/icons';
import logoUrl from './assets/logo.png';
import { gsap, SplitText, useGSAP } from './motion/gsap';
import { qMode, useQuality } from './motion/quality';
import { startSilentCheck } from './lib/updater';
import EnvironmentsPage from './pages/EnvironmentsPage';
import EnvironmentDetailPage from './pages/EnvironmentDetailPage';
import InstancesPage from './pages/InstancesPage';
import SettingsPage from './pages/SettingsPage';
import LoginProfilesPage from './pages/LoginProfilesPage';
import ExtensionsPage from './pages/ExtensionsPage';
import AgentApiPage from './pages/AgentApiPage';
import PopoverPage from './pages/PopoverPage';

const NAV: { to: string; label: string; icon: IconName; end?: boolean }[] = [
  { to: '/', label: 'Environments', icon: 'envs', end: true },
  { to: '/instances', label: 'Instances', icon: 'instances' },
  { to: '/login-profiles', label: 'Login Profiles', icon: 'profiles' },
  { to: '/extensions', label: 'Extensions', icon: 'extensions' },
  { to: '/agent-api', label: 'Agent API', icon: 'api' },
  { to: '/settings', label: 'Settings', icon: 'settings' },
];

/** 当前路径对应的导航项下标（环境详情归属 Environments） */
function activeIndex(pathname: string): number {
  if (pathname.startsWith('/environments/')) return 0;
  const i = NAV.findIndex((n) => (n.end ? pathname === n.to : pathname.startsWith(n.to)));
  return i < 0 ? 0 : i;
}

function Sidebar() {
  // 动态版本号：读 tauri.conf.json 的 version（CI 打 tag 时三处同步，与安装包永远一致）
  const [version, setVersion] = useState('');
  const running = useStore((s) => s.envs.reduce((a, e) => a + e.runningInstances, 0));
  const { pathname } = useLocation();
  const nav = useRef<HTMLElement>(null);
  const ind = useRef<HTMLElement>(null);
  const first = useRef(true);
  const idx = activeIndex(pathname);

  useEffect(() => {
    void getVersion().then(setVersion).catch(() => {});
  }, []);

  // 滑动指示器：首次瞬时就位，之后 back.out 弹性滑动
  useLayoutEffect(() => {
    const btn = nav.current?.querySelectorAll<HTMLElement>('.nav-item')[idx];
    if (!btn || !ind.current) return;
    const animate = !first.current && qMode() !== 'static';
    first.current = false;
    gsap.to(ind.current, {
      y: btn.offsetTop,
      duration: animate ? 0.7 : 0,
      ease: animate ? 'back.out(1.5)' : 'none',
      overwrite: 'auto',
    });
  }, [idx]);

  // 悬停：图标描边重绘 + 轻微磁吸（full 档）
  const onOver = (e: RPointerEvent) => {
    const b = (e.target as HTMLElement).closest('.nav-item');
    if (!b || qMode() === 'static') return;
    gsap.fromTo(b.querySelectorAll('svg > *'), { drawSVG: '0%' }, { drawSVG: '100%', duration: 0.5, stagger: 0.05, ease: 'power2.out', overwrite: true });
  };
  const onMove = (e: RPointerEvent) => {
    const b = (e.target as HTMLElement).closest<HTMLElement>('.nav-item');
    if (!b || qMode() !== 'full') return;
    const r = b.getBoundingClientRect();
    gsap.to(b, { x: gsap.utils.clamp(-4, 4, (e.clientX - r.left - r.width / 2) * 0.08), duration: 0.5, ease: 'power3', overwrite: 'auto' });
  };
  const onLeave = (e: RPointerEvent) => {
    const b = (e.target as HTMLElement).closest<HTMLElement>('.nav-item');
    if (b) gsap.to(b, { x: 0, duration: 0.5, ease: 'power3', overwrite: 'auto' });
  };

  // 悬停 Logo：轻微回弹摆动（非 static 档）
  const onBrandEnter = (e: RPointerEvent) => {
    if (qMode() === 'static') return;
    const logo = e.currentTarget.querySelector('.logo');
    if (logo) gsap.fromTo(logo, { rotation: -14 }, { rotation: 0, duration: 0.9, ease: 'elastic.out(1, 0.4)', overwrite: true });
  };

  return (
    <aside className="sidebar">
      <div className="brand" onPointerEnter={onBrandEnter}>
        <img className="logo" src={logoUrl} alt="" draggable={false} />
        <span className="brand-name">Chrome Host</span>
      </div>
      <nav className="nav" ref={nav} onPointerOver={onOver} onPointerMove={onMove} onPointerOut={onLeave}>
        <i className="nav-ind" ref={ind} />
        {NAV.map((item, i) => (
          <NavLink key={item.to} to={item.to} end={item.end} className={`nav-item ${i === idx ? 'on' : ''}`}>
            <Icon name={item.icon} />
            <span>{item.label}</span>
            {item.to === '/instances' && running > 0 && <em className="nav-badge">{running}</em>}
          </NavLink>
        ))}
      </nav>
      <div className="side-bottom">
        <SideHello />
        <div className="side-foot mono">
          <span>{version ? `v${version}` : ''}</span>
          <button
            type="button"
            className="side-foot-gh"
            title="GitHub 仓库"
            aria-label="GitHub 仓库"
            onClick={() => void openUrl('https://github.com/augustVino/chrome-host')}
          >
            <Icon name="github" width={13} height={13} />
          </button>
        </div>
      </div>
    </aside>
  );
}

function Shell() {
  const initEvents = useStore((s) => s.initEvents);
  const showToast = useStore((s) => s.showToast);
  const envIds = useStore((s) => s.envs.map((e) => e.id).join(','));
  const shellRef = useRef<HTMLDivElement>(null);
  const scrollerRef = useRef<HTMLDivElement>(null);
  const pageRef = useRef<HTMLDivElement>(null);
  const location = useLocation();
  const [shown, setShown] = useState(location);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    initEvents().then((fn) => (unlisten = fn));
    return () => unlisten?.();
  }, [initEvents]);

  // 启动静默检查更新：6h 间隔；发现新版 Toast 提示去 Settings 安装；失败静默
  useEffect(() => {
    const stop = startSilentCheck((version) => {
      showToast('ok', `发现新版本 v${version}，可在 Settings 页更新`);
    });
    return stop;
  }, [showToast]);

  // 3s 轮询：触发后端惰性对账（用户手动关闭 Chrome → 状态 ≤3s 回正）。
  // 后端无后台监听进程退出，读路径即对账，前端轮询即对账触发器。
  useEffect(() => {
    const t = setInterval(() => {
      const s = useStore.getState();
      s.fetchEnvs();
      Object.keys(s.instancesByEnv).forEach((id) => s.fetchInstances(id));
    }, 3000);
    return () => clearInterval(t);
  }, []);

  // ── 以下均为 v2 视觉层新增的只读/展示型副作用（不改动上方既有逻辑） ──
  // 1) 首屏即拉一次内核状态与全部环境的实例列表：供侧栏 foot、Aurora、Live Feed、环境卡实例 chip 使用。
  //    之后的 3s 轮询会自动覆盖这些已加载的 key。
  useEffect(() => {
    useStore.getState().fetchKernel();
  }, []);
  useEffect(() => {
    const s = useStore.getState();
    s.envs.forEach((e) => {
      if (!(e.id in s.instancesByEnv)) s.fetchInstances(e.id);
    });
  }, [envIds]);
  // 2) Live Feed 事件派生
  useEffect(() => startFeed(), []);
  // 3) 自动降档提示
  useEffect(
    () =>
      useQuality.subscribe((s, p) => {
        if (p.mode === 'full' && s.mode === 'lite' && s.pref === 'auto') {
          showToast('ok', '帧率偏低，已自动切换到 Lite 动效档位（可在 Settings 调整）');
        }
      }),
    [showToast],
  );

  // 路由转场：旧页上浮淡出 → 切换 → 新页由各页 usePageEnter 接管
  useLayoutEffect(() => {
    if (location.pathname === shown.pathname) {
      if (location !== shown) setShown(location);
      return;
    }
    const el = pageRef.current;
    if (!el || qMode() === 'static') {
      setShown(location);
      return;
    }
    const tw = gsap.to(el, { autoAlpha: 0, y: -10, duration: 0.2, ease: 'power2.in', overwrite: true, onComplete: () => setShown(location) });
    return () => {
      tw.kill();
    };
  }, [location, shown]);

  useLayoutEffect(() => {
    if (scrollerRef.current) scrollerRef.current.scrollTop = 0;
  }, [shown.pathname]);

  // 序曲（§5.1）：极光渐显 → 侧栏弹入（Logo 描边 + 变形）→ 品牌名逐字 → 导航描边 → 内容上浮
  useGSAP(
    () => {
      const mode = qMode();
      if (mode === 'static') return;
      const brand = SplitText.create('.brand-name', { type: 'chars' });
      gsap.set('.aurora', { scale: 1.3, autoAlpha: 0 });
      gsap.set('.sidebar', { autoAlpha: 0 });
      gsap.set('.content', { autoAlpha: 0 });
      gsap.set('.nav-item', { autoAlpha: 0 });
      gsap.set('.side-foot, .side-hello', { autoAlpha: 0 });
      gsap.set('.logo', { autoAlpha: 0, scale: 0.4, rotation: -25 });
      const tl = gsap.timeline({ defaults: { ease: 'ch-out' } });
      tl.to('.aurora', { scale: 1, autoAlpha: 1, duration: 1.8, ease: 'power2.out' }, 0)
        .to('.sidebar', { autoAlpha: 1, duration: 0.1 }, 0.35)
        .fromTo('.sidebar', { x: -30, scaleX: 0.96, transformOrigin: 'left center' }, { x: 0, scaleX: 1, duration: 0.9, ease: 'back.out(1.3)' }, 0.35)
        .to('.logo', { autoAlpha: 1, scale: 1, rotation: 0, duration: 0.9, ease: 'ch-bounce' }, 0.4)
        .from(brand.chars, { yPercent: 100, autoAlpha: 0, duration: 0.6, stagger: 0.03 }, 0.6)
        .to('.nav-item', { autoAlpha: 1, duration: 0.5, stagger: 0.05 }, 0.75)
        .fromTo('.nav-item svg > *', { drawSVG: '0%' }, { drawSVG: '100%', duration: 0.6, stagger: 0.02, ease: 'power2.out' }, 0.75)
        .to('.side-foot, .side-hello', { autoAlpha: 1, duration: 0.5 }, 1.05)
        .fromTo('.content', { autoAlpha: 0, y: 24, scale: 0.985 }, { autoAlpha: 1, y: 0, scale: 1, duration: 0.9 }, 0.7)
        .add(() => brand.revert(), '>');
      // 点击任意处可跳过序曲
      const skip = () => tl.progress() < 1 && tl.progress(1);
      addEventListener('pointerdown', skip, { once: true });
      return () => removeEventListener('pointerdown', skip);
    },
    { scope: shellRef },
  );

  const isMac = navigator.userAgent.includes('Mac');
  return (
    <div className="shell" ref={shellRef}>
      <GradientDefs />
      <Aurora />
      {isMac && <div className="drag-region" data-tauri-drag-region />}
      <div className="layout">
        <Sidebar />
        <main className="content">
          <div className="scroller" ref={scrollerRef}>
            <div key={shown.pathname} ref={pageRef}>
              <Routes location={shown}>
                <Route path="/" element={<EnvironmentsPage />} />
                <Route path="/environments/:id" element={<EnvironmentDetailPage />} />
                <Route path="/instances" element={<InstancesPage />} />
                <Route path="/login-profiles" element={<LoginProfilesPage />} />
                <Route path="/extensions" element={<ExtensionsPage />} />
                <Route path="/agent-api" element={<AgentApiPage />} />
                <Route path="/settings" element={<SettingsPage />} />
                <Route path="*" element={<EnvironmentsPage />} />
              </Routes>
            </div>
          </div>
        </main>
      </div>
      <Toast />
      <ConfirmDialog />
    </div>
  );
}

export default function App() {
  return (
    <HashRouter>
      <Routes>
        {/* 托盘 Popover 独立窗口：无侧边栏/Shell，数据同源（store + HTTP API） */}
        <Route path="/popover" element={<PopoverPage />} />
        <Route path="*" element={<Shell />} />
      </Routes>
    </HashRouter>
  );
}
