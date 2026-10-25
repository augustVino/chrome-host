/**
 * runner：case 编排。
 *
 * 生命周期（每 case）：
 *   fixture server（随机端口）→ smoke 步骤（声明式 cli/wait/sleep/read，
 *   占位符替换）→ 250ms 证据缓冲 → oracle 判定 → 清理（env delete --yes，
 *   失败重试一次）→ 关 fixture server。
 *
 * 原则：
 * - 评估的就是 agent 真实接口——一切操作走 chrome-host CLI 子进程，不直连
 *   REST/CDP（fixture server 是被测页面的宿主，不是操作通道）。
 * - 诚实三态：步骤非预期失败 / 超时 → stepError → case 判 failed（不是
 *   unverified）；证据缺失 → unverified。清理失败显式报告，不静默。
 * - 隔离：每 case 独立 runId（{baseRunId}-{caseId}），env/实例名、事件流、
 *   fixture server 全部按 runId 命名空间隔离。
 */
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { startFixtureServer } from './fixture-server.mjs';
import { judgeCase, aggregate, pick } from './oracle.mjs';

/** 单条 CLI 步骤缺省预算：instance create（含启动 Chrome）~10s 量级，留足余量 */
const DEFAULT_CLI_TIMEOUT_MS = 120_000;
/** site 证据收口前的上报缓冲（任务书口径 250ms） */
const EVIDENCE_DRAIN_MS = 250;

export class FrameworkError extends Error {}

/** CLI 二进制探测：--cli 覆盖 > $CARGO_TARGET_DIR/debug/chrome-host > PATH */
export function findCliBin(override) {
  if (override) {
    if (!fs.existsSync(override)) throw new FrameworkError(`--cli 指定的路径不存在：${override}`);
    return override;
  }
  if (process.env.CARGO_TARGET_DIR) {
    const p = path.join(process.env.CARGO_TARGET_DIR, 'debug', 'chrome-host');
    if (fs.existsSync(p)) return p;
  }
  for (const dir of (process.env.PATH ?? '').split(':')) {
    const p = path.join(dir, 'chrome-host');
    if (fs.existsSync(p) && fs.statSync(p).isFile()) return p;
  }
  throw new FrameworkError(
    '找不到 chrome-host CLI：设置 CARGO_TARGET_DIR、传 --cli <路径>，或把 chrome-host 放进 PATH',
  );
}

/** 前置检查：app 必须已运行（不自强起）。连不上 → 明确报错退出。 */
export async function assertAppAlive(apiBase) {
  try {
    const res = await fetch(new URL('/api/v1/health', apiBase), { signal: AbortSignal.timeout(3000) });
    if (!res.ok) throw new Error(`health 返回 ${res.status}`);
    return true;
  } catch (e) {
    throw new FrameworkError(
      `chrome-host 应用未运行于 ${apiBase}（${e.message}）。\n` +
        `请先启动应用（如 cargo build 后运行 chrome-host-app --hidden）；框架不自起 app。`,
    );
  }
}

/** 加载 fixtures/pages/ 下全部页面模块：路由路径 /<模块名> → 模块 */
export async function loadFixturePages() {
  const dir = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'fixtures', 'pages');
  const pages = new Map();
  for (const f of fs.readdirSync(dir).filter((x) => x.endsWith('.mjs')).sort()) {
    const mod = await import(path.join(dir, f));
    if (typeof mod.render !== 'function' && typeof mod.handle !== 'function') {
      throw new FrameworkError(`fixture 页 ${f} 必须导出 render(ctx) 或 handle(req,res,ctx)`);
    }
    pages.set(`/${f.replace(/\.mjs$/, '')}`, mod);
  }
  return pages;
}

/**
 * 执行一个 case（编排主循环；清理在 finally 兜底，无论成败）。
 * @returns case 结果：{id,title,runId,status,stepError,assertions,cleanup,steps}
 */
