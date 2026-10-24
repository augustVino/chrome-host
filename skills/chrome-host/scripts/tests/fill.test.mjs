// fill: the fillability gate (pure DOM-shape function), the unified
// objectId pipeline (check → focus → clear → insertText → verify), ref vs
// selector target routing, and the coded failure branches.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fillStr, fillEditable } from '../lib/tools/fill.mjs';
import { RefStore } from '../lib/ref-store.mjs';
import { execCommand } from '../lib/daemon.mjs';
import { FakeCdp } from './helpers.mjs';

const SID = 'SESSION-1';

// ---------------------------------------------------------------------------
// fillEditable on DOM-shaped fakes (same style as hitTestWalk's node fakes)
// ---------------------------------------------------------------------------

function el(tagName, props = {}) {
  const e = {
    tagName,
    isConnected: true,
    readOnly: false,
    matches: (s) => s === ':disabled' && props.disabled === true,
    getAttribute: (n) => (props[n] !== undefined ? props[n] : null),
  };
  return Object.assign(e, props);
}

test('fillEditable: text-like inputs, textarea, and editor roots pass (with kind)', () => {
  assert.deepEqual(fillEditable(el('INPUT', { type: 'text' })), { editable: true, kind: 'native' });
  assert.deepEqual(fillEditable(el('INPUT')), { editable: true, kind: 'native' }); // no type attr → text
  assert.deepEqual(fillEditable(el('INPUT', { type: 'NUMBER' })), { editable: true, kind: 'native' }); // case-folded
  assert.deepEqual(fillEditable(el('TEXTAREA')), { editable: true, kind: 'native' });
  assert.deepEqual(
    fillEditable(el('DIV', { isContentEditable: true, parentElement: el('BODY') })),
    { editable: true, kind: 'editable' });
});

test('fillEditable: readonly / disabled / wrong type / non-root contenteditable reject with a why', () => {
  assert.match(fillEditable(el('INPUT', { type: 'text', readOnly: true })).why, /readonly/);
  assert.match(fillEditable(el('INPUT', { type: 'text', disabled: true })).why, /disabled/);
  assert.match(fillEditable(el('INPUT', { type: 'checkbox' })).why, /checkbox/);
  // Inner paragraph of an editing host: parent is contenteditable → reject.
  const host = el('DIV', { isContentEditable: true });
  assert.match(
    fillEditable(el('P', { isContentEditable: true, parentElement: host })).why,
    /editor root/);
  assert.match(fillEditable(el('BUTTON')).why, /not a text input/);
  assert.match(fillEditable({ isConnected: false }).why, /not in the document/);
});

// ---------------------------------------------------------------------------
// Pipeline fakes: the three Runtime.callFunctionOn scripts are told apart
// by unique markers (clear dispatches an event, verify checks isConnected).
// ---------------------------------------------------------------------------

function fillCdp(overrides = {}) {
  return new FakeCdp(async (method, params) => {
    if (method === 'DOM.getDocument') return overrides.document ?? { root: { nodeId: 1 } };
    if (method === 'DOM.querySelector') return overrides.query ?? { nodeId: 7 };
    if (method === 'DOM.resolveNode') return overrides.resolve ?? { object: { objectId: 'OBJ-1' } };
    if (method === 'DOM.scrollIntoViewIfNeeded') return {};
    if (method === 'DOM.focus') {
      if (overrides.focusThrows) throw new Error('cannot focus');
      return {};
    }
    if (method === 'Runtime.callFunctionOn') {
      // Script markers must be unique per script: clear dispatches an event,
      // verify takes multiLine/singleLine args. (NOT 'isConnected' — every
      // script inlines fillEditable, whose source mentions isConnected.)
      const fn = params.functionDeclaration;
      if (fn.includes('dispatchEvent')) return { result: { value: overrides.clear ?? { cleared: true } } };
      if (fn.includes('multiLine')) return { result: { value: overrides.verify ?? { matches: true, actual: 'hi', expected: 'hi' } } };
      return { result: { value: overrides.check ?? { tag: 'INPUT', kind: 'native', label: 'q' } } };
    }
    if (method === 'Input.insertText') return {};
    if (method === 'Runtime.releaseObject') return {};
    throw new Error('unexpected ' + method);
  });
}

