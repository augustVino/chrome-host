// click-native: the native click chain building blocks — coordinate and
// visibility classification, the hit-test walk (including shadow DOM and
// descendant hits), the Input dispatch sequence with release compensation,
// and [CODE] error propagation through the daemon protocol.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  ClickError, hitTestWalk, locateExpr, hitTestExpr,
  classifyLocate, classifyHit, dispatchClickAtPoint,
} from '../lib/tools/click-native.mjs';
import { clickStr } from '../lib/tools/interact.mjs';
import { execCommand } from '../lib/daemon.mjs';
import { FakeCdp } from './helpers.mjs';

// ---------------------------------------------------------------------------
// DOM-shaped fakes for hitTestWalk: parentNode chains, contains() as a
// parent-chain walk, getRootNode() returning an explicit root (shadow-root
// fakes carry a .host).
// ---------------------------------------------------------------------------

function node(id, parent) {
  const n = { id, parentNode: parent || null, root: null };
  n.contains = (other) => {
    for (let m = other; m; m = m.parentNode) if (m === n) return true;
    return false;
  };
  n.getRootNode = () => n.root || n;
  return n;
}

test('hitTestWalk: the target itself counts as a hit', () => {
  const target = node('target');
  assert.equal(hitTestWalk(target, target), true);
});

test('hitTestWalk: a descendant of the target counts as a hit', () => {
  // Clicking the <span> inside a <button> must still click the button.
  const target = node('button');
  const child = node('span', target);
  assert.equal(hitTestWalk(child, target), true);
});

test('hitTestWalk: an ancestor or sibling of the target is not a hit', () => {
  const parent = node('parent');
  const target = node('target', parent);
  const sibling = node('sibling', parent);
  assert.equal(hitTestWalk(parent, target), false);
  assert.equal(hitTestWalk(sibling, target), false);
});

test('hitTestWalk: walks out of shadow DOM via root.host up to the target', () => {
  // elementFromPoint retargets shadow content to the host, but if a nested
  // node is reported, the chain must still reach the target across boundaries.
  const target = node('host-button');
  const shadowRoot = { host: target };
  const inner = node('inner-button');
  inner.root = shadowRoot;
  assert.equal(hitTestWalk(inner, target), true);

  // Two shadow boundaries deep: inner → mid-shadow host → target-shadow host.
  const mid = node('mid-element');
  mid.root = shadowRoot;
  const innerRoot = { host: mid };
  const deep = node('deep-span');
  deep.root = innerRoot;
  assert.equal(hitTestWalk(deep, target), true);
});

// ---------------------------------------------------------------------------
// Expression builders
// ---------------------------------------------------------------------------

test('locateExpr scrolls to center and computes the rect center in-page', () => {
  const expr = locateExpr('button.primary');
  assert.ok(expr.includes('document.querySelector("button.primary")'));
  assert.ok(expr.includes("scrollIntoView({ block: 'center' })"));
  assert.ok(expr.includes('rect.left + rect.width / 2'));
  assert.ok(expr.includes('rect.top + rect.height / 2'));
});

test('locateExpr embeds the visibility checks: display, visibility, rect, offsetParent', () => {
  const expr = locateExpr('.x');
  assert.ok(expr.includes("style.display === 'none'"));
  assert.ok(expr.includes("style.visibility === 'hidden'"));
  assert.ok(expr.includes('rect.width <= 0 || rect.height <= 0'));
  assert.ok(expr.includes('!el.offsetParent'));
  // fixed/sticky elements have offsetParent null while still visible
  assert.ok(expr.includes("style.position !== 'fixed' && style.position !== 'sticky'"));
});

test('hitTestExpr embeds the coordinates and the shadow-aware walk', () => {
  const expr = hitTestExpr('.x', 11.5, 20.25);
  assert.ok(expr.includes('document.elementFromPoint(11.5, 20.25)'));
  assert.ok(expr.includes('target.contains'));
  assert.ok(expr.includes('root.host'));
  assert.ok(expr.includes('target-gone'));
});

// ---------------------------------------------------------------------------
// Expression execution against a fake DOM — the real coordinate math,
// visibility branches and hit-test walk run inside the generated IIFEs.
// ---------------------------------------------------------------------------

function domEl(tag, opts = {}) {
  const el = {
    tagName: tag, className: opts.className || '', textContent: opts.text || '',
    parentNode: opts.parent || null,
    offsetParent: 'offsetParent' in opts ? opts.offsetParent : {},
    scrollIntoView() {},
    getBoundingClientRect: () => opts.rect || { left: 100, top: 40, width: 51, height: 20 },
  };
  el.contains = (other) => {
    for (let m = other; m; m = m.parentNode) if (m === el) return true;
    return false;
  };
  el.getRootNode = () => (opts.root ? opts.root : el.parentNode ? el.parentNode.getRootNode() : {});
  return el;
}

