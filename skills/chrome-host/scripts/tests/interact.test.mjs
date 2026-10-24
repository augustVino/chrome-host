// click/clickxy/type/loadall: selector clicks, coordinate click event
// sequence, text input, and the load-more loop.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { clickStr, clickXyStr, typeStr, loadAllStr } from '../lib/tools/interact.mjs';
import { FakeCdp } from './helpers.mjs';

test('clickStr requires a selector', async () => {
  await assert.rejects(clickStr(new FakeCdp(), 's', undefined), { message: 'CSS selector required' });
});

function nativeClickCdp(locateJson, hitJson) {
  return new FakeCdp(async (method, params) => {
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') {
      const value = params.expression.includes('elementFromPoint') ? hitJson : locateJson;
      return { result: { value } };
    }
    return {};
  });
}

test('clickStr clicks natively and reports tag/text plus the dispatch point', async () => {
  const cdp = nativeClickCdp(
    JSON.stringify({ x: 100.5, y: 40, tag: 'BUTTON', text: 'Submit' }),
    JSON.stringify({ hit: true }),
  );
  assert.equal(await clickStr(cdp, 'SID', 'button.primary'),
    'Clicked <BUTTON> "Submit" (native input @100.5,40)');
  const expr = cdp.calls[1].params.expression; // locate eval
  assert.ok(expr.includes('document.querySelector("button.primary")'));
  assert.ok(expr.includes('scrollIntoView'));
  assert.ok(cdp.calls.some(c => c.method === 'Input.dispatchMouseEvent')); // native, not el.click()
});

test('clickStr surfaces "Element not found" from the page', async () => {
  const cdp = nativeClickCdp(JSON.stringify({ error: 'not_found' }), '');
  await assert.rejects(clickStr(cdp, 's', '.gone'),
    { code: 'element_not_found', message: 'Element not found: .gone' });
});

test('clickXyStr rejects non-numeric coordinates', async () => {
  await assert.rejects(clickXyStr(new FakeCdp(), 's', 'abc', '20'),
    { message: 'x and y must be numbers (CSS pixels)' });
  await assert.rejects(clickXyStr(new FakeCdp(), 's', '10', NaN.toString()),
    { message: 'x and y must be numbers (CSS pixels)' });
});

test('clickXyStr dispatches moved→pressed→released with full button state', async () => {
  const cdp = new FakeCdp(async () => ({}));
  assert.equal(await clickXyStr(cdp, 'SID', '10.5', '20'), 'Clicked at CSS (10.5, 20)');

  const events = cdp.callsOf('Input.dispatchMouseEvent');
  assert.deepEqual(events.map(e => e.params.type), ['mouseMoved', 'mousePressed', 'mouseReleased']);
  for (const e of events) {
    assert.equal(e.params.x, 10.5);
    assert.equal(e.params.y, 20);
    assert.equal(e.params.button, 'left');
    assert.equal(e.params.clickCount, 1);
    assert.equal(e.params.modifiers, 0);
    assert.equal(e.sessionId, 'SID');
  }
});

test('typeStr requires non-empty text and inserts it verbatim', async () => {
  await assert.rejects(typeStr(new FakeCdp(), 's', ''), { message: 'text required' });
  await assert.rejects(typeStr(new FakeCdp(), 's', undefined), { message: 'text required' });

  const cdp = new FakeCdp(async () => ({}));
  assert.equal(await typeStr(cdp, 'SID', 'héllo 你好'), 'Typed 8 characters');
  assert.deepEqual(cdp.calls, [{
    method: 'Input.insertText', params: { text: 'héllo 你好' }, sessionId: 'SID',
  }]);
});

test('loadAllStr requires a selector and loops until the element disappears', async () => {
  await assert.rejects(loadAllStr(new FakeCdp(), 's', ''), { message: 'CSS selector required' });

  let existsChecks = 0;
  const cdp = new FakeCdp(async (method, params) => {
    if (method !== 'Runtime.evaluate') return {};
    if (params.expression.startsWith('!!document.querySelector')) {
      existsChecks++;
      return { result: { value: existsChecks <= 2 ? 'true' : 'false' } };
    }
    return { result: { value: 'true' } }; // the click IIFE
  });

  assert.equal(await loadAllStr(cdp, 's', '.load-more', 10),
    'Clicked ".load-more" 2 time(s) until it disappeared');
  assert.equal(existsChecks, 3); // true, true, false
});

test('loadAllStr stops when the element vanishes mid-click', async () => {
  let existsChecks = 0;
  const cdp = new FakeCdp(async (method, params) => {
    if (method !== 'Runtime.evaluate') return {};
    if (params.expression.startsWith('!!document.querySelector')) {
      existsChecks++;
      return { result: { value: 'true' } };
    }
    return { result: { value: 'false' } }; // click reports element gone
  });
  assert.equal(await loadAllStr(cdp, 's', '.x', 10),
    'Clicked ".x" 0 time(s) until it disappeared');
});
