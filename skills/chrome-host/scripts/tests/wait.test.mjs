// wait: duration parsing, the poll state machine (state transitions,
// timeouts, eval-failure handling), argument validation, and daemon flag
// dispatch.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { waitStr, parseDuration, waitExpr } from '../lib/tools/wait.mjs';
import { sleep } from '../lib/paths.mjs';
import { execCommand } from '../lib/daemon.mjs';
import { FakeCdp } from './helpers.mjs';

const SID = 'SESSION-1';

// ---------------------------------------------------------------------------
// parseDuration
// ---------------------------------------------------------------------------

test('parseDuration: bare numbers are ms; units ms/s/m scale; decimals ok', () => {
  assert.equal(parseDuration('10s'), 10_000);
  assert.equal(parseDuration('250ms'), 250);
  assert.equal(parseDuration('500'), 500);
  assert.equal(parseDuration(500), 500);
  assert.equal(parseDuration('1.5s'), 1500);
  assert.equal(parseDuration('2m'), 120_000);
  assert.equal(parseDuration(' 10s '), 10_000);
});

test('parseDuration: garbage and negatives throw with the flag name', () => {
  for (const bad of ['abc', '-5', '10x', '', 's', undefined, NaN]) {
    assert.throws(() => parseDuration(bad, '--timeout'), (err) =>
      err.message.includes('--timeout') && err.message.includes('invalid duration'), `expected reject: ${bad}`);
  }
});

// ---------------------------------------------------------------------------
// the poll expression
// ---------------------------------------------------------------------------

test('waitExpr observes attachment and the locateExpr visibility semantics', () => {
  const expr = waitExpr('.x');
  assert.ok(expr.includes('document.querySelector(".x")'));
  assert.ok(expr.includes('getBoundingClientRect'));
  assert.ok(expr.includes("style.display !== 'none'"));
  assert.ok(expr.includes("style.visibility !== 'hidden'"));
  assert.ok(expr.includes('detached'));
});

// ---------------------------------------------------------------------------
// state machine (injected sleepFn)
// ---------------------------------------------------------------------------

function waitCdp(observations) {
  let i = 0;
  return new FakeCdp(async (method) => {
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') {
      const o = observations[Math.min(i++, observations.length - 1)];
      if (o === 'THROW') throw new Error('Cannot find context with specified id');
      return { result: { value: JSON.stringify({ observed: o }) } };
    }
    return {};
  });
}

function fakeSleep() {
  const sleeps = [];
  return { sleeps, sleepFn: (ms) => { sleeps.push(ms); return Promise.resolve(); } };
}

test('waitStr: satisfied on the first poll → no sleep, message has elapsed/polls', async () => {
  const { sleeps, sleepFn } = fakeSleep();
  const r = await waitStr(waitCdp(['visible']), SID, '.x', { sleepFn });
  assert.match(r, /^Waited: \.x is visible \(after \d+ms, 1 polls\)$/);
  assert.deepEqual(sleeps, []);
});

test('waitStr: polls at the interval until the state appears', async () => {
  const { sleeps, sleepFn } = fakeSleep();
  const r = await waitStr(waitCdp(['hidden', 'hidden', 'visible']), SID, '.x',
    { state: 'visible', intervalMs: 250, sleepFn });
  assert.match(r, /3 polls/);
  assert.deepEqual(sleeps, [250, 250]); // sleep between polls only
});

test('waitStr: attached accepts any attached element; detached accepts absence', async () => {
  assert.match(
    await waitStr(waitCdp(['hidden']), SID, '.x', { state: 'attached', sleepFn: fakeSleep().sleepFn }),
    /is attached/);
  assert.match(
    await waitStr(waitCdp(['detached']), SID, '.x', { state: 'detached', sleepFn: fakeSleep().sleepFn }),
    /is detached/);
});

test('waitStr: hidden requires ATTACHED — detached keeps polling to timeout', async () => {
  await assert.rejects(
    waitStr(waitCdp(['detached']), SID, '.x',
      { state: 'hidden', timeoutMs: 30, intervalMs: 5, sleepFn: sleep }), // real short sleeps
    (err) => err.code === 'wait_timeout' && /last observed: detached/.test(err.message));
});

test('waitStr: timeout reports the last observed state', async () => {
  await assert.rejects(
    waitStr(waitCdp(['hidden']), SID, '.loading',
      { state: 'visible', timeoutMs: 30, intervalMs: 5, sleepFn: sleep }),
    (err) => err.code === 'wait_timeout'
      && /\.loading.*visible/.test(err.message)
      && /last observed: hidden/.test(err.message)
      && /\d+ polls/.test(err.message));
});

test('waitStr: eval failures (navigation in flight) count as detached', async () => {
  // Waiting for detached: the eval error satisfies immediately.
  assert.match(
    await waitStr(waitCdp(['THROW']), SID, '.x', { state: 'detached', sleepFn: fakeSleep().sleepFn }),
    /is detached/);
  // Waiting for attached: the eval error must NOT satisfy — keep polling.
  await assert.rejects(
    waitStr(waitCdp(['THROW']), SID, '.x',
      { state: 'attached', timeoutMs: 30, intervalMs: 5, sleepFn: sleep }),
    (err) => err.code === 'wait_timeout' && /last observed: detached/.test(err.message));
});

test('waitStr: validation — selector, state whitelist, positive interval', async () => {
  const cdp = waitCdp(['visible']);
  await assert.rejects(waitStr(cdp, SID, '', { sleepFn: fakeSleep().sleepFn }), /CSS selector required/);
  await assert.rejects(
    waitStr(cdp, SID, '.x', { state: 'bogus', sleepFn: fakeSleep().sleepFn }),
    /--state must be one of visible\|hidden\|attached\|detached/);
  await assert.rejects(
    waitStr(cdp, SID, '.x', { intervalMs: 0, sleepFn: fakeSleep().sleepFn }),
    /--interval must be a positive/);
});

// ---------------------------------------------------------------------------
// daemon dispatch (flag parsing)
// ---------------------------------------------------------------------------

test('execCommand wait parses --state/--timeout/--interval (units included)', async () => {
  const cdp = waitCdp(['detached']);
  const res = await execCommand(cdp, SID, 'T',
    { cmd: 'wait', args: ['.x', '--state', 'detached', '--timeout', '100ms', '--interval', '5ms'] });
  assert.equal(res.ok, true);
  assert.match(res.result, /^Waited: \.x is detached \(after \d+ms, 1 polls\)$/);
});

test('execCommand wait: unknown state / flag garbage surface as errors', async () => {
  const bad = await execCommand(waitCdp(['visible']), SID, 'T',
    { cmd: 'wait', args: ['.x', '--state', 'bogus'] });
  assert.equal(bad.ok, false);
  assert.match(bad.error, /--state/);

  const extra = await execCommand(waitCdp(['visible']), SID, 'T',
    { cmd: 'wait', args: ['.x', '.y'] });
  assert.equal(extra.ok, false);
  assert.match(extra.error, /unexpected argument/);
});
