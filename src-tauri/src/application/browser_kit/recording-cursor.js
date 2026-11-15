// Vendored from vercel-labs/agent-browser (Apache-2.0), Copyright Vercel, Inc.
// Upstream: https://github.com/vercel-labs/agent-browser @e0c121a96d3a5412e2078e0c8eba3f4d2db2b616 cli/src/native/recording-cursor.js
// License text: LICENSES/Apache-2.0-agent-browser.txt
// CHROME-HOST ADAPTATIONS:
// - M0 机械 vendor 时遗漏本文件的来源头（本行补记，内容未改）。
// - 运行时字符串治理：自定义元素名/属性与全局清理标识前缀
//   agent-browser-recording-cursor / __agentBrowserRecordingCursorCleanup →
//   chrome-host-recording-cursor / __chromeHostRecordingCursorCleanup
//   （页面 DOM/全局可见标识，与 recording.rs 世界名同步更名）。

(() => {
  globalThis.__chromeHostRecordingCursorCleanup?.();
  let host, pointer, shadow;
  const removers = [];
  let disposed = false;

  function mount() {
    if (disposed || host || !document.documentElement) return;
    host = document.createElement('chrome-host-recording-cursor');
    host.setAttribute('data-chrome-host-recording-cursor', '');
    host.setAttribute('aria-hidden', 'true');
    host.setAttribute('inert', '');
    host.style.cssText = 'all:initial!important;position:fixed!important;inset:0!important;z-index:2147483647!important;pointer-events:none!important;overflow:visible!important;contain:layout style!important;';
    shadow = host.attachShadow({ mode: 'closed' });
    shadow.innerHTML = `<style>
      :host, * { pointer-events: none !important; }
      .pointer { position: fixed; top: 0; left: 0; display: none; }
      svg { display: block; width: 28px; height: 28px; overflow: visible; transform-origin: 0 0; filter: drop-shadow(0 1px 1px #0008); }
      .pressed svg { transform: scale(.8); }
      .ripple { position: fixed; width: 64px; height: 64px; margin: -32px; border-radius: 50%; border: 2px solid #60a5fa; background: #60a5fa80; box-sizing: border-box; animation: ripple .4s linear forwards; }
      @keyframes ripple { from { transform: scale(0); opacity: .8; } to { transform: scale(1); opacity: 0; } }
    </style><div class="pointer"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M0 0L14 8.5L7.5 10L4 16Z" fill="white" stroke="black" stroke-width="1.5" stroke-linejoin="round"/></svg></div>`;
    pointer = shadow.querySelector('.pointer');
    document.documentElement.appendChild(host);
  }

  function update(event) {
    if (!event.isTrusted || event.pointerType !== 'mouse') return;
    mount();
    if (!pointer) return;
    pointer.style.display = 'block';
    pointer.style.transform = `translate3d(${event.clientX}px,${event.clientY}px,0)`;
    pointer.classList.toggle('pressed', event.buttons !== 0);
    if (event.type === 'pointerdown') {
      const ripple = document.createElement('div');
      ripple.className = 'ripple';
      ripple.style.left = `${event.clientX}px`;
      ripple.style.top = `${event.clientY}px`;
      ripple.addEventListener('animationend', () => ripple.remove(), { once: true });
      shadow.insertBefore(ripple, pointer);
    }
  }

  function listen(type, handler) {
    addEventListener(type, handler, { capture: true, passive: true });
    removers.push(() => removeEventListener(type, handler, true));
  }

  listen('pointermove', update);
  listen('pointerdown', update);
  listen('pointerup', update);
  listen('pointerout', event => {
    if ((!event.relatedTarget || event.relatedTarget.localName === 'iframe') && pointer) pointer.style.display = 'none';
  });
  listen('DOMContentLoaded', mount);
  mount();
  globalThis.__chromeHostRecordingCursorCleanup = () => {
    disposed = true;
    removers.forEach(remove => remove());
    host?.remove();
    delete globalThis.__chromeHostRecordingCursorCleanup;
  };
})();