function stockedStore() {
  const store = new RefStore();
  store.replace([{ ref: 'e1', backendNodeId: 101, name: 'Search' }]);
  return store;
}

test('fillStr (selector): full pipeline order, insertText payload, object handle reuse', async () => {
  const cdp = fillCdp();
  assert.equal(await fillStr(cdp, SID, '#q', 'hi', new RefStore()),
    'Filled <INPUT> "q" with "hi" (2 chars)');

  assert.deepEqual(cdp.calls.map(c => c.method), [
    'DOM.getDocument', 'DOM.querySelector', 'DOM.resolveNode',
    'DOM.scrollIntoViewIfNeeded',
    'Runtime.callFunctionOn', 'DOM.focus', 'Runtime.callFunctionOn',
    'Input.insertText', 'Runtime.callFunctionOn', 'Runtime.releaseObject',
  ]);
  assert.equal(cdp.callsOf('DOM.querySelector')[0].params.selector, '#q');
  assert.deepEqual(cdp.callsOf('Input.insertText')[0].params, { text: 'hi' });
  // Identity stability: every page script runs on the SAME objectId (a
  // re-queried selector could silently hit a different element).
  for (const c of cdp.callsOf('Runtime.callFunctionOn')) assert.equal(c.params.objectId, 'OBJ-1');
  for (const c of [...cdp.callsOf('DOM.scrollIntoViewIfNeeded'), ...cdp.callsOf('DOM.focus')]) {
    assert.equal(c.params.objectId, 'OBJ-1');
  }
});

test('fillStr (@eN ref): resolves via the store backendNodeId, prefers the snap name', async () => {
  const cdp = fillCdp();
  assert.equal(await fillStr(cdp, SID, '@e1', 'hi', stockedStore()),
    'Filled <INPUT> "Search" with "hi" (2 chars)');
  assert.deepEqual(cdp.callsOf('DOM.resolveNode')[0].params, { backendNodeId: 101 });
  assert.equal(cdp.callsOf('DOM.getDocument').length, 0); // ref path skips DOM queries
});

