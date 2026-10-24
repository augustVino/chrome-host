// press: key name → descriptor resolution (named keys, single chars,
// modifier combos), the rawKeyDown/char/keyUp dispatch sequence with text
// suppression, stuck-key compensation, and --target focus routing.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { keyDescriptor, pressStr } from '../lib/tools/press.mjs';
import { RefStore } from '../lib/ref-store.mjs';
import { execCommand } from '../lib/daemon.mjs';
import { FakeCdp } from './helpers.mjs';

const SID = 'SESSION-1';

// ---------------------------------------------------------------------------
// keyDescriptor: the key table (pure function)
// ---------------------------------------------------------------------------

test('keyDescriptor: named keys carry key/code/text/vk from the SPEC table', () => {
  assert.deepEqual(
    { ...keyDescriptor('Enter'), modifierNames: [] },
    { key: 'Enter', code: 'Enter', text: '\r', vk: 13, modifiers: 0, modifierNames: [] });
  assert.deepEqual(
    { ...keyDescriptor('Tab') },
    { key: 'Tab', code: 'Tab', text: '\t', vk: 9, modifiers: 0, modifierNames: [] });
  assert.equal(keyDescriptor('Space').key, ' ');
  assert.equal(keyDescriptor('Space').code, 'Space');
  assert.equal(keyDescriptor('Space').text, ' ');
  assert.equal(keyDescriptor('Escape').text, undefined); // produces no char
  assert.equal(keyDescriptor('F1').vk, 112);
  assert.equal(keyDescriptor('F12').vk, 123);
  assert.equal(keyDescriptor('ArrowDown').vk, 40);
});

test('keyDescriptor: named-key lookup is case-insensitive', () => {
  assert.equal(keyDescriptor('enter').key, 'Enter');
  assert.equal(keyDescriptor('PAGEUP').code, 'PageUp');
  assert.equal(keyDescriptor('Escape').key, 'Escape');
});

test('keyDescriptor: single characters map to Key*/Digit* codes with text', () => {
  assert.deepEqual(
    keyDescriptor('a'),
    { key: 'a', code: 'KeyA', text: 'a', vk: 65, modifiers: 0, modifierNames: [] });
  assert.deepEqual(
    keyDescriptor('A'),
    { key: 'A', code: 'KeyA', text: 'A', vk: 65, modifiers: 0, modifierNames: [] });
  assert.deepEqual(
    keyDescriptor('7'),
    { key: '7', code: 'Digit7', text: '7', vk: 55, modifiers: 0, modifierNames: [] });
  // Punctuation: no standard code — text carries the char instead.
  assert.deepEqual(
    keyDescriptor(','),
    { key: ',', code: '', text: ',', modifiers: 0, modifierNames: [] });
  assert.deepEqual(keyDescriptor('+').text, '+'); // bare '+' still one keypress
});

test('keyDescriptor: modifier combos set the bit field (alt=1 ctrl=2 meta=4 shift=8)', () => {
  assert.equal(keyDescriptor('Control+A').modifiers, 2);
  assert.equal(keyDescriptor('Meta+V').modifiers, 4);
  assert.equal(keyDescriptor('Shift+ArrowDown').modifiers, 8);
  assert.equal(keyDescriptor('Alt+F4').modifiers, 1);
  assert.equal(keyDescriptor('Alt+F4').key, 'F4');
  // Aliases fold to canonical names; duplicates collapse.
  assert.deepEqual(keyDescriptor('ctrl+a').modifierNames, ['Ctrl']);
  assert.deepEqual(keyDescriptor('CMD+a').modifierNames, ['Meta']);
  assert.deepEqual(keyDescriptor('option+enter').modifierNames, ['Alt']);
  assert.deepEqual(keyDescriptor('super+a').modifierNames, ['Meta']);
  assert.equal(keyDescriptor('Control+Ctrl+A').modifiers, 2);
});

test('keyDescriptor: unknown keys → key_unknown with the supported-key hint', () => {
  for (const bad of ['Foo', 'Control', 'Control+Shift', 'A+B', '']) {
    assert.throws(() => keyDescriptor(bad), (err) => {
      assert.equal(err.code, 'key_unknown');
      assert.match(err.message, /Enter/); // hint lists the named keys
      return true;
    }, `expected key_unknown for ${JSON.stringify(bad)}`);
  }
});

// ---------------------------------------------------------------------------
// dispatch sequence
// ---------------------------------------------------------------------------

function pressCdp({ throwOnChar = false, focusJson } = {}) {
  return new FakeCdp(async (method, params) => {
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') return { result: { value: focusJson } };
    if (method === 'Input.dispatchKeyEvent') {
      if (throwOnChar && params.type === 'char') throw new Error('char dispatch failed');
      return {};
    }
    return {};
  });
}

test('pressStr: Enter = rawKeyDown(vk) → char(text, no vk) → keyUp(vk)', async () => {
  const cdp = pressCdp();
  assert.equal(await pressStr(cdp, SID, 'Enter', undefined, new RefStore()), 'Pressed Enter');

  const events = cdp.callsOf('Input.dispatchKeyEvent');
  assert.deepEqual(events.map(e => e.params.type), ['rawKeyDown', 'char', 'keyUp']);
  assert.deepEqual(
    { ...events[0].params, type: undefined },
    { key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, modifiers: 0, type: undefined });
  // The char event carries the text and, per SPEC, NO virtual key code.
  assert.equal(events[1].params.text, '\r');
  assert.equal('windowsVirtualKeyCode' in events[1].params, false);
  assert.equal(events[2].params.windowsVirtualKeyCode, 13);
});

