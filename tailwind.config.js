/** @type {import('tailwindcss').Config} */
export default {
    content: [
        "./index.html",
        "./src/**/*.{js,ts,jsx,tsx}",
    ],
    darkMode: 'class',
    theme: {
        extend: {
            colors: {
                // 设计规范见 DESIGN.md（macOS HIG 风格，与 docs/design/chrome-host-ui.pen 同源）
                accent: {
                    DEFAULT: '#007AFF',
                    hover: '#0071EB',
                },
                ink: {
                    DEFAULT: '#1D1D1F',
                    2: '#6E6E73',
                    3: '#AEAEB2',
                },
                hairline: '#E5E5EA',
                surface: {
                    sidebar: '#F5F5F7',
                    inset: '#F5F5F7',
                },
                status: {
                    running: '#34C759',
                    starting: '#FF9F0A',
                    stopped: '#D1D1D6',
                    error: '#FF3B30',
                },
                'run-text': '#248A3D',
            }
        }
    },
    plugins: [],
}
