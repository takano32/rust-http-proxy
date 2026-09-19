use crate::dns::Resolved;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, Ordering};

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

    /// 読んだ結果を `443,8080-8099` の形に書き戻す (空なら `""` = 制限なし)。
    /// `/config` が**効いている値**を出すために使う (読めなかった項目は落ちている。T14.15)。
    pub fn spec(&self) -> String {
        self.ranges
            .iter()
            .map(|(lo, hi)| {
                if lo == hi {
                    lo.to_string()
                } else {
                    format!("{}-{}", lo, hi)
                }
            })
            .collect::<Vec<_>>()
            .join(",")
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

/// 接続元 IP の許可リスト (`PROXY_ALLOW_CLIENTS`)。空なら全許可 (既定)。
///
/// 宛先の [`AclConfig`] とは**別物**で、こちらは accept した相手の IP を見る
/// ([`is_local_ip`] / `PROXY_ALLOW_LOCAL` は宛先の話なので無関係)。
/// **認証ではない** — 経路を絞るだけで、同じアドレスから来られれば誰でも通る。
///
/// 持ち方は「前置長の付いたアドレス」の配列 (`1.2.3.4` は `/32`、`2001:db8::` は `/128`)。
/// 想定は多くても数十件なので線形に照合する (16 件で 1 us 未満)。
/// v4-mapped IPv6 (`::ffff:1.2.3.4`) は [`crate::net::canonical_ip`] で IPv4 に直してから
/// 照合するので、デュアルスタックで待ち受けていても `1.2.3.4` の 1 行で書ける。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientAcl {
    v4: Vec<(u32, u8)>,
    v6: Vec<(u128, u8)>,
}

impl ClientAcl {
    /// `1.2.3.4,10.0.0.0/8,2001:db8::/32` を読む。空文字なら全許可。
    /// **書式が違う項目は無視する** ([`PortSet::parse`] と同じ作法。1 項目の書き損じで
    /// 全部が空 = 全許可 に化けると、絞ったつもりで絞れていないことになるため)。
    pub fn parse(spec: &str) -> ClientAcl {
        let mut acl = ClientAcl::default();
        for item in spec.split(',').map(str::trim).filter(|i| !i.is_empty()) {
            let (addr, len) = match item.split_once('/') {
                Some((a, b)) => match b.trim().parse::<u8>() {
                    Ok(n) => (a.trim(), Some(n)),
                    Err(_) => continue,
                },
                None => (item, None),
            };
            let Ok(ip) = addr
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
            else {
                continue;
            };
            match crate::net::canonical_ip(ip) {
                // `::ffff:1.2.3.0/120` のような書き方も IPv4 の数え方に直す
                IpAddr::V4(v4) => {
                    let bits = match len {
                        Some(n) if ip.is_ipv6() => match n.checked_sub(96) {
                            Some(n) => n,
                            None => continue,
                        },
                        Some(n) => n,
                        None => 32,
                    };
                    if bits <= 32 {
                        acl.v4.push((u32::from(v4), bits));
                    }
                }
                IpAddr::V6(v6) => {
                    let bits = len.unwrap_or(128);
                    if bits <= 128 {
                        acl.v6.push((u128::from(v6), bits));
                    }
                }
            }
        }
        acl
    }

    /// 1 項目も無いか (= 全許可。**既定の経路はこの分岐 1 回だけ**)。
    pub fn is_empty(&self) -> bool {
        self.v4.is_empty() && self.v6.is_empty()
    }

    /// この接続元を受けてよいか。空なら常に true。
    pub fn allows(&self, ip: IpAddr) -> bool {
        if self.is_empty() {
            return true;
        }
        // 前置長 0 (`0.0.0.0/0`) は `>>` が桁あふれるので先に拾う
        match crate::net::canonical_ip(ip) {
            IpAddr::V4(v4) => {
                let a = u32::from(v4);
                (self.v4.iter()).any(|&(net, bits)| bits == 0 || (a ^ net) >> (32 - bits) == 0)
            }
            IpAddr::V6(v6) => {
                let a = u128::from(v6);
                (self.v6.iter()).any(|&(net, bits)| bits == 0 || (a ^ net) >> (128 - bits) == 0)
            }
        }
    }
}

