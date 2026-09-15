#!/usr/bin/env node
/**
 * 构建 CLI sidecar 并放置到 src-tauri/binaries/，供 Tauri externalBin 打进 .app。
 * 由 tauri.conf.json 的 beforeDevCommand / beforeBuildCommand 自动调用，也可手动运行。
 *
 * target triple 必须优先取 TAURI_ENV_TARGET_TRIPLE（tauri CLI 注入）：CI 中
 * Intel 包是在 arm runner 上交叉编译的，按 host triple 编会产出错误架构。
 */
import { execFileSync } from 'node:child_process';
import { chmodSync, copyFileSync, existsSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), '..');

function hostTriple() {
  try {
    const out = execFileSync('rustc', ['-Vv'], { encoding: 'utf8' });
    return /^host:\s*(.+)$/m.exec(out)?.[1];
  } catch {
    return undefined;
  }
}

const triple = process.env.TAURI_ENV_TARGET_TRIPLE || hostTriple();
if (!triple) {
  console.error('[cli-sidecar] 无法确定 target triple（TAURI_ENV_TARGET_TRIPLE 未设置且 rustc 不可用）');
  process.exit(1);
}

// 跟随 tauri 注入的 triple 交叉编译；手动运行时按 host 编译
const isCross = Boolean(process.env.TAURI_ENV_TARGET_TRIPLE);
const cargoArgs = ['build', '--release', '-p', 'chrome-host-cli'];
if (isCross) cargoArgs.push('--target', triple);

console.log(`[cli-sidecar] cargo ${cargoArgs.join(' ')}`);
execFileSync('cargo', cargoArgs, { stdio: 'inherit', cwd: repoRoot });

const isWindows = triple.includes('windows');
const exeName = isWindows ? 'chrome-host.exe' : 'chrome-host';
// 显式 --target 时产物在 target/<triple>/release/，否则在 target/release/
const targetDir = process.env.CARGO_TARGET_DIR
  ? (isCross ? join(process.env.CARGO_TARGET_DIR, triple) : process.env.CARGO_TARGET_DIR)
  : join(repoRoot, 'target', ...(isCross ? [triple] : []));
const src = join(targetDir, 'release', exeName);
if (!existsSync(src)) {
  console.error(`[cli-sidecar] 构建产物不存在：${src}`);
  process.exit(1);
}

// Tauri externalBin 约定：src-tauri/binaries/<name>-<triple>[.exe]
// 文件名用 chrome-host-cli：Tauri 禁止 sidecar 与 Cargo 包名（chrome-host）同名；
// 打包后位于 .app/Contents/MacOS/chrome-host-cli，PATH 上的链接名不受影响（chrome-host）
const destDir = join(repoRoot, 'src-tauri', 'binaries');
const dest = join(destDir, `chrome-host-cli-${triple}${isWindows ? '.exe' : ''}`);
mkdirSync(destDir, { recursive: true });
copyFileSync(src, dest);
if (!isWindows) chmodSync(dest, 0o755);
console.log(`[cli-sidecar] ${src} → ${dest}`);
