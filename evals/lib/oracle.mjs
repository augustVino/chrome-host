/**
 * oracle：断言判定。三类证据源 × 三态结论。
 *
 * 三态语义（核心设计——证据缺失绝不静默当通过）：
 * - passed   ：证据存在且符合预期
 * - failed   ：证据存在但不符合预期（真实的行为违约）
 * - unverified：证据缺失（上报通道未打通 / 步骤未执行 / 保存值不存在），
 *               无法下结论——与 failed 严格区分，防止「页没加载导致的
 *               零事件」被误记为通过或失败
 *
 * case 总状态优先级：failed > passed-with-unverified > passed；
 * 步骤级错误（非预期退出码/超时）直接判 failed——case 没跑完不是「未验证」
 * 的问题，是执行失败。
 */

/** dot path 取值：pick({"error":{"code":"X"}}, "error.code") → "X"（缺失 → undefined） */
export function pick(obj, path) {
  return String(path)
    .split('.')
    .reduce((cur, key) => (cur == null ? undefined : cur[key]), obj);
}

/** 站点事件断言：事件名精确匹配 + where 各路径精确相等，计数 ≥ minCount */
function judgeSiteAssertion(a, siteEvents) {
  const minCount = a.minCount ?? 1;
  if (!siteEvents || siteEvents.length === 0) {
    return unverified(a, 'site 证据通道无任何事件（页面未加载 / 上报未发出 / fixture 未挂）');
  }
  const conds = Object.entries(a.where ?? {});
  const matched = siteEvents.filter(
    (e) =>
      e.event === a.event &&
      conds.every(([p, expected]) => JSON.stringify(pick(e, p)) === JSON.stringify(expected)),
  );
  const whereText = conds.map(([p, v]) => `${p}=${JSON.stringify(v)}`).join(' & ');
  return matched.length >= minCount
    ? passed(a, `${a.event}${whereText ? ` (${whereText})` : ''} ×${matched.length} ≥ ${minCount}`)
    : failed(a, `${a.event}${whereText ? ` (${whereText})` : ''} 命中 ${matched.length} < ${minCount}（共 ${siteEvents.length} 条事件）`);
}

/** 响应断言：全部 CLI stdout 拼接文本上跑正则 */
function judgeResponseAssertion(a, responseText) {
  let re;
  try {
    re = new RegExp(a.pattern);
  } catch (e) {
    return unverified(a, `pattern 非法正则：${e.message}`);
  }
  if (!responseText) {
    return unverified(a, '无任何 CLI stdout 可检（步骤全未执行或零输出）');
  }
  return re.test(responseText)
    ? passed(a, `/${a.pattern}/ 命中`)
    : failed(a, `/${a.pattern}/ 未命中（stdout 共 ${responseText.length} 字符）`);
}

/** 适配器断言：步骤 saveAs 保存的外部事实取值比对 */
function judgeAdapterAssertion(a, store) {
  if (!(store instanceof Map) || !store.has(a.evidenceKey)) {
    return unverified(a, `evidence "${a.evidenceKey}" 缺失（保存它的步骤未执行或未存值）`);
  }
  const actual = store.get(a.evidenceKey);
  if (a.op === 'exists') return passed(a, `${a.evidenceKey} 存在`);
  if ('equals' in a) {
    return sameValue(actual, a.equals)
      ? passed(a, `${a.evidenceKey} == ${JSON.stringify(a.equals)}`)
      : failed(a, `${a.evidenceKey} 期望 ${JSON.stringify(a.equals)}，实际 ${JSON.stringify(actual)}`);
  }
  if ('contains' in a) {
    return String(actual).includes(a.contains)
      ? passed(a, `${a.evidenceKey} contains ${JSON.stringify(a.contains)}`)
      : failed(a, `${a.evidenceKey} 期望包含 ${JSON.stringify(a.contains)}，实际 ${JSON.stringify(String(actual).slice(0, 120))}…`);
  }
  return unverified(a, 'adapter 断言缺比较操作（equals/contains/exists）');
}

/**
 * 值比较：严格相等优先；跨 JSON 形态（1 vs "1"、true vs "true"）放宽为字符串
 * 相等——evidence 来自 CLI JSON 输出，case 文件是手写 JSON，两种数字/布尔
 * 字面形态都合法，语义相同即通过。
 */
function sameValue(actual, expected) {
  return actual === expected || String(actual) === String(expected);
}

function passed(a, detail) {
  return { source: a.source, name: a.name, status: 'passed', detail };
}
function failed(a, detail) {
  return { source: a.source, name: a.name, status: 'failed', detail };
}
function unverified(a, detail) {
  return { source: a.source, name: a.name, status: 'unverified', detail };
}

/**
 * 判定一个 case。
 *
 * @param {object} def case 定义（loader 已校验）
 * @param {object} evidenceCtx 三类证据：
 *   - siteEvents: Array（fixture 事件流；无 fixture 的 case 为空数组）
 *   - responseText: string（全部 smoke 步骤 stdout 拼接）
 *   - store: Map<string, any>（saveAs 保存的外部事实）
 *   - stepError: string | null（步骤级失败——非预期退出码 / 超时）
 * @returns {{status: 'passed'|'passed-with-unverified'|'failed', stepError: string|null, assertions: Array}}
 */
export function judgeCase(def, evidenceCtx) {
  const { siteEvents = [], responseText = '', store = new Map(), stepError = null } = evidenceCtx;
  const assertions = [];

  for (const a of def.assertions?.site ?? []) {
    assertions.push(judgeSiteAssertion({ ...a, source: 'site', name: a.name ?? `site:${a.event}` }, siteEvents));
  }
  for (const a of def.assertions?.response ?? []) {
    assertions.push(judgeResponseAssertion({ ...a, source: 'response', name: a.name ?? `response:/${a.pattern}/` }, responseText));
  }
  for (const a of def.assertions?.adapter ?? []) {
    assertions.push(judgeAdapterAssertion({ ...a, source: 'adapter', name: a.name ?? `adapter:${a.evidenceKey}` }, store));
  }

  let status;
  if (stepError || assertions.some((x) => x.status === 'failed')) {
    status = 'failed';
  } else if (assertions.some((x) => x.status === 'unverified')) {
    status = 'passed-with-unverified';
  } else {
    status = 'passed';
  }
  return { status, stepError, assertions };
}

/** 聚合多 case → 汇总 + 进程退出码（failed=1 > passed-with-unverified=2 > 0） */
export function aggregate(cases) {
  const failed = cases.filter((c) => c.status === 'failed').length;
  const pwu = cases.filter((c) => c.status === 'passed-with-unverified').length;
  const passed = cases.filter((c) => c.status === 'passed').length;
  const exitCode = failed > 0 ? 1 : pwu > 0 ? 2 : 0;
  return { total: cases.length, passed, 'passed-with-unverified': pwu, failed, exitCode };
}
