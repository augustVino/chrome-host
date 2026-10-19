/**
 * 启动意图登记：点击 New 时记下按钮元素，新实例的状态点挂载后据此决定"光点飞入"的起点。
 * 纯 UI 侧信道——不改动任何业务调用（api.createInstance 等照旧）。
 */
const marks = new Map<string, { el: Element; t: number }>();

export function markLaunch(envId: string, el: Element | null) {
  if (el) marks.set(envId, { el, t: Date.now() });
}

export function takeLaunch(envId: string): Element | null {
  const m = marks.get(envId);
  marks.delete(envId);
  return m && Date.now() - m.t < 10000 && m.el.isConnected ? m.el : null;
}

/**
 * 详情页共享元素：卡片 → 详情头部的 Flip 状态。
 * 非破坏性读取（1.5s 内有效）：StrictMode 下 effect/render 会重复执行，不能"取一次就清"。
 */
let pendingFlip: { s: unknown; t: number } | null = null;
export const setPendingFlip = (s: unknown) => {
  pendingFlip = { s, t: Date.now() };
};
export const peekPendingFlip = <T,>(): T | null =>
  pendingFlip && Date.now() - pendingFlip.t < 1500 ? (pendingFlip.s as T) : null;
