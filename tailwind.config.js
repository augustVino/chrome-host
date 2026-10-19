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
            fontFamily: {
                sans: ['var(--ff)'],
                mono: ['var(--mono)'],
            },
            colors: {
                // 设计规范见 DESIGN.md（v2 Aurora Glass）。
                // 亮/暗双值定义在 src/main.css 的 CSS 变量里，语义类自动跟随系统主题；
                // 禁止写死十六进制，禁止 token 之外的新色值。
                brand: {
                    DEFAULT: 'rgb(var(--brand) / <alpha-value>)',
                    2: 'rgb(var(--brand-2) / <alpha-value>)',
                },
                mint: 'rgb(var(--mint) / <alpha-value>)',
                peach: 'rgb(var(--peach) / <alpha-value>)',
                sky: 'rgb(var(--sky) / <alpha-value>)',
                coral: 'rgb(var(--coral) / <alpha-value>)',
                amber: 'rgb(var(--amber) / <alpha-value>)',
                // 旧语义名 → 新色板映射（未改造页面自动继承）
                accent: {
                    DEFAULT: 'rgb(var(--brand) / <alpha-value>)',
                    hover: 'rgb(var(--brand-hover) / <alpha-value>)',
                },
                ink: {
                    DEFAULT: 'rgb(var(--ink) / <alpha-value>)',
                    2: 'rgb(var(--ink-2) / <alpha-value>)',
                    3: 'rgb(var(--ink-3) / <alpha-value>)',
                },
                // 描边/玻璃类为完整颜色值（含 alpha），不支持 /alpha modifier
                hairline: {
                    DEFAULT: 'var(--line)',
                    soft: 'var(--line-soft)',
                },
                surface: {
                    DEFAULT: 'rgb(var(--bg) / <alpha-value>)', // 窗口底
                    sidebar: 'var(--glass)',
                    card: 'var(--glass-2)',
                    raised: 'var(--glass-2)',
                    inset: 'var(--inset)',
                },
                toast: 'rgb(var(--toast) / <alpha-value>)',
                status: {
                    running: 'rgb(var(--mint) / <alpha-value>)',
                    starting: 'rgb(var(--amber) / <alpha-value>)',
                    stopped: 'rgb(var(--ink-3) / <alpha-value>)',
                    error: 'rgb(var(--coral) / <alpha-value>)',
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
