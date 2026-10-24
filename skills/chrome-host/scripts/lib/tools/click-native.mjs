// Native click chain (BEHAVIOR-SPEC §2): selector → scrollIntoView →
// viewport-CSS-pixel center → elementFromPoint hit-test → Input.dispatchMouseEvent.
// Shared by clickStr (selector clicks) and clickXyStr (coordinate clicks).

import { sleep } from '../paths.mjs';
import { evalStr } from './evaluate.mjs';

// Error carrying a machine-readable code (snake_case) that the daemon
// protocol forwards and the CLI prints as "[CODE] message" on stderr.
export class ClickError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

// Does the element hit at the click point belong to the click target?
// Walks up from the hit element through parentNode and shadow-DOM host
// boundaries (BEHAVIOR-SPEC §2.5.B); the target itself or any of its
// descendants counts as a hit. Pure function on DOM-shaped objects so it
// can be unit-tested in Node and inlined into the page eval via toString().
export function hitTestWalk(hit, target) {
  for (let n = hit; n; ) {
    if (n === target || target.contains(n)) return true;
    const root = n.getRootNode();
    n = n.parentNode || (root && root !== n ? root.host : null);
  }
  return false;
}

// Step 1: locate, scroll to center, visibility-check, compute center in
// viewport CSS pixels. Everything runs in one eval so the coordinates and
// the element are observed atomically. Returns a JSON string (page-side
// stringify keeps evalStr's round-trip lossless).
export function locateExpr(selector) {
  const sel = JSON.stringify(selector);
  return `(function() {
  const el = document.querySelector(${sel});
  if (!el) return JSON.stringify({ error: 'not_found' });
  el.scrollIntoView({ block: 'center' });
  const rect = el.getBoundingClientRect();
  const style = getComputedStyle(el);
  if (style.display === 'none') return JSON.stringify({ error: 'not_visible', why: 'display:none' });
  if (style.visibility === 'hidden') return JSON.stringify({ error: 'not_visible', why: 'visibility:hidden' });
  if (rect.width <= 0 || rect.height <= 0) return JSON.stringify({ error: 'not_visible', why: 'empty rect ' + Math.round(rect.width) + 'x' + Math.round(rect.height) });
  // offsetParent is null for unrendered elements — but also for position:fixed,
  // which is perfectly visible, so exempt fixed/sticky.
  if (!el.offsetParent && style.position !== 'fixed' && style.position !== 'sticky') return JSON.stringify({ error: 'not_visible', why: 'not rendered (no offsetParent)' });
  return JSON.stringify({
    x: rect.left + rect.width / 2,
    y: rect.top + rect.height / 2,
    tag: el.tagName,
    text: (el.textContent || '').trim().substring(0, 80),
  });
})()`;
}

// Step 2: hit-test the click point. Re-queries the target from the selector
// (no object identity survives between evals) and reports what actually sits
// at the point when it is not the target or a descendant.
export function hitTestExpr(selector, x, y) {
  const sel = JSON.stringify(selector);
  return `(function() {
  const target = document.querySelector(${sel});
  if (!target) return JSON.stringify({ hit: false, reason: 'target-gone' });
  const el = document.elementFromPoint(${x}, ${y});
  if (!el) return JSON.stringify({ hit: false, reason: 'nothing-at-point' });
  if ((${hitTestWalk})(el, target)) return JSON.stringify({ hit: true });
  return JSON.stringify({ hit: false, reason: 'covered', by: el.tagName + (el.className ? '.' + String(el.className) : '') });
})()`;
}

// Classify the locate result → { x, y, tag, text }, or throw the coded error.
export function classifyLocate(r, selector) {
  if (r && r.error === 'not_found') {
    throw new ClickError('element_not_found', `Element not found: ${selector}`);
  }
  if (r && r.error === 'not_visible') {
    throw new ClickError('element_not_visible', `Element not visible (${r.why || 'unknown reason'}): ${selector}`);
  }
  // Defensive: the page must return finite coordinates or the hit-test
  // expression could be built from injected content.
  if (!r || typeof r.x !== 'number' || !Number.isFinite(r.x)
    || typeof r.y !== 'number' || !Number.isFinite(r.y)) {
    throw new Error(`click: unexpected locate result for ${selector}`);
  }
  return r;
}

// Classify the hit-test result; a hit passes silently, anything else throws.
export function classifyHit(r, selector) {
  if (r && r.hit) return;
  if (r && r.reason === 'target-gone') {
    throw new ClickError('element_not_found', `Element not found: ${selector}`);
  }
  if (r && r.reason === 'covered') {
    throw new ClickError('element_blocked', `Click point on ${selector} is covered by <${r.by}>`);
  }
  if (r && r.reason === 'nothing-at-point') {
    throw new ClickError('element_blocked', `Click point on ${selector} hits nothing (outside viewport?)`);
  }
  throw new Error(`click: unexpected hit-test result for ${selector}`);
}

// Step 3: mouseMoved → mousePressed → 50ms → mouseReleased (BEHAVIOR-SPEC §2.4,
// the 50ms keeps current behavior). If anything fails after the press, a
// compensating mouseReleased is sent so the button is never left stuck down,
// and the rethrown error is flagged pressSent for the caller's effect_state
// wording (§2.7: pre-press failure = no effect, post-press = unknown).
export async function dispatchClickAtPoint(cdp, sid, x, y) {
  const base = { x, y, button: 'left', clickCount: 1, modifiers: 0 };
  await cdp.send('Input.dispatchMouseEvent', { ...base, type: 'mouseMoved' }, sid);
  let pressed = false;
  try {
    await cdp.send('Input.dispatchMouseEvent', { ...base, type: 'mousePressed' }, sid);
    pressed = true;
    await sleep(50);
    await cdp.send('Input.dispatchMouseEvent', { ...base, type: 'mouseReleased' }, sid);
  } catch (e) {
    if (pressed) {
      try { await cdp.send('Input.dispatchMouseEvent', { ...base, type: 'mouseReleased' }, sid); } catch {}
    }
    e.pressSent = pressed;
    throw e;
  }
}

// evalStr + JSON.parse with a clear error when the page returns garbage.
export async function evalJson(cdp, sid, expr) {
  const raw = await evalStr(cdp, sid, expr);
  try {
    return JSON.parse(raw);
  } catch {
    throw new Error(`click: unexpected eval result: ${String(raw).slice(0, 200)}`);
  }
}
