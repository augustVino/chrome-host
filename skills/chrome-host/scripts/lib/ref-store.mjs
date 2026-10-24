// @eN ref table for one tab daemon (BEHAVIOR-SPEC §3.1/3.5/3.6/3.7, simplified).
//
// Semantics: refs describe the LAST snap only.
//  - replace() on every snap: whole-table swap + generation++ (§3.5).
//  - invalidate() on navigation / document replacement: the whole table is
//    cleared and flagged stale — old refs failing afterwards is a FEATURE
//    (forces re-observation), not a bug.
//  - Per-tab by construction: each tab runs its own daemon with its own
//    RefStore, so a ref used from another tab simply misses here →
//    ref_not_found (§3.7 "cross-tab use is always ref_not_found").
//
// Known simplification vs. the source (§3.6 backendNodeId reuse race): the
// source guards "ref survived invalidation but the document changed" with a
// per-tab documentRevision counter checked right before input. We do NOT
// track revisions — invalidation clears the whole table, which covers the
// common race (old ref → ref_stale). The remaining window (snap racing a
// navigation, backendNodeId reused by the new document) is accepted; the
// backstop is that ref resolution itself fails → target_moved, never a
// silent click on a wrong-but-live node without a geometry re-read.

// §3.7 normalization: strip exactly ONE leading '@' ('@e3' → 'e3').
// Case-sensitive by design: 'E3'/'@E3' stay as-is, miss the table, and the
// caller reports ref_not_found.
export function normalizeRef(raw) {
  if (typeof raw !== 'string') return raw;
  return raw.startsWith('@') ? raw.slice(1) : raw;
}

export class RefStore {
  #entries = new Map(); // normalized 'eN' → { backendNodeId, name? }
  #generation = 0;
  #stale = false;
  #lastInvalidReason = null;

  // Whole-table replace (called by snap). entries: [{ref, backendNodeId, name?}]
  // from collectSnapshot. Defensive per entry: a bad entry is skipped, never
  // throws — a half-parsed AX tree must not break the swap.
  replace(entries) {
    const map = new Map();
    for (const entry of entries || []) {
      if (!entry || !Number.isInteger(entry.backendNodeId)) continue;
      const key = normalizeRef(String(entry.ref ?? ''));
      if (!/^e\d+$/.test(key)) continue;
      map.set(key, entry.name ? { backendNodeId: entry.backendNodeId, name: entry.name }
        : { backendNodeId: entry.backendNodeId });
    }
    this.#entries = map;
    this.#generation += 1;
    this.#stale = false;
    this.#lastInvalidReason = null;
  }

  // Whole-table invalidation (§3.6). Returns true if entries were dropped.
  // Stays sticky once set (repeat invalidations don't un-stale an empty
  // table) until the next successful replace().
  invalidate(reason = 'document changed') {
    if (this.#entries.size === 0) return false;
    this.#entries.clear();
    this.#generation += 1;
    this.#stale = true;
    this.#lastInvalidReason = reason;
    return true;
  }

  // Resolve a raw ref ('@e3' or 'e3') → { backendNodeId, name? } | null.
  // Normalizes inside; Map lookup stays case-sensitive ('E3' misses).
  resolve(rawRef) {
    return this.#entries.get(normalizeRef(rawRef)) ?? null;
  }

  get generation() { return this.#generation; }
  get size() { return this.#entries.size; }
  get stale() { return this.#stale; }
  get lastInvalidReason() { return this.#lastInvalidReason; }
}
