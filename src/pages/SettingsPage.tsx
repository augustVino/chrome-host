import { useCallback, useEffect, useState } from 'react';
import { api, errorText } from '../api/client';
import { copyText } from '../lib/clipboard';
import { useStore } from '../store';
import { btnGhost, btnPrimary, inputCls } from '../components/ui';
import { disable as autostartDisable, enable as autostartEnable, isEnabled as autostartIsEnabled } from '@tauri-apps/plugin-autostart';
import {
  checkExplicitly,
  downloadAndInstall,
  relaunchApp,
  type UpdateState,
} from '../lib/updater';

const STAGE_LABEL: Record<string, string> = {
  downloading: '下载中',
  extracting: '解压中',
  verifying: '校验中',
  done: '完成',
  error: '失败',
  cancelled: '已取消',
};

export default function SettingsPage() {
  const { kernel, kernelProgress, fetchKernel, showToast, settings, fetchSettings, updateSettings } = useStore();
  const [busy, setBusy] = useState(false);
  /** 开机自启：null=读取中，true/false=OS LoginItem 实际状态 */
  const [autoStart, setAutoStart] = useState<boolean | null>(null);
  /** 应用更新状态机：dev 模式下检查会静默失败，不影响其余功能 */
  const [updateState, setUpdateState] = useState<UpdateState>({ phase: 'idle' });
  /** Agent API 真实健康态：null=检测中，true=可达，false=不可达（不做静态渲染） */
  const [apiUp, setApiUp] = useState<boolean | null>(null);
  /** 默认起始页本地草稿：加载后从 settings 同步；空串合法（= about:blank） */
  const [startUrl, setStartUrl] = useState<string | null>(null);
  const [savingStartUrl, setSavingStartUrl] = useState(false);

  useEffect(() => {
    if (settings && startUrl === null) setStartUrl(settings.defaultStartUrl);
  }, [settings, startUrl]);

  const saveStartUrl = useCallback(async () => {
    if (startUrl === null) return;
    setSavingStartUrl(true);
    try {
      await updateSettings({ defaultStartUrl: startUrl.trim() });
      showToast('ok', '默认起始页已保存');
    } catch (e) {
      showToast('err', errorText(e));
    } finally {
      setSavingStartUrl(false);
    }
  }, [startUrl, updateSettings, showToast]);

  const toggleAutoStart = useCallback(async () => {
    const next = !(autoStart ?? false);
    try {
      // 先调 OS 再更新状态：失败时不撒谎；OS LoginItem 是唯一真相源，不入 app_settings
      if (next) {
        await autostartEnable();
      } else {
        await autostartDisable();
      }
      setAutoStart(next);
    } catch (e) {
      showToast('err', `开机自启设置失败：${String(e)}`);
    }
  }, [autoStart, showToast]);

  const checkApi = useCallback(async () => {
    try {
      const ctl = new AbortController();
      const timer = setTimeout(() => ctl.abort(), 2500);
      await fetch('http://127.0.0.1:17890/api/v1/kernel/status', { signal: ctl.signal });
      clearTimeout(timer);
      setApiUp(true);
    } catch {
      setApiUp(false);
    }
  }, []);

  useEffect(() => {
    fetchKernel();
    fetchSettings();
    void checkApi();
    autostartIsEnabled()
      .then(setAutoStart)
      .catch(() => setAutoStart(false));
    const t = setInterval(checkApi, 3000);
    return () => clearInterval(t);
  }, [fetchKernel, fetchSettings, checkApi]);

  const download = async () => {
    setBusy(true);
    try {
      const r = await api.kernelDownload();
      if (!r.ok) showToast('err', '已有下载任务进行中');
    } catch (e) {
      showToast('err', String(e));
    } finally {
      setBusy(false);
      fetchKernel();
    }
  };

  /** 手动检查更新 → 有新版则保持 available 态等待用户确认安装 */
  const checkUpdate = useCallback(async () => {
    setUpdateState({ phase: 'checking' });
    try {
      const update = await checkExplicitly();
      setUpdateState(update ? { phase: 'available', update } : { phase: 'up-to-date' });
    } catch (e) {
      // 透传真实错误：常见为网络不可达 github.com（内网/无代理环境），需可诊断
      setUpdateState({ phase: 'failed', message: String(e).slice(0, 200) });
    }
  }, []);

  const installUpdate = useCallback(async () => {
    if (updateState.phase !== 'available') return;
    setUpdateState({ phase: 'downloading', received: 0, total: null });
    try {
      await downloadAndInstall(updateState.update, (received, total) =>
        setUpdateState({ phase: 'downloading', received, total }),
      );
      setUpdateState({ phase: 'ready' });
    } catch (e) {
      setUpdateState({ phase: 'failed', message: String(e) });
    }
  }, [updateState]);

  return (
    <div className="mx-auto max-w-2xl">
      <h1 className="mb-5 text-base font-semibold text-ink">Settings</h1>

      {/* 内核卡 */}
      <section className="mb-4 rounded-xl border border-hairline bg-white p-4">
        <h2 className="text-sm font-medium text-ink">浏览器内核（Chrome for Testing）</h2>
        {!kernel ? (
          <p className="mt-2 text-xs text-ink-3">Agent API 未连接，无法读取内核状态。</p>
        ) : (
          <div className="mt-3 space-y-2 text-xs">
            <div className="flex gap-8">
              <span className="text-ink-2">
                pinned 版本：<span className="font-mono text-ink">{kernel.pinnedVersion}</span>
              </span>
              <span className="text-ink-2">
                本地版本：
                <span className="font-mono text-ink">{kernel.version ?? '未安装'}</span>
              </span>
            </div>
            {kernel.downloading || kernelProgress ? (
              <div>
                {(() => {
                  const p = kernelProgress;
                  return (
                    <div className="mb-1 flex items-center justify-between text-ink-2">
                      <span>{p ? (STAGE_LABEL[p.stage] ?? p.stage) : '下载中'}</span>
                      <span className="font-mono">{p ? `${p.percent}%` : ''}</span>
                    </div>
                  );
                })()}
                <div className="h-1.5 overflow-hidden rounded-full bg-neutral-100">
                  <div
                    className="h-full rounded-full bg-accent transition-all"
                    style={{ width: `${kernelProgress?.percent ?? 0}%` }}
                  />
                </div>
                <button className={`${btnGhost} mt-2`} onClick={() => api.kernelCancel()}>
                  取消下载
                </button>
              </div>
            ) : kernel.installed && !kernel.upgradeAvailable ? (
              <p className="text-run-text">✓ 内核已就绪</p>
            ) : (
              <div className="flex items-center gap-2">
                <span className={kernel.upgradeAvailable ? 'text-amber-600' : 'text-ink-2'}>
                  {kernel.upgradeAvailable ? '有可用升级（pinned 版本更新）' : '内核未安装'}
                </span>
                <button className={btnPrimary} disabled={busy} onClick={download}>
                  {kernel.upgradeAvailable ? '升级内核' : '下载内核'}
                </button>
              </div>
            )}
          </div>
        )}
      </section>

      {/* 默认起始页（实例/登录浏览器启动页；环境专属 host 优先） */}
      <section className="mb-4 rounded-xl border border-hairline bg-white p-4">
        <h2 className="text-sm font-medium text-ink">默认起始页</h2>
        <p className="mt-0.5 text-xs text-ink-3">
          新建环境无需再填起始页：实例与登录浏览器打开时使用此地址，留空则打开 about:blank。
          环境自身配置了专属 Host URL 时优先用环境配置。修改对之后的启动生效。
        </p>
        <div className="mt-3 flex gap-2">
          <input
            className={`${inputCls} flex-1`}
            value={startUrl ?? ''}
            onChange={(e) => setStartUrl(e.target.value)}
            placeholder="https://home.example.com（留空 = about:blank）"
            disabled={startUrl === null}
          />
          <button
            className={btnPrimary}
            disabled={startUrl === null || savingStartUrl || startUrl.trim() === (settings?.defaultStartUrl ?? '')}
            onClick={saveStartUrl}
          >
            {savingStartUrl ? '保存中…' : '保存'}
          </button>
        </div>
        {startUrl === null && (
          <p className="mt-2 text-[11px] text-status-error">设置未加载（Agent API 未连接）</p>
        )}
      </section>

      {/* Developer Mode */}
      <section className="mb-4 rounded-xl border border-hairline bg-white p-4">
        <div className="flex items-center justify-between">
          <div>
            <h2 className="text-sm font-medium text-ink">Developer Mode</h2>
            <p className="mt-0.5 text-xs text-ink-3">
              开启后实例行显示 PID / CDP 端口 / Profile 目录等技术字段
            </p>
          </div>
          <button
            role="switch"
            aria-checked={settings?.developerMode ?? false}
            className={`relative h-5 w-9 shrink-0 rounded-full transition ${
              settings?.developerMode ? 'bg-status-running' : 'bg-status-stopped'
            }`}
            onClick={() => updateSettings({ developerMode: !settings?.developerMode })}
          >
            <span
              className={`absolute top-0.5 h-4 w-4 rounded-full bg-white shadow transition-all ${
                settings?.developerMode ? 'left-[18px]' : 'left-0.5'
              }`}
            />
          </button>
        </div>
      </section>

      {/* 应用更新 */}
      <section className="mb-4 rounded-xl border border-hairline bg-white p-4">
        <div className="flex items-center justify-between">
          <div>
            <h2 className="text-sm font-medium text-ink">应用更新</h2>
            <p className="mt-0.5 text-xs text-ink-3">
              {updateState.phase === 'ready'
                ? '新版本已安装，重启应用后生效'
                : updateState.phase === 'downloading'
                  ? `下载中 ${updateState.total ? `${Math.round((updateState.received / updateState.total) * 100)}%` : `${(updateState.received / 1024 / 1024).toFixed(1)} MB`}`
                  : updateState.phase === 'available'
                    ? `发现新版本 v${updateState.update.version}，可下载安装`
                    : updateState.phase === 'up-to-date'
                      ? '当前已是最新版本'
                      : updateState.phase === 'failed'
                        ? `检查失败：${updateState.message}`
                        : '应用启动时每 6 小时自动检查一次'}
            </p>
          </div>
          {updateState.phase === 'available' ? (
            <button className={btnPrimary} onClick={installUpdate}>
              下载并安装
            </button>
          ) : updateState.phase === 'ready' ? (
            <button className={btnPrimary} onClick={() => void relaunchApp()}>
              重启应用
            </button>
          ) : (
            <button className={btnGhost} disabled={updateState.phase === 'checking' || updateState.phase === 'downloading'} onClick={checkUpdate}>
              {updateState.phase === 'checking' ? '检查中…' : '检查更新'}
            </button>
          )}
        </div>
        {updateState.phase === 'downloading' && updateState.total !== null && (
          <div className="mt-2 h-1.5 overflow-hidden rounded-full bg-neutral-100">
            <div
              className="h-full rounded-full bg-accent transition-all"
              style={{ width: `${Math.round((updateState.received / updateState.total) * 100)}%` }}
            />
          </div>
        )}
      </section>

      {/* 开机自启 */}
      <section className="mb-4 rounded-xl border border-hairline bg-white p-4">
        <div className="flex items-center justify-between">
          <div>
            <h2 className="text-sm font-medium text-ink">开机自启</h2>
            <p className="mt-0.5 text-xs text-ink-3">
              登录系统后自动启动 Chrome Host：主窗口隐藏，仅托盘常驻
            </p>
          </div>
          <button
            role="switch"
            aria-checked={autoStart === true}
            disabled={autoStart === null}
            className={`relative h-5 w-9 shrink-0 rounded-full transition disabled:opacity-50 ${
              autoStart ? 'bg-status-running' : 'bg-status-stopped'
            }`}
            onClick={toggleAutoStart}
          >
            <span
              className={`absolute top-0.5 h-4 w-4 rounded-full bg-white shadow transition-all ${
                autoStart ? 'left-[18px]' : 'left-0.5'
              }`}
            />
          </button>
        </div>
      </section>

      {/* 环境标识卡（位置/颜色可配） */}
      <section className="mb-4 rounded-xl border border-neutral-200 bg-white p-4">
        <h2 className="text-sm font-medium text-neutral-700">环境标识（Environment Label）</h2>
        <p className="mt-0.5 text-xs text-neutral-400">
          实例页面角落显示环境标签，便于区分多实例。修改后对新启动的实例生效。
        </p>
        <div className="mt-3 space-y-3 text-xs">
          <div>
            <p className="mb-1.5 text-neutral-500">位置</p>
            <div className="flex gap-1.5">
              {([
                ['top-left', '左上'],
                ['top-right', '右上'],
                ['bottom-left', '左下'],
                ['bottom-right', '右下'],
              ] as const).map(([value, label]) => (
                <button
                  key={value}
                  className={`rounded-md border px-2.5 py-1 transition ${
                    settings?.envLabelPosition === value
                      ? 'border-neutral-800 bg-neutral-800 text-white'
                      : 'border-neutral-200 text-neutral-600 hover:bg-neutral-50'
                  }`}
                  onClick={() => updateSettings({ envLabelPosition: value })}
                >
                  {label}
                </button>
              ))}
            </div>
          </div>
          <div>
            <p className="mb-1.5 text-neutral-500">颜色</p>
            <div className="flex gap-1.5">
              {([
                ['red', 'rgb(239,68,68)', '红'],
                ['blue', 'rgb(37,99,235)', '蓝'],
                ['green', 'rgb(34,197,94)', '绿'],
                ['purple', 'rgb(168,85,247)', '紫'],
              ] as const).map(([value, swatch, label]) => (
                <button
                  key={value}
                  className={`flex items-center gap-1.5 rounded-md border px-2.5 py-1 transition ${
                    settings?.envLabelColor === value
                      ? 'border-neutral-800 bg-neutral-800 text-white'
                      : 'border-neutral-200 text-neutral-600 hover:bg-neutral-50'
                  }`}
                  onClick={() => updateSettings({ envLabelColor: value })}
                >
                  <span className="size-2.5 rounded-full" style={{ backgroundColor: swatch }} />
                  {label}
                </button>
              ))}
            </div>
          </div>
        </div>
      </section>

      {/* Agent API 卡 */}
      <section className="rounded-xl border border-hairline bg-white p-4">
        <h2 className="text-sm font-medium text-ink">Agent API</h2>
        <div className="mt-3 space-y-1.5 text-xs text-ink-2">
          <p className="flex items-center gap-2">
            <span
              className={`size-2 rounded-full ${
                apiUp === false ? 'bg-status-error' : apiUp === null ? 'bg-status-stopped' : 'bg-status-running'
              }`}
            />
            {apiUp === false ? (
              <span className="text-status-error">未连接（应用内 API 未启动或端口被占用）</span>
            ) : (
              <>
                {apiUp === null ? '检测中 · ' : 'Running · '}
                <span className="font-mono">http://127.0.0.1:17890</span>
              </>
            )}
            <button
              className={btnGhost}
              onClick={async () => {
                await copyText('http://127.0.0.1:17890');
                showToast('ok', 'Endpoint 已复制');
              }}
            >
              Copy
            </button>
          </p>
          {apiUp === false && (
            <p className="text-[11px] text-status-error">
              请完全退出应用后重新启动；若仍失败，检查 17890 端口是否被其他程序占用。
            </p>
          )}
          <p className="text-[11px] text-ink-3">
            仅绑定本机回环地址。Agent 通过 REST 管理环境与实例，经 CDP 直连浏览器。
          </p>
        </div>
      </section>
    </div>
  );
}
