// snapshotStr tree rendering: node filtering, formatting, and the dual
// childIds/parentId child ordering with dedupe.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  shouldShowAxNode, formatAxNode, orderedAxChildren, snapshotStr,
  collectSnapshot, isRefWorthyAxNode,
} from '../lib/tools/snapshot.mjs';
import { FakeCdp } from './helpers.mjs';

const node = (over) => over;

test('shouldShowAxNode hides none/generic and nameless-valueless leaves', () => {
  assert.equal(shouldShowAxNode(node({ role: { value: 'none' }, name: { value: 'x' } })), false);
  assert.equal(shouldShowAxNode(node({ role: { value: 'generic' }, name: { value: 'x' } })), false);
  // Hidden: no name AND no value — content-free containers.
  assert.equal(shouldShowAxNode(node({ role: { value: 'text' }, name: { value: '' }, value: { value: '' } })), false);
  assert.equal(shouldShowAxNode(node({ role: { value: 'text' } })), false);
  // Visible: has a name, or carries a value.
  assert.equal(shouldShowAxNode(node({ role: { value: 'button' }, name: { value: 'OK' } })), true);
  assert.equal(shouldShowAxNode(node({ role: { value: 'textbox' }, value: { value: 'typed' } })), true);
});

test('shouldShowAxNode hides InlineTextBox only in compact mode', () => {
  const inline = node({ role: { value: 'InlineTextBox' }, name: { value: 'Hi' } });
  assert.equal(shouldShowAxNode(inline, true), false);
  assert.equal(shouldShowAxNode(inline, false), true);
});

test('shouldShowAxNode keeps nameless interactive nodes visible so their @eN is discoverable', () => {
  // Icon-only button: no name/value, but interactive — must render (with a ref).
  assert.equal(shouldShowAxNode(node({ role: { value: 'button' }, name: { value: '' } })), true);
  // Same shape but static — stays hidden.
  assert.equal(shouldShowAxNode(node({ role: { value: 'heading' }, name: { value: '' } })), false);
  // none/generic never render, interactive or not (they are never ref-worthy).
  assert.equal(shouldShowAxNode(node({ role: { value: 'generic' }, name: { value: '' } })), false);
});

test('isRefWorthyAxNode: interactive roles (case-insensitive + internal aliases), URL rows', () => {
  // Canonical ARIA roles
  for (const role of ['button', 'link', 'textbox', 'checkbox', 'radio', 'combobox',
    'listbox', 'menuitem', 'tab', 'option', 'searchbox', 'slider', 'switch']) {
    assert.equal(isRefWorthyAxNode(node({ role: { value: role }, name: { value: 'x' } })), true, role);
  }
  // Chrome camelCase variants survive via lowercasing + alias folding
  assert.equal(isRefWorthyAxNode(node({ role: { value: 'Button' } })), true);
  assert.equal(isRefWorthyAxNode(node({ role: { value: 'TextField' } })), true); // → textbox
  assert.equal(isRefWorthyAxNode(node({ role: { value: 'RadioButton' } })), true); // → radio
  assert.equal(isRefWorthyAxNode(node({ role: { value: 'PopUpButton' } })), true); // → combobox
  // Static content gets no ref
  assert.equal(isRefWorthyAxNode(node({ role: { value: 'heading' }, name: { value: 'Title' } })), false);
  assert.equal(isRefWorthyAxNode(node({ role: { value: 'generic' } })), false);
  assert.equal(isRefWorthyAxNode(node({ role: { value: 'none' } })), false);
  // URL-bearing structural rows: only with an http(s) name
  assert.equal(isRefWorthyAxNode(node({ role: { value: 'row' }, name: { value: 'https://example.com/a' } })), true);
  assert.equal(isRefWorthyAxNode(node({ role: { value: 'listitem' }, name: { value: 'http://x.io' } })), true);
  assert.equal(isRefWorthyAxNode(node({ role: { value: 'row' }, name: { value: 'Quarterly totals' } })), false);
});

test('formatAxNode: indent capped at depth 10, name and value rendering', () => {
  assert.equal(formatAxNode(node({ role: { value: 'button' }, name: { value: 'OK' } }), 0), '[button] OK');
  assert.equal(formatAxNode(node({ role: { value: 'textbox' }, value: { value: 'a"b' } }), 2),
    '    [textbox] = "a\\"b"');
  assert.equal(formatAxNode(node({ role: { value: 'root' } }), 99), ' '.repeat(20) + '[root]');
});

test('formatAxNode prefixes the ref inline when given: @e3 [button] 提交', () => {
  assert.equal(formatAxNode(node({ role: { value: 'button' }, name: { value: '提交' } }), 1, 'e3'),
    '  @e3 [button] 提交');
  // No ref → unchanged legacy format
  assert.equal(formatAxNode(node({ role: { value: 'button' }, name: { value: 'OK' } }), 0), '[button] OK');
});

