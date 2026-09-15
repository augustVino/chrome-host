import { WebviewWindow, getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';

/** 唤出主窗口并隐藏 Popover（托盘 Popover 的 Open Manager / Add Environment 路径） */
export async function openManager(): Promise<void> {
  const main = await WebviewWindow.getByLabel('main');
  if (main) {
    await main.show();
    await main.unminimize();
    await main.setFocus();
  }
  await getCurrentWebviewWindow().hide();
}
