//! `acl` の単体試験 (T.34.65 で `acl.rs` の `mod tests` と `mod local_tests` をそのまま移した)。

use super::*;
use std::net::IpAddr;

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
    assert!(!is_local_target("93.184.216.34"), "外向きの IP は通す");
    // T.28.41: 私設アドレス (と v4-mapped の書き方) も既定では断る
    for t in [
        "10.0.0.1",
        "10.255.255.1:80",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.1:27042",
        "100.64.0.1",
        "100.127.255.255",
        "[fd00::1]",
        "[fc00::1]:443",
        "[::ffff:10.0.0.1]",
        "[::ffff:192.168.0.1]:80",
    ] {
        assert!(is_local_target(t), "{}", t);
    }
    // 境の外は通す
    for t in [
        "172.15.255.255",
        "172.32.0.1",
        "100.63.255.255",
        "100.128.0.1",
        "11.0.0.1",
        "192.169.0.1",
        "[fe00::1]",
        "[2001:db8::1]",
    ] {
        assert!(!is_local_target(t), "{}", t);
    }
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
        let (local, resolved) = crate::acl::resolve_target(&addr, false);
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

/// T.28.41: 名前を引いた答えが私設アドレスなら、既定 (`allow_private` が偽) では断り、
/// `private` では通す。ループバックはどちらでも断る。
#[test]
fn names_answering_private_addresses_are_refused_by_default() {
    let _resolve = crate::dns::RESOLVE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::dns::set_ttl(std::time::Duration::from_secs(60));
    crate::dns::clear();
    let ip = |s: &str| s.parse::<IpAddr>().unwrap();
    for (host, addr, private_ok) in [
        ("t2841-a.example", "10.255.255.1", true),
        ("t2841-b.example", "fd00::1", true),
        ("t2841-c.example", "100.64.0.1", true),
        ("t2841-d.example", "127.0.0.1", false),
        ("t2841-e.example", "169.254.169.254", false),
    ] {
        crate::dns::put_answer_for_test(host, &[ip(addr)], std::time::Duration::ZERO);
        let target = format!("{}:443", host);
        assert!(resolve_target(&target, false).0, "off: {} ({})", host, addr);
        assert_eq!(
            !resolve_target(&target, true).0,
            private_ok,
            "private: {} ({})",
            host,
            addr
        );
    }
    // 答えのどれか 1 つでも私設アドレスなら断る (外向きの答えと混ざっていても)
    crate::dns::put_answer_for_test(
        "t2841-mixed.example",
        &[ip("93.184.216.34"), ip("192.168.0.10")],
        std::time::Duration::ZERO,
    );
    assert!(resolve_target("t2841-mixed.example:80", false).0);
    assert!(!resolve_target("t2841-mixed.example:80", true).0);
    crate::dns::clear();
}

#[test]
fn allow_local_reads_three_ways() {
    assert_eq!(AllowLocal::default(), AllowLocal::Off);
    assert!(!AllowLocal::Off.allows_private());
    assert!(AllowLocal::Private.allows_private() && AllowLocal::On.allows_private());
    assert_eq!(AllowLocal::Off.json(), "false");
    assert_eq!(AllowLocal::On.json(), "true");
    assert_eq!(AllowLocal::Private.json(), "\"private\"");
}

/// T21.3: 判定が serve-stale の古い答えを引いたら、接続へ渡す答えに印が付く
/// (接続側が繋げなかったときに引き直すため)。期限内の答えには付かない。
#[test]
fn the_resolved_answer_carries_the_stale_mark() {
    let _resolve = crate::dns::RESOLVE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let ttl = std::time::Duration::from_secs(60);
    crate::dns::set_ttl(ttl);
    crate::dns::set_serve_stale(crate::dns::SERVE_STALE);
    crate::dns::clear();
    let addr = std::net::IpAddr::from([192, 0, 2, 1]);
    // 期限内 (齢 0) → 印なし。期限切れ (齢 120 秒) → 印あり
    for (age, want) in [(0, false), (120, true)] {
        crate::dns::put_answer_for_test(
            "t213-acl.example",
            &[addr],
            std::time::Duration::from_secs(age),
        );
        let (local, resolved) = crate::acl::resolve_target("t213-acl.example:443", false);
        assert!(!local);
        let resolved = resolved.expect("答えが返る");
        assert_eq!(resolved.addrs(), &[addr]);
        assert_eq!(resolved.is_stale(), want, "齢 {} 秒", age);
    }
    crate::dns::clear();
}

/// T.34.66: 大文字のパターンも小文字の名前に当たる (`host` 側は小文字のまま比べる)。
#[test]
fn match_pattern_lowers_the_pattern_but_not_the_host() {
    assert!(match_pattern("Example.COM", "example.com"));
    assert!(match_pattern("*.Example.Com", "api.example.com"));
    assert!(match_pattern("*.EXAMPLE.com", "example.com"));
    // 前と同じく、`host` は小文字で来る約束なので大文字の名前には当たらない
    assert!(!match_pattern("example.com", "Example.com"));
    assert!(!match_pattern("example.com", "example.co"));
    assert!(!match_pattern("", "a"));
    assert!(match_pattern("", ""));
}

/// T.34.66: `*.a.b` は `a.b` と `x.a.b` に当たり、`xa.b` (`.` の無い続き) には当たらない。
#[test]
fn a_wildcard_matches_the_name_and_its_subdomains_only() {
    assert!(match_pattern("*.a.b", "a.b"));
    assert!(match_pattern("*.a.b", "x.a.b"));
    assert!(match_pattern("*.a.b", "y.x.a.b"));
    assert!(!match_pattern("*.a.b", "xa.b"));
    assert!(!match_pattern("*.a.b", "b"));
    assert!(!match_pattern("*.a.b", ".b"));
    assert!(match_pattern("*.a.b", ".a.b"));
    // `*.` だけのパターンは前と同じく、空の名前と `.` で終わる名前に当たる
    assert!(match_pattern("*.", ""));
    assert!(match_pattern("*.", "x."));
    assert!(!match_pattern("*.", "x"));
    // 文字の途中で切っても落ちない (バイトで比べる)
    assert!(!match_pattern("*.b", "あb"));
    assert!(match_pattern("*.ä", "x.ä"));
}
