// execCommand: the daemon's command dispatch table (cdp injected — no real
// socket). sendCommand: the CLI-side NDJSON framing over a fake connection.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'events';
import { execCommand, sendCommand } from '../lib/daemon.mjs';
import { FakeCdp } from './helpers.mjs';

const SID = 'SESSION-1';
const TARGET = 'AAAABBBBCCCCDDDD0000';

test('list: filters to pages, drops chrome:// tabs, formats like formatPageList', async () => {
  const cdp = new FakeCdp(async () => ({
    targetInfos: [
      { targetId: 'AAAABBBBCCCCDDDD0000', type: 'page', title: 'Home', url: 'https://a.example/' },
      { targetId: 'FFFFFFFFFFFFFFFFFFFF', type: 'page', title: 'Internal', url: 'chrome://newtab/' },
      { targetId: 'EEEEEEEEEEEEEEEEEEEE', type: 'iframe', title: 'x', url: 'https://b.example/' },
    ],
  }));
  const res = await execCommand(cdp, SID, TARGET, { cmd: 'list', args: [] });
  assert.equal(res.ok, true);
  assert.equal(res.result, 'AAAABBBB  ' + 'Home'.padEnd(54) + '  https://a.example/');
});

test('list_raw: returns the filtered pages array as JSON', async () => {
  const cdp = new FakeCdp(async () => ({
    targetInfos: [
      { targetId: 'AAAABBBBCCCCDDDD0000', type: 'page', title: 'Home', url: 'https://a.example/' },
      { targetId: 'FFFFFFFFFFFFFFFFFFFF', type: 'page', title: 'Internal', url: 'chrome://newtab/' },
    ],
  }));
  const res = await execCommand(cdp, SID, TARGET, { cmd: 'list_raw', args: [] });
  assert.deepEqual(JSON.parse(res.result), [
    { targetId: 'AAAABBBBCCCCDDDD0000', type: 'page', title: 'Home', url: 'https://a.example/' },
  ]);
});

test('snap (and snapshot alias) uses compact mode on the attached session', async () => {
  const nodes = [
    { nodeId: '1', role: { value: 'WebArea' }, name: { value: 'P' } },
    { nodeId: '2', parentId: '1', role: { value: 'InlineTextBox' }, name: { value: 'noise' } },
    { nodeId: '3', parentId: '1', role: { value: 'button' }, name: { value: 'OK' } },
  ];
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Accessibility.getFullAXTree') return { nodes };
    return {};
  });
  for (const cmd of ['snap', 'snapshot']) {
    const res = await execCommand(cdp, SID, TARGET, { cmd, args: [] });
    assert.equal(res.ok, true);
    // InlineTextBox must be hidden — daemon snapshots are compact.
    assert.equal(res.result, '[WebArea] P\n  [button] OK');
    assert.equal(cdp.callsOf('Accessibility.getFullAXTree').at(-1).sessionId, SID);
  }
});

test('eval evaluates the first arg on the session', async () => {
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') return { result: { value: 42 } };
    throw new Error('unexpected ' + method);
  });
  const res = await execCommand(cdp, SID, TARGET, { cmd: 'eval', args: ['6*7'] });
  assert.deepEqual(res, { ok: true, result: '42' });
  assert.equal(cdp.callsOf('Runtime.evaluate')[0].params.expression, '6*7');
});

test('nav/navigate and net/network aliases dispatch to the same handlers', async () => {
  const navCdp = new FakeCdp(async (method) => {
    if (method === 'Page.navigate') return {};
    if (method === 'Runtime.evaluate') return { result: { value: 'complete' } };
    return {};
  });
  navCdp.waitForEvent = () => ({ promise: Promise.resolve({}), cancel() {} });
  for (const cmd of ['nav', 'navigate']) {
    const res = await execCommand(navCdp, SID, TARGET, { cmd, args: ['https://example.com/'] });
    assert.deepEqual(res, { ok: true, result: 'Navigated to https://example.com/' });
  }

  const netCdp = new FakeCdp(async (method) => {
    if (method === 'Runtime.evaluate') {
      return { result: { value: JSON.stringify([
        { name: 'https://x/y', type: 'fetch', duration: 12, size: 100 },
      ]) } };
    }
    return {};
  });
  for (const cmd of ['net', 'network']) {
    const res = await execCommand(netCdp, SID, TARGET, { cmd, args: [] });
  // type column: padEnd(8) + 2-space gap → 5 spaces after a 5-char type.
  assert.equal(res.result, '   12ms       100B  fetch     https://x/y');
  }
});