test('fillStr: not fillable → target_not_fillable, nothing typed', async () => {
  const cdp = fillCdp({ check: { not_fillable: 'readonly' } });
  await assert.rejects(fillStr(cdp, SID, '#q', 'hi', new RefStore()),
    { code: 'target_not_fillable', message: /#q.*not fillable: readonly/ });
  assert.equal(cdp.callsOf('Input.insertText').length, 0);
});

test('fillStr: selector matches nothing → element_not_found (same code as click)', async () => {
  const cdp = fillCdp({ query: { nodeId: 0 } });
  await assert.rejects(fillStr(cdp, SID, '.gone', 'hi', new RefStore()),
    { code: 'element_not_found', message: 'Element not found: .gone' });
});

test('fillStr: DOM.focus failure re-checks fillability (SPEC step 3 fallback)', async () => {
  // Focus throws but the element is still editable → focus failure.
  await assert.rejects(
    fillStr(fillCdp({ focusThrows: true }), SID, '#q', 'hi', new RefStore()),
    { code: 'fill_focus_lost', message: /could not be focused/ });

  // Focus throws because the page disabled the element mid-flight → the
  // recheck reclassifies as target_not_fillable (Chrome refuses DOM.focus
  // on disabled controls).
  await assert.rejects(
    fillStr(fillCdp({ focusThrows: true, check: { not_fillable: 'disabled' } }), SID, '#q', 'hi', new RefStore()),
    { code: 'target_not_fillable' });
});

test('fillStr: focus lost between focus and clear → fill_focus_lost, nothing typed', async () => {
  const cdp = fillCdp({ clear: { focus_lost: true } });
  await assert.rejects(fillStr(cdp, SID, '#q', 'hi', new RefStore()),
    { code: 'fill_focus_lost', message: /does not have focus/ });
  assert.equal(cdp.callsOf('Input.insertText').length, 0);
});

test('fillStr: value mismatch → fill_value_mismatch with expected/actual summary', async () => {
  const cdp = fillCdp({ verify: { matches: false, actual: 'hel', expected: 'hello' } });
  await assert.rejects(fillStr(cdp, SID, '#q', 'hello', new RefStore()),
    { code: 'fill_value_mismatch', message: /expected "hello", got "hel"/ });
});

test('fillStr: mismatch summary truncates both sides to 40 chars', async () => {
  const text = 'x'.repeat(60);
  const cdp = fillCdp({ verify: { matches: false, actual: 'y'.repeat(60), expected: text } });
  const err = await fillStr(cdp, SID, '#q', text, new RefStore()).then(() => null, (e) => e);
  assert.equal(err.code, 'fill_value_mismatch');
  assert.ok(err.message.includes(`"${'x'.repeat(40)}…"`), 'expected side truncated');
  assert.ok(err.message.includes(`"${'y'.repeat(40)}…"`), 'actual side truncated');
});

test('fillStr: element removed during fill → fill_value_mismatch (write may have applied)', async () => {
  const cdp = fillCdp({ verify: { gone: true } });
  await assert.rejects(fillStr(cdp, SID, '#q', 'hi', new RefStore()),
    { code: 'fill_value_mismatch', message: /removed or replaced during fill/ });
});

test('fillStr: empty text = clear-only (no insertText), success message still verifies', async () => {
  const cdp = fillCdp({ verify: { matches: true, actual: '', expected: '' } });
  assert.equal(await fillStr(cdp, SID, '#q', '', new RefStore()),
    'Filled <INPUT> "q" with "" (0 chars)');
  assert.equal(cdp.callsOf('Input.insertText').length, 0);
  const verifyArgs = cdp.callsOf('Runtime.callFunctionOn').at(-1).params.arguments;
  assert.deepEqual(verifyArgs, [{ value: '' }, { value: '' }]);
});

test('fillStr: CRLF normalized for insertText; single-line variant precomputed for verify', async () => {
  const cdp = fillCdp();
  await fillStr(cdp, SID, '#q', 'a\r\nb', new RefStore());
  assert.deepEqual(cdp.callsOf('Input.insertText')[0].params, { text: 'a\nb' });
  const verifyArgs = cdp.callsOf('Runtime.callFunctionOn').at(-1).params.arguments;
  assert.deepEqual(verifyArgs, [{ value: 'a\nb' }, { value: 'a b' }]);
});

test('fillStr: text must be a string (empty allowed)', async () => {
  await assert.rejects(fillStr(fillCdp(), SID, '#q', undefined, new RefStore()),
    /text required/);
});

test('fillStr: unknown ref → ref_not_found; stale table → ref_stale; dead node → target_moved', async () => {
  const store = stockedStore();
  await assert.rejects(fillStr(fillCdp(), SID, '@e9', 'hi', store), /ref_not_found/);
  store.invalidate('navigation');
  await assert.rejects(fillStr(fillCdp(), SID, '@e1', 'hi', store), /ref_stale/);

  const deadNode = fillCdp({ resolve: {} }); // no objectId
  await assert.rejects(fillStr(deadNode, SID, '@e1', 'hi', stockedStore()),
    /target_moved: ref no longer resolves/);
});

// ---------------------------------------------------------------------------
// daemon dispatch
// ---------------------------------------------------------------------------

test('execCommand fill routes through the daemon ref store', async () => {
  const res = await execCommand(fillCdp(), SID, 'T', { cmd: 'fill', args: ['@e1', 'hi'] }, undefined, stockedStore());
  assert.equal(res.ok, true);
  assert.equal(res.result, 'Filled <INPUT> "Search" with "hi" (2 chars)');
});

test('execCommand fill surfaces coded errors with the code field', async () => {
  const res = await execCommand(fillCdp({ check: { not_fillable: 'readonly' } }), SID, 'T',
    { cmd: 'fill', args: ['#q', 'hi'] }, undefined, new RefStore());
  assert.equal(res.ok, false);
  assert.equal(res.code, 'target_not_fillable');
});
