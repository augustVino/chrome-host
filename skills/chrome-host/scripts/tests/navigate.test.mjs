// navStr: URL validation must happen before any CDP traffic; happy path
// waits for Page.loadEventFired + document ready.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { navStr } from '../lib/tools/navigate.mjs';
import { FakeCdp } from './helpers.mjs';

function neverSentCdp() {
  return new FakeCdp(() => { throw new Error('cdp.send must not be called'); });
}

test('rejects non-http(s) schemes before touching CDP', async () => {
  const cdp = neverSentCdp();
  await assert.rejects(navStr(cdp, 's', 'ftp://example.com/x'), {
    message: 'Only http/https URLs allowed, got: ftp://example.com/x',
  });
  await assert.rejects(navStr(cdp, 's', 'file:///etc/passwd'), /Only http\/https URLs allowed/);
  assert.equal(cdp.calls.length, 0);
  assert.equal(cdp.eventWaiters.length, 0);
});

test('rejects unparseable URLs before touching CDP', async () => {
  const cdp = neverSentCdp();
  await assert.rejects(navStr(cdp, 's', 'not a url'), { message: 'Invalid URL: not a url' });
  assert.equal(cdp.calls.length, 0);
});

function navCdp(navigateResult, loadEvent) {
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Page.enable') return {};
    if (method === 'Page.navigate') return navigateResult;
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') return { result: { value: 'complete' } };
    throw new Error('unexpected ' + method);
  });
  cdp.waitForEvent = () => loadEvent;
  return cdp;
}

test('navigates, awaits loadEventFired, then waits for readyState', async () => {
  let canceled = false;
  const cdp = navCdp({ loaderId: 'L1' }, { promise: Promise.resolve({}), cancel() { canceled = true; } });

  assert.equal(await navStr(cdp, 'SID-7', 'https://example.com/page'), 'Navigated to https://example.com/page');
  assert.equal(canceled, false); // load event was needed → not canceled
  assert.deepEqual(cdp.calls.map(c => c.method),
    ['Page.enable', 'Page.navigate', 'Runtime.enable', 'Runtime.evaluate']);
  assert.ok(cdp.calls.every(c => c.sessionId === 'SID-7'));
  assert.deepEqual(cdp.calls[1].params, { url: 'https://example.com/page' });
});

test('surface Page.navigate errorText and cancels the load-event wait', async () => {
  let canceled = false;
  const cdp = navCdp({ errorText: 'net::ERR_NAME_NOT_RESOLVED' },
    { promise: new Promise(() => {}), cancel() { canceled = true; } });
  await assert.rejects(navStr(cdp, 's', 'https://missing.example'),
    { message: 'net::ERR_NAME_NOT_RESOLVED' });
  assert.equal(canceled, true);
});

test('same-document navigation (no loaderId) cancels the load-event wait', async () => {
  let canceled = false;
  const cdp = navCdp({}, { promise: new Promise(() => {}), cancel() { canceled = true; } });
  await navStr(cdp, 's', 'https://example.com/#anchor');
  assert.equal(canceled, true);
});
