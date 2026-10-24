// P-1 JS dialog takeover: defensive event parsing, ring buffer with cursor,
// unconditional accept, and dialogs_seen attachment on daemon responses.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  parseDialogOpeningParams, DialogTracker, handleDialogOpening, dialogsStr,
  MAX_DIALOG_BUFFER,
} from '../lib/dialogs.mjs';
import { execCommand } from '../lib/daemon.mjs';
import { FakeCdp } from './helpers.mjs';

const SID = 'SESSION-1';
const TARGET = 'AAAABBBBCCCCDDDD0000';

// ---------------------------------------------------------------------------
// parseDialogOpeningParams — every field degrades instead of throwing (§1.3)
// ---------------------------------------------------------------------------

test('parse: known types pass through, unknown types normalize to alert', () => {
  for (const type of ['alert', 'confirm', 'prompt', 'beforeunload']) {
    assert.equal(parseDialogOpeningParams({ type }).type, type);
  }
  // e.g. a future CDP type must not crash the daemon nor leak into the buffer
  assert.equal(parseDialogOpeningParams({ type: 'foo' }).type, 'alert');
  assert.equal(parseDialogOpeningParams({}).type, 'alert');
});

test('parse: message keeps strings only (beforeunload boolean false becomes "")', () => {
  // Some Chrome versions send message:false for beforeunload — must not
  // become "false" in the buffer.
  assert.equal(parseDialogOpeningParams({ message: false }).message, '');
  assert.equal(parseDialogOpeningParams({ message: null }).message, '');
  assert.equal(parseDialogOpeningParams({ message: 'Leave?' }).message, 'Leave?');
});

test('parse: fields longer than 4096 chars are truncated with a marker', () => {
  const exact = 'a'.repeat(4096);
  const over = 'a'.repeat(4097);
  assert.equal(parseDialogOpeningParams({ message: exact }).message, exact);
  const truncated = parseDialogOpeningParams({ message: over }).message;
  assert.equal(truncated, exact + '... [truncated]');
  // url and defaultPrompt share the rule
  assert.equal(parseDialogOpeningParams({ url: over }).url, exact + '... [truncated]');
  assert.equal(parseDialogOpeningParams({ defaultPrompt: over }).defaultPrompt, exact + '... [truncated]');
});

test('parse: url/defaultPrompt non-strings drop to undefined, hasBrowserHandler keeps booleans only', () => {
  const p = parseDialogOpeningParams({ url: 42, defaultPrompt: true, hasBrowserHandler: 'yes' });
  assert.equal(p.url, undefined);
  assert.equal(p.defaultPrompt, undefined);
  assert.equal(p.hasBrowserHandler, undefined);
  const q = parseDialogOpeningParams({ url: 'https://x/', defaultPrompt: 'hi', hasBrowserHandler: false });
  assert.equal(q.url, 'https://x/');
  assert.equal(q.defaultPrompt, 'hi');
  assert.equal(q.hasBrowserHandler, false);
});

// ---------------------------------------------------------------------------
// DialogTracker — sequence, ring buffer eviction, cursor queries (§1.5/§1.6)
// ---------------------------------------------------------------------------

test('tracker: record assigns monotonically increasing sequence and builds the wire entry', () => {
  const t = new DialogTracker(TARGET);
  const e1 = t.record(parseDialogOpeningParams({
    type: 'prompt', message: 'name?', url: 'https://x/', defaultPrompt: 'Bob',
  }));
  const e2 = t.record(parseDialogOpeningParams({ type: 'alert', message: 'hey' }));
  assert.deepEqual(e1, {
    tab_id: TARGET, type: 'prompt', message: 'name?', url: 'https://x/',
    default_prompt: 'Bob', handled: 'accepted', sequence: 1,
  });
  // absent optional fields stay absent (JSON.stringify would drop them anyway)
  assert.deepEqual(e2, { tab_id: TARGET, type: 'alert', message: 'hey', handled: 'accepted', sequence: 2 });
  assert.equal(t.cursor(), 2);
});

