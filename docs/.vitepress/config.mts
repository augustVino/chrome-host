import { defineConfig } from 'vitepress'

// 项目站路径带仓库名（https://augustVino.github.io/chrome-host/），base 必须配置；
// 绑定自定义域名时改回 '/'
export default defineConfig({
  lang: 'zh-CN',
  title: 'Chrome Host',
  description:
    'Chrome 浏览器多环境管理工具：环境隔离 · 登录态快照 · hosts 注入 · CDP 自动化',
  base: '/chrome-host/',
  themeConfig: {
    nav: [
      { text: '指南', link: '/guide/quickstart' },
      { text: '参考手册', items: [
        { text: 'CLI 手册', link: '/reference/cli' },
        { text: 'Agent API', link: '/reference/agent-api' },
        { text: 'MCP', link: '/reference/mcp' },
      ] },
      {
        text: 'GitHub',
        link: 'https://github.com/augustVino/chrome-host',
      },
    ],
    sidebar: {
      '/guide/': [
        {
          text: '指南',
          items: [
            { text: '快速开始', link: '/guide/quickstart' },
            { text: '单环境闭环演练', link: '/guide/walkthrough' },
            { text: '核心概念', link: '/guide/concepts' },
            { text: '日常使用', link: '/guide/daily' },
            { text: 'AI Agent Skills', link: '/guide/skills' },
            { text: '远程接入', link: '/guide/remote-access' },
          ],
        },
        {
          text: '开发',
          items: [{ text: '开发指南', link: '/guide/develop' }],
        },
      ],
      '/reference/': [
        {
          text: '参考手册',
          items: [
            { text: 'CLI 手册', link: '/reference/cli' },
            { text: 'Agent API', link: '/reference/agent-api' },
            { text: 'MCP', link: '/reference/mcp' },
          ],
        },
      ],
    },
    search: { provider: 'local' },
    outline: { level: [2, 3] },
    docFooter: { prev: '上一篇', next: '下一篇' },
    returnToTopLabel: '回到顶部',
    sidebarMenuLabel: '菜单',
    darkModeSwitchLabel: '主题',
  },
})
