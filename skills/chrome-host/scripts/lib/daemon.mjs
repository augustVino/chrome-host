// Per-tab daemon: holds a CDP session open, serves NDJSON commands over a
// Unix socket / named pipe. Also hosts the CLI↔daemon client helpers.

import { unlinkSync, existsSync, readFileSync } from 'fs';
import { spawn } from 'child_process';
import net from 'net';
import {
  IDLE_TIMEOUT, DAEMON_CONNECT_RETRIES, DAEMON_CONNECT_DELAY,
  IS_WINDOWS, PAGES_CACHE, sockPath, sleep,
} from './paths.mjs';
import { CDP, getWsUrl } from './cdp-client.mjs';
import { getPages, formatPageList, resolvePrefix } from './pages.mjs';
import { snapshotStr } from './tools/snapshot.mjs';
import { evalStr, htmlStr, netStr } from './tools/evaluate.mjs';
import { navStr } from './tools/navigate.mjs';
import { clickStr, clickXyStr, typeStr, loadAllStr } from './tools/interact.mjs';
import { shotStr } from './tools/screenshot.mjs';
import { evalRawStr } from './tools/raw.mjs';
import { DialogTracker, handleDialogOpening, dialogsStr } from './dialogs.mjs';

// Dispatch one daemon command. cdp is injected so tests can pass a fake.
// dialogs is the per-tab DialogTracker; when present, every command marks the
// dialog cursor on entry and its response gains an optional dialogs_seen
// field if new dialogs were auto-accepted while it ran (BEHAVIOR-SPEC §1.6).
// Returns { ok, result } / { ok, error } / { ok, result, stopAfter }.
export async function execCommand(cdp, sessionId, targetId, req, dialogs) {
  const cursor = dialogs?.cursor();
  const res = await dispatchCommand(cdp, sessionId, targetId, req, dialogs);
  if (dialogs) {
    const seen = dialogs.since(cursor);
    if (seen.length > 0) res.dialogs_seen = seen; // omitted when empty: wire stays backward compatible
  }
  return res;
}

async function dispatchCommand(cdp, sessionId, targetId, { cmd, args }, dialogs) {
  try {
    let result;
    switch (cmd) {
      case 'list': {
        const pages = await getPages(cdp);
        result = formatPageList(pages);
        break;
      }
      case 'list_raw': {
        const pages = await getPages(cdp);
        result = JSON.stringify(pages);
        break;
      }
      case 'snap': case 'snapshot': result = await snapshotStr(cdp, sessionId, true); break;
      case 'eval': result = await evalStr(cdp, sessionId, args[0]); break;
      case 'shot': case 'screenshot': result = await shotStr(cdp, sessionId, args[0], targetId); break;
      case 'html': result = await htmlStr(cdp, sessionId, args[0]); break;
      case 'nav': case 'navigate': result = await navStr(cdp, sessionId, args[0]); break;
      case 'net': case 'network': result = await netStr(cdp, sessionId); break;
      case 'click': result = await clickStr(cdp, sessionId, args[0]); break;
      case 'clickxy': result = await clickXyStr(cdp, sessionId, args[0], args[1]); break;
      case 'type': result = await typeStr(cdp, sessionId, args[0]); break;
      case 'loadall': result = await loadAllStr(cdp, sessionId, args[0], args[1] ? parseInt(args[1]) : 1500); break;
      case 'evalraw': result = await evalRawStr(cdp, sessionId, args[0], args[1]); break;
      case 'dialogs': result = dialogsStr(dialogs, args); break;
      case 'stop': return { ok: true, result: '', stopAfter: true };
      default: return { ok: false, error: `Unknown command: ${cmd}` };
    }
    return { ok: true, result: result ?? '' };
  } catch (e) {
    return { ok: false, error: e.message };
  }
}

