// wait <selector> [--state visible|hidden|attached|detached]
//      [--timeout 10s] [--interval 250ms]: poll a CSS selector's presence /
// visibility until the state is reached or the timeout expires.
//
// Visibility reuses click-native locateExpr's semantics: a rendered non-zero
// rect that is not display:none / visibility:hidden. (A display:none
// ANCESTOR also zeroes the rect, so checking the element alone suffices.)
// state semantics: hidden = attached but not visible (detached ≠ hidden);
// eval failures (e.g. navigation in flight) count as detached and keep
// polling until the timeout.

import { sleep } from '../paths.mjs';
import { ClickError, evalJson } from './click-native.mjs';

export const WAIT_STATES = ['visible', 'hidden', 'attached', 'detached'];

// '10s' | '250ms' | '500' | 500 → milliseconds. Bare numbers are ms.
export function parseDuration(input, what = 'duration') {
  const fail = () => {
    throw new Error(`${what}: invalid duration "${String(input)}" — use a number of ms or e.g. 10s / 250ms / 2m`);
  };
  if (typeof input === 'number') {
    if (!Number.isFinite(input) || input < 0) fail();
    return input;
  }
  if (typeof input !== 'string') fail();
  const m = input.trim().match(/^(\d+(?:\.\d+)?)(ms|s|m)?$/);
  if (!m) fail();
  const n = parseFloat(m[1]);
  return n * (m[2] === 's' ? 1000 : m[2] === 'm' ? 60_000 : 1);
}

// One eval → { observed: 'visible' | 'hidden' | 'detached' }.
export function waitExpr(selector) {
  const sel = JSON.stringify(selector);
  return `(function() {
  const el = document.querySelector(${sel});
  if (!el) return JSON.stringify({ observed: 'detached' });
  const rect = el.getBoundingClientRect();
  const style = getComputedStyle(el);
  const visible = rect.width > 0 && rect.height > 0
    && style.display !== 'none' && style.visibility !== 'hidden';
  return JSON.stringify({ observed: visible ? 'visible' : 'hidden' });
})()`;
}

function satisfied(observed, want) {
  if (want === 'visible') return observed === 'visible';
  if (want === 'detached') return observed === 'detached';
  if (want === 'attached') return observed !== 'detached';
  if (want === 'hidden') return observed === 'hidden'; // attached but not visible
  return false;
}

// sleepFn is injectable so tests can drive the state machine without real
// timers (or with short real ones for timeout tests).
export async function waitStr(cdp, sid, selector, opts = {}) {
  const {
    state = 'visible',
    timeoutMs = 10_000,
    intervalMs = 250,
    sleepFn = sleep,
  } = opts;
  if (!selector) throw new Error('CSS selector required');
  if (!WAIT_STATES.includes(state)) {
    throw new Error(`--state must be one of ${WAIT_STATES.join('|')} (got "${state}")`);
  }
  if (!(intervalMs > 0)) throw new Error('--interval must be a positive duration');

  const start = Date.now();
  let polls = 0;
  let last = 'unknown';
  for (;;) {
    polls++;
    try {
      const r = await evalJson(cdp, sid, waitExpr(selector));
      last = r && typeof r.observed === 'string' ? r.observed : 'unknown';
    } catch {
      last = 'detached'; // eval failed (navigation in flight) → not attached
    }
    if (satisfied(last, state)) {
      return `Waited: ${selector} is ${state} (after ${Date.now() - start}ms, ${polls} polls)`;
    }
    const elapsed = Date.now() - start;
    if (elapsed >= timeoutMs) {
      throw new ClickError('wait_timeout',
        `Timed out after ${Math.round(elapsed)}ms waiting for "${selector}" to be ${state} (last observed: ${last}, ${polls} polls)`);
    }
    await sleepFn(Math.min(intervalMs, timeoutMs - elapsed)); // never sleep past the deadline
  }
}
