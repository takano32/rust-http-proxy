use super::*;
use crate::origin::Scheme;

/// T32.12: 不正な authority と `Host` は断り、正当な形 (IP リテラル・先頭の 0・大文字・空のポート) は通す。
#[test]
fn invalid_authority_and_host_are_refused() {
    for bad in [
        "example.invalid x",
        "a, b",
        "a/b",
        "a b:443",
        "exa mple.com",
        "127.0.0.1:99999",
        "h:+22501",
        "h:123456",
        "h:8a",
        "[::1",
        "[]:80",
        "[::1]x",
        "[g::1]:80",
        ":80",
        "h%2",
        "h;x",
    ] {
        assert!(check_authority(bad).is_err(), "{:?}", bad);
        // 対象の中の `/` は authority の終わりなので、絶対形式で試すのは `/` の無い形だけ
        if !bad.contains('/') {
            let target = format!("http://{}/x", bad);
            assert!(parse_origin(&target, None).is_err(), "{:?}", target);
            assert!(target_host(&target, None).is_err(), "{:?}", target);
        }
        assert!(parse_origin("/x", Some(bad)).is_err(), "Host: {:?}", bad);
        assert!(target_host("/x", Some(bad)).is_err(), "Host: {:?}", bad);
        // 絶対形式の要求でも `Host` は入口 (`target_host`) が確かめる
        assert!(target_host("http://h/x", Some(bad)).is_err(), "{:?}", bad);
    }
    for good in [
        "[::1]:8080",
        "[2001:db8::1]",
        "h:080",
        "UPPER.Example.COM",
        "127.0.0.1:65535",
        "a-b_c~d.example",
        "h%41",
        "::1",
    ] {
        assert_eq!(check_authority(good).unwrap(), good, "{:?}", good);
        assert_eq!(target_host("/x", Some(good)).unwrap(), good);
    }
    // 速い道と遅い道は同じ答え
    for x in [
        "h",
        "h:1",
        "h:65535",
        "h:65536",
        "h:00080",
        "h:000080",
        "h:",
        ":1",
        "a.b-c_d~e:8080",
        "a b",
        "a@b",
        "a%41",
        "::1",
        "[::1]:80",
        "h:8a",
        "h:+1",
        "",
        "日本",
    ] {
        assert_eq!(
            check_authority(x).ok(),
            check_authority_slow(x).ok(),
            "{:?}",
            x
        );
    }
    // 空のポートは既定のポート (`:` を落とす)
    assert_eq!(check_authority("h:").unwrap(), "h");
    assert_eq!(check_authority("[::1]:").unwrap(), "[::1]");
    assert_eq!(parse_origin("http://h:/x", None).unwrap().host_port, "h");
    assert_eq!(target_host("/x", Some("h:")).unwrap(), "h");
    // 空の `Host` は絶対形式なら通す (宛先は要求の対象)
    assert_eq!(parse_origin("http://h/x", Some("")).unwrap().host_port, "h");
}

/// [`parse_origin`] の `host_port` と 1 文字も違わないこと (乖離すると ACL がすり抜ける)。
#[test]
fn matches_parse_origin() {
    let cases = [
        ("http://example.com/a/b", None),
        ("http://example.com:8080/", None),
        ("https://[::1]:8443/x", None),
        ("/https/example.com/a", None),
        ("/http/example.com:81/a", None),
        ("/only/path", Some("host.example:80")),
        ("/", Some("h")),
        ("example.com:443", None),
        ("example.com", None),
        ("http://example.com?q=1", None),
        ("http://example.com#frag", None),
        ("http://user:pw@example.com/", None),
        ("HTTP://Example.com:8080/a", None),
        ("Https://[::1]:8443/x", None),
    ];
    for (target, host) in cases {
        let want = parse_origin(target, host).map(|o| o.host_port);
        let got = target_host(target, host).map(|h| h.to_string());
        match (want, got) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "target={:?} host={:?}", target, host),
            (Err(_), Err(_)) => {}
            (a, b) => panic!("target={:?} host={:?}: {:?} vs {:?}", target, host, a, b),
        }
    }
    // どちらも「ホストが無い」で失敗すること
    for (target, host) in [("http:///a", None), ("/p", None), ("/p", Some(""))] {
        assert!(parse_origin(target, host).is_err());
        assert!(target_host(target, host).is_err());
    }
}

