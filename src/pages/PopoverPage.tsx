import { useCallback, useEffect, useRef, useState } from 'react';
import { emit } from '@tauri-apps/api/event';
import { api, errorText } from '../api/client';
import type { Instance } from '../api/types';
import { useStore } from '../store';
import { openManager } from '../lib/windows';
import { copyText } from '../lib/clipboard';
import { confirmDeleteEnv, deleteEnv } from '../lib/deleteEnv';
import { ConfirmDialog } from '../components/confirm';
import { StatusDot, btnGhost, btnPrimary } from '../components/ui';

/** Popover 高度自适应：内容决定高度，540 封顶（超出后列表区内部滚动）；宽度固定由 Rust 持有 */
const POPOVER_MAX_HEIGHT = 540;
const POPOVER_MIN_HEIGHT = 280;

/** 实例显示名：环境内创建顺序序号 */
function displayName(list: Instance[], id: string): string {
  const idx = list.findIndex((i) => i.id === id);
  return `Chrome #${idx + 1}`;
}

/**
 * 托盘 Popover 内容：只做高频四件事——查看摘要 / New / Focus / Stop。
 * 数据通道不变（HTTP API + 后端事件）；窗口隐藏时 JS 持续轮询，弹出即最新。
 */
export default function PopoverPage() {
  const { envs, instancesByEnv, fetchEnvs, initEvents, showToast, toast } = useStore();
  const [busyId, setBusyId] = useState<string | null>(null);
  /** 根容器显式像素高度：不依赖 100vh（程序化 resize 后 vh 不可靠） */
  const [contentH, setContentH] = useState<number | null>(null);
  const headerRef = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const footerRef = useRef<HTMLDivElement>(null);

  /** 高度自适应：实测 header + 列表内容 + footer 的自然高度，上报给 Rust 执行 setSize。
   *  内容 ≤ max 时窗口收缩到内容高度；超出时窗口封顶（clamp 在 Rust 侧），列表区内部滚动。 */
  const syncHeight = useCallback(async () => {
    const { header, content, footer } = {
      header: headerRef.current,
      content: contentRef.current,
      footer: footerRef.current,
    };
    if (!header || !content || !footer) return;
    // 注意：不能测 list.scrollHeight——滚动容器被 flex 约束后 scrollHeight ≥ 自身高度，
    // 测量值被"当前窗口高度"污染，自适应死循环失效；content 包装器高度才是自然内容高度
    const natural = header.offsetHeight + content.offsetHeight + footer.offsetHeight;
    const clamped = Math.round(Math.min(Math.max(natural, POPOVER_MIN_HEIGHT), POPOVER_MAX_HEIGHT));
    setContentH((prev) => (prev === clamped ? prev : clamped));
    try {
      await emit('popover-height', { height: clamped });
    } catch {
      /* 事件通道不可用时静默：窗口保持创建尺寸 */
    }
  }, []);

  // 内容尺寸变化（环境/实例增减、首次数据加载）→ 重算窗口高度
  useEffect(() => {
    const content = contentRef.current;
    if (!content) return;
    let raf = 0;
    const ro = new ResizeObserver(() => {
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(() => void syncHeight());
    });
    ro.observe(content);
    return () => {
      ro.disconnect();
      cancelAnimationFrame(raf);
    };
  }, [syncHeight]);

  const refresh = useCallback(async () => {
    const s = useStore.getState();
    await s.fetchEnvs();
    s.envs.forEach((e) => s.fetchInstances(e.id));
  }, [fetchEnvs]);

  useEffect(() => {
    refresh();
    initEvents();
    const t = setInterval(refresh, 2000);
    void syncHeight();
    return () => clearInterval(t);
  }, [refresh, initEvents, syncHeight]);

  const act = useCallback(
    async (fn: () => Promise<unknown>, key: string) => {
      setBusyId(key);
      try {
        await fn();
      } catch (e) {
        showToast('err', errorText(e));
      } finally {
        setBusyId(null);
        refresh();
      }
    },
    [showToast, refresh],
  );

  const removeEnv = useCallback(
    async (envId: string, name: string, running: number) => {
      if (!(await confirmDeleteEnv(name, running))) return;
      try {
        await deleteEnv(envId, running);
        showToast('ok', '环境已删除');
      } catch (e) {
        showToast('err', errorText(e));
      }
      refresh();
    },
    [showToast, refresh],
  );

  const isMac = navigator.userAgent.includes('Mac');
  return (
    <div
      className={`flex flex-col overflow-hidden rounded-xl ${
        isMac ? '' : 'bg-surface-card/95 shadow-2xl'
      }`}
      style={{ height: contentH ? `${contentH}px` : '100vh' }}
    >
      <div
        ref={headerRef}
        className="flex items-center justify-between border-b border-popover-line px-3.5 py-2.5"
      >
        <p className="text-[11px] font-semibold uppercase tracking-wide text-ink-2">
          Chrome Host
        </p>
        <button className={btnGhost} onClick={() => openManager()}>
          Open Manager
        </button>
      </div>

      <div ref={listRef} className="min-h-0 flex-1 overflow-y-auto">
        <div ref={contentRef} className="space-y-2.5 p-3">
        {envs.length === 0 && (
          <p className="py-10 text-center text-xs text-ink-3">暂无环境，去主窗口创建</p>
        )}
        {envs.map((env) => {
          const instances = instancesByEnv[env.id] ?? [];
          const running = instances.filter((i) => i.status === 'running').length;
          return (
            <div key={env.id} className="rounded-lg border border-popover-line bg-popover-card p-2.5 backdrop-blur-sm">
              <div className="flex items-center justify-between gap-2">
                <div className="min-w-0">
                  <p className="truncate text-xs font-medium text-ink">{env.name}</p>
                </div>
                <div className="flex shrink-0 items-center gap-1">
                  <button
                    className="rounded-md px-1.5 py-1 text-[11px] text-ink-2 transition hover:bg-red-50 hover:text-status-error dark:hover:bg-red-500/15"
                    title="删除环境"
                    onClick={() =>
                      removeEnv(env.id, env.name, instances.filter((i) => i.status === 'running').length)
                    }
                  >
                    删除
                  </button>
                  <button
                    className={btnPrimary}
                    disabled={busyId === env.id}
                    onClick={() =>
                      act(async () => {
                        await api.createInstance(env.id);
                        showToast('ok', '实例已启动');
                      }, env.id)
                    }
                  >
                    {busyId === env.id ? '…' : '+ New'}
                  </button>
                </div>
              </div>

              {instances.length > 0 && (
                <ul className="mt-2 space-y-1 border-t border-popover-line pt-2">
                  {instances.map((ins) => (
                    <li key={ins.id} className="flex items-center gap-2">
                      <StatusDot status={ins.status} />
                      <span className="text-[11px] text-ink-2">
                        {displayName(instances, ins.id)}
                      </span>
                      <span className="ml-auto flex items-center gap-1">
                        {ins.status === 'running' && (
                          <>
                            <button
                              className="rounded-md border border-popover-line bg-popover-btn px-1.5 py-0.5 text-[10px] text-ink-2 transition hover:bg-popover-btn-hover disabled:opacity-40"
                              disabled={busyId === ins.id}
                              onClick={() =>
                                act(async () => {
                                  const cdp = (await api.cdpEndpoint(ins.id)) as { httpUrl: string };
                                  await copyText(cdp.httpUrl);
                                  showToast('ok', 'CDP 已复制');
                                }, ins.id)
                              }
                            >
                              CDP
                            </button>
                            <button
                              className="rounded-md border border-popover-line bg-popover-btn px-1.5 py-0.5 text-[10px] text-ink-2 transition hover:bg-popover-btn-hover disabled:opacity-40"
                              disabled={busyId === ins.id}
                              onClick={() => act(() => api.restartInstance(ins.id), ins.id)}
                            >
                              Restart
                            </button>
                            <button
                              className="rounded-md border border-popover-line bg-popover-btn px-1.5 py-0.5 text-[10px] text-ink-2 transition hover:bg-popover-btn-hover disabled:opacity-40"
                              disabled={busyId === ins.id}
                              onClick={() => act(() => api.stopInstance(ins.id), ins.id)}
                            >
                              Stop
                            </button>
                          </>
                        )}
                        {(ins.status === 'stopped' || ins.status === 'error') && (
                          <button
                            className="rounded-md border border-popover-line bg-popover-btn px-1.5 py-0.5 text-[10px] text-ink-2 transition hover:bg-popover-btn-hover disabled:opacity-40"
                            disabled={busyId === ins.id}
                            onClick={() => act(() => api.startInstance(ins.id), ins.id)}
                          >
                            Start
                          </button>
                        )}
                      </span>
                    </li>
                  ))}
                </ul>
              )}
              {running > 0 && (
                <p className="mt-1.5 text-[10px] text-run-text">{running} 个运行中</p>
              )}
            </div>
          );
        })}
        </div>
      </div>

      <div
        ref={footerRef}
        className="border-t border-popover-line px-3.5 py-2.5"
      >
        <button className={`${btnGhost} w-full justify-center`} onClick={() => openManager()}>
          + Add Environment
        </button>
      </div>

      {toast && <Toast toast={toast} />}
      <ConfirmDialog />
    </div>
    );
}

function Toast({ toast }: { toast: { kind: 'ok' | 'err'; text: string } }) {
  return (
    <div
      className={`absolute bottom-4 left-1/2 -translate-x-1/2 rounded-lg px-3 py-1.5 text-[11px] shadow-lg ${
        toast.kind === 'ok' ? 'bg-toast text-white' : 'bg-status-error text-white'
      }`}
    >
      {toast.text}
    </div>
  );
}