test('pressStr: keys without text (Escape) skip the char event', async () => {
  const cdp = pressCdp();
  await pressStr(cdp, SID, 'Escape', undefined, new RefStore());
  assert.deepEqual(cdp.callsOf('Input.dispatchKeyEvent').map(e => e.params.type), ['rawKeyDown', 'keyUp']);
});

test('pressStr: ctrl/meta/alt suppress the char; shift does not', async () => {
  const ctrl = pressCdp();
  await pressStr(ctrl, SID, 'Control+A', undefined, new RefStore());
  const ctrlEvents = ctrl.callsOf('Input.dispatchKeyEvent');
  assert.deepEqual(ctrlEvents.map(e => e.params.type), ['rawKeyDown', 'keyUp']); // no char for Ctrl+A
  for (const e of ctrlEvents) assert.equal(e.params.modifiers, 2);

  const alt = pressCdp();
  await pressStr(alt, SID, 'Alt+F4', undefined, new RefStore());
  assert.equal(alt.callsOf('Input.dispatchKeyEvent').length, 2);

  const shift = pressCdp();
  await pressStr(shift, SID, 'Shift+Tab', undefined, new RefStore());
  const shiftEvents = shift.callsOf('Input.dispatchKeyEvent');
  assert.deepEqual(shiftEvents.map(e => e.params.type), ['rawKeyDown', 'char', 'keyUp']);
  assert.equal(shiftEvents[1].params.text, '\t');
  assert.equal(shiftEvents[1].params.modifiers, 8);
});

test('pressStr: a char-event failure still sends keyUp (never leaves the key stuck)', async () => {
  const cdp = pressCdp({ throwOnChar: true });
  await assert.rejects(pressStr(cdp, SID, 'Enter', undefined, new RefStore()), /char dispatch failed/);
  // rawKeyDown, failed char, then the compensating keyUp — a keyUp arrived
  // despite the failure (the in-try keyUp is skipped when char throws).
  assert.deepEqual(cdp.callsOf('Input.dispatchKeyEvent').map(e => e.params.type), ['rawKeyDown', 'char', 'keyUp']);
});

test('pressStr: key required', async () => {
  await assert.rejects(pressStr(pressCdp(), SID, '', undefined, new RefStore()), /key required/);
});

// ---------------------------------------------------------------------------
// --target focus routing
// ---------------------------------------------------------------------------

function stockedStore() {
  const store = new RefStore();
  store.replace([{ ref: 'e1', backendNodeId: 101, name: 'Search' }]);
  return store;
}

test('pressStr --target <selector>: in-page focus eval, labeled result', async () => {
  const cdp = pressCdp({ focusJson: JSON.stringify({ tag: 'INPUT', label: 'q' }) });
  assert.equal(await pressStr(cdp, SID, 'Enter', '#q', new RefStore()),
    'Pressed Enter on <INPUT> "q"');
  const focusCall = cdp.callsOf('Runtime.evaluate')[0];
  assert.ok(focusCall.params.expression.includes('document.querySelector("#q")'));
  assert.ok(focusCall.params.expression.includes('.focus()'));
});

test('pressStr --target <selector>: not found / focus refused', async () => {
  await assert.rejects(
    pressStr(pressCdp({ focusJson: JSON.stringify({ error: 'not_found' }) }), SID, 'Enter', '.gone', new RefStore()),
    { code: 'element_not_found' });
  await assert.rejects(
    pressStr(pressCdp({ focusJson: JSON.stringify({ error: 'focus_failed' }) }), SID, 'Enter', '.x', new RefStore()),
    { code: 'focus_failed' });
});

test('pressStr --target <@eN>: DOM.focus on the store backendNodeId', async () => {
  const cdp = pressCdp();
  assert.equal(await pressStr(cdp, SID, 'Enter', '@e1', stockedStore()),
    'Pressed Enter on @e1 "Search"');
  assert.deepEqual(cdp.callsOf('DOM.focus')[0].params, { backendNodeId: 101 });
  assert.ok(cdp.calls.some(c => c.method === 'DOM.scrollIntoViewIfNeeded'));

  await assert.rejects(pressStr(pressCdp(), SID, 'Enter', '@e9', stockedStore()), /ref_not_found/);
});

// ---------------------------------------------------------------------------
// daemon dispatch
// ---------------------------------------------------------------------------

test('execCommand press parses --target in either position', async () => {
  for (const args of [['--target', '@e1', 'Enter'], ['Enter', '--target', '@e1']]) {
    const res = await execCommand(pressCdp(), SID, 'T', { cmd: 'press', args }, undefined, stockedStore());
    assert.equal(res.ok, true, JSON.stringify(args));
    assert.equal(res.result, 'Pressed Enter on @e1 "Search"');
  }
});

test('execCommand press: --target without a value is an explicit error', async () => {
  const res = await execCommand(pressCdp(), SID, 'T', { cmd: 'press', args: ['Enter', '--target'] });
  assert.equal(res.ok, false);
  assert.match(res.error, /--target requires a value/);
});