function runExpr(expr, doc, computedStyle) {
  return JSON.parse(new Function('document', 'getComputedStyle', `return ${expr}`)(doc, computedStyle));
}

function docFor(target, selector, atPoint) {
  return { querySelector: (s) => (s === selector ? target : null), elementFromPoint: () => atPoint };
}

const VISIBLE_STYLE = () => ({ display: 'block', visibility: 'visible', position: 'static' });

test('locateExpr (executed): center = left + width/2, top + height/2; text trimmed', () => {
  const el = domEl('BUTTON', { text: '  Submit  ' });
  const r = runExpr(locateExpr('#b'), docFor(el, '#b', el), VISIBLE_STYLE);
  assert.deepEqual(r, { x: 125.5, y: 50, tag: 'BUTTON', text: 'Submit' });
});

test('locateExpr (executed): visibility branches classify not_visible', () => {
  const cases = [
    [{ display: 'none', visibility: 'visible', position: 'static' }, {}, 'display:none'],
    [{ display: 'block', visibility: 'hidden', position: 'static' }, {}, 'visibility:hidden'],
    // offsetParent null + static = unrendered; fixed is exempt (visible)
    [VISIBLE_STYLE(), { offsetParent: null }, 'not rendered (no offsetParent)'],
  ];
  for (const [style, opts, why] of cases) {
    const el = domEl('BUTTON', { ...opts });
    assert.deepEqual(runExpr(locateExpr('#b'), docFor(el, '#b', el), () => style),
      { error: 'not_visible', why }, style.display + ' ' + why);
  }
  // zero-size rect
  const flat = domEl('BUTTON', { rect: { left: 0, top: 0, width: 0, height: 10 } });
  assert.equal(runExpr(locateExpr('#b'), docFor(flat, '#b', flat), VISIBLE_STYLE).error, 'not_visible');

  // fixed/sticky with offsetParent null stays clickable
  for (const position of ['fixed', 'sticky']) {
    const el = domEl('BUTTON', { offsetParent: null });
    const r = runExpr(locateExpr('#b'), docFor(el, '#b', el),
      () => ({ display: 'block', visibility: 'visible', position }));
    assert.equal(r.error, undefined, position + ' must be exempt from the offsetParent check');
    assert.equal(r.x, 125.5);
  }
});

test('hitTestExpr (executed): hit / descendant / shadow / covered / miss', () => {
  const target = domEl('BUTTON');
  const doc = (atPoint) => docFor(target, '#b', atPoint);

  assert.deepEqual(runExpr(hitTestExpr('#b', 125.5, 50), doc(target), VISIBLE_STYLE), { hit: true });

  // clicking the span inside the button still clicks the button
  const child = domEl('SPAN', { parent: target });
  assert.deepEqual(runExpr(hitTestExpr('#b', 125, 50), doc(child), VISIBLE_STYLE), { hit: true });

  // a hit inside the target's own shadow tree escapes via root.host
  const inner = domEl('SPAN', { root: { host: target } });
  assert.deepEqual(runExpr(hitTestExpr('#b', 1, 1), doc(inner), VISIBLE_STYLE), { hit: true });

  // covered by an unrelated element → report what actually sits on top
  const cover = domEl('DIV', { className: 'modal mask' });
  assert.deepEqual(runExpr(hitTestExpr('#b', 1, 1), doc(cover), VISIBLE_STYLE),
    { hit: false, reason: 'covered', by: 'DIV.modal mask' });

  assert.deepEqual(runExpr(hitTestExpr('#b', 1, 1), doc(null), VISIBLE_STYLE),
    { hit: false, reason: 'nothing-at-point' });
  assert.deepEqual(runExpr(hitTestExpr('#b', 1, 1), docFor(target, '#other', null), VISIBLE_STYLE),
    { hit: false, reason: 'target-gone' });
});

// ---------------------------------------------------------------------------
// Classification → coded errors
// ---------------------------------------------------------------------------

test('classifyLocate passes through valid coordinates', () => {
  const located = { x: 100.5, y: 40, tag: 'BUTTON', text: 'Submit' };
  assert.equal(classifyLocate(located, '.ok'), located);
});

test('classifyLocate maps not_found / not_visible to coded errors', () => {
  assert.throws(() => classifyLocate({ error: 'not_found' }, '.gone'),
    { code: 'element_not_found', message: 'Element not found: .gone' });
  assert.throws(() => classifyLocate({ error: 'not_visible', why: 'display:none' }, '.hid'),
    { code: 'element_not_visible', message: 'Element not visible (display:none): .hid' });
});

