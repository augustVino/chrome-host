// fill <sel|@eN> <text> (BEHAVIOR-SPEC §4.1, simplified): fillability check →
// focus → clear → Input.insertText → read-back verify.
//
// Both target paths converge on ONE pipeline: resolve the target to a stable
// object handle (DOM.resolveNode → objectId) and run every page script via
// Runtime.callFunctionOn on that objectId. This is SPEC's DOM-script
// principle (§公共设计模式 #6): object identity stays stable across calls,
// whereas re-querying a selector between evals can silently hit a different
// element after a page re-render. The selector path acquires the handle via
// DOM.getDocument + DOM.querySelector; the @eN ref path via the RefStore's
// backendNodeId. Focus uses DOM.focus per SPEC step 3 (SPEC explicitly
// rejects a Runtime.callFunctionOn `.focus()` fallback).
//
// Clearing follows SPEC step 4: the prototype value setter writes '' for
// input/textarea (bypasses a page-hijacked `value` property), textContent=''
// for contenteditable roots, then an 'input' event. NOT select-all+Backspace
// via key events — that costs extra CDP round trips and breaks on controls
// without a selection API (number inputs).
//
// Simplifications vs. SPEC are listed in the task report: no caret-position
// round, no ready/background state machine, one read-back verify;
// number/maxLength violations surface as fill_value_mismatch instead of
// fill_value_invalid.

import { ClickError } from './click-native.mjs';
import { isRefTarget, refMissError } from './interact.mjs';
import { normalizeRef } from '../ref-store.mjs';

// Fillability gate (SPEC §4.1 FILL_EDITABLE_FUNCTION, simplified): an
// enabled text-like <input> / <textarea> / the contenteditable editor root.
// readonly/disabled reject; descendants INSIDE an editing host reject (fill
// the root). Pure function on element-shaped objects → unit-testable in
// Node and inlined into page scripts via toString().
export function fillEditable(el) {
  if (!el || !el.isConnected) return { editable: false, why: 'element is not in the document' };
  const disabled = el.matches(':disabled'); // covers ancestor <fieldset disabled> too
  if (el.tagName === 'TEXTAREA') {
    if (disabled) return { editable: false, why: 'disabled' };
    if (el.readOnly) return { editable: false, why: 'readonly' };
    return { editable: true, kind: 'native' };
  }
  if (el.tagName === 'INPUT') {
    const type = (el.getAttribute('type') || 'text').toLowerCase();
    const TEXTUAL = ['text', 'search', 'tel', 'url', 'email', 'password', 'number'];
    if (!TEXTUAL.includes(type)) {
      return { editable: false, why: `input type "${type}" is not fillable (use a text-like input)` };
    }
    if (disabled) return { editable: false, why: 'disabled' };
    if (el.readOnly) return { editable: false, why: 'readonly' };
    return { editable: true, kind: 'native' };
  }
  if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) {
    return { editable: true, kind: 'editable' };
  }
  return {
    editable: false,
    why: el.isContentEditable
      ? 'not the contenteditable editor root (fill the editing host itself instead)'
      : 'not a text input, textarea, or contenteditable editor root',
  };
}

// Page-side scripts. Each runs with this = the target element (stable
// objectId). fillEditable is inlined via toString() — the page has no module
// scope to import from (same trick as click-native's hitTestWalk).

function checkFn() {
  return `function() {
  const check = (${fillEditable})(this);
  if (!check.editable) return { not_fillable: check.why };
  const label = this.getAttribute('aria-label') || this.getAttribute('name')
    || this.getAttribute('placeholder') || this.id
    || (this.textContent || '').trim().substring(0, 40);
  return { tag: this.tagName, kind: check.kind, label: label || '' };
}`;
}

function clearFn() {
  return `function() {
  const check = (${fillEditable})(this);
  if (!check.editable) return { not_fillable: check.why };
  // DOM focus (getRootNode semantics work in background tabs, unlike
  // document.hasFocus()).
  if (this.getRootNode().activeElement !== this) return { focus_lost: true };
  if (check.kind === 'native') {
    const proto = this.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(this, '');
  } else {
    this.textContent = '';
  }
  this.dispatchEvent(new Event('input', { bubbles: true }));
  return { cleared: true };
}`;
}

// Read-back verify (SPEC step 9, one independent call). Node precomputes
// both expected variants — SPEC §4.1: insertText drops CRLF carriers, and
// single-line inputs turn newlines into spaces — so the page script stays
// regex-free.
function verifyFn() {
  return `function(multiLine, singleLine) {
  if (!this.isConnected) return { gone: true };
  const check = (${fillEditable})(this);
  const actual = check.kind === 'native' ? this.value : this.textContent;
  const expected = check.kind === 'native' && this.tagName === 'INPUT' ? singleLine : multiLine;
  return { matches: actual === expected, actual: String(actual === undefined ? '' : actual), expected };
}`;
}

async function callFn(cdp, sid, objectId, functionDeclaration, args = []) {
  const res = await cdp.send('Runtime.callFunctionOn', {
    objectId, functionDeclaration,
    arguments: args.map(value => ({ value })),
    returnByValue: true,
  }, sid);
  if (res?.exceptionDetails) {
    const d = res.exceptionDetails;
    throw new Error(`fill: page script failed: ${d.text || d.exception?.description || 'unknown error'}`);
  }
  return res?.result?.value;
}

