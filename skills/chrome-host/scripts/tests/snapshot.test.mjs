// snapshotStr tree rendering: node filtering, formatting, and the dual
// childIds/parentId child ordering with dedupe.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  shouldShowAxNode, formatAxNode, orderedAxChildren, snapshotStr,
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

test('formatAxNode: indent capped at depth 10, name and value rendering', () => {
  assert.equal(formatAxNode(node({ role: { value: 'button' }, name: { value: 'OK' } }), 0), '[button] OK');
  assert.equal(formatAxNode(node({ role: { value: 'textbox' }, value: { value: 'a"b' } }), 2),
    '    [textbox] = "a\\"b"');
  assert.equal(formatAxNode(node({ role: { value: 'root' } }), 99), ' '.repeat(20) + '[root]');
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
