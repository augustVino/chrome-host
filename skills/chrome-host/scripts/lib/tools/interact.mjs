// Click / coordinate-click / typing / load-more interactions.

import { sleep } from '../paths.mjs';
import { evalStr } from './evaluate.mjs';
import {
  ClickError, evalJson, locateExpr, hitTestExpr,
  classifyLocate, classifyHit, dispatchClickAtPoint,
} from './click-native.mjs';

// Click element by CSS selector via the native Input event chain
// (BEHAVIOR-SPEC §2): locate + scrollIntoView + visibility check → hit-test
// the center point → Input.dispatchMouseEvent. unsafeSynth keeps the legacy
// el.click() path for pages that deliberately hook untrusted (synthetic)
// clicks, e.g. test hooks.
export async function clickStr(cdp, sid, selector, unsafeSynth = false) {
  if (!selector) throw new Error('CSS selector required');
  if (unsafeSynth) return synthClickStr(cdp, sid, selector);

  const located = classifyLocate(await evalJson(cdp, sid, locateExpr(selector)), selector);
  classifyHit(await evalJson(cdp, sid, hitTestExpr(selector, located.x, located.y)), selector);
  try {
    await dispatchClickAtPoint(cdp, sid, located.x, located.y);
  } catch (e) {
    throw new ClickError('dispatch_failed', `${e.message} (effect_state: ${
      e.pressSent ? 'unknown — mousePressed was already dispatched' : 'none — no input was dispatched'})`);
  }
  return `Clicked <${located.tag}> "${located.text}" (native input @${located.x},${located.y})`;
}

// Legacy synthetic path (--unsafe-synth): el.click() produces an untrusted
// event that skips the native pipeline — wrong by default, but occasionally
// the only thing a page's test hook responds to.
async function synthClickStr(cdp, sid, selector) {
  const expr = `
    (function() {
      const el = document.querySelector(${JSON.stringify(selector)});
      if (!el) return JSON.stringify({ error: 'not_found' });
      el.scrollIntoView({ block: 'center' });
      el.click();
      return JSON.stringify({ ok: true, tag: el.tagName, text: (el.textContent || '').trim().substring(0, 80) });
    })()
  `;
  const r = await evalJson(cdp, sid, expr);
  if (r && r.error === 'not_found') throw new ClickError('element_not_found', `Element not found: ${selector}`);
  if (!r || !r.ok) throw new Error(`click: unexpected eval result for ${selector}`);
  return `Clicked <${r.tag}> "${r.text}" (synthetic el.click())`;
}

// Click at CSS pixel coordinates using Input.dispatchMouseEvent
export async function clickXyStr(cdp, sid, x, y) {
  const cx = parseFloat(x);
  const cy = parseFloat(y);
  if (isNaN(cx) || isNaN(cy)) throw new Error('x and y must be numbers (CSS pixels)');
  await dispatchClickAtPoint(cdp, sid, cx, cy);
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