test('collectSnapshot assigns @eN in DFS order; only interactive nodes consume numbers', async () => {
  const nodes = [
    { nodeId: '1', role: { value: 'WebArea' }, name: { value: 'Home' }, childIds: ['2', '3', '5'], backendDOMNodeId: 100 },
    { nodeId: '2', parentId: '1', role: { value: 'button' }, name: { value: 'OK' }, backendDOMNodeId: 101, childIds: ['4'] },
    { nodeId: '4', parentId: '2', role: { value: 'InlineTextBox' }, name: { value: 'OK text' }, backendDOMNodeId: 104 },
    { nodeId: '3', parentId: '1', role: { value: 'heading' }, name: { value: 'Title' }, backendDOMNodeId: 102 },
    { nodeId: '5', parentId: '1', role: { value: 'textbox' }, name: { value: 'Search' }, value: { value: 'typed' }, backendDOMNodeId: 103 },
  ];
  const cdp = new FakeCdp(() => ({ nodes }));

  const { text, refs } = await collectSnapshot(cdp, 'SID');
  // heading consumes no number; InlineTextBox neither; refs land inline.
  assert.equal(text, [
    '[WebArea] Home',
    '  @e1 [button] OK',
    '    [InlineTextBox] OK text',
    '  [heading] Title',
    '  @e2 [textbox] Search = "typed"',
  ].join('\n'));
  assert.deepEqual(refs, [
    { ref: 'e1', backendNodeId: 101, name: 'OK' },
    { ref: 'e2', backendNodeId: 103, name: 'Search' },
  ]);
});

test('collectSnapshot: interactive without backendDOMNodeId gets no ref and no number; compact keeps refs', async () => {
  const nodes = [
    { nodeId: '1', role: { value: 'WebArea' }, name: { value: 'P' }, childIds: ['2', '3'] },
    // Synthetic button with no DOM backing — visible but not referenceable.
    { nodeId: '2', parentId: '1', role: { value: 'button' }, name: { value: 'ghost' } },
    { nodeId: '3', parentId: '1', role: { value: 'link' }, name: { value: 'Next' }, backendDOMNodeId: 9 },
  ];
  const cdp = new FakeCdp(() => ({ nodes }));

  const full = await collectSnapshot(cdp, 'SID');
  assert.deepEqual(full.refs, [{ ref: 'e1', backendNodeId: 9, name: 'Next' }]);
  assert.equal(full.text, '[WebArea] P\n  [button] ghost\n  @e1 [link] Next');

  const compact = await collectSnapshot(cdp, 'SID', true);
  assert.match(compact.text, /@e1 \[link\] Next/); // compact carries refs too
});

test('orderedAxChildren: childIds first, parentId backfill, dedupe across both', () => {
  const a = { nodeId: 'a' };
  const b = { nodeId: 'b' };
  const c = { nodeId: 'c' }; // only reachable via parentId backfill
  const nodesById = new Map([['a', a], ['b', b], ['c', c]]);
  const childrenByParent = new Map([['p', [b, c]]]); // b duplicates a childId
  const parent = { nodeId: 'p', childIds: ['a', 'b', 'ghost'] };

  assert.deepEqual(orderedAxChildren(parent, nodesById, childrenByParent), [a, b, c]);
});

test('snapshotStr renders tree, hides filtered nodes, adopts orphans', async () => {
  const nodes = [
    { nodeId: '1', role: { value: 'WebArea' }, name: { value: 'Home' }, childIds: ['2', '3'] },
    { nodeId: '2', parentId: '1', role: { value: 'button' }, name: { value: 'OK' }, childIds: ['4'] },
    { nodeId: '3', parentId: '1', role: { value: 'generic' }, name: { value: 'wrapper' } },
    { nodeId: '4', parentId: '2', role: { value: 'InlineTextBox' }, name: { value: 'OK text' } },
    { nodeId: '5', parentId: 'ghost', role: { value: 'link' }, name: { value: 'Orphan' } },
  ];
  const cdp = new FakeCdp(() => ({ nodes }));

  const full = await snapshotStr(cdp, 'SID');
  assert.equal(full, [
    '[WebArea] Home',
    '  [button] OK',
    '    [InlineTextBox] OK text',
    '[link] Orphan',
  ].join('\n'));

  // Compact mode (what daemon "snap" uses) drops InlineTextBox noise.
  const compact = await snapshotStr(cdp, 'SID', true);
  assert.equal(compact, [
    '[WebArea] Home',
    '  [button] OK',
    '[link] Orphan',
  ].join('\n'));

  assert.equal(cdp.callsOf('Accessibility.getFullAXTree').length, 2); // full + compact
  assert.ok(cdp.calls.every(c => c.method === 'Accessibility.getFullAXTree'));
  assert.equal(cdp.calls[0].sessionId, 'SID');
});

test('snapshotStr does not loop on cross-referenced nodes', async () => {
  // 1 lists 2 as a child while 2 also lists 1 — the visited set must break it.
  const nodes = [
    { nodeId: '1', role: { value: 'a' }, name: { value: 'A' }, childIds: ['2'] },
    { nodeId: '2', parentId: '1', role: { value: 'b' }, name: { value: 'B' }, childIds: ['1'] },
  ];
  const cdp = new FakeCdp(() => ({ nodes }));
  const out = await snapshotStr(cdp, undefined);
  assert.equal(out, '[a] A\n  [b] B');
});
