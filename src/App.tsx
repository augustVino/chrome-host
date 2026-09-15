import { useEffect, useState } from 'react';
import { HashRouter, NavLink, Route, Routes } from 'react-router-dom';
import { getVersion } from '@tauri-apps/api/app';
import { useStore } from './store';
import { ConfirmDialog } from './components/confirm';
import { startSilentCheck } from './lib/updater';
import EnvironmentsPage from './pages/EnvironmentsPage';
import EnvironmentDetailPage from './pages/EnvironmentDetailPage';
import InstancesPage from './pages/InstancesPage';
import SettingsPage from './pages/SettingsPage';
import LoginProfilesPage from './pages/LoginProfilesPage';
import ExtensionsPage from './pages/ExtensionsPage';
import AgentApiPage from './pages/AgentApiPage';
import PopoverPage from './pages/PopoverPage';

const NAV: { to: string; label: string; end?: boolean }[] = [
  { to: '/', label: 'Environments', end: true },
  { to: '/instances', label: 'Instances' },
  { to: '/login-profiles', label: 'Login Profiles' },
  { to: '/extensions', label: 'Extensions' },
  { to: '/agent-api', label: 'Agent API' },
  { to: '/settings', label: 'Settings' },
];

function Sidebar() {
  // 动态版本号：读 tauri.conf.json 的 version（CI 打 tag 时三处同步，与安装包永远一致）
  const [version, setVersion] = useState('');
  useEffect(() => {
    void getVersion().then(setVersion).catch(() => {});
  }, []);
  return (
    <aside className="flex w-44 shrink-0 flex-col border-r border-hairline bg-surface-sidebar px-2.5 py-4">
      <p className="mb-5 px-2 text-[11px] font-semibold uppercase tracking-wide text-ink-3">
        Chrome Host
      </p>
      <nav className="space-y-0.5">
        {NAV.map((item) => (
          <NavLink
            key={item.to}
            to={item.to}
            end={item.end}
            className={({ isActive }) =>
              `flex items-center rounded-lg px-2.5 py-1.5 text-xs transition ${
                isActive
                  ? 'bg-white font-medium text-ink shadow-sm ring-1 ring-hairline'
                  : 'text-ink-2 hover:bg-white/60 hover:text-ink'
              }`
            }
          >
            {item.label}
          </NavLink>
        ))}
      </nav>
      <p className="mt-auto px-2 text-[10px] text-ink-3">{version ? `v${version}` : ''}</p>
    </aside>
  );
}

function Toast() {
  const { toast } = useStore();
  if (!toast) return null;
  return (
    <div
      className={`fixed bottom-5 left-1/2 z-50 -translate-x-1/2 rounded-lg px-3.5 py-2 text-xs shadow-lg ${
        toast.kind === 'ok' ? 'bg-ink text-white' : 'bg-status-error text-white'
      }`}
    >
      {toast.text}
    </div>
  );
}

function Shell() {
  const initEvents = useStore((s) => s.initEvents);
  const showToast = useStore((s) => s.showToast);
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

  return (
    <div className="flex h-screen bg-white text-ink">
      <Sidebar />
      <main className="flex-1 overflow-y-auto px-8 py-6">
        <Routes>
          <Route path="/" element={<EnvironmentsPage />} />
          <Route path="/environments/:id" element={<EnvironmentDetailPage />} />
          <Route path="/instances" element={<InstancesPage />} />
          <Route path="/login-profiles" element={<LoginProfilesPage />} />
          <Route path="/extensions" element={<ExtensionsPage />} />
          <Route path="/agent-api" element={<AgentApiPage />} />
          <Route path="/settings" element={<SettingsPage />} />
          <Route path="*" element={<EnvironmentsPage />} />
        </Routes>
      </main>
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
