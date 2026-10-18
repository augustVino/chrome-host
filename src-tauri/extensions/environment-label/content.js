// 环境标签角标：document_start 立即渲染（不等 load——慢页面/白屏期间即可区分环境）。
// 配置由 chrome-host 物化管道写入同目录 config.js（先于本文件执行）：
//   { text: "环境名 · Chrome #N", position: "top-right", color: "red" }
// 已知边界（产品决策接受）：错误页/崩溃页/about:blank 不运行 content script，无标签。

const cfg = self.__CHROME_HOST_CONFIG__ || {};
const environmentName = cfg.text || '';
const labelPosition = cfg.position || 'bottom-left';
const labelColor = cfg.color || 'red';

/** 'top-left' | 'top-right' | 'bottom-left' | 'bottom-right' → 定位样式 */
function getPositionStyle(pos) {
    const [v = 'bottom', h = 'left'] = pos.split('-');
    return `${v}: 10px; ${h}: 10px;`;
}

const COLOR_MAP = {
    red: 'rgb(239,68,68)',
    blue: 'rgb(37,99,235)',
    green: 'rgb(34,197,94)',
    purple: 'rgb(168,85,247)',
};

let labelEl = null;
let observer = null;

function createLabel() {
    const el = document.createElement('div');
    el.textContent = environmentName;
    el.setAttribute('data-environment-label', '');
    el.style.cssText = `
        position: fixed;
        ${getPositionStyle(labelPosition)}
        background-color: ${COLOR_MAP[labelColor] || COLOR_MAP.red};
        color: white;
        padding: 5px 10px;
        border-radius: 4px;
        font-weight: bold;
        z-index: 2147483647;
        font-size: 14px;
        box-shadow: 0 2px 4px rgba(0, 0, 0, 0.3);
        opacity: 0.9;
        pointer-events: none;
        font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
        max-width: 40vw;
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
    `;
    return el;
}

// 挂到 documentElement 而非 body：document_start 时 body 尚未解析，
// fixed 定位不依赖父容器；SPA 替换 body 也不受影响。
function mount() {
    const root = document.documentElement;
    if (!root) return;
    if (labelEl && labelEl.isConnected) return;
    if (!labelEl) labelEl = createLabel();
    root.appendChild(labelEl);
    // 守护观察：页面脚本移除标签或替换根节点时重新挂载。
    // 重新 observe 以覆盖 documentElement 被整体替换（document.write）的场景。
    if (observer) observer.disconnect();
    observer = new MutationObserver(() => {
        if (!labelEl || !labelEl.isConnected) mount();
    });
    observer.observe(root, { childList: true, subtree: true });
}

mount();