export async function runDaemon(targetId) {
  const sp = sockPath(targetId);

  const cdp = new CDP();
  try {
    await cdp.connect(await getWsUrl());
  } catch (e) {
    process.stderr.write(`Daemon: cannot connect to Chrome: ${e.message}\n`);
    process.exit(1);
  }

  let sessionId;
  try {
    const res = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
    sessionId = res.sessionId;
  } catch (e) {
    process.stderr.write(`Daemon: attach failed: ${e.message}\n`);
    cdp.close();
    process.exit(1);
  }

  // P-1: take over JS dialogs (alert/confirm/prompt/beforeunload). While one
  // is open, every CDP command on this tab blocks. Subscribe BEFORE
  // Page.enable — dialog events only start flowing after the enable, so no
  // opening event can slip past the handler.
  const dialogs = new DialogTracker(targetId);
  cdp.onEvent('Page.javascriptDialogOpening', (params) => {
    void handleDialogOpening(cdp, sessionId, dialogs, params);
  });
  try {
    await cdp.send('Page.enable', {}, sessionId);
  } catch (e) {
    process.stderr.write(`Daemon: Page.enable failed: ${e.message}\n`);
    cdp.close();
    process.exit(1);
  }

  // Shutdown helpers
  let alive = true;
  function shutdown() {
    if (!alive) return;
    alive = false;
    server.close();
    if (!IS_WINDOWS) try { unlinkSync(sp); } catch {}
    cdp.close();
    process.exit(0);
  }

  // Exit if target goes away or Chrome disconnects
  cdp.onEvent('Target.targetDestroyed', (params) => {
    if (params.targetId === targetId) shutdown();
  });
  cdp.onEvent('Target.detachedFromTarget', (params) => {
    if (params.sessionId === sessionId) shutdown();
  });
  cdp.onClose(() => shutdown());
  process.on('SIGTERM', shutdown);
  process.on('SIGINT', shutdown);

  // Idle timer
  let idleTimer = setTimeout(shutdown, IDLE_TIMEOUT);
  function resetIdle() {
    clearTimeout(idleTimer);
    idleTimer = setTimeout(shutdown, IDLE_TIMEOUT);
  }

  // Handle a command
  async function handleCommand(req) {
    resetIdle();
    return execCommand(cdp, sessionId, targetId, req, dialogs);
  }

  // Unix socket server — NDJSON protocol
  const server = net.createServer((conn) => {
    let buf = '';
    conn.on('data', (chunk) => {
      buf += chunk.toString();
      const lines = buf.split('\n');
      buf = lines.pop(); // keep incomplete last line
      for (const line of lines) {
        if (!line.trim()) continue;
        let req;
        try {
          req = JSON.parse(line);
        } catch {
          conn.write(JSON.stringify({ ok: false, error: 'Invalid JSON request', id: null }) + '\n');
          continue;
        }
        handleCommand(req).then((res) => {
          const payload = JSON.stringify({ ...res, id: req.id }) + '\n';
          if (res.stopAfter) conn.end(payload, shutdown);
          else conn.write(payload);
        });
      }
    });
  });

  server.on('error', (e) => {
    process.stderr.write(`Daemon server listen failed: ${e.message}\n`);
    process.exit(1);
  });

  if (!IS_WINDOWS) try { unlinkSync(sp); } catch {}
  server.listen(sp);
}

// ---------------------------------------------------------------------------
// CLI ↔ daemon communication
// ---------------------------------------------------------------------------

function connectToSocket(sp) {
  return new Promise((resolve, reject) => {
    const conn = net.connect(sp);
    conn.on('connect', () => resolve(conn));
    conn.on('error', reject);
  });
}

export async function getOrStartTabDaemon(targetId) {
  const sp = sockPath(targetId);
  // Try existing daemon
  try { return await connectToSocket(sp); } catch {}

  // Clean stale socket
  if (!IS_WINDOWS) try { unlinkSync(sp); } catch {}

  // Spawn daemon
  const child = spawn(process.execPath, [process.argv[1], '_daemon', targetId], {
    detached: true,
    stdio: 'ignore',
  });
  child.unref();

  // Wait for socket (includes time for user to click Allow)
  for (let i = 0; i < DAEMON_CONNECT_RETRIES; i++) {
    await sleep(DAEMON_CONNECT_DELAY);
    try { return await connectToSocket(sp); } catch {}
  }
  throw new Error('Daemon failed to start — did you click Allow in Chrome?');
}

export function sendCommand(conn, req) {
  return new Promise((resolve, reject) => {
    let buf = '';
    let settled = false;

    const cleanup = () => {
      conn.off('data', onData);
      conn.off('error', onError);
      conn.off('end', onEnd);
      conn.off('close', onClose);
    };

    const onData = (chunk) => {
      buf += chunk.toString();
      const idx = buf.indexOf('\n');
      if (idx === -1) return;
      settled = true;
      cleanup();
      resolve(JSON.parse(buf.slice(0, idx)));
      conn.end();
    };

    const onError = (error) => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(error);
    };

    const onEnd = () => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(new Error('Connection closed before response'));
    };

    const onClose = () => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(new Error('Connection closed before response'));
    };

    conn.on('data', onData);
    conn.on('error', onError);
    conn.on('end', onEnd);
    conn.on('close', onClose);
    req.id = 1;
    conn.write(JSON.stringify(req) + '\n');
  });
}

// ---------------------------------------------------------------------------
// Stop daemons
// ---------------------------------------------------------------------------

export async function stopDaemons(targetPrefix) {
  if (!existsSync(PAGES_CACHE)) return;
  const pages = JSON.parse(readFileSync(PAGES_CACHE, 'utf8'));
  const targets = targetPrefix
    ? [resolvePrefix(targetPrefix, pages.map(p => p.targetId), 'target')]
    : pages.map(p => p.targetId);

  for (const targetId of targets) {
    const sp = sockPath(targetId);
    try {
      const conn = await connectToSocket(sp);
      await sendCommand(conn, { cmd: 'stop' });
    } catch {
      if (!IS_WINDOWS) try { unlinkSync(sp); } catch {}
    }
  }
}
