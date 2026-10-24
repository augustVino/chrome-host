// RefStore + normalizeRef: whole-table replace semantics, invalidation
// stickiness, and the §3.7 normalization rules (one '@', case-sensitive).

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { RefStore, normalizeRef } from '../lib/ref-store.mjs';

test('normalizeRef strips exactly one leading @, keeps case', () => {
  assert.equal(normalizeRef('@e3'), 'e3');
  assert.equal(normalizeRef('e3'), 'e3');
  assert.equal(normalizeRef('@@e3'), '@e3'); // only ONE '@' — lookup then misses
  assert.equal(normalizeRef('@E3'), 'E3');   // case-sensitive, never folded
  assert.equal(normalizeRef('E3'), 'E3');
});

test('replace swaps the whole table and bumps generation; resolve takes raw or @-prefixed refs', () => {
  const store = new RefStore();
  assert.equal(store.generation, 0);
  store.replace([
    { ref: 'e1', backendNodeId: 101, name: 'OK' },
    { ref: 'e2', backendNodeId: 102 },
  ]);
  assert.equal(store.generation, 1);
  assert.equal(store.size, 2);

  assert.deepEqual(store.resolve('e1'), { backendNodeId: 101, name: 'OK' });
  assert.deepEqual(store.resolve('@e2'), { backendNodeId: 102 }); // normalized inside

  // Whole-table swap: old refs die even if the new snap found fewer nodes.
  store.replace([{ ref: 'e1', backendNodeId: 201 }]);
  assert.equal(store.generation, 2);
  assert.equal(store.resolve('e2'), null);
  assert.deepEqual(store.resolve('e1'), { backendNodeId: 201 });
});

test('resolve is case-sensitive per §3.7: E3/@E3 never match e3', () => {
  const store = new RefStore();
  store.replace([{ ref: 'e3', backendNodeId: 33 }]);
  assert.equal(store.resolve('E3'), null);
  assert.equal(store.resolve('@E3'), null);
});

test('invalidate drops all entries, flags stale with reason; replace clears stale', () => {
  const store = new RefStore();
  store.replace([{ ref: 'e1', backendNodeId: 101 }]);

  assert.equal(store.invalidate('navigation'), true);
  assert.equal(store.size, 0);
  assert.equal(store.generation, 2);
  assert.equal(store.stale, true);
  assert.equal(store.lastInvalidReason, 'navigation');
  assert.equal(store.resolve('e1'), null);

  // Repeat invalidation on an empty table: no-op, stays stale.
  assert.equal(store.invalidate('document updated'), false);
  assert.equal(store.stale, true);
  assert.equal(store.lastInvalidReason, 'navigation');

  store.replace([{ ref: 'e1', backendNodeId: 301 }]);
  assert.equal(store.stale, false);
  assert.equal(store.lastInvalidReason, null);
  assert.deepEqual(store.resolve('e1'), { backendNodeId: 301 });
});

test('invalidate on a never-populated store neither bumps generation nor flags stale', () => {
  const store = new RefStore();
  assert.equal(store.invalidate(), false);
  assert.equal(store.generation, 0);
  assert.equal(store.stale, false);
});

test('replace skips malformed entries defensively instead of throwing', () => {
  const store = new RefStore();
  store.replace([
    null,
    { ref: 'e1' },                       // no backendNodeId
    { ref: 'e2', backendNodeId: 'x' },   // non-integer backendNodeId
    { ref: 'button', backendNodeId: 5 }, // key not eN form
    { ref: 'E5', backendNodeId: 6 },    // uppercase — regex is case-sensitive
    { ref: 'e4', backendNodeId: 7, name: '' }, // falsy name omitted
  ]);
  assert.equal(store.size, 1);
  assert.deepEqual(store.resolve('e4'), { backendNodeId: 7 });
});
