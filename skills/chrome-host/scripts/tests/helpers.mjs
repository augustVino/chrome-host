// Test doubles shared across test files. Not a *.test.mjs file on purpose:
// node --test only collects files matching its test-name patterns.

// Scriptable stand-in for the CDP class: send() delegates to a handler
// function, every call is recorded in .calls for assertions.
export class FakeCdp {
  constructor(handler = () => { throw new Error('FakeCdp: unexpected send()'); }) {
    this.handler = handler;
    this.calls = [];
    this.eventWaiters = [];
  }

  async send(method, params = {}, sessionId) {
    this.calls.push({ method, params, sessionId });
    return this.handler(method, params, sessionId);
  }

  waitForEvent(method, timeout) {
    const waiter = {
      method, timeout,
      promise: Promise.resolve({}),
      cancel() { waiter.canceled = true; },
      canceled: false,
    };
    this.eventWaiters.push(waiter);
    return waiter;
  }

  callsOf(method) {
    return this.calls.filter(c => c.method === method);
  }
}

// Stand-in for the global WebSocket. Tests drive it via open()/error()/
// closeEvent()/message(); the class under test wires on* handlers.
export class FakeWebSocket {
  static instances = [];

  constructor(url) {
    this.url = url;
    this.sent = [];
    this.closed = false;
    FakeWebSocket.instances.push(this);
  }

  send(data) { this.sent.push(data); }
  close() { this.closed = true; }

  // --- test-side emitters ---
  open() { this.onopen(); }
  error(e = { message: 'fake error' }) { this.onerror(e); }
  closeEvent() { this.onclose(); }
  message(obj) { this.onmessage({ data: JSON.stringify(obj) }); }
}

export function lastFakeSocket() {
  return FakeWebSocket.instances[FakeWebSocket.instances.length - 1];
}