function requireFillable(r, desc) {
  if (r && r.not_fillable !== undefined) {
    throw new ClickError('target_not_fillable',
      `fill target ${desc} is not fillable: ${r.not_fillable} — choose a text input, textarea, or the contenteditable editor root`);
  }
}

// Target → stable objectId. Ref path: RefStore → backendNodeId → resolveNode
// (failure = target_moved, same semantics as resolveRefPoint). Selector
// path: DOM.getDocument → DOM.querySelector (nodeId 0 = element_not_found,
// same code as click) → resolveNode.
async function resolveFillTarget(cdp, sid, target, store) {
  if (isRefTarget(target)) {
    if (!store) throw new Error('ref_not_found: no ref table — run "snap <target>" first');
    const entry = store.resolve(target);
    if (!entry) throw refMissError(store, target);
    try {
      const { object } = await cdp.send('DOM.resolveNode', { backendNodeId: entry.backendNodeId }, sid);
      if (!object?.objectId) throw new Error('no objectId');
      return { objectId: object.objectId, refName: entry.name, desc: `@${normalizeRef(target)}` };
    } catch (e) {
      throw new Error(
        `target_moved: ref no longer resolves to a live node (${e.message}) — run "snap <target>" again and use fresh @eN refs`);
    }
  }
  if (!target || typeof target !== 'string') throw new Error('CSS selector or @eN ref required');
  const { root } = await cdp.send('DOM.getDocument', {}, sid);
  const { nodeId } = await cdp.send('DOM.querySelector', { nodeId: root?.nodeId, selector: target }, sid);
  if (!nodeId) throw new ClickError('element_not_found', `Element not found: ${target}`);
  try {
    const { object } = await cdp.send('DOM.resolveNode', { nodeId }, sid);
    if (!object?.objectId) throw new Error('no objectId');
    return { objectId: object.objectId, refName: undefined, desc: target };
  } catch (e) {
    throw new Error(`fill: could not resolve ${target} to a node (${e.message})`);
  }
}

function truncate(s) { return s.length > 40 ? s.slice(0, 40) + '…' : s; }

// fill <sel|@eN> <text> — text '' means "clear only" (insertText skipped).
export async function fillStr(cdp, sid, target, text, store) {
  if (typeof text !== 'string') throw new Error('text required (pass "" to only clear the target)');
  const { objectId, refName, desc } = await resolveFillTarget(cdp, sid, target, store);

  try {
    // Scroll best-effort before focus so the element is in the viewport.
    try { await cdp.send('DOM.scrollIntoViewIfNeeded', { objectId }, sid); } catch {}

    const check = await callFn(cdp, sid, objectId, checkFn());
    requireFillable(check, desc);

    try {
      await cdp.send('DOM.focus', { objectId }, sid);
    } catch (e) {
      // SPEC step 3 fallback: Chrome refuses DOM.focus on disabled controls —
      // re-check fillability so that case reports target_not_fillable, not a
      // generic focus failure.
      const recheck = await callFn(cdp, sid, objectId, checkFn());
      requireFillable(recheck, desc);
      throw new ClickError('fill_focus_lost', `fill target ${desc} could not be focused (${e.message})`);
    }

    const clear = await callFn(cdp, sid, objectId, clearFn());
    requireFillable(clear, desc); // page may have disabled it since the check
    if (clear && clear.focus_lost) {
      throw new ClickError('fill_focus_lost',
        `fill target ${desc} does not have focus (page moved it after focus)`);
    }

    const multiLine = text.replace(/\r\n?/g, '\n');
    if (multiLine !== '') {
      await cdp.send('Input.insertText', { text: multiLine }, sid);
    }

    const verify = await callFn(cdp, sid, objectId, verifyFn(), [multiLine, multiLine.replace(/\n/g, ' ')]);
    if (verify && verify.gone) {
      throw new ClickError('fill_value_mismatch',
        `fill target ${desc} was removed or replaced during fill — observe the page before deciding whether to retry`);
    }
    if (!verify || typeof verify.matches !== 'boolean') {
      throw new ClickError('fill_value_mismatch', `fill could not verify the value of ${desc} (unexpected verify result)`);
    }
    if (!verify.matches) {
      // The write may still have taken effect — report honestly, let the
      // caller observe before retrying (SPEC's fill_value_mismatch stance).
      throw new ClickError('fill_value_mismatch',
        `fill could not verify the expected value (the write may have partially applied — observe before retrying): expected "${truncate(String(verify.expected ?? multiLine))}", got "${truncate(String(verify.actual ?? ''))}"`);
    }

    const tag = check?.tag || 'ELEMENT';
    const label = refName || check?.label || '';
    const shown = truncate(text);
    return `Filled <${tag}>${label ? ` "${label}"` : ''} with "${shown}" (${text.length} ${text.length === 1 ? 'char' : 'chars'})`;
  } finally {
    try { await cdp.send('Runtime.releaseObject', { objectId }, sid); } catch {}
  }
}
