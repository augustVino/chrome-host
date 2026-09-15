/**
 * 剪贴板写入：Tauri WKWebView 限制较多（navigator.clipboard 报 NotAllowedError、
 * execCommand 也可能返回 false），故优先走官方剪贴板插件（原生 IPC），双层降级兜底。
 */
import { writeText as pluginWriteText } from '@tauri-apps/plugin-clipboard-manager';

export async function copyText(text: string): Promise<void> {
  // 1. Tauri 官方剪贴板插件（原生通道，最可靠）
  try {
    await pluginWriteText(text);
    return;
  } catch {
    /* fallthrough */
  }

  // 2. Web Clipboard API（非 Tauri 环境 / 浏览器调试）
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    /* fallthrough */
  }

  // 3. execCommand 降级（需在用户手势内）
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.style.position = 'fixed';
  ta.style.opacity = '0';
  document.body.appendChild(ta);
  ta.focus();
  ta.select();
  const ok = document.execCommand('copy');
  document.body.removeChild(ta);
  if (!ok) {
    throw new Error('复制失败：当前环境不支持剪贴板写入');
  }
}
