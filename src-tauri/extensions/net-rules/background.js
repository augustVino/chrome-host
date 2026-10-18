// 哑应用层：规则编译在 chrome-host 侧完成（domain/net_rules.rs 纯函数），
// config.js 内容形如 { "rules": [ ...declarativeNetRequest 规则 ] }。
// 选 DNR 而非 webRequest：规则在浏览器层生效，service worker 休眠不影响拦截。
importScripts('config.js');

const rules = (self.__CHROME_HOST_CONFIG__ || {}).rules || [];

async function apply() {
    // 先清空既有 dynamic rules：实例 profile 内可能残留上一代规则
    const existing = await chrome.declarativeNetRequest.getDynamicRules();
    await chrome.declarativeNetRequest.updateDynamicRules({
        removeRuleIds: existing.map((r) => r.id),
        addRules: rules,
    });
}

chrome.runtime.onInstalled.addListener(apply);
chrome.runtime.onStartup.addListener(apply);
apply().catch((e) => console.error('[net-rules] 应用规则失败:', e));
