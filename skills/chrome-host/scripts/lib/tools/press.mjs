// press <key> [--target <ref|sel>] (BEHAVIOR-SPEC §4.2, simplified): key
// name → descriptor → rawKeyDown (+char) → keyUp via Input.dispatchKeyEvent.
//
// Dispatch follows SPEC's tested sequence — rawKeyDown / char / keyUp, never
// plain keyDown: the char event is sent separately (without
// windowsVirtualKeyCode) so text insertion is explicit and suppressible.
// Ctrl/Meta/Alt combos suppress the char (Ctrl+A must not type "a"); Shift
// does not suppress. Modifier keys themselves are never dispatched as
// keyDown/keyUp pairs — they are expressed through the modifiers bit field
// only.
//
// --target focuses an element first. Selector path: one in-page eval
// (scrollIntoView + el.focus() + DOM-focus re-check). Ref path: RefStore →
// backendNodeId → DOM.scrollIntoViewIfNeeded + DOM.focus (SPEC §4.2 uses
// DOM.focus for both; the eval route is the selector-only fallback since
// selectors have no backendNodeId without a resolve round trip).

import { ClickError, evalJson } from './click-native.mjs';
import { isRefTarget, refMissError } from './interact.mjs';
import { normalizeRef } from '../ref-store.mjs';

// Modifier bit field (SPEC §2.4/§4.2: alt=1 ctrl=2 meta=4 shift=8).
const MODIFIER_BITS = { alt: 1, ctrl: 2, meta: 4, shift: 8 };
const MODIFIER_ALIASES = { control: 'ctrl', option: 'alt', opt: 'alt', cmd: 'meta', command: 'meta', super: 'meta' };
const MODIFIER_ORDER = ['alt', 'ctrl', 'meta', 'shift']; // canonical display order

// Named keys (SPEC §4.2 SPECIAL_KEYMAP, complete). text is the char the key
// produces (Enter/Tab/Space only); vk is windowsVirtualKeyCode — included
// because pages reading legacy e.keyCode depend on it.
export const NAMED_KEYS = {
  Enter: { key: 'Enter', code: 'Enter', text: '\r', vk: 13 },
  Escape: { key: 'Escape', code: 'Escape', vk: 27 },
  Tab: { key: 'Tab', code: 'Tab', text: '\t', vk: 9 },
  Backspace: { key: 'Backspace', code: 'Backspace', vk: 8 },
  Delete: { key: 'Delete', code: 'Delete', vk: 46 },
  Home: { key: 'Home', code: 'Home', vk: 36 },
  End: { key: 'End', code: 'End', vk: 35 },
  PageUp: { key: 'PageUp', code: 'PageUp', vk: 33 },
  PageDown: { key: 'PageDown', code: 'PageDown', vk: 34 },
  ArrowUp: { key: 'ArrowUp', code: 'ArrowUp', vk: 38 },
  ArrowDown: { key: 'ArrowDown', code: 'ArrowDown', vk: 40 },
  ArrowLeft: { key: 'ArrowLeft', code: 'ArrowLeft', vk: 37 },
  ArrowRight: { key: 'ArrowRight', code: 'ArrowRight', vk: 39 },
  Space: { key: ' ', code: 'Space', text: ' ', vk: 32 },
};
for (let i = 1; i <= 12; i++) NAMED_KEYS[`F${i}`] = { key: `F${i}`, code: `F${i}`, vk: 111 + i };

// Case-insensitive lookup for named keys ('ENTER', 'pageUp' → canonical).
const NAMED_LOOKUP = new Map(Object.keys(NAMED_KEYS).map(k => [k.toLowerCase(), NAMED_KEYS[k]]));

function singleCharDescriptor(ch) {
  if (/[a-z]/.test(ch)) return { key: ch, code: `Key${ch.toUpperCase()}`, text: ch, vk: ch.toUpperCase().charCodeAt(0) };
  if (/[A-Z]/.test(ch)) return { key: ch, code: `Key${ch}`, text: ch, vk: ch.charCodeAt(0) };
  if (/[0-9]/.test(ch)) return { key: ch, code: `Digit${ch}`, text: ch, vk: ch.charCodeAt(0) };
  // Punctuation and other single chars: no standard code — the browser
  // synthesizes it from text (SPEC resolveKeyDescriptor).
  return { key: ch, code: '', text: ch };
}

const UNKNOWN_KEY_HINT = 'press: unknown key "%s" — supported: named keys (Enter, Tab, Escape, Backspace, Delete, '
  + 'Home, End, PageUp, PageDown, ArrowUp/Down/Left/Right, Space, F1-F12), single characters, and modifier '
  + 'combos like Control+A / Meta+V / Shift+ArrowDown (see "cdp help")';

