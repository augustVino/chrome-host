import React from 'react';
import ReactDOM from 'react-dom/client';
import '@fontsource-variable/plus-jakarta-sans';
import '@fontsource-variable/jetbrains-mono';
import App from './App';
import { startQualityGovernor } from './motion/quality';
import './main.css';

// 平台标记：仅 macOS 使用 Overlay 标题栏（侧栏需为红绿灯让位）
document.documentElement.classList.toggle('is-mac', navigator.userAgent.includes('Mac'));
startQualityGovernor();

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
