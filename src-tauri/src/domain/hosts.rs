//! HostRules 值对象与 hosts 解析。
//! 纯函数无 IO，全部可单测；IO 在 infrastructure/hosts。
//!
//! 解析规则（逐条对应单测）：
//! - 行内注释剔除：`#` 之后内容全部去除
//! - `IP host1 host2 ...` 每行一条；不足两段跳过
//! - IP 校验：IPv4 正则；含 `:` 视为 IPv6 放行
//! - 剔除系统/回环条目（localhost、127.* 等），防止实例内 localhost 被劫持
//! - 同名 host 后者覆盖前者

use std::collections::BTreeMap;

const SYSTEM_HOSTNAMES: [&str; 4] = ["localhost", "broadcasthost", "ip6-localhost", "ip6-loopback"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRules(BTreeMap<String, String>);

impl Default for HostRules {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl HostRules {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 解析 hosts 格式文本（本地与远程同构）
    pub fn parse(text: &str) -> Self {
        let mut map = BTreeMap::new();
        for raw_line in text.lines() {
            // 行内注释：# 之后全部剔除
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            let ip = parts[0];
            if !is_valid_ip(ip) || is_system_ip(ip) {
                continue;
            }
            for host in &parts[1..] {
                let host = host.to_lowercase();
                if !host.is_empty() && !SYSTEM_HOSTNAMES.contains(&host.as_str()) {
                    map.insert(host, ip.to_string()); // 同名后者覆盖
                }
            }
        }
        HostRules(map)
    }

    /// 合并：local 打底，remote 同名覆盖（远程为准）
    pub fn merge(local: &HostRules, remote: &HostRules) -> Self {
        let mut map = local.0.clone();
        for (k, v) in &remote.0 {
            map.insert(k.clone(), v.clone());
        }
        HostRules(map)
    }

    /// 拼 --host-resolver-rules 参数值：`MAP host ip, MAP host ip, ...`
    pub fn to_resolver_rules(&self) -> String {
        self.0
            .iter()
            .map(|(host, ip)| format!("MAP {host} {ip}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// 固化快照 JSON（落库 instances.host_rules）
    pub fn to_snapshot_json(&self) -> String {
        serde_json::to_string(&self.0).unwrap_or_else(|_| "{}".to_string())
    }
}

/// IP 校验：IPv4 点分格式；含 `:` 视为 IPv6 放行（不做 8 位段强校验）
pub fn is_valid_ip(value: &str) -> bool {
    if value.contains(':') {
        return true;
    }
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.parse::<u16>().is_ok())
}

/// 剔除系统/回环地址：注入会把实例内 localhost 等劫持到错误地址
fn is_system_ip(ip: &str) -> bool {
    ip == "::1"
        || ip == "0.0.0.0"
        || ip == "255.255.255.255"
        || ip.starts_with("127.")
        || ip.starts_with("fe80:")
}

/// hostsSourceUrl 合法性：任意 http(s) URL（响应体按 hosts 文本格式解析）。
pub fn is_valid_source_url(raw: &str) -> bool {
    match url::Url::parse(raw) {
        Ok(u) => u.scheme() == "http" || u.scheme() == "https",
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# 整行注释
10.1.1.1  a.internal.com  b.internal.com   # 行内注释
10.1.1.2  a.internal.com               # 同名后者覆盖
# 被剔除的条目
127.0.0.1  danger.com
0.0.0.0    ads.com
localhost  mine.com
10.1.1.3  localhost
bad_ip     x.com
10.1.1.4  onlyip.local
"#;

    #[test]
    fn parse_extracts_dedupes_and_filters() {
        let rules = HostRules::parse(SAMPLE);
        assert_eq!(rules.len(), 3, "a/b 覆盖合并 + onlyip，共 3 条");
        let json = rules.to_snapshot_json();
        assert!(json.contains("a.internal.com"));
        assert!(json.contains("10.1.1.4"), "系统条目/坏 IP 行不入映射");
        assert!(!json.contains("danger.com"));
        assert!(!json.contains("ads.com"));
        assert!(!json.contains("mine.com"));
        assert!(!json.contains("x.com"));
    }

    #[test]
    fn resolver_rules_format() {
        let rules = HostRules::parse("10.0.0.1 alpha.test\n10.0.0.2 beta.test\n");
        assert_eq!(
            rules.to_resolver_rules(),
            "MAP alpha.test 10.0.0.1, MAP beta.test 10.0.0.2"
        );
    }

    #[test]
    fn merge_remote_overrides_local() {
        let local = HostRules::parse("10.9.9.9 shared.com\n10.9.9.8 local-only.com\n");
        let remote = HostRules::parse("10.1.1.1 shared.com\n");
        let merged = HostRules::merge(&local, &remote);
        assert_eq!(merged.len(), 2);
        let json = merged.to_snapshot_json();
        assert!(json.contains("10.1.1.1"), "远程覆盖同名");
        assert!(json.contains("local-only.com"), "本地独有保留");
    }

    #[test]
    fn ipv6_entries_pass_but_linklocal_and_loopback_filtered() {
        let rules = HostRules::parse("2400:cb00::1 v6.local\nfe80::1 link.local\n::1 loop.v6\n");
        assert!(rules.to_snapshot_json().contains("v6.local"), "全球单播 IPv6 放行");
        assert!(!rules.to_snapshot_json().contains("link.local"), "链路本地 fe80:: 剧除");
        assert!(!rules.to_snapshot_json().contains("loop.v6"), "回环 ::1 剧除");
    }

    /// 任意 http(s) URL 合法（响应体按 hosts 文本解析）；其他协议/非 URL 拒绝
    #[test]
    fn source_url_accepts_any_http_url() {
        assert!(is_valid_source_url("https://example.com/hosts.txt"));
        assert!(is_valid_source_url("https://example.com/hosts/qa.txt"));
        assert!(!is_valid_source_url("ftp://example.com/hosts"));
        assert!(!is_valid_source_url("not-a-url"));
    }
}
