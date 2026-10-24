// evalRawStr: passthrough validation + JSON formatting of the raw result.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { evalRawStr } from '../lib/tools/raw.mjs';
import { FakeCdp } from './helpers.mjs';

test('requires a CDP method', async () => {
  await assert.rejects(evalRawStr(new FakeCdp(), 's', undefined),
    { message: 'CDP method required (e.g. "DOM.getDocument")' });
});

test('rejects non-JSON params with the offending text', async () => {
  await assert.rejects(evalRawStr(new FakeCdp(), 's', 'DOM.getDocument', '{bad'),
    { message: 'Invalid JSON params: {bad' });
});

test('sends parsed params on the session and formats the result', async () => {
  const cdp = new FakeCdp(async () => ({ root: { nodeId: 1, childCount: 3 } }));
  const out = await evalRawStr(cdp, 'SID', 'DOM.getDocument', '{"depth":1}');
  assert.equal(out, JSON.stringify({ root: { nodeId: 1, childCount: 3 } }, null, 2));
  assert.deepEqual(cdp.calls, [{
    method: 'DOM.getDocument', params: { depth: 1 }, sessionId: 'SID',
  }]);
});

test('omits params entirely when none given', async () => {
  const cdp = new FakeCdp(async () => ({}));
  const out = await evalRawStr(cdp, undefined, 'Target.getTargets');
  assert.equal(out, '{}');
  assert.deepEqual(cdp.calls[0].params, {});
});
