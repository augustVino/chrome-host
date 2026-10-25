//! 事件上报端点前缀：fixture 页 JS 把证据 POST 到 /__evidence/<runId>。
//! 按 runId 隔离——server 本身每 case 独立，runId 校验是双保险（串错 run
//! 的事件 404 丢弃，绝不混流）。
import http from 'node:http';

export const EVIDENCE_PREFIX = '/__evidence/';

/**
 * 起 fixture server（每 case 一个实例、随机端口）。
 *
 * @param {object} opts
 * @param {string} opts.runId 本 case 的 run 标识（页面注入 + 事件流隔离）
 * @param {Map<string, object>} opts.pages 路径 → 页面模块（fixtures/pages/*.mjs），
 *   模块契约：`render(ctx) → string`（返回完整 HTML）或
 *   `handle(req, res, ctx) → boolean`（完全自控路由，返回 false 交回 404）
 * @returns {Promise<{port: number, baseUrl: string, evidenceFor: (runId: string) => Array, close: () => Promise<void>}>}
 */
export function startFixtureServer({ runId, pages }) {
  return new Promise((resolve, reject) => {
    /** @type {Map<string, Array<{event: string, data: object, ts: number}>>} */
    const evidence = new Map();

    const server = http.createServer((req, res) => {
      const url = new URL(req.url, 'http://fixture.local');
      const path = url.pathname;

      // ---- 事件上报端点（写）----
      if (req.method === 'POST' && path.startsWith(EVIDENCE_PREFIX)) {
        const senderRunId = path.slice(EVIDENCE_PREFIX.length);
        if (senderRunId !== runId) {
          respond(res, 404, 'text/plain', `isolated run: expected ${runId}`);
          return;
        }
        readBody(req, 1024 * 1024)
          .then((buf) => {
            const entry = JSON.parse(buf.toString('utf8') || '{}');
            const list = evidence.get(senderRunId) ?? [];
            list.push({
              event: String(entry.event ?? ''),
              data: entry.data ?? {},
              ts: Date.now(),
            });
            evidence.set(senderRunId, list);
            respond(res, 204);
          })
          .catch(() => respond(res, 400, 'text/plain', 'bad evidence body'));
        return;
      }

      // ---- 事件上报端点（读，调试用；runner 走进程内 evidenceFor）----
      if (req.method === 'GET' && path.startsWith(EVIDENCE_PREFIX)) {
        const senderRunId = path.slice(EVIDENCE_PREFIX.length);
        respond(res, 200, 'application/json', JSON.stringify(evidence.get(senderRunId) ?? []));
        return;
      }

      // ---- fixture 页面 ----
      const page = pages.get(path);
      if (req.method === 'GET' && page) {
        const ctx = { runId, baseUrl: `http://127.0.0.1:${server.address()?.port ?? ''}` };
        try {
          if (typeof page.handle === 'function' && page.handle(req, res, ctx) === true) return;
          const html = page.render(ctx);
          respond(res, 200, 'text/html; charset=utf-8', html);
        } catch (e) {
          respond(res, 500, 'text/plain', `fixture page error: ${e.message}`);
        }
        return;
      }

      respond(res, 404, 'text/plain', `no fixture page at ${path}`);
    });

    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const port = server.address().port;
      resolve({
        port,
        baseUrl: `http://127.0.0.1:${port}`,
        evidenceFor: (rid) => (rid === runId ? [...(evidence.get(rid) ?? [])] : []),
        close: () =>
          new Promise((done) => {
            // closeAllConnections：node 18.2+；旧 node 由 keep-alive 自然过期兜底
            server.closeAllConnections?.();
            server.close(() => done());
          }),
      });
    });
  });
}

// —— 零依赖约束：只用 node 内建模块 ——

/** 读请求体（带上限，防异常页打爆内存） */
function readBody(req, limit) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on('data', (c) => {
      size += c.length;
      if (size > limit) {
        reject(new Error('body too large'));
        req.destroy();
        return;
      }
      chunks.push(c);
    });
    req.on('end', () => resolve(Buffer.concat(chunks)));
    req.on('error', reject);
  });
}

function respond(res, status, type = 'text/plain', body = '') {
  res.writeHead(status, {
    'content-type': `${type}${type.startsWith('text/') ? '; charset=utf-8' : ''}`,
    'cache-control': 'no-store',
  });
  res.end(body);
}
