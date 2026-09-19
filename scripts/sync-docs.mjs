#!/usr/bin/env node
// 构建前把根目录三份 agent 向手册同步进 docs/reference/。
// 单一事实源保持在仓库根目录（远程/本地 agent 直接读根目录 md），
// 文档站只做渲染，构建时复制，避免双源漂移。docs:dev / docs:build 均先执行。
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const MAPPING = [
  ['CLI.md', 'cli.md', 'CLI 手册'],
  ['AGENT-API.md', 'agent-api.md', 'Agent API'],
  ['MCP.md', 'mcp.md', 'MCP'],
];

const outDir = path.join(root, 'docs', 'reference');
mkdirSync(outDir, { recursive: true });

// 手册间互链（如 ](CLI.md)）复制进 docs/reference/ 后会死链，
// 改写为站点内路径；根目录源文件保持原样（仓库内 agent 直接读）
const SITE_LINKS = {
  'CLI.md': '/reference/cli',
  'AGENT-API.md': '/reference/agent-api',
  'MCP.md': '/reference/mcp',
};
const rewriteLinks = (md) =>
  md.replace(/\]\((?:\.?\/)?(CLI|AGENT-API|MCP)\.md(#[^)]*)?\)/g,
    (_, file, anchor = '') => `](${SITE_LINKS[`${file}.md`]}${anchor})`);

for (const [src, dst, title] of MAPPING) {
  const body = rewriteLinks(readFileSync(path.join(root, src), 'utf8'));
  writeFileSync(path.join(outDir, dst), `---\ntitle: ${title}\n---\n\n${body}`);
  console.log(`synced ${src} -> docs/reference/${dst}`);
}