test('tracker: buffer holds at most 32 entries, FIFO drops the oldest without a loss marker', () => {
  const t = new DialogTracker(TARGET);
  for (let i = 0; i < MAX_DIALOG_BUFFER + 8; i++) {
    t.record(parseDialogOpeningParams({ type: 'alert', message: `m${i + 1}` }));
  }
  assert.equal(t.entries.length, MAX_DIALOG_BUFFER);
  // survivors are the newest 32; since(0) silently misses the 8 evicted ones
  const seen = t.since(0);
  assert.equal(seen.length, MAX_DIALOG_BUFFER);
  assert.equal(seen[0].sequence, 9);
  assert.equal(seen[0].message, 'm9');
});

test('tracker: since() is strictly greater-than', () => {
  const t = new DialogTracker(TARGET);
  for (let i = 0; i < 3; i++) t.record(parseDialogOpeningParams({ type: 'alert' }));
  assert.deepEqual(t.since(1).map(e => e.sequence), [2, 3]);
  assert.deepEqual(t.since(3), []);
  assert.deepEqual(t.since(0).length, 3);
});

// ---------------------------------------------------------------------------
// handleDialogOpening — unconditional accept (§1.4)
// ---------------------------------------------------------------------------

test('handle: alert/confirm/beforeunload send {accept:true} without promptText', async () => {
  for (const type of ['alert', 'confirm', 'beforeunload']) {
    const t = new DialogTracker(TARGET);
    const cdp = new FakeCdp(async () => ({}));
    await handleDialogOpening(cdp, SID, t, { type, message: 'm' });
    const call = cdp.callsOf('Page.handleJavaScriptDialog').at(-1);
    assert.deepEqual(call.params, { accept: true });
    assert.equal(call.sessionId, SID);
    assert.equal(t.entries.length, 1);
  }
});

test('handle: prompt sends promptText = defaultPrompt ?? ""', async () => {
  const t = new DialogTracker(TARGET);
  const cdp = new FakeCdp(async () => ({}));
  await handleDialogOpening(cdp, SID, t, { type: 'prompt', message: 'name?', defaultPrompt: 'Bob' });
  assert.deepEqual(cdp.callsOf('Page.handleJavaScriptDialog')[0].params,
    { accept: true, promptText: 'Bob' });

  const t2 = new DialogTracker(TARGET);
  await handleDialogOpening(cdp, SID, t2, { type: 'prompt', message: 'name?' });
  assert.deepEqual(cdp.callsOf('Page.handleJavaScriptDialog')[1].params,
    { accept: true, promptText: '' });
});

test('handle: failed handle is swallowed, not recorded, and does not advance the cursor', async () => {
  const t = new DialogTracker(TARGET);
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Page.handleJavaScriptDialog') throw new Error('boom');
    return {};
  });
  // must not reject into the daemon's event dispatch
  await handleDialogOpening(cdp, SID, t, { type: 'alert', message: 'm' });
  assert.equal(t.entries.length, 0);
  assert.equal(t.cursor(), 0);

  // the next successful dialog still gets sequence 1 (failure moved nothing)
  const okCdp = new FakeCdp(async () => ({}));
  await handleDialogOpening(okCdp, SID, t, { type: 'alert', message: 'n' });
  assert.equal(t.entries[0].sequence, 1);
});

// ---------------------------------------------------------------------------
// execCommand — cursor mark on entry, dialogs_seen on exit (§1.6)
// ---------------------------------------------------------------------------

function dialogDuringEvalCdp(dialogParams, { failEval = false } = {}) {
  // Simulates the real blocking order: a command is in flight when the page
  // opens a dialog; the daemon handles it; only then does the command return.
  const tracker = new DialogTracker(TARGET);
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Runtime.enable') return {};
    if (method === 'Page.handleJavaScriptDialog') return {};
    if (method === 'Runtime.evaluate') {
      await handleDialogOpening(cdp, SID, tracker, dialogParams);
      if (failEval) throw new Error('Renderer blocked');
      return { result: { value: 'done' } };
    }
    throw new Error('unexpected ' + method);
  });
  return { tracker, cdp };
}

test('execCommand: dialog accepted during a command is attached as dialogs_seen', async () => {
  const { tracker, cdp } = dialogDuringEvalCdp({ type: 'confirm', message: 'Sure?', url: 'https://x/' });
  const res = await execCommand(cdp, SID, TARGET, { cmd: 'eval', args: ['1'] }, tracker);
  assert.equal(res.ok, true);
  assert.equal(res.result, 'done');
  assert.deepEqual(res.dialogs_seen, [{
    tab_id: TARGET, type: 'confirm', message: 'Sure?', url: 'https://x/',
    handled: 'accepted', sequence: 1,
  }]);
});

