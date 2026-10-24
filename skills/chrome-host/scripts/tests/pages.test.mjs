// resolvePrefix / getDisplayPrefixLength / formatPageList — the contract the
// CLI relies on for "<target> is a unique targetId prefix" semantics.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  resolvePrefix, getDisplayPrefixLength, formatPageList,
} from '../lib/pages.mjs';

test('resolvePrefix: exact match returns the candidate', () => {
  assert.equal(resolvePrefix('ABC123', ['ABC123', 'DEF456']), 'ABC123');
});

test('resolvePrefix: matching is case-insensitive', () => {
  assert.equal(resolvePrefix('abc', ['ABC123', 'DEF456']), 'ABC123');
});

test('resolvePrefix: no match throws with noun and optional hint', () => {
  assert.throws(() => resolvePrefix('XYZ', ['ABC123']), {
    message: 'No target matching prefix "XYZ".',
  });
  assert.throws(() => resolvePrefix('XYZ', ['ABC123'], 'target', 'Run "cdp list".'),
    { message: 'No target matching prefix "XYZ". Run "cdp list".' });
  assert.throws(() => resolvePrefix('XYZ', ['A'], 'widget'), /No widget matching/);
});

test('resolvePrefix: multiple matches throw as ambiguous with the count', () => {
  assert.throws(() => resolvePrefix('AB', ['AB1', 'AB2', 'AB3']), {
    message: 'Ambiguous prefix "AB" — matches 3 targets. Use more characters.',
  });
});

test('getDisplayPrefixLength: empty list falls back to minimum length', () => {
  assert.equal(getDisplayPrefixLength([]), 8);
});

test('getDisplayPrefixLength: unique at 8 chars stays at 8', () => {
  assert.equal(getDisplayPrefixLength(['AAAABBBB111', 'CCCCDDDD222']), 8);
});

test('getDisplayPrefixLength: collision at 8 chars forces a longer prefix', () => {
  assert.equal(getDisplayPrefixLength(['AAAABBBB1', 'AAAABBBB2']), 9);
});

test('getDisplayPrefixLength: uniqueness is case-insensitive', () => {
  // 'a' vs 'A' would collide as a prefix, so 9 chars are needed.
  assert.equal(getDisplayPrefixLength(['AAAABBBBa', 'AAAABBBBA']), 9);
});

test('getDisplayPrefixLength: degenerate inputs return maxLen', () => {
  // IDs shorter than the 8-char floor, and fully identical IDs, never become
  // unique — the function yields maxLen and ambiguity is surfaced elsewhere.
  assert.equal(getDisplayPrefixLength(['ABC']), 3);
  assert.equal(getDisplayPrefixLength(['AAAAAAAA', 'AAAAAAAA']), 8);
});

test('formatPageList: fixed-width id and 54-char padded title columns', () => {
  const pages = [
    { targetId: 'AAAABBBB111', title: 'Short', url: 'https://a.example/' },
    { targetId: 'BBBACCCC222', title: '', url: 'https://b.example/' },
  ];
  const lines = formatPageList(pages).split('\n');
  assert.equal(lines.length, 2);
  assert.equal(lines[0], 'AAAABBBB' + '  ' + 'Short'.padEnd(54) + '  https://a.example/');
  // Empty titles still occupy the full 54-char column, keeping URLs aligned.
  assert.equal(lines[1], 'BBBACCCC' + '  ' + ' '.repeat(54) + '  https://b.example/');
});

test('formatPageList: title truncated to 54 chars', () => {
  const longTitle = 'T'.repeat(80);
  const [line] = formatPageList([{ targetId: 'AAAABBBB1', title: longTitle, url: 'u' }]).split('\n');
  assert.ok(line.startsWith('AAAABBBB' + '  ' + 'T'.repeat(54)));
  assert.ok(!line.includes('T'.repeat(55)));
});

test('formatPageList: prefix column widens when 8 chars collide', () => {
  const pages = [
    { targetId: 'AAAABBBB1', title: 'x', url: 'u1' },
    { targetId: 'AAAABBBB2', title: 'y', url: 'u2' },
  ];
  const lines = formatPageList(pages).split('\n');
  assert.ok(lines[0].startsWith('AAAABBBB1  '));
  assert.ok(lines[1].startsWith('AAAABBBB2  '));
});