/// 起動ログに「実際に読めた項目」を出すため (書き損じた項目は消えているので気づける)。
impl std::fmt::Display for ClientAcl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let v4 = (self.v4.iter())
            .map(|&(net, bits)| (IpAddr::V4(std::net::Ipv4Addr::from(net)), bits, 32u8));
        let v6 = (self.v6.iter())
            .map(|&(net, bits)| (IpAddr::V6(std::net::Ipv6Addr::from(net)), bits, 128u8));
        for (i, (ip, bits, full)) in v4.chain(v6).enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            if bits == full {
                write!(f, "{}", ip)?;
            } else {
                write!(f, "{}/{}", ip, bits)?;
            }
        }
        Ok(())
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
        // 自己ベンチ (T14.43) の相手役だけは `PROXY_ALLOW_LOCAL=off` でも通す (3 秒だけ)
        return (is_local_ip(ip) && !is_self_bench_target(ip, port), None);
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

/// 起動時の自己ベンチ (T14.43) が使う**自分の中の相手役**のポート (`0` = 無し)。
///
/// 自己ベンチの宛先は `127.0.0.1` の使い捨てポート 2 つ (固定応答のオリジンと、すぐ閉じる
/// sink) で、どちらも**このプロセスの中**にある。`PROXY_ALLOW_LOCAL=off` (既定) のままでは
/// 自分の中のオリジンにも 403 を返してしまい、測れるのが「403 を返す費用」になってしまうので、
/// **自己ベンチが回っている 3 秒だけ**この 2 ポートを判定から外す。
/// 開けるのも閉じるのも `crates/run/src/lib.rs` の自己ベンチの前後 1 回ずつで、外から同じポートを
/// 指されても行き先は 1 KiB を返すオリジンか、すぐ閉じる sink しかない。
static SELF_BENCH_PORTS: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

/// 自己ベンチの相手役のポートを覚える (空のスライスで**閉じる**)。
pub fn set_self_bench_ports(ports: &[u16]) {
    for (i, slot) in SELF_BENCH_PORTS.iter().enumerate() {
        slot.store(ports.get(i).copied().unwrap_or(0) as u32, Ordering::Relaxed);
    }
}

