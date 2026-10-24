// CDP class: request/response id routing, timeouts, event dispatch — driven
// through an injected FakeWebSocket (the wsFactory constructor parameter is
// the single injection point; production callers use the global WebSocket).

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { CDP } from '../lib/cdp-client.mjs';
import { FakeWebSocket, lastFakeSocket } from './helpers.mjs';

async function connectedCdp() {
  const cdp = new CDP(FakeWebSocket);
  const pending = cdp.connect('ws://fake/devtools/browser');
  lastFakeSocket().open();
  await pending;
  return cdp;
}

test('connect resolves on open and uses the injected wsFactory', async () => {
  const cdp = await connectedCdp();
  assert.equal(lastFakeSocket().url, 'ws://fake/devtools/browser');
});

test('connect rejects with WebSocket error message', async () => {
  const cdp = new CDP(FakeWebSocket);
  const pending = cdp.connect('ws://fake');
  lastFakeSocket().error({ message: 'boom' });
  await assert.rejects(pending, { message: 'WebSocket error: boom' });
});

test('send assigns incrementing ids and resolves with the matching response', async (t) => {
  // Resolved sends keep their 15s timeout timer armed (original behavior) —
  // mock the timer clock so the test process doesn't wait it out.
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const cdp = await connectedCdp();
  const ws = lastFakeSocket();

  const p1 = cdp.send('Foo.bar');
  const msg1 = JSON.parse(ws.sent[0]);
  assert.equal(msg1.id, 1);
  assert.equal(msg1.method, 'Foo.bar');
  assert.deepEqual(msg1.params, {});
  assert.equal('sessionId' in msg1, false);

  ws.message({ id: msg1.id, result: { value: 42 } });
  assert.deepEqual(await p1, { value: 42 });

  const p2 = cdp.send('Baz.qux', { a: 1 }, 'SID-1');
  const msg2 = JSON.parse(ws.sent[1]);
  assert.equal(msg2.id, 2);
  assert.equal(msg2.sessionId, 'SID-1');
  ws.message({ id: msg2.id, result: {} });
  await p2;
});

test('send rejects with the CDP error message on an error response', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const cdp = await connectedCdp();
  const ws = lastFakeSocket();
  const p = cdp.send('Bad.method');
  ws.message({ id: JSON.parse(ws.sent[0]).id, error: { message: 'no such domain' } });
  await assert.rejects(p, { message: 'no such domain' });
});

test('send rejects after TIMEOUT when no response arrives', async (t) => {
  const cdp = await connectedCdp();
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const expectation = assert.rejects(cdp.send('Slow.method'), { message: 'Timeout: Slow.method' });
  t.mock.timers.tick(15001);
  await expectation;
});

test('a late response for an already-settled id is ignored', async () => {
  const cdp = await connectedCdp();
  const ws = lastFakeSocket();
  ws.message({ id: 999, result: { stale: true } }); // unknown id → no throw
  ws.message({ method: 'Nothing.Registered', params: {} }); // no handler → no throw
});

test('onEvent dispatches params and raw msg to every handler until unsubscribed', async () => {
  const cdp = await connectedCdp();
  const ws = lastFakeSocket();
  const seen = [];
  const off = cdp.onEvent('Page.frameNavigated', (params, msg) => seen.push([params, msg.method]));

  ws.message({ method: 'Page.frameNavigated', params: { frameId: 'f1' } });
  assert.equal(seen.length, 1);
  assert.equal(seen[0][0].frameId, 'f1');
  assert.equal(seen[0][1], 'Page.frameNavigated');

  // params omitted in the wire message still reach handlers as {}
  ws.message({ method: 'Page.frameNavigated' });
  assert.deepEqual(seen[1][0], {});

  off();
  ws.message({ method: 'Page.frameNavigated', params: { frameId: 'f2' } });
  assert.equal(seen.length, 2); // nothing dispatched after unsubscribe
});

test('waitForEvent resolves with event params', async () => {
  const cdp = await connectedCdp();
  const ws = lastFakeSocket();
  const w = cdp.waitForEvent('Page.loadEventFired', 1000);
  ws.message({ method: 'Page.loadEventFired', params: { ts: 7 } });
  assert.deepEqual(await w.promise, { ts: 7 });
});

test('waitForEvent rejects on its own timeout', async (t) => {
  const cdp = await connectedCdp();
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const w = cdp.waitForEvent('Page.loadEventFired', 5000);
  const expectation = assert.rejects(w.promise, { message: 'Timeout waiting for event: Page.loadEventFired' });
  t.mock.timers.tick(5001);
  await expectation;
});

test('waitForEvent cancel keeps the promise forever pending', async () => {
  const cdp = await connectedCdp();
  const ws = lastFakeSocket();
  const w = cdp.waitForEvent('Page.loadEventFired', 60_000);
  w.cancel();
  let settled = false;
  w.promise.then(() => { settled = true; }, () => { settled = true; });
  ws.message({ method: 'Page.loadEventFired', params: {} });
  await new Promise(r => setTimeout(r, 20));
  assert.equal(settled, false);
});

test('onClose handlers fire when the socket closes', async () => {
  const cdp = await connectedCdp();
  let closed = 0;
  cdp.onClose(() => closed++);
  cdp.onClose(() => closed++);
  lastFakeSocket().closeEvent();
  assert.equal(closed, 2);
});