export async function runCase({ def, cliBin, apiBase, pages, baseRunId, log = () => {} }) {
  const runId = `${baseRunId}-${def.id}`;
  const store = new Map();
  const stepRecords = [];
  let stepError = null;
  let fixture = null;

  // 占位符变量表：{runId}/{baseUrl}/{saveAs 键}；未登记的 {xxx} 原样保留
  //（拼错的占位符会让 CLI 拿到字面 "{xxx}" 参数而失败——显式暴露而非静默）。
  const buildVars = () => ({
    runId,
    ...(fixture ? { baseUrl: fixture.baseUrl } : {}),
    ...Object.fromEntries(
      [...store.entries()].map(([k, v]) => [k, typeof v === 'string' ? v : JSON.stringify(v)]),
    ),
  });
  const subst = (text) => {
    const vars = buildVars();
    return String(text).replace(/\{(\w+)\}/g, (m, key) => (key in vars ? vars[key] : m));
  };

  const runCli = (args, timeoutMs = DEFAULT_CLI_TIMEOUT_MS) =>
    execCli(cliBin, args, { timeoutMs, env: { ...process.env, CHROME_HOST_API_URL: apiBase } });

  const result = {
    id: def.id,
    title: def.title,
    runId,
    status: 'failed',
    stepError: null,
    assertions: [],
    cleanup: null,
    steps: stepRecords,
  };

  try {
    if (def.fixture) {
      fixture = await startFixtureServer({ runId, pages });
      log(`  fixture ${fixture.baseUrl}${def.fixture.startPath}`);
    }

    steps: for (let i = 0; i < (def.smoke ?? []).length; i++) {
      const step = def.smoke[i];
      const doc = `步骤${i}(${step.step})`;

      if (step.step === 'sleep') {
        stepRecords.push({ index: i, step: 'sleep', ms: step.ms });
        await sleep(step.ms);
        continue;
      }

      if (step.step === 'cli') {
        const args = step.args.map(subst);
        if (step.json) args.push('--json');
        const r = await runCli(args, step.timeoutMs);
        stepRecords.push({ index: i, step: 'cli', args, exitCode: r.code, stdout: r.stdout, stderr: r.stderr });
        saveStepValue(step, r.stdout, store);
        if (r.code !== 0 && !step.allowFailure) {
          stepError = `${doc} 退出码 ${r.code}：${firstLine(r.stderr || r.stdout)}`;
          break steps;
        }
        continue;
      }

      if (step.step === 'wait') {
        const args = step.args.map(subst).concat('--json');
        const timeoutMs = step.timeoutMs ?? 15_000;
        const deadline = Date.now() + timeoutMs;
        let last = null;
        for (;;) {
          const r = await runCli(args, Math.min(DEFAULT_CLI_TIMEOUT_MS, timeoutMs));
          last = r;
          const parsed = tryParseJson(r.stdout);
          const actual = parsed.ok ? pick(parsed.value, step.until.path) : undefined;
          const ok =
            parsed.ok &&
            r.code === 0 &&
            ('equals' in step.until
              ? String(actual) === String(step.until.equals)
              : String(actual ?? '').includes(step.until.contains));
          if (ok) {
            if (step.saveAs) store.set(step.saveAs, step.path ? pick(parsed.value, step.path) : parsed.value);
            stepRecords.push({ index: i, step: 'wait', args, exitCode: r.code, matched: true, stdout: r.stdout });
            continue steps;
          }
          if (Date.now() >= deadline) {
            stepError = `${doc} 等待超时（${timeoutMs}ms）：until.${step.until.path} 期望 ${JSON.stringify(step.until.equals ?? step.until.contains)}，最后输出 ${firstLine(last.stdout) || firstLine(last.stderr) || '(空)'}`;
            stepRecords.push({ index: i, step: 'wait', args, exitCode: r.code, matched: false, stdout: r.stdout });
            break steps;
          }
          await sleep(step.intervalMs ?? 500);
        }
      }

      if (step.step === 'read') {
        const from = store.get(step.from);
        store.set(step.saveAs, step.path ? pick(from, step.path) : from);
        stepRecords.push({ index: i, step: 'read', from: step.from, saveAs: step.saveAs });
        continue;
      }
    }

    // —— 收 site 证据（250ms 缓冲让在途上报落地）——
    let siteEvents = [];
    if (fixture) {
      await sleep(EVIDENCE_DRAIN_MS);
      siteEvents = fixture.evidenceFor(runId);
    }

    const responseText = stepRecords
      .map((s) => s.stdout ?? '')
      .filter(Boolean)
      .join('\n');
    // 断言值里的占位符（where 值 / pattern / equals / contains）在此替换——
    // oracle 只负责判定，不感知变量表
    const judged = judgeCase(
      { ...def, assertions: deepSubst(def.assertions, subst) },
      { siteEvents, responseText, store, stepError },
    );
    result.status = judged.status;
    result.stepError = judged.stepError;
    result.assertions = judged.assertions;
    return result;
  } finally {
    // —— 清理：无论成败都执行；env delete --yes 失败重试一次，仍败显式报告 ——
    result.cleanup = await cleanupEnv({ store, subst, runCli, log });
    await fixture?.close();
  }
}