test('execCommand: dialogs_seen is omitted when nothing popped (backward compatible)', async () => {
  const tracker = new DialogTracker(TARGET);
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') return { result: { value: '42' } };
    throw new Error('unexpected ' + method);
  });
  const res = await execCommand(cdp, SID, TARGET, { cmd: 'eval', args: ['1'] }, tracker);
  assert.deepEqual(res, { ok: true, result: '42' });
  assert.equal('dialogs_seen' in res, false);
});

test('execCommand: a later command after a dialog no longer reports it (cursor moved)', async () => {
  const { tracker, cdp } = dialogDuringEvalCdp({ type: 'alert', message: 'once' });
  await execCommand(cdp, SID, TARGET, { cmd: 'eval', args: ['1'] }, tracker);
  const quiet = new FakeCdp(async (method) => {
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') return { result: { value: '2' } };
    throw new Error('unexpected ' + method);
  });
  const res2 = await execCommand(quiet, SID, TARGET, { cmd: 'eval', args: ['2'] }, tracker);
  assert.equal('dialogs_seen' in res2, false);
});

test('execCommand: failing commands also carry dialogs_seen — the dialog is the "why"', async () => {
  // The P-1 diagnosis case: command times out/errs because the renderer was
  // blocked on a dialog; the attached entry is the missing cause.
  const { tracker, cdp } = dialogDuringEvalCdp({ type: 'alert', message: 'blocker' }, { failEval: true });
  const res = await execCommand(cdp, SID, TARGET, { cmd: 'eval', args: ['1'] }, tracker);
  assert.equal(res.ok, false);
  assert.equal(res.error, 'Renderer blocked');
  assert.deepEqual(res.dialogs_seen.map(d => d.message), ['blocker']);
});

test('execCommand: works unchanged without a tracker (old call sites / tests)', async () => {
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') return { result: { value: '7' } };
    throw new Error('unexpected ' + method);
  });
  const res = await execCommand(cdp, SID, TARGET, { cmd: 'eval', args: ['1'] });
  assert.deepEqual(res, { ok: true, result: '7' });
});

// ---------------------------------------------------------------------------
// `dialogs` command formatting
// ---------------------------------------------------------------------------

test('dialogs command: lists buffered entries, --since filters strictly greater', async () => {
  const tracker = new DialogTracker(TARGET);
  tracker.record(parseDialogOpeningParams({ type: 'alert', message: 'first' }));
  tracker.record(parseDialogOpeningParams({
    type: 'prompt', message: 'name?', url: 'https://x/', defaultPrompt: 'Bob',
  }));

  const all = await execCommand(new FakeCdp(), SID, TARGET, { cmd: 'dialogs', args: [] }, tracker);
  assert.equal(all.ok, true);
  assert.equal(all.result,
    '#1 alert  "first"\n#2 prompt  "name?"  (default: "Bob")  https://x/');

  const since = await execCommand(new FakeCdp(), SID, TARGET,
    { cmd: 'dialogs', args: ['--since', '1'] }, tracker);
  assert.equal(since.result, '#2 prompt  "name?"  (default: "Bob")  https://x/');
});

test('dialogs command: empty buffer yields empty result', async () => {
  const tracker = new DialogTracker(TARGET);
  const res = await execCommand(new FakeCdp(), SID, TARGET, { cmd: 'dialogs', args: [] }, tracker);
  assert.deepEqual(res, { ok: true, result: '' });
});

test('dialogs command: invalid --since value is an error, missing tracker is an error', async () => {
  const tracker = new DialogTracker(TARGET);
  const bad = await execCommand(new FakeCdp(), SID, TARGET,
    { cmd: 'dialogs', args: ['--since', 'x'] }, tracker);
  assert.equal(bad.ok, false);
  assert.match(bad.error, /--since requires a non-negative integer/);

  const noTracker = await execCommand(new FakeCdp(), SID, TARGET, { cmd: 'dialogs', args: [] });
  assert.equal(noTracker.ok, false);
  assert.match(noTracker.error, /Dialog buffer unavailable/);
});
