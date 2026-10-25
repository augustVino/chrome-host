#!/usr/bin/env node
/**
 * evals 框架入口：list / run / report。
 *
 * 用法：
 *   node evals/cli.mjs list
 *   node evals/cli.mjs run [--case <id>]... [--cli <path>] [--api-url <url>] [--json]
 *   node evals/cli.mjs report [--json]
 *
 * 退出码（run / report 同语义，可接 CI）：
 *   0 全部 passed；1 存在 failed；2 全部非 failed 但有 passed-with-unverified；
 *   3 应用未运行 / CLI 未找到（前置失败）；4 case 加载/校验失败。
 */
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { loadAllCases } from './lib/case-loader.mjs';
import {
  runCase,
  loadFixturePages,
  findCliBin,
  assertAppAlive,
  aggregate,
  FrameworkError,
} from './lib/runner.mjs';

const EVALS_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)));
const LAST_RUN_FILE = path.join(EVALS_DIR, '.last-run.json');
const DEFAULT_API_BASE = 'http://127.0.0.1:17890';

const argv = process.argv.slice(2);
const command = argv[0];
const flags = parseFlags(argv.slice(1));

try {
  switch (command) {
    case 'list':
      await cmdList(flags);
      break;
    case 'run':
      await cmdRun(flags);
      break;
    case 'report':
      cmdReport(flags);
      break;
    default:
      printUsage();
      process.exit(command ? 4 : 0);
  }
} catch (e) {
  if (e instanceof FrameworkError) {
    console.error(`[evals] ${e.message}`);
    process.exit(3);
  }
  console.error(`[evals] ${e.stack ?? e.message}`);
  process.exit(4);
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------
async function cmdList(flags) {
  const cases = loadAllCases();
  if (flags.json) {
    console.log(JSON.stringify(cases.map(({ file, def }) => ({ file: rel(file), ...def })), null, 2));
    return;
  }
  console.log(`共 ${cases.length} 个 case（evals/cases/**/*.case.json）：\n`);
  for (const { file, def } of cases) {
    const a = def.assertions ?? {};
    const counts = [
      a.site?.length ? `site ${a.site.length}` : null,
      a.response?.length ? `response ${a.response.length}` : null,
      a.adapter?.length ? `adapter ${a.adapter.length}` : null,
    ]
      .filter(Boolean)
      .join(' · ');
    console.log(`  ${def.id.padEnd(24)} ${def.title}`);
    console.log(
      `  ${''.padEnd(24)} ${rel(file)} · smoke ${def.smoke?.length ?? 0} 步 · 断言 ${counts || '无'}${def.fixture ? ` · fixture ${def.fixture.startPath}` : ''}\n`,
    );
  }
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------
async function cmdRun(flags) {
  const apiBase = flags['api-url'] ?? DEFAULT_API_BASE;
  const cliBin = findCliBin(flags.cli);
  const all = loadAllCases();
  const selected = flags.case?.length
    ? (() => {
        const byId = new Map(all.map((c) => [c.def.id, c]));
        return flags.case.map((id) => {
          if (!byId.has(id)) throw new Error(`--case "${id}" 不存在（node evals/cli.mjs list 查看）`);
          return byId.get(id);
        });
      })()
    : all;

  await assertAppAlive(apiBase);
  const pages = await loadFixturePages();
  const baseRunId = 'ev' + crypto.randomUUID().replace(/-/g, '').slice(0, 8);
  const startedAt = new Date().toISOString();

  if (!flags.json) {
    console.log(`chrome-host evals · run ${baseRunId} · ${startedAt}`);
    console.log(`  cli  = ${cliBin}`);
    console.log(`  api  = ${apiBase}`);
    console.log(`  case = ${selected.map((c) => c.def.id).join(', ')}\n`);
  }

  const results = [];
  for (let i = 0; i < selected.length; i++) {
    const { def } = selected[i];
    if (!flags.json) console.log(`[${i + 1}/${selected.length}] ${def.id} —— ${def.title}`);
    const result = await runCase({
      def,
      cliBin,
      apiBase,
      pages,
      baseRunId,
      log: (m) => !flags.json && console.log(m),
    });
    results.push(result);
    if (!flags.json) printCaseResult(result, false);
  }

  const summary = aggregate(results);
  const runRecord = {
    meta: { runId: baseRunId, startedAt, finishedAt: new Date().toISOString(), cli: cliBin, api: apiBase },
    summary,
    cases: results,
  };
  fs.writeFileSync(LAST_RUN_FILE, JSON.stringify(runRecord, null, 2));

  if (flags.json) {
    console.log(JSON.stringify(runRecord, null, 2));
  } else {
    printSummary(summary);
    console.log(`\n明细已写入 ${rel(LAST_RUN_FILE)}（node evals/cli.mjs report 重看）`);
  }
  process.exit(summary.exitCode);
}

// ---------------------------------------------------------------------------
// report
// ---------------------------------------------------------------------------
function cmdReport(flags) {
  if (!fs.existsSync(LAST_RUN_FILE)) {
    console.error(`[evals] 尚无运行记录（先 node evals/cli.mjs run）`);
    process.exit(4);
  }
  const record = JSON.parse(fs.readFileSync(LAST_RUN_FILE, 'utf8'));
  if (flags.json) {
    console.log(JSON.stringify(record, null, 2));
  } else {
    console.log(`chrome-host evals · run ${record.meta.runId} · ${record.meta.startedAt}`);
    console.log(`  cli  = ${record.meta.cli}`);
    console.log(`  api  = ${record.meta.api}\n`);
    for (const c of record.cases) printCaseResult(c, false);
    printSummary(record.summary);
  }
  process.exit(record.summary.exitCode);
}

// ---------------------------------------------------------------------------
// 渲染（纯文本，无 ANSI——输出要能直接贴进报告/日志）
// ---------------------------------------------------------------------------
function printCaseResult(c) {
  const mark = { passed: '✓ PASSED', 'passed-with-unverified': '? PASSED (with unverified)', failed: '✗ FAILED' };
  console.log(`  ${mark[c.status] ?? c.status}`);
  if (c.stepError) console.log(`      stepError: ${c.stepError}`);
  for (const a of c.assertions) {
    const icon = a.status === 'passed' ? '✓' : a.status === 'failed' ? '✗' : '?';
    console.log(`      ${icon} ${a.source.padEnd(8)} ${a.name}`);
    if (a.status !== 'passed') console.log(`        ${a.detail}`);
  }
  if (c.cleanup && c.cleanup.status !== 'ok') {
    console.log(`      cleanup: [${c.cleanup.status}] ${c.cleanup.detail}`);
  }
  console.log('');
}

function printSummary(s) {
  console.log(
    `汇总：${s.total} cases · passed ${s.passed} · passed-with-unverified ${s['passed-with-unverified']} · failed ${s.failed} → exit ${s.exitCode}`,
  );
}

// ---------------------------------------------------------------------------
// 小件
// ---------------------------------------------------------------------------
/** 手写 flag 解析（零依赖）：--key value / --key=value；布尔 flag（json）不取值；--case 可重复 */
function parseFlags(args) {
  const BOOL_FLAGS = new Set(['json']);
  const out = { case: [] };
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (!a.startsWith('--')) throw new Error(`无法识别的参数 "${a}"`);
    const eq = a.indexOf('=');
    const key = (eq === -1 ? a : a.slice(0, eq)).slice(2);
    if (eq === -1 && BOOL_FLAGS.has(key)) {
      out[key] = true;
      continue;
    }
    const value = eq === -1 ? args[++i] : a.slice(eq + 1);
    if (value === undefined || value.startsWith('--')) {
      throw new Error(`--${key} 缺值（布尔 flag 请声明为无值开关）`);
    }
    if (key === 'case') out.case.push(value);
    else out[key] = value;
  }
  return out;
}

function rel(p) {
  return path.relative(path.join(EVALS_DIR, '..'), p);
}

function printUsage() {
  console.log(`用法：
  node evals/cli.mjs list                                  列出全部 case
  node evals/cli.mjs run [--case <id>]... [--cli <path>]   执行（默认全部 case）
      [--api-url <url>] [--json]
  node evals/cli.mjs report [--json]                       重看最近一次运行

前置：chrome-host 应用已运行（默认 http://127.0.0.1:17890，--api-url 覆盖）；
      chrome-host CLI 可发现（$CARGO_TARGET_DIR/debug/chrome-host > PATH > --cli）。

退出码：0 全过 · 1 有 failed · 2 有 unverified · 3 前置失败 · 4 加载失败`);
}
