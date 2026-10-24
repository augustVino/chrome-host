// evalStr value coercion + exception surfacing, htmlStr expression shaping,
// netStr line formatting, waitForDocumentReady polling.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { evalStr, htmlStr, netStr, waitForDocumentReady } from '../lib/tools/evaluate.mjs';
import { FakeCdp } from './helpers.mjs';

function cdpReturning(value) {
  return new FakeCdp(async (method) => {
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') return { result: { value } };
    throw new Error('unexpected ' + method);
  });
}

test('evalStr returns primitives as strings', async () => {
  assert.equal(await evalStr(cdpReturning('hello'), 's', "'hello'"), 'hello');
  assert.equal(await evalStr(cdpReturning(42), 's', '42'), '42');
  assert.equal(await evalStr(cdpReturning(false), 's', 'false'), 'false');
  // null is typeof 'object' → JSON round-trip; undefined → empty string.
  assert.equal(await evalStr(cdpReturning(null), 's', 'null'), 'null');
  assert.equal(await evalStr(cdpReturning(undefined), 's', 'undefined'), '');
});

test('evalStr returns objects as 2-space JSON', async () => {
  assert.equal(await evalStr(cdpReturning({ a: [1, 2] }), 's', 'x'),
    JSON.stringify({ a: [1, 2] }, null, 2));
});

test('evalStr throws exceptionDetails.text (or description fallback)', async () => {
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Runtime.evaluate') return { exceptionDetails: { text: 'ReferenceError: x is not defined' } };
    return {};
  });
  await assert.rejects(evalStr(cdp, 's', 'x()'), { message: 'ReferenceError: x is not defined' });

  const cdp2 = new FakeCdp(async (method) => {
    if (method === 'Runtime.evaluate')
      return { exceptionDetails: { exception: { description: 'TypeError: boom\n    at <anonymous>' } } };
    return {};
  });
  await assert.rejects(evalStr(cdp2, 's', 'x'), /TypeError: boom/);
});

test('evalStr enables Runtime and evaluates with awaitPromise on the session', async () => {
  const cdp = cdpReturning(1);
  await evalStr(cdp, 'SID-9', '1+1');
  assert.deepEqual(cdp.calls.map(c => c.method), ['Runtime.enable', 'Runtime.evaluate']);
  assert.ok(cdp.calls.every(c => c.sessionId === 'SID-9'));
  assert.deepEqual(cdp.calls[1].params, {
    expression: '1+1', returnByValue: true, awaitPromise: true,
  });
});

test('htmlStr builds the selector expression or falls back to the whole document', async () => {
  const withSel = cdpReturning('<div id="x"/>');
  await htmlStr(withSel, 's', '#x');
  const expr = withSel.calls[1].params.expression;
  assert.equal(expr, `document.querySelector("#x")?.outerHTML || 'Element not found'`);

  const noSel = cdpReturning('<html/>');
  await htmlStr(noSel, 's', undefined);
  assert.equal(noSel.calls[1].params.expression, 'document.documentElement.outerHTML');
});

test('netStr formats duration/size/type/name with fixed padding', async () => {
  const entries = [
    { name: 'https://a.example/api/v1/very-long-url-that-gets-cut', type: 'fetch', duration: 1234.6, size: 2048 },
    { name: 'https://a.example/pixel.gif', type: 'other', duration: 5.2, size: 0 }, // size 0 → '?'
  ];
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Runtime.evaluate') {
      // Mirror the page-side mapping the real expression performs.
      return { result: { value: JSON.stringify(entries.map(e => ({
        name: e.name.substring(0, 120), type: e.type,
        duration: Math.round(e.duration), size: e.size,
      }))) } };
    }
    return {};
  });

  assert.equal(await netStr(cdp, 's'), [
    ` 1235ms      2048B  fetch     https://a.example/api/v1/very-long-url-that-gets-cut`,
    `    5ms         ?B  other     https://a.example/pixel.gif`,
  ].join('\n'));
});

test('waitForDocumentReady returns as soon as readyState is complete', async () => {
  const cdp = cdpReturning('complete');
  await waitForDocumentReady(cdp, 's', 5000);
  assert.equal(cdp.callsOf('Runtime.evaluate').length, 1);
});

test('waitForDocumentReady polls every 200ms until complete', async () => {
  let polls = 0;
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Runtime.evaluate') {
      polls++;
      return { result: { value: polls >= 2 ? 'complete' : 'loading' } };
    }
    return {};
  });
  await waitForDocumentReady(cdp, 's', 5000);
  assert.equal(polls, 2);
});

test('waitForDocumentReady reports last readyState on timeout', async () => {
  const cdp = cdpReturning('loading');
  await assert.rejects(waitForDocumentReady(cdp, 's', 300),
    { message: 'Timed out waiting for navigation to finish (last readyState: loading)' });
});

test('waitForDocumentReady reports the eval error when every poll failed', async () => {
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Runtime.evaluate') throw new Error('Cannot find context');
    return {};
  });
  await assert.rejects(waitForDocumentReady(cdp, 's', 300),
    { message: 'Timed out waiting for navigation to finish (Cannot find context)' });
});
