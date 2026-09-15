import { api } from '../api/client';
import { confirm } from '../components/confirm';

/**
 * 环境删除确认流：
 * - 无运行中实例：单次确认
 * - 有运行中实例：二次确认（先自动 stop-all 再删，后端 DELETE 409 契约不变）
 * 返回 true = 用户确认，调用方执行删除。
 * 注意：确认 UI 走应用内 ConfirmDialog（Tauri WKWebView 不支持 window.confirm）。
 */
export async function confirmDeleteEnv(name: string, runningInstances: number): Promise<boolean> {
  const running = runningInstances > 0;
  return confirm({
    title: `删除环境「${name}」`,
    message: [
      ...(running ? [`该环境有 ${runningInstances} 个运行中实例，删除前将自动停止它们。`] : []),
      '登录快照与全部实例数据将被清除，不可恢复。',
    ],
    confirmText: running ? '停止并删除' : '删除',
  });
}

/** 确认后的执行体：有运行实例先 stop-all（后端 DELETE 会拒绝运行中环境） */
export async function deleteEnv(envId: string, runningInstances: number): Promise<void> {
  if (runningInstances > 0) {
    await api.stopAll(envId);
  }
  await api.deleteEnvironment(envId);
}