test('classifyLocate rejects non-finite coordinates (eval tampering guard)', () => {
  assert.throws(() => classifyLocate({ x: '1);steal()', y: 2 }, '.x'), Error);
  assert.throws(() => classifyLocate({ x: Infinity, y: 2 }, '.x'), Error);
  assert.throws(() => classifyLocate(undefined, '.x'), Error);
});

test('classifyHit accepts hits and maps miss reasons to coded errors', () => {
  classifyHit({ hit: true }, '.ok'); // must not throw
  assert.throws(() => classifyHit({ hit: false, reason: 'covered', by: 'DIV.modal-backdrop' }, '.x'),
    { code: 'element_blocked', message: 'Click point on .x is covered by <DIV.modal-backdrop>' });
  assert.throws(() => classifyHit({ hit: false, reason: 'nothing-at-point' }, '.x'),
    { code: 'element_blocked', message: 'Click point on .x hits nothing (outside viewport?)' });
  // target vanished between the locate and hit-test evals → not_found again
  assert.throws(() => classifyHit({ hit: false, reason: 'target-gone' }, '.x'),
    { code: 'element_not_found' });
});

// ---------------------------------------------------------------------------
// Input dispatch sequence
// ---------------------------------------------------------------------------

test('dispatchClickAtPoint sends moved→pressed→released with full button state', async () => {
  const cdp = new FakeCdp(async () => ({}));
  await dispatchClickAtPoint(cdp, 'SID', 10.5, 20);

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

test('dispatchClickAtPoint compensates with a release when post-press dispatch fails', async () => {
  let releases = 0;
  const cdp = new FakeCdp(async (method, params) => {
    if (params.type === 'mouseReleased') {
      releases++;
      if (releases === 1) throw new Error('release blew up');
      return {};
    }
    return {};
  });
  await assert.rejects(dispatchClickAtPoint(cdp, 'S', 1, 2), (e) =>
    e.message === 'release blew up' && e.pressSent === true);
  // original release + compensating release — the button must not stay stuck
  assert.equal(releases, 2);
});

test('dispatchClickAtPoint flags pre-press failures as pressSent=false and skips release', async () => {
  const cdp = new FakeCdp(async (method, params) => {
    if (params.type === 'mousePressed') throw new Error('press blew up');
    return {};
  });
  await assert.rejects(dispatchClickAtPoint(cdp, 'S', 1, 2), (e) =>
    e.message === 'press blew up' && e.pressSent === false);
  assert.equal(cdp.callsOf('Input.dispatchMouseEvent').filter(e => e.params.type === 'mouseReleased').length, 0);
});

// ---------------------------------------------------------------------------
// clickStr end-to-end over a scripted FakeCdp
// ---------------------------------------------------------------------------

// locate/hit results are JSON strings, as the page-side IIFEs return them.
function clickCdp(locateJson, hitJson, inputHandler = async () => ({})) {
  return new FakeCdp(async (method, params) => {
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') {
      const value = params.expression.includes('elementFromPoint') ? hitJson : locateJson;
      return { result: { value } };
    }
    if (method === 'Input.dispatchMouseEvent') return inputHandler(method, params);
    throw new Error('unexpected ' + method);
  });
}

test('clickStr runs locate→hit-test→native dispatch and reports the coordinates', async () => {
  const cdp = clickCdp(
    JSON.stringify({ x: 100.5, y: 40, tag: 'BUTTON', text: 'Submit' }),
    JSON.stringify({ hit: true }),
  );
  assert.equal(await clickStr(cdp, 'SID', 'button.primary'),
    'Clicked <BUTTON> "Submit" (native input @100.5,40)');

  const exprs = cdp.callsOf('Runtime.evaluate').map(c => c.params.expression);
  assert.equal(exprs.length, 2); // locate, then hit-test
  assert.ok(exprs[0].includes('document.querySelector("button.primary")'));
  assert.ok(exprs[1].includes('elementFromPoint(100.5, 40)'));

  const events = cdp.callsOf('Input.dispatchMouseEvent');
  assert.deepEqual(events.map(e => e.params.type), ['mouseMoved', 'mousePressed', 'mouseReleased']);
  for (const e of events) assert.equal(e.params.x, 100.5, 'dispatched at the reported center');
});

test('clickStr stops before any Input event when the point is covered', async () => {
  const cdp = clickCdp(
    JSON.stringify({ x: 5, y: 5, tag: 'BUTTON', text: 'x' }),
    JSON.stringify({ hit: false, reason: 'covered', by: 'DIV.modal-backdrop' }),
  );
  await assert.rejects(clickStr(cdp, 'SID', '#b'),
    { code: 'element_blocked', message: 'Click point on #b is covered by <DIV.modal-backdrop>' });
  assert.equal(cdp.callsOf('Input.dispatchMouseEvent').length, 0);
});

test('clickStr maps element_not_found / element_not_visible without dispatching', async () => {
  const notFound = clickCdp(JSON.stringify({ error: 'not_found' }), '');
  await assert.rejects(clickStr(notFound, 's', '.gone'),
    { code: 'element_not_found', message: 'Element not found: .gone' });
  assert.equal(notFound.callsOf('Input.dispatchMouseEvent').length, 0);

  const notVisible = clickCdp(JSON.stringify({ error: 'not_visible', why: 'display:none' }), '');
  await assert.rejects(clickStr(notVisible, 's', '.hid'),
    { code: 'element_not_visible', message: 'Element not visible (display:none): .hid' });
});

test('clickStr wraps post-press dispatch failures as dispatch_failed with unknown effect', async () => {
  const cdp = clickCdp(
    JSON.stringify({ x: 1, y: 2, tag: 'BUTTON', text: 'x' }),
    JSON.stringify({ hit: true }),
    async (method, params) => {
      if (params.type === 'mousePressed') throw new Error('cdp broke');
      return {};
    },
  );
  await assert.rejects(clickStr(cdp, 's', '#b'), (e) =>
    e.code === 'dispatch_failed'
    && e.message.includes('cdp broke')
    && e.message.includes('effect_state: none — no input was dispatched'));

  // …and post-press failures must report the unknown effect state.
  const postPress = clickCdp(
    JSON.stringify({ x: 1, y: 2, tag: 'BUTTON', text: 'x' }),
    JSON.stringify({ hit: true }),
    async (method, params) => {
      if (params.type === 'mouseReleased') throw new Error('cdp broke mid-click');
      return {};
    },
  );
  await assert.rejects(clickStr(postPress, 's', '#b'), (e) =>
    e.code === 'dispatch_failed'
    && e.message.includes('effect_state: unknown — mousePressed was already dispatched'));
  // compensating release was attempted
  assert.equal(postPress.callsOf('Input.dispatchMouseEvent').filter(e => e.params.type === 'mouseReleased').length, 2);
});

test('clickStr --unsafe-synth keeps the synthetic el.click() legacy path', async () => {
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') {
      return { result: { value: JSON.stringify({ ok: true, tag: 'A', text: 'hook' }) } };
    }
    throw new Error('unexpected ' + method);
  });
  assert.equal(await clickStr(cdp, 'SID', '.hook', true),
    'Clicked <A> "hook" (synthetic el.click())');
  assert.ok(cdp.calls[1].params.expression.includes('el.click()'));
  assert.equal(cdp.callsOf('Input.dispatchMouseEvent').length, 0);

  const missing = new FakeCdp(async (method) => {
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') return { result: { value: JSON.stringify({ error: 'not_found' }) } };
    throw new Error('unexpected ' + method);
  });
  await assert.rejects(clickStr(missing, 's', '.gone', true),
    { code: 'element_not_found', message: 'Element not found: .gone' });
});

