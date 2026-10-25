/**
 * case 加载 + schema 校验。
 *
 * 校验是框架的第一道质量门：case 文件写错（路径形态、步骤类型、断言形状）
 * 在加载期就报精确错误，绝不到运行中期才以 unverified 的形态迷惑人。
 */
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const CASES_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'cases');

/** 递归收集 *.case.json（子目录即分组，目录名只影响展示不影响 id） */
export function discoverCaseFiles(dir = CASES_DIR) {
  const out = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) out.push(...discoverCaseFiles(full));
    else if (entry.name.endsWith('.case.json')) out.push(full);
  }
  return out.sort();
}

/**
 * @returns {Array<{file: string, def: object}>} 校验失败的文件直接 throw（带文件名与全部错误行）
 */
export function loadAllCases(dir = CASES_DIR) {
  const files = discoverCaseFiles(dir);
  if (files.length === 0) throw new Error(`evals/cases/ 下没有任何 *.case.json`);
  const seenIds = new Set();
  return files.map((file) => {
    const def = JSON.parse(fs.readFileSync(file, 'utf8'));
    const errors = validate(def, seenIds);
    if (errors.length) {
      throw new Error(`case 校验失败 ${file}:\n${errors.map((e) => `  - ${e}`).join('\n')}`);
    }
    return { file, def };
  });
}

function validate(def, seenIds) {
  const errs = [];
  const push = (m) => errs.push(m);
  const t = (v) => Array.isArray(v) ? 'array' : typeof v;

  if (!(typeof def.id === 'string' && /^[a-z0-9][a-z0-9-]*$/.test(def.id))) {
    push(`id 必须是小写字母/数字/连字符的字符串（got ${JSON.stringify(def.id)}）`);
  } else if (seenIds.has(def.id)) {
    push(`id "${def.id}" 与其他 case 重复`);
  } else {
    seenIds.add(def.id);
  }
  if (typeof def.title !== 'string' || !def.title) push('title 必须是非空字符串');
  if (typeof def.task !== 'string' || !def.task) push('task 必须是非空字符串（描述 agent 要完成什么）');

  if (def.fixture !== undefined) {
    if (t(def.fixture) !== 'object' || Array.isArray(def.fixture)) {
      push('fixture 必须是对象 {startPath}');
    } else if (typeof def.fixture.startPath !== 'string' || !def.fixture.startPath.startsWith('/')) {
      push('fixture.startPath 必须是以 / 开头的路径');
    }
  }

  // —— 断言 ——
  if (def.assertions !== undefined && (t(def.assertions) !== 'object' || Array.isArray(def.assertions))) {
    push('assertions 必须是对象 {site?, response?, adapter?}');
  } else if (def.assertions) {
    for (const key of ['site', 'response', 'adapter']) {
      const list = def.assertions[key];
      if (list === undefined) continue;
      if (!Array.isArray(list)) {
        push(`assertions.${key} 必须是数组`);
        continue;
      }
      list.forEach((a, i) => {
        const at = `assertions.${key}[${i}]`;
        if (t(a) !== 'object') return push(`${at} 必须是对象`);
        if (key === 'site') {
          if (typeof a.event !== 'string' || !a.event) push(`${at}.event 必须是非空字符串`);
          if (a.where !== undefined && t(a.where) !== 'object') push(`${at}.where 必须是对象（dot path → 期望值）`);
          if (a.minCount !== undefined && (!Number.isInteger(a.minCount) || a.minCount < 1)) push(`${at}.minCount 必须是 ≥1 的整数`);
        }
        if (key === 'response') {
          if (typeof a.pattern !== 'string' || !a.pattern) push(`${at}.pattern 必须是非空字符串（正则）`);
          else {
            try {
              new RegExp(a.pattern);
            } catch (e) {
              push(`${at}.pattern 非法正则：${e.message}`);
            }
          }
        }
        if (key === 'adapter') {
          if (typeof a.evidenceKey !== 'string' || !a.evidenceKey) push(`${at}.evidenceKey 必须是非空字符串`);
          const ops = ['equals', 'contains', 'exists'].filter((k) => k in a);
          if (ops.length !== 1) push(`${at} 必须恰好含一个比较操作 equals|contains|exists（got ${ops.length} 个）`);
          if (ops[0] === 'exists' && a.exists !== true) push(`${at}.exists 只能为 true`);
        }
      });
    }
  }

  // —— smoke 步骤 ——
  if (def.smoke !== undefined && !Array.isArray(def.smoke)) {
    push('smoke 必须是数组');
  } else if (def.smoke) {
    const saveKeys = new Set();
    def.smoke.forEach((s, i) => {
      const at = `smoke[${i}]`;
      if (t(s) !== 'object') return push(`${at} 必须是对象`);
      const doc = `${at}(${s.step})`;
      switch (s.step) {
        case 'cli':
          if (!Array.isArray(s.args) || s.args.some((x) => typeof x !== 'string')) {
            push(`${doc}.args 必须是字符串数组`);
          }
          if (s.json !== undefined && typeof s.json !== 'boolean') push(`${doc}.json 必须是布尔`);
          if (s.allowFailure !== undefined && typeof s.allowFailure !== 'boolean') push(`${doc}.allowFailure 必须是布尔`);
          checkSaveAs(doc, s, saveKeys, push);
          break;
        case 'sleep':
          if (!Number.isInteger(s.ms) || s.ms <= 0) push(`${doc}.ms 必须是正整数`);
          break;
        case 'wait':
          if (!Array.isArray(s.args) || s.args.some((x) => typeof x !== 'string')) {
            push(`${doc}.args 必须是字符串数组`);
          }
          if (t(s.until) !== 'object' || typeof s.until.path !== 'string') {
            push(`${doc}.until 必须是 {path, equals?|contains?}`);
          } else if (!('equals' in s.until) && !('contains' in s.until)) {
            push(`${doc}.until 必须含 equals 或 contains 之一`);
          }
          checkSaveAs(doc, s, saveKeys, push);
          break;
        case 'read':
          if (typeof s.from !== 'string') push(`${doc}.from 必须是字符串（已保存的 evidence key）`);
          if (typeof s.saveAs !== 'string' || !s.saveAs) push(`${doc}.saveAs 必须是非空字符串`);
          checkSaveAs(doc, s, saveKeys, push);
          break;
        default:
          push(`${at}.step 必须是 cli|sleep|wait|read 之一（got ${JSON.stringify(s.step)}）`);
      }
    });
    if (def.fixture && (def.assertions?.site ?? []).length > 0 && !def.smoke?.length) {
      push('fixture 存在但 smoke 为空——页面谁来打开？');
    }
  } else if (def.fixture) {
    push('声明了 fixture 却没有 smoke 步骤（fixture 页无人打开）');
  }
  return errs;
}

function checkSaveAs(doc, s, saveKeys, push) {
  if (s.saveAs !== undefined) {
    if (typeof s.saveAs !== 'string' || !/^\w+$/.test(s.saveAs)) {
      push(`${doc}.saveAs 必须是 \\w+ 标识符（占位符 {${s.saveAs}} 要能替换）`);
    } else if (saveKeys.has(s.saveAs)) {
      push(`${doc}.saveAs "${s.saveAs}" 重复（后保存会覆盖前者，请改名）`);
    } else {
      saveKeys.add(s.saveAs);
    }
  }
}
