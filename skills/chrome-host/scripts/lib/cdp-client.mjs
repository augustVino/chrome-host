// CDP WebSocket client + endpoint discovery.

import { TIMEOUT } from './paths.mjs';

export async function getWsUrl() {
  // CDP_BASE：chrome-host 会话代理模式（http://host:port/cdp/<ins>/<sid>）。
  // 设置后仅改此处的 /json/version 入口；webSocketDebuggerUrl 由服务端改写为
  // 指向代理的 ws://host/cdp/...，下游连接自然走代理，无需其他改动。
  const base = process.env.CDP_BASE;
  if (base) {
    const res = await fetch(`${base.replace(/\/+$/, '')}/json/version`);
    if (!res.ok) throw new Error(`Cannot reach CDP proxy at ${base} (${res.status}) — session may have expired (30min); re-issue via chrome-host instance cdp-session`);
    const { webSocketDebuggerUrl } = await res.json();
    if (!webSocketDebuggerUrl) throw new Error(`No webSocketDebuggerUrl in /json/version response from ${base}`);
    return webSocketDebuggerUrl;
  }
  const host = process.env.CDP_HOST || '127.0.0.1';
  const port = process.env.CDP_PORT || '9222';
  const res = await fetch(`http://${host}:${port}/json/version`);
  if (!res.ok) throw new Error(`Cannot connect to Chrome DevTools at ${host}:${port} — launch Chrome with --remote-debugging-port=${port}`);
  const { webSocketDebuggerUrl } = await res.json();
  if (!webSocketDebuggerUrl) throw new Error(`No webSocketDebuggerUrl in /json/version response from ${host}:${port}`);
  return webSocketDebuggerUrl;
}

export class CDP {
  #ws; #id = 0; #pending = new Map(); #eventHandlers = new Map(); #closeHandlers = [];
  #wsFactory;

  // wsFactory is injectable for tests; production callers use the built-in WebSocket.
  constructor(wsFactory = globalThis.WebSocket) {
    this.#wsFactory = wsFactory;
  }

  async connect(wsUrl) {
    return new Promise((res, rej) => {
      this.#ws = new (this.#wsFactory)(wsUrl);
      this.#ws.onopen = () => res();
      this.#ws.onerror = (e) => rej(new Error('WebSocket error: ' + (e.message || e.type)));
      this.#ws.onclose = () => this.#closeHandlers.forEach(h => h());
      this.#ws.onmessage = (ev) => {
        const msg = JSON.parse(ev.data);
        if (msg.id && this.#pending.has(msg.id)) {
          const { resolve, reject } = this.#pending.get(msg.id);
          this.#pending.delete(msg.id);
          if (msg.error) reject(new Error(msg.error.message));
          else resolve(msg.result);
        } else if (msg.method && this.#eventHandlers.has(msg.method)) {
          for (const handler of [...this.#eventHandlers.get(msg.method)]) {
            handler(msg.params || {}, msg);
          }
        }
      };
    });
  }

  send(method, params = {}, sessionId) {
    const id = ++this.#id;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      const msg = { id, method, params };
      if (sessionId) msg.sessionId = sessionId;
      this.#ws.send(JSON.stringify(msg));
      setTimeout(() => {
        if (this.#pending.has(id)) {
          this.#pending.delete(id);
          reject(new Error(`Timeout: ${method}`));
        }
      }, TIMEOUT);
    });
  }

  onEvent(method, handler) {
    if (!this.#eventHandlers.has(method)) this.#eventHandlers.set(method, new Set());
    const handlers = this.#eventHandlers.get(method);
    handlers.add(handler);
    return () => {
      handlers.delete(handler);
      if (handlers.size === 0) this.#eventHandlers.delete(method);
    };
  }

  waitForEvent(method, timeout = TIMEOUT) {
    let settled = false;
    let off;
    let timer;
    const promise = new Promise((resolve, reject) => {
      off = this.onEvent(method, (params) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        off();
        resolve(params);
      });
      timer = setTimeout(() => {
        if (settled) return;
        settled = true;
        off();
        reject(new Error(`Timeout waiting for event: ${method}`));
      }, timeout);
    });
    return {
      promise,
      cancel() {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        off?.();
      },
    };
  }

  onClose(handler) { this.#closeHandlers.push(handler); }
  close() { this.#ws.close(); }
}
