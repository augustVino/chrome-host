// click/clickxy/type/loadall: selector clicks, coordinate click event
// sequence, text input, and the load-more loop.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  clickStr, clickXyStr, typeStr, loadAllStr, isRefTarget, clickRefStr, resolveRefPoint,
} from '../lib/tools/interact.mjs';
import { RefStore } from '../lib/ref-store.mjs';
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

// ---------------------------------------------------------------------------
// @eN ref routing + resolution chain (T5)
// ---------------------------------------------------------------------------

test('isRefTarget: @-prefixed or bare eN tokens, nothing else', () => {
  assert.equal(isRefTarget('@e3'), true);
  assert.equal(isRefTarget('e3'), true);
  assert.equal(isRefTarget('@E3'), true); // routed to ref path to fail case-sensitively
  assert.equal(isRefTarget('E3'), false);
  assert.equal(isRefTarget('button.primary'), false);
  assert.equal(isRefTarget('#main .btn'), false);
  assert.equal(isRefTarget(undefined), false);
  assert.equal(isRefTarget(''), false);
});

// FakeCdp answering the ref geometry chain: scroll → quads → native click.
function refChainCdp(quad = [0, 0, 10, 0, 10, 10, 0, 10]) {
  return new FakeCdp(async (method) => {
    if (method === 'DOM.scrollIntoViewIfNeeded') return {};
    if (method === 'DOM.getContentQuads') return { quads: [quad] };
    if (method === 'Input.dispatchMouseEvent') return {};
    throw new Error('unexpected ' + method);
  });
}

function stockedStore() {
  const store = new RefStore();
  store.replace([{ ref: 'e1', backendNodeId: 101, name: 'OK' }]);
  return store;
}

test('clickStr routes @eN through the ref chain and clicks the quad centroid', async () => {
  const cdp = refChainCdp();
  const store = stockedStore();

  assert.equal(await clickStr(cdp, 'SID', '@e1', store), 'Clicked @e1 "OK" at CSS (5, 5)');

  // Geometry asked for the mapped backendNodeId, not a DOM query.
  assert.deepEqual(cdp.callsOf('DOM.getContentQuads')[0].params, { backendNodeId: 101 });
  assert.equal(cdp.callsOf('DOM.getContentQuads')[0].sessionId, 'SID');
  // Native click sequence dispatched at the resolved point.
  const events = cdp.callsOf('Input.dispatchMouseEvent');
  assert.deepEqual(events.map(e => e.params.type), ['mouseMoved', 'mousePressed', 'mouseReleased']);
  for (const e of events) assert.equal(e.params.x, 5);
});

test('bare e3 and @e3 both resolve; selector-looking strings never enter the ref path', async () => {
  const store = stockedStore();
  store.replace([{ ref: 'e3', backendNodeId: 31 }]);

  const refCdp = refChainCdp();
  await clickStr(refCdp, 'SID', 'e3', store); // bare form
  assert.equal(refCdp.callsOf('Input.dispatchMouseEvent').length, 3);

  const selCdp = nativeClickCdp(
    JSON.stringify({ x: 10, y: 10, tag: 'A', text: 'x' }),
    JSON.stringify({ hit: true }),
  );
  await clickStr(selCdp, 'SID', 'a.link', store); // CSS path untouched by the store
  const locateCall = selCdp.calls.find(c => c.method === 'Runtime.evaluate');
  assert.ok(locateCall.params.expression.includes('document.querySelector'));
  assert.ok(selCdp.calls.some(c => c.method === 'Input.dispatchMouseEvent'));
});

test('ref click picks the LARGEST quad (occlusion/transform leftovers discarded)', async () => {
  const cdp = new FakeCdp(async (method) => {
    if (method === 'DOM.scrollIntoViewIfNeeded') return {};
    if (method === 'DOM.getContentQuads') {
      return { quads: [[0, 0, 1, 0, 1, 1, 0, 1], [100, 100, 200, 100, 200, 160, 100, 160]] };
    }
    return {};
  });
  // Largest quad = 100x60 at (100,100) → centroid (150,130); the 1x1 junk
  // quad (zero-visibility leftovers) must lose.
  assert.deepEqual(await resolveRefPoint(cdp, 'SID', 1), { x: 150, y: 130 });
});
test('resolveRefPoint falls back to getBoundingClientRect when quads fail', async () => {
  const cdp = new FakeCdp(async (method) => {
    if (method === 'DOM.scrollIntoViewIfNeeded') return {};
    if (method === 'DOM.getContentQuads') return { quads: [] }; // empty → fallback
    if (method === 'DOM.resolveNode') return { object: { objectId: 'OBJ-1' } };
    if (method === 'Runtime.callFunctionOn') {
      return { result: { value: { x: 0, y: 0, w: 20, h: 10 } } };
    }
    throw new Error('unexpected ' + method);
  });
  assert.deepEqual(await resolveRefPoint(cdp, 'SID', 101), { x: 10, y: 5 });
  assert.equal(cdp.callsOf('Runtime.callFunctionOn')[0].params.objectId, 'OBJ-1');
});

test('ref resolution failure → target_moved with a re-snap hint', async () => {
  const cdp = new FakeCdp(async (method) => {
    if (method === 'DOM.scrollIntoViewIfNeeded') return {};
    if (method === 'DOM.getContentQuads') throw new Error('No node with given id');
    if (method === 'DOM.resolveNode') return {}; // no objectId either
    throw new Error('unexpected ' + method);
  });
  await assert.rejects(resolveRefPoint(cdp, 'SID', 101),
    /target_moved: .*run "snap <target>" again/);
});

test('unknown ref → ref_not_found; stale table → ref_stale; both suggest re-snap', async () => {
  const store = stockedStore();

  await assert.rejects(clickRefStr(refChainCdp(), 'SID', '@e9', store),
    /ref_not_found: @e9 .*run "snap <target>"/);

  store.invalidate('navigation');
  await assert.rejects(clickRefStr(refChainCdp(), 'SID', '@e1', store),
    /ref_stale: page changed since the last snap \(navigation\) — run "snap <target>"/);
});

test('@E3 is case-sensitive: routes to the ref path, then misses the table', async () => {
  const store = stockedStore();
  const cdp = refChainCdp();
  await assert.rejects(clickStr(cdp, 'SID', '@E3', store), /ref_not_found/);
  assert.equal(cdp.callsOf('Input.dispatchMouseEvent').length, 0); // never clicked
});

test('ref click without a store fails with ref_not_found, not a TypeError', async () => {
  await assert.rejects(clickStr(refChainCdp(), 'SID', '@e1', undefined),
    { message: 'ref_not_found: no ref table — run "snap <target>" first' });
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
