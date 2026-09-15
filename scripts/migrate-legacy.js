#!/usr/bin/env node
/**
 * 旧版 chrome-host 数据迁移（P4 DoD-6，PRD §11）
 *
 * 用法：
 *   node scripts/migrate-legacy.js <旧 chrome-host.db 路径> [--api http://127.0.0.1:17890/api/v1] [--host https://a.example.com]
 *
 * 行为：
 *   1. 读取旧库 hosts 表（name / host_url）
 *   2. 按名称幂等去重：新应用中已存在同名环境 → 跳过并提示
 *   3. 调用本应用 REST 创建环境：name → name；host_url（旧版 hosts 源 URL）→ hostsSourceUrl
 *
 * 边界（PRD §11/§16）：
 *   - 旧库 host 字段（hosts 映射 JSON）不迁移——新模型在实例启动时现拉最新配置
 *   - 新环境启动页 host 使用 --host 参数值（默认 https://a.example.com），可自行 PATCH
 *   - 需要 Node ≥ 22.13（node:sqlite）
 */

import { DatabaseSync } from 'node:sqlite';
import process from 'node:process';

const args = process.argv.slice(2);
const dbPath = args.find((a, i) => i === 0 && !a.startsWith('--'));
const apiFlag = args.indexOf('--api');
const API = apiFlag >= 0 ? args[apiFlag + 1] : 'http://127.0.0.1:17890/api/v1';
const hostFlag = args.indexOf('--host');
const DEFAULT_HOST = hostFlag >= 0 ? args[hostFlag + 1] : 'https://a.example.com';

if (!dbPath) {
  console.error('用法: node scripts/migrate-legacy.js <旧 chrome-host.db 路径> [--api URL] [--host 起始页]');
  process.exit(1);
}

let oldDb;
try {
  oldDb = new DatabaseSync(dbPath, { readOnly: true });
} catch (e) {
  console.error(`无法打开旧库 ${dbPath}: ${e.message}`);
  process.exit(1);
}

let rows;
try {
  rows = oldDb.prepare('SELECT name, host_url FROM hosts ORDER BY name').all();
} catch (e) {
  console.error(`读取 hosts 表失败（确认是旧版 chrome-host.db）: ${e.message}`);
  process.exit(1);
}

if (rows.length === 0) {
  console.log('旧库 hosts 表为空，无需迁移。');
  process.exit(0);
}

const existing = await fetch(`${API}/environments`)
  .then((r) => r.json())
  .then((list) => new Set(list.map((e) => e.name)))
  .catch((e) => {
    console.error(`Agent API 不可达（${API}）：请先启动应用再执行迁移。${e}`);
    process.exit(1);
  });

let created = 0;
let skipped = 0;
for (const row of rows) {
  const name = (row.name || '').trim();
  if (!name) continue;
  if (existing.has(name)) {
    console.log(`↷ 跳过（同名环境已存在）: ${name}`);
    skipped++;
    continue;
  }
  const resp = await fetch(`${API}/environments`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      name,
      host: DEFAULT_HOST,
      hostsSourceUrl: row.host_url || null,
    }),
  });
  if (resp.ok) {
    created++;
    console.log(`✓ 已迁移: ${name}`);
  } else {
    const body = await resp.text();
    console.error(`✗ 迁移失败: ${name} → HTTP ${resp.status} ${body.slice(0, 200)}`);
  }
}

console.log(`\n完成：新建 ${created}，跳过 ${skipped}，共 ${rows.length} 条。`);
console.log('提示：旧 hosts 映射 JSON 不迁移；实例启动时会按 hostsSourceUrl 现拉最新配置。');
