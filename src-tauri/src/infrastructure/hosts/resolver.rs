//! hosts 拉取与合并。
//! 关键决策：reqwest 强制 `no_proxy()`——hosts 源常为内网地址，
//! 不能被终端代理环境变量劫持，并补齐双超时防挂起。

use std::time::Duration;

use once_cell::sync::Lazy;

use crate::domain::hosts::HostRules;
use crate::error::AppError;

static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("构建 hosts HTTP 客户端失败")
});

const LOCAL_HOSTS_PATH: &str = "/etc/hosts";

/// 读取本地 hosts 打底；读不到（权限/平台差异）返回空映射，不阻断启动
pub fn read_local_hosts() -> HostRules {
    std::fs::read_to_string(LOCAL_HOSTS_PATH)
        .map(|text| HostRules::parse(&text))
        .unwrap_or_default()
}

/// 拉取远程 hosts；非 2xx 显式报错
pub async fn fetch_remote(url: &str) -> Result<HostRules, AppError> {
    let resp = CLIENT
        .get(url)
        .send()
        .await?
        .error_for_status()
        .map_err(|e| {
            AppError::business(500, "HOSTS_FETCH_FAILED", format!("HTTP 请求失败: {e}"))
        })?;
    let text = resp.text().await?;
    Ok(HostRules::parse(&text))
}

/// 解析实例要注入的 HostRules：
/// - source 为 None → Ok(None)（不注入）
/// - 本地 /etc/hosts 打底，远程同名覆盖；结果为空 → HOSTS_EMPTY
/// - 拉取失败 → HOSTS_FETCH_FAILED（由调用方决定：Instance 启动即失败，不起"缺环境"实例）
pub async fn resolve(source: Option<&str>) -> Result<Option<HostRules>, AppError> {
    let Some(url) = source else {
        return Ok(None);
    };

    let remote = match fetch_remote(url).await {
        Ok(r) => r,
        Err(e) => {
            return Err(AppError::business(
                500,
                "HOSTS_FETCH_FAILED",
                format!("拉取 DNS 配置失败({url}): {e}"),
            ))
        }
    };

    let merged = HostRules::merge(&read_local_hosts(), &remote);
    if merged.is_empty() {
        return Err(AppError::business(
            500,
            "HOSTS_EMPTY",
            format!("hosts 配置为空（源无有效记录）: {url}"),
        ));
    }
    Ok(Some(merged))
}