/// T27.8: authority は `/`・`?`・`#` の手前まで、userinfo は断る、fragment はオリジンへ送らない。
#[test]
fn authority_ends_at_slash_query_or_fragment_and_userinfo_is_refused() {
    let o = |t: &str| {
        let o = parse_origin(t, Some("h.example")).unwrap();
        (o.host_port, o.path)
    };
    assert_eq!(o("http://h?q=1"), ("h".into(), "/?q=1".into()));
    assert_eq!(o("http://h/a?q#frag"), ("h".into(), "/a?q".into()));
    assert_eq!(o("http://h#frag"), ("h".into(), "/".into()));
    assert_eq!(o("http://h?q#f"), ("h".into(), "/?q".into()));
    assert_eq!(o("/a#frag"), ("h.example".into(), "/a".into()));
    assert_eq!(o("/https/h:8443/x#y"), ("h:8443".into(), "/x".into()));
    for t in ["http://user:pw@h/", "http://user@h", "/https/u@h/x"] {
        assert!(parse_origin(t, None).is_err(), "{}", t);
        assert!(target_host(t, None).is_err(), "{}", t);
    }
}

/// [`target_scheme`] が [`parse_origin`] の `scheme` と食い違わないこと (T14.48)。
///
/// 食い違うと、個票の宛先に補う既定のポートが `pool_key` のものとずれる。
#[test]
fn the_scheme_matches_parse_origin() {
    let cases = [
        ("http://example.com/a/b", None),
        ("https://example.com/a/b", None),
        ("https://[::1]:8443/x", None),
        ("/https/example.com/a", None),
        ("/http/example.com:81/a", None),
        ("/only/path", Some("host.example")),
        ("example.com:443", None),
        ("example.com", None),
        ("HTTP://example.com/a", None),
        ("HTTPS://example.com/a", None),
        ("hTTpS://example.com/a", None),
    ];
    for (target, host) in cases {
        let want = parse_origin(target, host).unwrap().scheme;
        assert_eq!(want, target_scheme(target), "target={:?}", target);
    }
    assert_eq!(target_scheme("https://h/x").default_port(), 443);
    assert_eq!(target_scheme("h").default_port(), 80);
}

/// T28.33: 絶対 URI のスキームは大小を区別せず、小文字と同じ [`Origin`] になる
/// (URL・プールキー・接続先も小文字のスキームで組み立てる)。マッピング形式はパスなので小文字だけ。
#[test]
fn the_scheme_is_case_insensitive() {
    for (upper, lower) in [
        ("HTTP://h.example:81/a?q", "http://h.example:81/a?q"),
        ("Http://h.example", "http://h.example"),
        ("HTTPS://[::1]:8443/x", "https://[::1]:8443/x"),
        ("hTtPs://h.example/x#f", "https://h.example/x#f"),
    ] {
        let a = parse_origin(upper, None).unwrap();
        let b = parse_origin(lower, None).unwrap();
        assert_eq!(
            (a.scheme, &a.host_port, &a.path, a.mapped),
            (b.scheme, &b.host_port, &b.path, b.mapped),
            "{}",
            upper
        );
        assert_eq!(a.locate().url(), b.locate().url(), "{}", upper);
    }
    assert_eq!(strip_scheme("HTTP://h/x"), Some((Scheme::Http, "h/x")));
    assert_eq!(strip_scheme("httpS://h"), Some((Scheme::Https, "h")));
    assert_eq!(strip_scheme("htt"), None);
    assert_eq!(strip_scheme("ftp://h/"), None);
    assert_eq!(strip_scheme("/http/h/x"), None);
    // 空のホストは大文字でも断る
    assert!(parse_origin("HTTP:///a", None).is_err());
    assert!(target_host("HTTPS:///a", None).is_err());
    // 多バイト文字の途中で切っても落ちない
    assert_eq!(strip_scheme("日本語://h"), None);
}
