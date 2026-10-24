// JS dialog (alert/confirm/prompt/beforeunload) takeover — P-1.
//
// Native JS dialogs block every CDP command on their tab until handled.
// The daemon subscribes to Page.javascriptDialogOpening right after attach,
// auto-accepts each dialog, and records it in a bounded buffer so commands
// can report what popped up while they ran.
//
// Follows BEHAVIOR-SPEC §1 (BrowserSkill re-implementation) adapted to this
// CLI's architecture: one daemon process per tab, so the original's per-tab
// Maps collapse into a single tracker per process.

const DIALOG_TYPES = new Set(['alert', 'confirm', 'prompt', 'beforeunload']);
const MAX_FIELD_LEN = 4096;

export const MAX_DIALOG_BUFFER = 32;

function stringField(value) {
  if (typeof value !== 'string') return undefined;
  return value.length > MAX_FIELD_LEN
    ? value.slice(0, MAX_FIELD_LEN) + '... [truncated]'
    : value;
}

// Defensive parsing per BEHAVIOR-SPEC §1.3: wrong types degrade, never throw.
// beforeunload arrives with message:false on some Chrome versions — the
// "string only" rule turns that into ''.
export function parseDialogOpeningParams(params = {}) {
  return {
    type: DIALOG_TYPES.has(params.type) ? params.type : 'alert',
    message: stringField(params.message) ?? '',
    url: stringField(params.url),
    defaultPrompt: stringField(params.defaultPrompt),
    hasBrowserHandler: typeof params.hasBrowserHandler === 'boolean'
      ? params.hasBrowserHandler
      : undefined,
  };
}

export class DialogTracker {
  constructor(tabId) {
    this.tabId = tabId;
    this.entries = [];
    this.sequence = 0;
  }

  // Cursor value for command-entry marking (BEHAVIOR-SPEC §1.6).
  cursor() { return this.sequence; }

  // Append after a successful handle. Sequence only moves on success (§1.5),
  // so a failed handle never shifts the cursor.
  record(parsed) {
    const sequence = ++this.sequence;
    const entry = {
      tab_id: this.tabId,
      type: parsed.type,
      message: parsed.message,
      handled: 'accepted',
      sequence,
    };
    if (parsed.url !== undefined) entry.url = parsed.url;
    if (parsed.defaultPrompt !== undefined) entry.default_prompt = parsed.defaultPrompt;
    if (parsed.hasBrowserHandler !== undefined) entry.has_browser_handler = parsed.hasBrowserHandler;
    this.entries.push(entry);
    while (this.entries.length > MAX_DIALOG_BUFFER) this.entries.shift(); // FIFO, drop oldest
    return entry;
  }

  // Entries with sequence strictly greater than the cursor (§1.6). No
  // "N evicted" marker: a cursor older than the buffer only sees survivors.
  since(cursor) {
    return this.entries.filter(e => e.sequence > cursor);
  }
}

// Page.javascriptDialogOpening handler. Unconditional accept (§1.4): no
// cooperation mode in this CLI; accept on beforeunload means "allow leaving".
// prompt must carry promptText (defaultPrompt ?? "").
// On failure: log to stderr, do not record — the dialog stays open and CDP
// commands keep blocking until their own timeout (same as upstream).
export async function handleDialogOpening(cdp, sessionId, tracker, params) {
  const parsed = parseDialogOpeningParams(params);
  const handle = { accept: true };
  if (parsed.type === 'prompt') handle.promptText = parsed.defaultPrompt ?? '';
  try {
    await cdp.send('Page.handleJavaScriptDialog', handle, sessionId);
  } catch (e) {
    process.stderr.write(`Daemon: Page.handleJavaScriptDialog failed: ${e.message}\n`);
    return;
  }
  tracker.record(parsed);
}

// `dialogs <target> [--since N]` — one line per buffered dialog.
export function dialogsStr(dialogs, args = []) {
  if (!dialogs) throw new Error('Dialog buffer unavailable');
  let since = 0;
  const i = args.indexOf('--since');
  if (i !== -1) {
    const n = Number(args[i + 1]);
    if (!Number.isInteger(n) || n < 0) throw new Error('--since requires a non-negative integer');
    since = n;
  }
  return dialogs.since(since).map((e) => {
    let line = `#${e.sequence} ${e.type}  ${JSON.stringify(e.message)}`;
    if (e.default_prompt !== undefined) line += `  (default: ${JSON.stringify(e.default_prompt)})`;
    if (e.url !== undefined) line += `  ${e.url}`;
    return line;
  }).join('\n');
}