/// いま自己ベンチが使っているループバックの宛先か。
///
/// **費用は「ループバック宛ての IP リテラル」のときの原子読み 1〜2 回だけ**。
/// [`resolve_target`] で [`is_local_ip`] が真になった後にしか呼ばないので、
/// 普通の要求 (名前宛て・外向きの IP) では 1 命令も増えない。
fn is_self_bench_target(ip: IpAddr, port: Option<u16>) -> bool {
    let Some(port) = port.filter(|p| *p != 0) else {
        return false;
    };
    ip.is_loopback()
        && SELF_BENCH_PORTS
            .iter()
            .any(|slot| slot.load(Ordering::Relaxed) == u32::from(port))
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

    /// 自己ベンチ (T14.43) の相手役だけは `PROXY_ALLOW_LOCAL=off` でも通る。
    #[test]
    fn self_bench_targets_are_exempt_while_the_bench_runs() {
        assert!(is_local_target("127.0.0.1:18081"), "普段はローカル宛て");
        set_self_bench_ports(&[18081, 18082]);
        assert!(!is_local_target("127.0.0.1:18081"), "自己ベンチの相手役");
        assert!(
            !is_local_target("[::1]:18082"),
            "IPv6 のループバックでも同じ"
        );
        assert!(
            is_local_target("127.0.0.1:18083"),
            "覚えていないポートはそのまま"
        );
        assert!(
            is_local_target("169.254.169.254:80"),
            "メタデータは開けない"
        );
        set_self_bench_ports(&[]);
        assert!(is_local_target("127.0.0.1:18081"), "3 秒で閉じる");
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
        // `/config` に出す「効いている値」は読めた項目だけ (T14.15)
        assert_eq!(set.spec(), "443,80,8080-8099");
        assert_eq!(PortSet::parse("443,junk").spec(), "443");
        assert_eq!(none.spec(), "");
    }

    #[test]
    fn client_acl_matches_addresses_and_cidr() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        // 空なら全許可 (既定)
        let none = ClientAcl::parse("  ");
        assert!(none.is_empty() && none.allows(ip("203.0.113.9")));

        let acl = ClientAcl::parse("1.2.3.4, 10.0.0.0/8 ,2001:db8::/32");
        assert!(!acl.is_empty());
        assert!(acl.allows(ip("1.2.3.4")));
        assert!(!acl.allows(ip("1.2.3.5")));
        assert!(acl.allows(ip("10.0.0.1")) && acl.allows(ip("10.255.255.255")));
        assert!(!acl.allows(ip("11.0.0.1")));
        assert!(acl.allows(ip("2001:db8::1")) && !acl.allows(ip("2001:db9::1")));
        // v4-mapped IPv6 は IPv4 として照合する (デュアルスタックの待ち受け)
        assert!(acl.allows(ip("::ffff:10.0.0.1")) && !acl.allows(ip("::ffff:11.0.0.1")));
        // IPv4 しか書いていなければ本物の IPv6 は通さない (逆も同じ)
        assert!(!ClientAcl::parse("10.0.0.0/8").allows(ip("2001:db8::1")));
        assert!(!ClientAcl::parse("2001:db8::/32").allows(ip("10.0.0.1")));
        assert_eq!(acl.to_string(), "1.2.3.4, 10.0.0.0/8, 2001:db8::/32");
    }

    #[test]
    fn client_acl_edges_and_bad_entries() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        // `/0` は全部 (`>>` の桁あふれを起こさないこと)
        assert!(ClientAcl::parse("0.0.0.0/0").allows(ip("203.0.113.9")));
        assert!(ClientAcl::parse("::/0").allows(ip("2001:db8::1")));
        // `/32` と `/128` は 1 つだけ
        let one = ClientAcl::parse("[::1]/128");
        assert!(one.allows(ip("::1")) && !one.allows(ip("::2")));
        // 書式が違う項目は落ちるだけで、残りは効く (全部落ちて「全許可」に化けない)
        let acl = ClientAcl::parse("junk, 10.0.0.0/nope, 10.0.0.0/33, 127.0.0.0/8");
        assert!(!acl.is_empty(), "残った 1 項目で絞り続ける");
        assert!(acl.allows(ip("127.0.0.1")) && !acl.allows(ip("10.0.0.1")));
        // 書き損じだけなら空 = 全許可 (絞る手段が 1 つも無いので止めようがない)
        assert!(ClientAcl::parse("junk").is_empty());
        // v4-mapped を前置長つきで書いても IPv4 の数え方に直る (`/120` = `/24`)
        let mapped = ClientAcl::parse("::ffff:10.0.0.0/120");
        assert_eq!(mapped.to_string(), "10.0.0.0/24");
        assert!(mapped.allows(ip("10.0.0.1")) && !mapped.allows(ip("10.0.1.1")));
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

    /// T12.7: **判定 (ACL) と接続で名前解決を 2 回しない。**
    ///
    /// `PROXY_DNS_TTL_SECS=0` (キャッシュ無効) でも 1 要求 1 回で、接続は判定と同じ
    /// 答えを使う (別の答えを引くと DNS rebinding でローカル宛ての判定をすり抜けられる)。
    /// 表とカウンタは全テストで共有しているので直列に回す。
    #[test]
    fn the_check_and_the_connect_share_one_lookup() {
        let _resolve = crate::dns::RESOLVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // localhost が 2 候補 (::1 と 127.0.0.1) の環境では Happy Eyeballs の
        // 全体の勝敗が動くので、そちらのテストとも直列にする
        let _ipv6 = crate::net::IPV6_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("localhost:{}", listener.local_addr().unwrap().port());
        let lookups = || {
            let [hits, misses, _, _, _] = crate::dns::counters();
            hits + misses
        };
        for ttl in [
            std::time::Duration::from_secs(60),
            std::time::Duration::ZERO,
        ] {
            crate::dns::set_ttl(ttl);
            crate::dns::clear();
            let before = lookups();

            // 1. ローカル宛ての判定: ここで 1 回だけ引き、答えを持って帰る
            let (local, resolved) = crate::acl::resolve_target(&addr);
            assert!(local, "localhost はローカル宛て");
            let resolved = resolved.expect("判定に使った答えが返る");
            assert_eq!(lookups() - before, 1, "判定で 1 回 (ttl={:?})", ttl);

            // 2. 接続: 判定の答えを使うので引き直さない
            let stream =
                crate::net::connect_with(&addr, Some(&resolved), std::time::Duration::from_secs(5))
                    .unwrap();
            assert!(
                resolved.addrs().contains(&stream.peer_addr().unwrap().ip()),
                "判定に使った答えの中の 1 つに繋いでいる"
            );
            assert_eq!(lookups() - before, 1, "接続では引かない (ttl={:?})", ttl);

            // 3. 答えを渡さなければ (T12.7 の前の形) もう 1 回引く
            drop(crate::net::connect_with(&addr, None, std::time::Duration::from_secs(5)).unwrap());
            assert_eq!(lookups() - before, 2, "渡さないと 2 回 (ttl={:?})", ttl);
        }
        crate::dns::set_ttl(std::time::Duration::from_secs(60));
        crate::dns::clear();
    }
}