// '<key>' | 'Mod+Mod+Key' → { key, code, text?, vk?, modifiers, modifierNames }.
// Parsing per SPEC parseKeySpec: split on '+', trim, drop empty tokens;
// ≤1 remaining token → the whole string is the base key; otherwise the first
// non-modifier token is the base (all-modifiers input takes the last token,
// which then fails named lookup — pressing a bare modifier is unsupported).
export function keyDescriptor(input) {
  const raw = String(input ?? '');
  const tokens = raw.split('+').map(t => t.trim()).filter(Boolean);
  const mods = new Set();
  const nonMods = [];
  let base;
  if (tokens.length <= 1) {
    base = raw.trim();
  } else {
    for (const t of tokens) {
      const word = t.toLowerCase();
      const name = MODIFIER_BITS[word] ? word : MODIFIER_ALIASES[word];
      if (name) mods.add(name);
      else nonMods.push(t);
    }
    if (nonMods.length === 0) base = tokens[tokens.length - 1];
    else if (nonMods.length === 1) base = nonMods[0];
    else throw new ClickError('key_unknown', UNKNOWN_KEY_HINT.replace('%s', raw)); // e.g. "A+B"
  }
  if (!base) throw new ClickError('key_unknown', UNKNOWN_KEY_HINT.replace('%s', raw));

  let d;
  if (base.length === 1) d = singleCharDescriptor(base);
  else {
    const named = NAMED_LOOKUP.get(base.toLowerCase());
    if (!named) throw new ClickError('key_unknown', UNKNOWN_KEY_HINT.replace('%s', raw));
    d = { ...named };
  }
  let modifiers = 0;
  for (const m of mods) modifiers |= MODIFIER_BITS[m];
  d.modifiers = modifiers;
  d.modifierNames = MODIFIER_ORDER.filter(m => mods.has(m)).map(m => m[0].toUpperCase() + m.slice(1));
  return d;
}

function displayKey(d) {
  const key = d.key === ' ' ? 'Space' : d.key; // Space's key is literally " "
  return [...d.modifierNames, key].join('+');
}

// Focus the --target element. Returns a display label ("<INPUT> \"q\"" for
// selectors, "@e3 \"name\"" for refs).
async function focusPressTarget(cdp, sid, target, store) {
  if (isRefTarget(target)) {
    if (!store) throw new Error('ref_not_found: no ref table — run "snap <target>" first');
    const entry = store.resolve(target);
    if (!entry) throw refMissError(store, target);
    try { await cdp.send('DOM.scrollIntoViewIfNeeded', { backendNodeId: entry.backendNodeId }, sid); } catch {}
    try {
      await cdp.send('DOM.focus', { backendNodeId: entry.backendNodeId }, sid);
    } catch (e) {
      throw new ClickError('focus_failed', `press --target ${target} could not be focused (${e.message})`);
    }
    const key = normalizeRef(target);
    return entry.name ? `@${key} "${entry.name}"` : `@${key}`;
  }
  if (!target) throw new Error('--target requires a value (<@eN ref or CSS selector>)');
  const sel = JSON.stringify(target);
  const r = await evalJson(cdp, sid, `(function() {
  const el = document.querySelector(${sel});
  if (!el) return JSON.stringify({ error: 'not_found' });
  el.scrollIntoView({ block: 'center' });
  el.focus();
  if (el.getRootNode().activeElement !== el) return JSON.stringify({ error: 'focus_failed' });
  const label = el.getAttribute('aria-label') || el.getAttribute('name')
    || el.getAttribute('placeholder') || el.id || '';
  return JSON.stringify({ tag: el.tagName, label });
})()`);
  if (r && r.error === 'not_found') throw new ClickError('element_not_found', `Element not found: ${target}`);
  if (r && r.error === 'focus_failed') {
    throw new ClickError('focus_failed', `press --target ${target} could not be focused`);
  }
  if (!r || typeof r.tag !== 'string') throw new Error(`press: unexpected focus result for ${target}`);
  return `<${r.tag}>${r.label ? ` "${r.label}"` : ''}`;
}

// press <key> [--target <ref|sel>]
export async function pressStr(cdp, sid, key, target, store) {
  if (!key || typeof key !== 'string') {
    throw new Error('key required (e.g. Enter, a, Control+A — see "cdp help")');
  }
  const d = keyDescriptor(key);
  const where = (target !== undefined && target !== null && target !== '')
    ? ` on ${await focusPressTarget(cdp, sid, target, store)}`
    : '';

  // Text suppression (SPEC §4.2): ctrl/meta/alt combos must not type the
  // char. Shift does NOT suppress.
  const suppressText = (d.modifiers & (MODIFIER_BITS.alt | MODIFIER_BITS.ctrl | MODIFIER_BITS.meta)) !== 0;

  const common = { key: d.key, modifiers: d.modifiers };
  if (d.code) common.code = d.code;
  if (d.vk !== undefined) common.windowsVirtualKeyCode = d.vk;

  await cdp.send('Input.dispatchKeyEvent', { ...common, type: 'rawKeyDown' }, sid);
  try {
    if (d.text && !suppressText) {
      // The char event carries the text and, per SPEC, no vk.
      const char = { type: 'char', key: d.key, text: d.text, modifiers: d.modifiers };
      if (d.code) char.code = d.code;
      await cdp.send('Input.dispatchKeyEvent', char, sid);
    }
    await cdp.send('Input.dispatchKeyEvent', { ...common, type: 'keyUp' }, sid);
  } catch (e) {
    // Never leave the key stuck down (same compensation as the click chain).
    try { await cdp.send('Input.dispatchKeyEvent', { ...common, type: 'keyUp' }, sid); } catch {}
    throw e;
  }
  return `Pressed ${displayKey(d)}${where}`;
}