test('type / clickxy / evalraw wire args through to the tool layer', async () => {
  const cdp = new FakeCdp(async (method, params) => {
    if (method === 'Input.insertText') return {};
    if (method === 'Input.dispatchMouseEvent') return {};
    if (method === 'DOM.getDocument') return { root: {} };
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') return { result: { value: 'false' } }; // loadall exists-check
    throw new Error('unexpected ' + method);
  });

  assert.deepEqual(await execCommand(cdp, SID, TARGET, { cmd: 'type', args: ['hi'] }),
    { ok: true, result: 'Typed 2 characters' });
  assert.deepEqual(await execCommand(cdp, SID, TARGET, { cmd: 'clickxy', args: ['5', '6'] }),
    { ok: true, result: 'Clicked at CSS (5, 6)' });
  const raw = await execCommand(cdp, SID, TARGET, { cmd: 'evalraw', args: ['DOM.getDocument'] });
  assert.equal(raw.result, JSON.stringify({ root: {} }, null, 2));
  // loadall parses its interval arg as int (here the selector is gone at once)
  assert.deepEqual(await execCommand(cdp, SID, TARGET, { cmd: 'loadall', args: ['.more', '10'] }),
    { ok: true, result: 'Clicked ".more" 0 time(s) until it disappeared' });
});

test('unknown commands are reported as ok:false, not thrown', async () => {
  const res = await execCommand(new FakeCdp(), SID, TARGET, { cmd: 'explode', args: [] });
  assert.deepEqual(res, { ok: false, error: 'Unknown command: explode' });
});

test('tool errors are wrapped as ok:false with the message', async () => {
  const cdp = new FakeCdp(async () => { throw new Error('Session gone'); });
  const res = await execCommand(cdp, SID, TARGET, { cmd: 'eval', args: ['1'] });
  assert.deepEqual(res, { ok: false, error: 'Session gone' });
});

test('undefined tool results normalize to empty string', async () => {
  // netStr on zero entries returns '' → daemon must reply {result: ''}.
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Runtime.evaluate') return { result: { value: JSON.stringify([]) } };
    return {};
  });
  const res = await execCommand(cdp, SID, TARGET, { cmd: 'net', args: [] });
  assert.deepEqual(res, { ok: true, result: '' });
});

test('stop is acknowledged with stopAfter so the server can shut down', async () => {
  const res = await execCommand(new FakeCdp(), SID, TARGET, { cmd: 'stop', args: [] });
  assert.deepEqual(res, { ok: true, result: '', stopAfter: true });
});

// ---------------------------------------------------------------------------
// sendCommand — CLI side of the NDJSON protocol
// ---------------------------------------------------------------------------

class FakeConn extends EventEmitter {
  constructor() {
    super();
    this.written = [];
    this.ended = false;
  }
  write(s) { this.written.push(s); }
  end(s, cb) {
    if (typeof s === 'string') this.written.push(s);
    this.ended = true;
    if (typeof cb === 'function') cb();
  }
}

test('sendCommand writes {"id":1,...} NDJSON and resolves with the reply', async () => {
  const conn = new FakeConn();
  const p = sendCommand(conn, { cmd: 'eval', args: ['1+1'] });
  // req.id = 1 is assigned onto the request object after construction, so it
  // serializes last — byte-exact assertion of what goes over the wire.
  assert.equal(conn.written[0], '{"cmd":"eval","args":["1+1"],"id":1}\n');

  conn.emit('data', '{"id":1,"ok":true,"result":"2"}\n');
  assert.deepEqual(await p, { id: 1, ok: true, result: '2' });
  assert.equal(conn.ended, true); // CLI closes the connection after one reply
});

test('sendCommand buffers partial chunks until a newline arrives', async () => {
  const conn = new FakeConn();
  const p = sendCommand(conn, { cmd: 'eval', args: [] });
  conn.emit('data', '{"id":1,"ok":tr');
  conn.emit('data', 'ue,"result":"x"}');
  conn.emit('data', '\n{"id":2}');
  assert.deepEqual(await p, { id: 1, ok: true, result: 'x' });
  assert.equal(conn.written.length, 1); // nothing further written
});

test('sendCommand rejects on connection error', async () => {
  const conn = new FakeConn();
  const p = sendCommand(conn, { cmd: 'eval', args: [] });
  const expectation = assert.rejects(p, { code: 'ECONNREFUSED' });
  conn.emit('error', Object.assign(new Error('refused'), { code: 'ECONNREFUSED' }));
  await expectation;
});

test('sendCommand rejects when the connection ends before a reply', async () => {
  const conn = new FakeConn();
  const p = sendCommand(conn, { cmd: 'eval', args: [] });
  const expectation = assert.rejects(p, { message: 'Connection closed before response' });
  conn.emit('end');
  await expectation;
});
