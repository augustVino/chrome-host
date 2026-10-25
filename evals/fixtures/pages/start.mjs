/**
 * /start：导航链起点页。特征标记 #nav-marker=start-page 供 wait/html 断言锚定。
 */
export function render() {
  return `<!DOCTYPE html>
<html lang="zh">
<head><meta charset="utf-8"><title>evals start</title></head>
<body>
  <main style="font-family: system-ui; padding: 24px">
    <h1 id="nav-marker">start-page</h1>
    <p><a href="/other">去 other 页</a></p>
  </main>
</body>
</html>`;
}
