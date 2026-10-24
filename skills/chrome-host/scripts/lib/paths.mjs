// Runtime paths, shared constants, and small shared helpers.
// Imported by every other module — keep it dependency-light (fs/os/path only).

import { mkdirSync } from 'fs';
import { homedir } from 'os';
import { resolve } from 'path';

export const TIMEOUT = 15000;
export const NAVIGATION_TIMEOUT = 30000;
export const IDLE_TIMEOUT = 20 * 60 * 1000;
export const DAEMON_CONNECT_RETRIES = 20;
export const DAEMON_CONNECT_DELAY = 300;
export const MIN_TARGET_PREFIX_LEN = 8;
export const IS_WINDOWS = process.platform === 'win32';

if (!IS_WINDOWS) process.umask(0o077);

export const RUNTIME_DIR = IS_WINDOWS
  ? resolve(process.env.LOCALAPPDATA || resolve(homedir(), 'AppData', 'Local'), 'cdp')
  : process.env.XDG_RUNTIME_DIR
    ? resolve(process.env.XDG_RUNTIME_DIR, 'cdp')
    : resolve(homedir(), '.cache', 'cdp');
try { mkdirSync(RUNTIME_DIR, { recursive: true, mode: 0o700 }); } catch {}

export const PAGES_CACHE = resolve(RUNTIME_DIR, 'pages.json');

export function sockPath(targetId) {
  return IS_WINDOWS
    ? `\\\\.\\pipe\\cdp-${targetId}`
    : resolve(RUNTIME_DIR, `cdp-${targetId}.sock`);
}

export const sleep = (ms) => new Promise(r => setTimeout(r, ms));
