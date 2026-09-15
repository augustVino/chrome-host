/**
 * 应用自动更新：Tauri v2 updater。
 * - 启动静默检查（6h 间隔 + 启动即查一次），GitHub 不可达/pubkey 未生效时静默跳过
 * - Settings 页手动检查：发现新版 → 内联展示版本与 changelog → 确认后下载安装 → relaunch
 * 状态机：idle → checking → available → downloading → ready →(relaunch)
 */
import { check, type Update } from '@tauri-apps/plugin-updater';
import { relaunch } from '@tauri-apps/plugin-process';

export type UpdateState =
  | { phase: 'idle' }
  | { phase: 'checking' }
  | { phase: 'up-to-date' }
  | { phase: 'available'; update: Update }
  | { phase: 'downloading'; received: number; total: number | null }
  | { phase: 'ready' }
  | { phase: 'failed'; message: string };

const CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;

/** 静默检查：结果经 onAvailable 回调（发现新版时提示），其余一切失败仅 stderr 级留痕 */
export function startSilentCheck(onAvailable: (version: string) => void): () => void {
  const tick = async () => {
    try {
      const update = await check();
      if (update) onAvailable(update.version);
    } catch {
      /* 静默：未打包 dev / GitHub 不可达 / 无更新端点 */
    }
  };
  void tick();
  const t = setInterval(tick, CHECK_INTERVAL_MS);
  return () => clearInterval(t);
}

/** 手动检查（Settings 入口）：把全部失败显式抛给调用方展示 */
export async function checkExplicitly(): Promise<Update | null> {
  return check();
}

export async function downloadAndInstall(
  update: Update,
  onProgress: (received: number, total: number | null) => void,
): Promise<void> {
  let total: number | null = null;
  let received = 0;
  await update.downloadAndInstall((event) => {
    if (event.event === 'Started') {
      total = event.data.contentLength ?? null;
      received = 0;
      onProgress(0, total);
    } else if (event.event === 'Progress') {
      received += event.data.chunkLength;
      onProgress(received, total);
    }
    // Finished 由调用方转入 ready 态
  });
}

export { relaunch as relaunchApp };
