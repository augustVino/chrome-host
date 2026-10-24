// Click / coordinate-click / typing / load-more interactions.
//
// clickStr is the unified entry (T5): a target starting with '@' or a bare
// 'eN' token is an @eN ref from snap → ref resolution chain → clickXyStr;
// anything else is a CSS selector → clickSelectorStr (T4 owns that body).

import { sleep } from '../paths.mjs';
import { evalStr } from './evaluate.mjs';
import { normalizeRef } from '../ref-store.mjs';

// Does this click target look like a ref? Routing is deliberately generous
// for '@…' (CSS selectors can't start with '@'), strict for the bare form:
// only lowercase 'e<digits>'. '@E3' routes here too and then fails lookup
// case-sensitively (SPEC §3.7) instead of being misread as a selector.
export function isRefTarget(target) {
  return typeof target === 'string' && (target.startsWith('@') || /^e\d+$/.test(target));
}

// Centroid of the largest quad (element boxes are near-rectangles, so the
// vertex average ≈ center even when rotated). Quads are viewport CSS pixels.
function largestQuadCentroid(quads) {
  let best = null;
  let bestArea = 0;
  for (const q of quads || []) {
    if (!Array.isArray(q) || q.length !== 8 || q.some(n => typeof n !== 'number' || !Number.isFinite(n))) continue;
    let area2 = 0;
    let cx = 0;
    let cy = 0;
    for (let i = 0; i < 4; i++) {
      const x1 = q[i * 2];
      const y1 = q[i * 2 + 1];
      const j = ((i + 1) % 4) * 2;
      area2 += x1 * q[j + 1] - q[j] * y1;
      cx += x1;
      cy += y1;
    }
    const area = Math.abs(area2) / 2;
    if (area > bestArea) {
      bestArea = area;
      best = { x: cx / 4, y: cy / 4 };
    }
  }
  return best;
}

// ref resolution chain (SPEC §2.3 geometry, simplified): backendNodeId →
// viewport CSS point. Scroll first (best-effort) so the point is inside the
// viewport; primary geometry is DOM.getContentQuads, fallback is
// resolveNode + Runtime.callFunctionOn getBoundingClientRect.
// Any resolution failure → target_moved (ref named a node that is gone).
export async function resolveRefPoint(cdp, sid, backendNodeId) {
  try { await cdp.send('DOM.scrollIntoViewIfNeeded', { backendNodeId }, sid); } catch {}

  try {
    const { quads } = await cdp.send('DOM.getContentQuads', { backendNodeId }, sid);
    const point = largestQuadCentroid(quads);
    if (point) return point;
  } catch { /* fall through to the rect fallback */ }

  try {
    const { object } = await cdp.send('DOM.resolveNode', { backendNodeId }, sid);
    const objectId = object?.objectId;
    if (!objectId) throw new Error('no objectId');
    const res = await cdp.send('Runtime.callFunctionOn', {
      objectId,
      functionDeclaration: 'function(){ const r = this.getBoundingClientRect(); return { x: r.x, y: r.y, w: r.width, h: r.height }; }',
      returnByValue: true,
    }, sid);
    const rect = res?.result?.value;
    if (!rect || !(rect.w > 0) || !(rect.h > 0)) throw new Error('empty rect');
    return { x: rect.x + rect.w / 2, y: rect.y + rect.h / 2 };
  } catch (e) {
    throw new Error(
      `target_moved: ref no longer resolves to a visible node (${e.message}) — run "snap <target>" again and use fresh @eN refs`);
  }
}

function refMissError(store, rawRef) {
  const hint = 'run "snap <target>" and use the fresh @eN refs';
  if (store.stale) {
    return new Error(`ref_stale: page changed since the last snap (${store.lastInvalidReason ?? 'document changed'}) — ${hint}`);
  }
  return new Error(`ref_not_found: ${rawRef} is not a ref from the last snap — ${hint}`);
}

// @eN click: store lookup → geometry → native coordinate click (T4's chain).
export async function clickRefStr(cdp, sid, rawRef, store) {
  if (!store) throw new Error('ref_not_found: no ref table — run "snap <target>" first');
  const entry = store.resolve(rawRef);
  if (!entry) throw refMissError(store, rawRef);
  const key = normalizeRef(rawRef);
  const { x, y } = await resolveRefPoint(cdp, sid, entry.backendNodeId);
  await clickXyStr(cdp, sid, x, y);
  const label = entry.name ? ` "${entry.name}"` : '';
  return `Clicked @${key}${label} at CSS (${x}, ${y})`;
}

// Unified click entry — parameter routing only (T5). Selector path stays in
// clickSelectorStr (T4's territory); ref path in clickRefStr.
export async function clickStr(cdp, sid, target, store) {
  if (isRefTarget(target)) return clickRefStr(cdp, sid, target, store);
  return clickSelectorStr(cdp, sid, target);
}

// Click element by CSS selector
export async function clickSelectorStr(cdp, sid, selector) {
  if (!selector) throw new Error('CSS selector required');
  const expr = `
    (function() {
      const el = document.querySelector(${JSON.stringify(selector)});
      if (!el) return { ok: false, error: 'Element not found: ' + ${JSON.stringify(selector)} };
      el.scrollIntoView({ block: 'center' });
      el.click();
      return { ok: true, tag: el.tagName, text: el.textContent.trim().substring(0, 80) };
    })()
  `;
  const result = await evalStr(cdp, sid, expr);
  const r = JSON.parse(result);
  if (!r.ok) throw new Error(r.error);
  return `Clicked <${r.tag}> "${r.text}"`;
}

// Click at CSS pixel coordinates using Input.dispatchMouseEvent
export async function clickXyStr(cdp, sid, x, y) {
  const cx = parseFloat(x);
  const cy = parseFloat(y);
  if (isNaN(cx) || isNaN(cy)) throw new Error('x and y must be numbers (CSS pixels)');
  const base = { x: cx, y: cy, button: 'left', clickCount: 1, modifiers: 0 };
  await cdp.send('Input.dispatchMouseEvent', { ...base, type: 'mouseMoved' }, sid);
  await cdp.send('Input.dispatchMouseEvent', { ...base, type: 'mousePressed' }, sid);
  await sleep(50);
  await cdp.send('Input.dispatchMouseEvent', { ...base, type: 'mouseReleased' }, sid);
  return `Clicked at CSS (${cx}, ${cy})`;
}

// Type text using Input.insertText (works in cross-origin iframes, unlike eval)
export async function typeStr(cdp, sid, text) {
  if (text == null || text === '') throw new Error('text required');
  await cdp.send('Input.insertText', { text }, sid);
  return `Typed ${text.length} characters`;
}

// Load-more: repeatedly click a button/selector until it disappears
export async function loadAllStr(cdp, sid, selector, intervalMs = 1500) {
  if (!selector) throw new Error('CSS selector required');
  let clicks = 0;
  const deadline = Date.now() + 5 * 60 * 1000; // 5-minute hard cap
  while (Date.now() < deadline) {
    const exists = await evalStr(cdp, sid,
      `!!document.querySelector(${JSON.stringify(selector)})`
    );
    if (exists !== 'true') break;
    const clickExpr = `
      (function() {
        const el = document.querySelector(${JSON.stringify(selector)});
        if (!el) return false;
        el.scrollIntoView({ block: 'center' });
        el.click();
        return true;
      })()
    `;
    const clicked = await evalStr(cdp, sid, clickExpr);
    if (clicked !== 'true') break;
    clicks++;
    await sleep(intervalMs);
  }
  return `Clicked "${selector}" ${clicks} time(s) until it disappeared`;
}