/** saveAs 落库：stdout 优先按 JSON 解析（--json 步骤），失败则原样存字符串 */
function saveStepValue(step, stdout, store) {
  if (!step.saveAs) return;
  const parsed = tryParseJson(stdout);
  const value = parsed.ok ? parsed.value : stdout;
  store.set(step.saveAs, step.path ? pick(value, step.path) : value);
}

/** 深度替换对象内全部字符串的占位符（断言值用） */
function deepSubst(node, subst) {
  if (typeof node === 'string') return subst(node);
  if (Array.isArray(node)) return node.map((x) => deepSubst(x, subst));
  if (node && typeof node === 'object') {
    return Object.fromEntries(Object.entries(node).map(([k, v]) => [k, deepSubst(v, subst)]));
  }
  return node;
}

async function cleanupEnv({ store, subst, runCli, log }) {
  const envId = store.get('envId');
  if (!envId) return { status: 'skipped', detail: '无 envId（创建环境的步骤未成功执行）' };
  const args = ['env', 'delete', subst('{envId}'), '--yes'];
  for (let attempt = 1; attempt <= 2; attempt++) {
    const r = await runCli(args);
    if (r.code === 0) return { status: 'ok', detail: `env ${envId} 已删除` };
    log(`  清理第 ${attempt} 次失败（exit ${r.code}）：${firstLine(r.stderr || r.stdout)}`);
    if (attempt < 2) await sleep(1500);
  }
  return { status: 'failed', detail: `env ${envId} 删除失败（已重试一次）——残留环境请手动清理` };
}

function execCli(bin, args, { timeoutMs, env }) {
  return new Promise((resolve) => {
    const child = spawn(bin, args, { env });
    let stdout = '';
    let stderr = '';
    let settled = false;
    const timer = setTimeout(() => {
      if (settled) return;
      settled = true;
      child.kill('SIGKILL');
      resolve({ code: null, stdout, stderr: stderr + `\n(evals: CLI 超时 ${timeoutMs}ms 被杀)` });
    }, timeoutMs);
    child.stdout.on('data', (c) => (stdout += c));
    child.stderr.on('data', (c) => (stderr += c));
    child.on('error', (e) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve({ code: null, stdout, stderr: `spawn 失败：${e.message}` });
    });
    child.on('close', (code) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve({ code: code ?? null, stdout, stderr });
    });
  });
}

function tryParseJson(text) {
  try {
    return { ok: true, value: JSON.parse(text) };
  } catch {
    return { ok: false, value: null };
  }
}

function firstLine(s) {
  return String(s ?? '')
    .split('\n')
    .find((l) => l.trim()) ?? '';
}

export function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

export { aggregate };