// ---------------------------------------------------------------------------
// Daemon protocol: error codes survive the {ok:false} round-trip
// ---------------------------------------------------------------------------

test('execCommand propagates click error codes; plain errors stay code-less', async () => {
  const cdp = clickCdp(JSON.stringify({ error: 'not_found' }), '');
  assert.deepEqual(await execCommand(cdp, 'SID', 'T', { cmd: 'click', args: ['.gone'] }),
    { ok: false, error: 'Element not found: .gone', code: 'element_not_found' });

  const throwing = new FakeCdp(async () => { throw new Error('Session gone'); });
  assert.deepEqual(await execCommand(throwing, 'SID', 'T', { cmd: 'eval', args: ['1'] }),
    { ok: false, error: 'Session gone' });
});

test('execCommand routes --unsafe-synth through the daemon args', async () => {
  const cdp = new FakeCdp(async (method) => {
    if (method === 'Runtime.enable') return {};
    if (method === 'Runtime.evaluate') {
      return { result: { value: JSON.stringify({ ok: true, tag: 'A', text: 'hook' }) } };
    }
    throw new Error('unexpected ' + method);
  });
  const res = await execCommand(cdp, 'SID', 'T', { cmd: 'click', args: ['.hook', '--unsafe-synth'] });
  assert.deepEqual(res, { ok: true, result: 'Clicked <A> "hook" (synthetic el.click())' });
});

test('ClickError instances are instanceof Error', () => {
  const e = new ClickError('element_blocked', 'covered');
  assert.ok(e instanceof Error);
  assert.equal(e.code, 'element_blocked');
});
