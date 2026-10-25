#!/usr/bin/env node
/**
 * check-api-catalog：GUI Agent API 目录一致性守护。
 *
 * 对比两侧并打印差异、不一致时非零退出（可进 CI）：
 *   - 路由真源：src-tauri/src/api/server.rs 的 .route("<path>", <method>(...)) 注册
 *   - 目录：    src/api/catalog.ts 的 API_CATALOG 条目（method/path 字段）
 *
 * 规范化规则（两侧统一后比较，写清防再犯）：
 *   1. path 去掉 /api/v1 前缀（catalog 无前缀，server 有）；
 *   2. 路径参数形态统一为单个 ":" 占位符——server 的 {id}/{environment_id}/
 *      {*rest} 与 catalog 的 :id/:envId 全部归一为 ":"（参数命名差异不是
 *      遗漏，路径结构差异才是）；
 *   3. method 统一大写。
 *
 * 豁免清单（EXEMPT）：**有意不进 GUI 目录**的路由。修改前先在 git 历史/
 *      AGENT-API.md 里确认「不进目录」仍是有效约定，并维护理由注释。
 *
 * 用法：node scripts/check-api-catalog.mjs   （或 pnpm check:api）
 * 退出码：0 一致；1 存在差异。
 */
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const SERVER_RS = path.join(root, 'src-tauri/src/api/server.rs');
const CATALOG_TS = path.join(root, 'src/api/catalog.ts');

/**
 * 有意不进 GUI 目录的路由（规范化后的 METHOD path 形态）：
 * - 'GET /cdp/:/:/:/'：CDP 代理透传面（ws/http 代理内部通道），不是给
 *   agent 消费的 REST 资源——AGENT-API.md 也未收录该原始路径；
 * - 'GET /health'：进程级存活探针（evals 框架/运维用），目录定位是
 *   「Agent 业务 API」，已有 /events 之外的基础设施面一并豁免。
 *   如后续目录想收基础设施端点，删掉对应豁免行即可（脚本会要求补条目）。
 */
const EXEMPT = new Set(['GET /cdp/:/:/:', 'GET /health']);

// —— 规范化 ——
const normPath = (p) =>
  p
    .replace(/^\/api\/v1(?=\/)/, '')
    .replace(/\{\*?[A-Za-z_]+\}/g, ':') // server 的 {id} / {environment_id} / {*rest}
    .replace(/:[A-Za-z_]+/g, ':') // catalog 的 :id / :envId
    .replace(/\/+$/, ''); // 尾斜杠差异

/** server.rs → [["GET", "/environments"], ...]（解析失败 = 脚本失明，直接报错）。
 *  注意方法链：同一路径可 `get(..).post(..).delete(..)` 链式注册多个方法，
 * 必须收齐整段 handler 链（只看第一个方法 = 漏拍 — 本脚本第一版就犯过）。
 * `.route_service(`（/mcp 的 rmcp 挂载）不是 REST 路由：不比对，由 MCP.md
 * 管辖（见头部注释与豁免清单）。 */
function parseServerRoutes(src) {
  // 段分割：每个 .route("path", <handler链>) 到下一个 .route(/.route_service/
  // /.layer/.with_state 或文末；段内收全部方法 token（fn 路径如 ::get 后无括号，
  // 不会误命中 \bget\(）
  const re = /\.route\(\s*"([^"]+)"\s*,([\s\S]*?)(?=\.route\(|\.route_service\(|\.layer\(|\.with_state\(|$)/g;
  const methodRe = /\b(get|post|put|patch|delete)\(/g;
  const out = [];
  let m;
  while ((m = re.exec(src))) {
    let mm;
    while ((mm = methodRe.exec(m[2]))) out.push([mm[1].toUpperCase(), normPath(m[1])]);
  }
  if (out.length === 0) throw new Error('server.rs 未解析到任何 .route 注册——正则失明，请修脚本');
  return out;
}

/** catalog.ts → [["GET", "/environments"], ...]。只认 API_CATALOG 数组段内的条目 */
function parseCatalog(src) {
  const start = src.indexOf('API_CATALOG');
  if (start === -1) throw new Error('catalog.ts 缺 API_CATALOG 导出');
  const seg = src.slice(start);
  // method: 'GET', path: '/environments' —— 引号单双均可，method 与 path
  // 可同行或跨行（历史上两种排版并存，漏一种 = 假阳性）
  const re = /method:\s*['"](GET|POST|PUT|PATCH|DELETE)['"]\s*,\s*path:\s*['"]([^'"]+)['"]/g;
  const out = [];
  let m;
  while ((m = re.exec(seg))) out.push([m[1], normPath(m[2])]);
  if (out.length === 0) throw new Error('catalog.ts 未解析到任何条目——正则失明，请修脚本');
  return out;
}

const serverRoutes = parseServerRoutes(fs.readFileSync(SERVER_RS, 'utf8'));
const catalogItems = parseCatalog(fs.readFileSync(CATALOG_TS, 'utf8'));

const key = ([m, p]) => `${m} ${p}`;
const serverSet = new Set(serverRoutes.map(key));
const catalogSet = new Set(catalogItems.map(key));

const missing = [...serverSet].filter((k) => !catalogSet.has(k) && !EXEMPT.has(k)).sort();
const stale = [...catalogSet].filter((k) => !serverSet.has(k)).sort();
const exempted = [...serverSet].filter((k) => EXEMPT.has(k) && !catalogSet.has(k)).sort();
const dupServer = serverRoutes.map(key).filter((k, i, a) => a.indexOf(k) !== i);
const dupCatalog = catalogItems.map(key).filter((k, i, a) => a.indexOf(k) !== i);

if (missing.length === 0 && stale.length === 0 && dupServer.length === 0 && dupCatalog.length === 0) {
  console.log(
    `check-api-catalog: OK · server ${serverSet.size} 路由 = catalog ${catalogSet.size} 条目` +
      `（豁免 ${exempted.length} 条：${[...EXEMPT].filter((k) => !catalogSet.has(k)).join('、') || '无'}）`,
  );
  process.exit(0);
}

console.error('check-api-catalog: 发现目录不一致\n');
if (missing.length) {
  console.error(`  [缺失] server.rs 有、catalog.ts 无（${missing.length} 条）——补条目或加豁免：`);
  missing.forEach((k) => console.error(`    - ${k}`));
}
if (stale.length) {
  console.error(`  [多余] catalog.ts 有、server.rs 无（${stale.length} 条）——过期条目应删除：`);
  stale.forEach((k) => console.error(`    - ${k}`));
}
if (dupServer.length) console.error(`  [重复] server.rs 重复注册：${[...new Set(dupServer)].join('、')}`);
if (dupCatalog.length) console.error(`  [重复] catalog.ts 重复条目：${[...new Set(dupCatalog)].join('、')}`);
process.exit(1);
