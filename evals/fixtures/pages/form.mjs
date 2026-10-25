/**
 * /form：表单填写链 fixture。
 *
 * 页面契约（被 page-controls case 消费）：
 * - #demo-input：文本输入框（page fill 的定位目标，label 便于 snap AX 树展示）
 * - #submit-btn：提交按钮（page click 的定位目标；type=button 不触发原生导航，
 *   交互闭环完全在页内完成，证据只走 __evidence 通道）
 * - #form-result：提交后的页内回显（page html 读取它做 adapter 证据）
 * - 提交 → POST /__evidence/{runId} {event:'form.submitted', data:{text}}
 */
export function render({ runId }) {
  return `<!DOCTYPE html>
<html lang="zh">
<head><meta charset="utf-8"><title>evals form fixture</title></head>
<body>
  <main style="font-family: system-ui; padding: 24px">
    <h1>evals · 表单页</h1>
    <form id="demo-form">
      <label for="demo-input">搜索词</label>
      <input id="demo-input" type="text" name="q" placeholder="填入内容" />
      <button id="submit-btn" type="button">提交</button>
    </form>
    <p id="form-result" style="min-height: 1.2em"></p>
  </main>
  <script>
    document.getElementById('submit-btn').addEventListener('click', () => {
      const text = document.getElementById('demo-input').value;
      fetch('/__evidence/${runId}', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ event: 'form.submitted', data: { text } }),
      })
        .then(() => { document.getElementById('form-result').textContent = 'submitted: ' + text; })
        .catch(() => { document.getElementById('form-result').textContent = 'evidence-failed: ' + text; });
    });
  </script>
</body>
</html>`;
}
