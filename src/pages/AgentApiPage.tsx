import { useCallback, useEffect, useRef, useState } from 'react';
import { copyText } from '../lib/clipboard';
import { API_CATALOG, type CatalogItem } from '../api/catalog';
import { PageHeader, btnGhostSm, btnPrimary } from '../components/ui';
import { usePageEnter } from '../motion/hooks';

const BASE = 'http://127.0.0.1:17890/api/v1';

const METHOD_COLOR: Record<string, string> = {
  GET: 'mtag mtag-get',
  POST: 'mtag mtag-post',
  PUT: 'mtag mtag-put',
  PATCH: 'mtag mtag-patch',
  DELETE: 'mtag mtag-delete',
};

/**
 * Agent API 页：endpoint + Copy + 分组 API 清单 + 最小 Explorer。
 * 不做全功能 Postman，只做"可试"：点击条目 → 可编辑 path/body → 发起真实请求。
 */
export default function AgentApiPage() {
  const [selected, setSelected] = useState<CatalogItem>(API_CATALOG[0].items[0]);
  const [path, setPath] = useState(selected.path);
  const [body, setBody] = useState(selected.body ?? '');
  const [resp, setResp] = useState<{ status: number; text: string } | null>(null);
  const [running, setRunning] = useState(false);
  const [apiUp, setApiUp] = useState<boolean | null>(null);
  const root = useRef<HTMLDivElement>(null);
  usePageEnter(root);

  useEffect(() => {
    const check = () =>
      fetch(`${BASE}/kernel/status`)
        .then((r) => setApiUp(r.ok))
        .catch(() => setApiUp(false));
    check();
    const t = setInterval(check, 5000);
    return () => clearInterval(t);
  }, []);

  const pick = (item: CatalogItem) => {
    setSelected(item);
    setPath(item.path);
    setBody(item.body ?? '');
    setResp(null);
  };

  const run = useCallback(async () => {
    setRunning(true);
    setResp(null);
    try {
      const hasBody = selected.method !== 'GET' && selected.method !== 'DELETE' && body.trim();
      const resp = await fetch(`${BASE}${path}`, {
        method: selected.method,
        headers: hasBody ? { 'content-type': 'application/json' } : undefined,
        body: hasBody ? body : undefined,
      });
      const text = await resp.text();
      let pretty = text;
      try {
        pretty = JSON.stringify(JSON.parse(text), null, 2);
      } catch {
        /* 非 JSON 响应原样展示 */
      }
      setResp({ status: resp.status, text: pretty });
    } catch (e) {
      setResp({ status: 0, text: String(e) });
    } finally {
      setRunning(false);
    }
  }, [selected, path, body]);

  return (
    <div ref={root} className="mx-auto max-w-5xl">
      <PageHeader
        title="Agent API"
        sub="供 AI Agent / CLI / MCP 调用的本地 HTTP 接口"
        right={
          <>
            <span className={`pill ${apiUp === false ? 'err' : apiUp === null ? '' : 'ok'}`}>
              <i />
              {apiUp === false ? 'API 未连接' : 'Running'}
            </span>
            <span className="mono text-xs text-ink-3">{BASE}</span>
            <button
              className={btnGhostSm}
              onClick={async () => {
                await copyText(BASE);
              }}
            >
              Copy
            </button>
          </>
        }
      />

      {/* MCP 说明卡（与 REST 平行的协议视图，不混入 REST 目录） */}
      <section className="gcard rv mb-4 flex items-center justify-between gap-4 px-4 py-3">
        <div className="min-w-0">
          <h2 className="text-xs font-semibold text-ink">MCP（AI 客户端接入）</h2>
          <p className="mt-1 text-[11px] leading-5 text-ink-2">
            内嵌 MCP over Streamable HTTP，随应用启动。25 个 tools 覆盖环境/实例/标签页/CDP/登录态/扩展管理，业务错误结构化透传。
          </p>
          <p className="mt-1.5 truncate font-mono text-[10px] text-ink-3">
            claude mcp add --transport http chrome-host http://127.0.0.1:17890/mcp
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          <code className="rounded-lg bg-surface-inset px-2.5 py-1 font-mono text-[10px] text-ink">
            http://127.0.0.1:17890/mcp
          </code>
          <button
            className={btnGhostSm}
            onClick={async () => {
              await copyText('http://127.0.0.1:17890/mcp');
            }}
          >
            Copy
          </button>
        </div>
      </section>

      <div className="rv flex gap-4">
        {/* 分组清单 */}
        <div className="w-72 shrink-0 space-y-4">
          {API_CATALOG.map((g) => (
            <div key={g.group}>
              <p className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-ink-3">
                {g.group}
              </p>
              <ul className="space-y-0.5">
                {g.items.map((item) => {
                  const key = `${item.method} ${item.path}`;
                  const active = selected === item;
                  return (
                    <li key={key + item.desc}>
                      <button
                        className={`flex w-full items-center gap-1.5 rounded-lg px-2 py-1 text-left text-[11px] transition ${
                          active ? 'bg-brand/10 text-brand' : 'text-ink-2 hover:bg-ink/5'
                        }`}
                        onClick={() => pick(item)}
                      >
                        <span
                          className={`w-12 shrink-0 ${METHOD_COLOR[item.method]}`}
                        >
                          {item.method}
                        </span>
                        <span className="truncate font-mono">{item.path}</span>
                      </button>
                    </li>
                  );
                })}
              </ul>
            </div>
          ))}
        </div>

        {/* Explorer */}
        <div className="min-w-0 flex-1 space-y-3">
          <div>
            <div className="flex items-center gap-2">
              <span
                className={METHOD_COLOR[selected.method]}
              >
                {selected.method}
              </span>
              <p className="truncate text-sm text-ink">{selected.desc}</p>
            </div>
            <input
              className="input mono mt-2"
              value={path}
              onChange={(e) => setPath(e.target.value)}
              spellCheck={false}
            />
          </div>

          {(selected.method === 'POST' || selected.method === 'PUT' || selected.method === 'PATCH') && (
            <textarea
              className="input mono h-24"
              value={body}
              onChange={(e) => setBody(e.target.value)}
              placeholder="{}"
              spellCheck={false}
            />
          )}

          <div className="flex items-center gap-2">
            <button className={btnPrimary} disabled={running} onClick={run}>
              {running ? '请求中…' : 'Run'}
            </button>
            {selected.note && <p className="text-[11px] text-amber">{selected.note}</p>}
          </div>

          {resp && (
            <div>
              <p className="mb-1 text-[11px] text-ink-3">
                响应状态：
                <span className={resp.status >= 200 && resp.status < 300 ? 'text-run-text' : 'text-status-error'}>
                  {resp.status || '网络错误'}
                </span>
              </p>
              <pre className="code max-h-80 overflow-auto">
                {resp.text}
              </pre>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
