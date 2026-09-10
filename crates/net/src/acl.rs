use crate::dns::Resolved;
use std::net::IpAddr;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AclConfig {
    pub allow_hosts: Vec<String>,
    pub deny_hosts: Vec<String>,
}

impl AclConfig {
    pub fn new(allow: Option<&str>, deny: Option<&str>) -> Self {
        let parse_list = |opt: Option<&str>| -> Vec<String> {
            match opt {
                Some(s) => s
                    .split(',')
                    .map(|item| item.trim().to_ascii_lowercase())
                    .filter(|item| !item.is_empty())
                    .collect(),
                None => Vec::new(),
            }
        };

        Self {
            allow_hosts: parse_list(allow),
            deny_hosts: parse_list(deny),
        }
    }

    pub fn is_allowed(&self, host_or_addr: &str) -> bool {
        // 許可も拒否も設定されていなければ何もしない (既定の経路で文字列を作らない)
        if self.allow_hosts.is_empty() && self.deny_hosts.is_empty() {
            return true;
        }
        let host = extract_host(host_or_addr).to_ascii_lowercase();

        // 1. Check deny list first
        for pattern in &self.deny_hosts {
            if match_pattern(pattern, &host) {
                return false;
            }
        }

        // 2. If allow list is configured, host must match at least one pattern
        if !self.allow_hosts.is_empty() {
            return self.allow_hosts.iter().any(|p| match_pattern(p, &host));
        }

        true
    }
}

/// 許可するポートの集合 (`PROXY_CONNECT_PORTS`)。空なら制限なし。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortSet {
    ranges: Vec<(u16, u16)>,
}

impl PortSet {
    /// `443,80,8080-8099` 形式を読む。空文字なら制限なし。書式が違う項目は無視する。
    pub fn parse(spec: &str) -> PortSet {
        let mut ranges = Vec::new();
        for item in spec.split(',').map(str::trim).filter(|i| !i.is_empty()) {
            match item.split_once('-') {
                Some((lo, hi)) => {
                    if let (Ok(lo), Ok(hi)) = (lo.trim().parse::<u16>(), hi.trim().parse::<u16>()) {
                        ranges.push((lo.min(hi), lo.max(hi)));
                    }
                }
                None => {
                    if let Ok(p) = item.parse::<u16>() {
                        ranges.push((p, p));
                    }
                }
            }
        }
        PortSet { ranges }
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// 制限が無ければ常に true。
    pub fn allows(&self, port: u16) -> bool {
        self.ranges.is_empty()
            || self
                .ranges
                .iter()
                .any(|(lo, hi)| port >= *lo && port <= *hi)
    }
}

/// ループバック・リンクローカル (`169.254.0.0/16`, `fe80::/10`) と、未指定アドレス。
/// クラウドのメタデータ (`169.254.169.254`) 経由の SSRF を防ぐために使う。
pub fn is_local_ip(ip: IpAddr) -> bool {
    match crate::net::canonical_ip(ip) {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_link_local() || v4.is_unspecified(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// `host[:port]` の宛先がローカル宛てかを判定し、**判定に使った答えを一緒に返す** (T12.7)。
///
/// 名前はここで 1 回だけ解決し、接続側はこの答えをそのまま使う
/// ([`crate::net::connect_with`])。こうしないと判定と接続で表を 2 回引くことになり、
/// `PROXY_DNS_TTL_SECS=0` では `getaddrinfo` を 2 回呼んで**別の答えで接続しうる**
/// (DNS rebinding で判定をすり抜けられる)。
///
/// IP リテラルは解決するものが無いので `None` (接続側もその場で使う)。
/// 解決できないものはローカル扱いにしない (先で 502 になる)。
pub fn resolve_target(host_or_addr: &str) -> (bool, Option<Resolved<'_>>) {
    let (host, port) = crate::net::split_host_port_ref(host_or_addr);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return (is_local_ip(ip), None);
    }
    match crate::dns::resolve_host(host, port.unwrap_or(80)) {
        Ok((addrs, preferred)) => {
            let local = addrs.iter().any(|&ip| is_local_ip(ip));
            (local, Some(Resolved::new(host, addrs, preferred)))
        }
        // 解決できないときだけ名前で見る (リゾルバが壊れていても localhost は止める)
        Err(_) => (host.eq_ignore_ascii_case("localhost"), None),
    }
}

/// [`resolve_target`] の、答えが要らない呼び出し側のための版。
pub fn is_local_target(host_or_addr: &str) -> bool {
    resolve_target(host_or_addr).0
}

fn extract_host(host_or_addr: &str) -> &str {
    crate::net::split_host_port_ref(host_or_addr).0
}

pub fn match_pattern(pattern: &str, host: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    if pattern == host {
        return true;
    }

    if let Some(suffix) = pattern.strip_prefix("*.")
        && (host == suffix || host.ends_with(&format!(".{}", suffix)))
    {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ipv6_literal_host() {
        let acl = AclConfig::new(Some("2001:db8::1, example.com"), None);
        assert!(acl.is_allowed("[2001:db8::1]:443"));
        assert!(acl.is_allowed("[2001:db8::1]"));
        assert!(acl.is_allowed("example.com:80"));
        assert!(!acl.is_allowed("[2001:db8::2]:443"));
    }

    #[test]
    fn test_acl_default_allows_all() {
        let acl = AclConfig::new(None, None);
        assert!(acl.is_allowed("example.com"));
        assert!(acl.is_allowed("example.com:443"));
    }

    #[test]
    fn test_acl_deny() {
        let acl = AclConfig::new(None, Some("bad.com, *.blocked.org"));
        assert!(!acl.is_allowed("bad.com"));
        assert!(!acl.is_allowed("bad.com:80"));
        assert!(!acl.is_allowed("sub.blocked.org"));
        assert!(!acl.is_allowed("blocked.org"));
        assert!(acl.is_allowed("good.com"));
    }

    #[test]
    fn test_acl_allow() {
        let acl = AclConfig::new(Some("*.example.com, rust-lang.org"), None);
        assert!(acl.is_allowed("example.com"));
        assert!(acl.is_allowed("api.example.com"));
        assert!(acl.is_allowed("rust-lang.org:443"));
        assert!(!acl.is_allowed("other.com"));
    }
}

#[cfg(test)]
mod local_tests {
    use super::*;

    #[test]
    fn port_set_parses_lists_and_ranges() {
        let set = PortSet::parse("443, 80 , 8080-8099");
        assert!(set.allows(443) && set.allows(80) && set.allows(8085));
        assert!(!set.allows(22) && !set.allows(8100));
        let none = PortSet::parse("  ");
        assert!(none.is_empty() && none.allows(22), "空なら制限なし");
        assert!(PortSet::parse("junk").is_empty());
    }

    #[test]
    fn local_targets_are_recognised() {
        // 名前を引くので、解決の回数を数えるテストとは直列に回す
        let _guard = crate::dns::RESOLVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(is_local_target("127.0.0.1:8080"));
        assert!(is_local_target("localhost"));
        assert!(is_local_target("169.254.169.254"), "cloud metadata");
        assert!(is_local_target("[::1]:443"));
        assert!(is_local_target("[fe80::1]"));
        assert!(is_local_target("[::ffff:127.0.0.1]"), "v4-mapped");
        assert!(!is_local_target("93.184.216.34"));
        assert!(!is_local_target("10.0.0.1"), "私有アドレスは対象外");
    }
}
