/**
 * /other：导航链目标页。特征标记 #nav-marker=other-page。
 */
export function render() {
  return `<!DOCTYPE html>
<html lang="zh">
<head><meta charset="utf-8"><title>evals other</title></head>
<body>
  <main style="font-family: system-ui; padding: 24px">
    <h1 id="nav-marker">other-page</h1>
    <p><a href="/start">回 start 页</a></p>
  </main>
</body>
</html>`;
}
