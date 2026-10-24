// Accessibility tree snapshot with @eN ref assignment (BEHAVIOR-SPEC §3.2/3.3).

// Roles that make an AX node referenceable (SPEC §3.2 INTERACTIVE_ROLES,
// matched case-insensitively to absorb Chrome's camelCase internalRole values).
const INTERACTIVE_ROLES = new Set([
  'button', 'checkbox', 'combobox', 'listbox', 'link', 'menuitem',
  'menuitemcheckbox', 'menuitemradio', 'option', 'radio', 'scrollbar',
  'searchbox', 'slider', 'spinbutton', 'switch', 'tab', 'textbox', 'treeitem',
]);

// Chrome reports internalRole names for some native form controls where the
// string differs from the ARIA role; fold them onto the canonical names so
// native controls get refs too (task: union of AX roles and form controls —
// form controls are covered by their AX role, no extra DOM-side judgment).
const INTERACTIVE_ROLE_ALIASES = new Map([
  ['textfield', 'textbox'],
  ['searchfield', 'searchbox'],
  ['radiobutton', 'radio'],
  ['popupbutton', 'combobox'],
  ['menulistoption', 'option'],
]);

// Structural rows/cells are referenceable only when the name is a URL —
// the "whole row is clickable" table convention (SPEC §3.2).
const URL_BEARING_ROLES = new Set(['row', 'cell', 'gridcell', 'listitem', 'rowheader']);

function normalizedRole(node) {
  const role = (node.role?.value || '').toLowerCase();
  return INTERACTIVE_ROLE_ALIASES.get(role) || role;
}

// Pure function: does this AX node get an @eN ref? (SPEC §3.2)
export function isRefWorthyAxNode(node) {
  const role = normalizedRole(node);
  if (INTERACTIVE_ROLES.has(role)) return true;
  if (URL_BEARING_ROLES.has(role)) {
    const name = node.name?.value;
    return typeof name === 'string' && /^https?:\/\//.test(name);
  }
  return false;
}

export function shouldShowAxNode(node, compact = false) {
  const role = node.role?.value || '';
  const name = node.name?.value ?? '';
  const value = node.value?.value;
  if (compact && role === 'InlineTextBox') return false;
  if (role === 'none' || role === 'generic') return false;
  // Nameless+valueless nodes are hidden UNLESS they are referenceable —
  // an interactive node must stay visible so its @eN is discoverable.
  if (name === '' && (value === '' || value == null)) return isRefWorthyAxNode(node);
  return true;
}

export function formatAxNode(node, depth, ref) {
  const role = node.role?.value || '';
  const name = node.name?.value ?? '';
  const value = node.value?.value;
  const indent = '  '.repeat(Math.min(depth, 10));
  let line = `${indent}`;
  if (ref) line += `@${ref} `;
  line += `[${role}]`;
  if (name !== '') line += ` ${name}`;
  if (!(value === '' || value == null)) line += ` = ${JSON.stringify(value)}`;
  return line;
}

export function orderedAxChildren(node, nodesById, childrenByParent) {
  const children = [];
  const seen = new Set();
  for (const childId of node.childIds || []) {
    const child = nodesById.get(childId);
    if (child && !seen.has(child.nodeId)) {
      seen.add(child.nodeId);
      children.push(child);
    }
  }
  for (const child of childrenByParent.get(node.nodeId) || []) {
    if (!seen.has(child.nodeId)) {
      seen.add(child.nodeId);
      children.push(child);
    }
  }
  return children;
}

// Snapshot + ref collection in one traversal. Returns { text, refs } where
// refs = [{ref: 'eN', backendNodeId, name?}] in assignment order.
// Numbering (SPEC §3.3): pre-order DFS, starting at 1; only ref-worthy nodes
// consume numbers (hidden/non-interactive nodes never do). Nodes without a
// backendDOMNodeId get no ref — a ref we cannot resolve to a CDP node would
// be a guaranteed dead click.
export async function collectSnapshot(cdp, sid, compact = false) {
  const { nodes } = await cdp.send('Accessibility.getFullAXTree', {}, sid);
  const nodesById = new Map(nodes.map(node => [node.nodeId, node]));
  const childrenByParent = new Map();
  for (const node of nodes) {
    if (!node.parentId) continue;
    if (!childrenByParent.has(node.parentId)) childrenByParent.set(node.parentId, []);
    childrenByParent.get(node.parentId).push(node);
  }

  const lines = [];
  const refs = [];
  const visited = new Set();
  let nextRef = 1;
  function visit(node, depth) {
    if (!node || visited.has(node.nodeId)) return;
    visited.add(node.nodeId);
    let ref;
    if (isRefWorthyAxNode(node) && Number.isInteger(node.backendDOMNodeId)) {
      ref = `e${nextRef++}`;
      refs.push({ ref, backendNodeId: node.backendDOMNodeId, name: node.name?.value || undefined });
    }
    if (shouldShowAxNode(node, compact)) lines.push(formatAxNode(node, depth, ref));
    for (const child of orderedAxChildren(node, nodesById, childrenByParent)) {
      visit(child, depth + 1);
    }
  }

  const roots = nodes.filter(node => !node.parentId || !nodesById.has(node.parentId));
  for (const root of roots) visit(root, 0);
  for (const node of nodes) visit(node, 0);

  return { text: lines.join('\n'), refs };
}

export async function snapshotStr(cdp, sid, compact = false) {
  return (await collectSnapshot(cdp, sid, compact)).text;
}
