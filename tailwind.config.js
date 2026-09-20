/** @type {import('tailwindcss').Config} */
export default {
    content: [
        "./index.html",
        "./src/**/*.{js,ts,jsx,tsx}",
    ],
    // 跟随系统（prefers-color-scheme）：dark: 变体与 main.css 变量同机制，零 JS
    darkMode: 'media',
    theme: {
        extend: {
            colors: {
                // 设计规范见 DESIGN.md（macOS HIG 风格，与 docs/design/chrome-host-ui.pen 同源）。
                // 亮/暗双值定义在 src/main.css 的 CSS 变量里，语义类自动跟随系统主题；
                // 禁止写死十六进制，禁止 token 之外的新色值（豁免灰阶须配 dark: 补丁）。
                accent: {
                    DEFAULT: 'rgb(var(--accent) / <alpha-value>)',
                    hover: 'rgb(var(--accent-hover) / <alpha-value>)',
                },
                ink: {
                    DEFAULT: 'rgb(var(--ink) / <alpha-value>)',
                    2: 'rgb(var(--ink-2) / <alpha-value>)',
                    3: 'rgb(var(--ink-3) / <alpha-value>)',
                },
                hairline: {
                    DEFAULT: 'rgb(var(--hairline) / <alpha-value>)',
                    soft: 'rgb(var(--hairline-soft) / <alpha-value>)',
                },
                surface: {
                    DEFAULT: 'rgb(var(--surface) / <alpha-value>)', // 窗口内容区底
                    sidebar: 'rgb(var(--surface-sidebar) / <alpha-value>)',
                    card: 'rgb(var(--surface-card) / <alpha-value>)',
                    raised: 'rgb(var(--surface-raised) / <alpha-value>)',
                    inset: 'rgb(var(--surface-inset) / <alpha-value>)',
                },
                toast: 'rgb(var(--toast) / <alpha-value>)',
                status: {
                    running: 'rgb(var(--status-running) / <alpha-value>)',
                    starting: 'rgb(var(--status-starting) / <alpha-value>)',
                    stopped: 'rgb(var(--status-stopped) / <alpha-value>)',
                    error: 'rgb(var(--status-error) / <alpha-value>)',
                },
                'run-text': 'rgb(var(--run-text) / <alpha-value>)',
                // Popover 半透明材质（完整颜色值变量，不支持 /alpha modifier）
                popover: {
                    card: 'var(--popover-card)',
                    line: 'var(--popover-line)',
                    btn: 'var(--popover-btn)',
                    'btn-hover': 'var(--popover-btn-hover)',
                },
            }
        }
    },
    plugins: [],
}
