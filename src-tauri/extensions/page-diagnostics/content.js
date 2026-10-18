// 页面诊断采集（chrome-host M5）：
// - world MAIN 是硬前提：页面世界的 JS 错误只有 MAIN world 的 error 监听能捕获，
//   且 Runtime.evaluate 默认在主世界求值——isolated world 挂的变量查询端点读不到；
// - document_idle 注入 + PerformanceObserver buffered:true，不漏早期条目；
// - 只写 window.__chromeHostDiagnostics 一个全局（IIFE 收敛作用域），环形上限
//   100 条/类，查询端点 returnByValue 按值序列化当下快照；
// - 边界（产品接受）：content script 不注入的页面（错误页/about:blank/data:）
//   查询返回 diagnostics: null。
(() => {
  if (self.__chromeHostDiagnostics) return; // 幂等：重复注入直接退出

  const MAX = 100;
  const push = (arr, item) => {
    arr.push(item);
    if (arr.length > MAX) arr.shift();
  };

  const state = {
    startedAt: Date.now(),
    url: location.href,
    errors: [], // window error（JS 异常）+ unhandledrejection
    resources: [], // 失败资源：HTTP status >= 400，或 transferSize == 0（缓存命中/被阻断的启发式信号）
    lcp: null,
    fcp: null,
  };

  addEventListener('error', (e) => {
    push(state.errors, {
      ts: Date.now(),
      type: 'error',
      message: e.message || String(e),
      source: e.filename || null,
      line: e.lineno || null,
    });
  });

  addEventListener('unhandledrejection', (e) => {
    const reason = e.reason;
    push(state.errors, {
      ts: Date.now(),
      type: 'unhandledrejection',
      message: reason && reason.message ? reason.message : String(reason),
      source: null,
      line: null,
    });
  });

  new PerformanceObserver((list) => {
    for (const r of list.getEntries()) {
      const status = r.responseStatus || 0;
      if (status >= 400 || r.transferSize === 0) {
        push(state.resources, {
          ts: Date.now(),
          url: r.name,
          status: status || null,
          transferSize: r.transferSize,
          initiatorType: r.initiatorType || null,
          durationMs: Math.round(r.duration),
        });
      }
    }
  }).observe({ type: 'resource', buffered: true });

  new PerformanceObserver((list) => {
    const entries = list.getEntries();
    if (entries.length) {
      state.lcp = Math.round(entries[entries.length - 1].startTime);
    }
  }).observe({ type: 'largest-contentful-paint', buffered: true });

  new PerformanceObserver((list) => {
    for (const p of list.getEntries()) {
      if (p.name === 'first-contentful-paint') {
        state.fcp = Math.round(p.startTime);
      }
    }
  }).observe({ type: 'paint', buffered: true });

  self.__chromeHostDiagnostics = state;
})();
