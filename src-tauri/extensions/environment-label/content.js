// 获取环境名称
const environmentName = 'ENVIRONMENT_NAME_PLACEHOLDER';

// 获取环境标签位置
const labelPosition = 'ENVIRONMENT_LABEL_POSITION_PLACEHOLDER';

// 获取环境标签样式
const labelStyle = 'ENVIRONMENT_LABEL_STYLE_PLACEHOLDER';

/**
 * 解析环境标签位置
 * @param {*} pos 'top-left' | 'top-right' | 'bottom-left' | 'bottom-right'
 */
function getLabelPosition(pos) {
    const [v = 'bottom', h = 'left'] = pos.split('-');
    return `${v}: 10px; ${h}: 10px;`;
}

/**
 * 解析环境标签样式
 * @param {*} style 'red' | 'blue' | 'green' | 'purple'
 */
function getLabelStyle(style) {
    const map = {
        red: 'rgb(239,68,68)',
        blue: 'rgb(37,99,235)',
        green: 'rgb(34,197,94)',
        purple: 'rgb(168,85,247)',
    }
    return map[style] || map.red;
}

// 创建环境标签元素
function createEnvironmentLabel() {
    try {
        // 如果标签已存在，则不重复创建
        if (document.querySelector('[data-environment-label]')) {
            return;
        }

        // 创建标签容器
        const container = document.createElement('div');
        container.textContent = environmentName;
        container.setAttribute('data-environment-label', '');

        // 设置内联样式
        container.style.cssText = `
            position: fixed;
            ${getLabelPosition(labelPosition)}
            background-color: ${getLabelStyle(labelStyle)};
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
            min-width: 50px;
            text-align: center;
            transform: none;
            margin: 0;
            max-width: none;
            max-height: none;
            min-height: 0;
            visibility: visible;
            clip: auto;
            overflow: visible;
        `;

        document.body.appendChild(container);
    } catch (error) {
        console.warn('创建环境标签失败:', error);
    }
}

// 在页面加载完成后创建标签
if (document.readyState === 'complete') {
    createEnvironmentLabel();
} else {
    window.addEventListener('load', createEnvironmentLabel);
}