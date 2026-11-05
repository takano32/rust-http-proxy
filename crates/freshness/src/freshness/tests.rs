use std::time::Duration;

use super::policy::heuristic_ttl;
use super::*;
use crate::cache::CacheConfig;

/// 304 の `no-store`・`private`・`Vary: Cookie` は保存を禁じる。`Set-Cookie` と
/// `Vary: Accept-Encoding` は禁じない (T29.0)。
#[test]
fn not_modified_forbidding_storage() {
    let f = |hs: &[(&str, &str)]| not_modified_forbids_storing(&hdrs(hs));
    assert!(f(&[("cache-control", "max-age=60, No-Store")]));
    assert!(f(&[("cache-control", "private")]));
    assert!(f(&[("Pragma", "no-store")]));
    assert!(f(&[("vary", "Cookie")]));
    assert!(!f(&[("set-cookie", "a=b"), ("etag", "\"v1\"")]));
    assert!(!f(&[
        ("vary", "Accept-Encoding"),
        ("cache-control", "max-age=60")
    ]));
    assert!(!f(&[]));
}

fn cfg() -> CacheConfig {
    CacheConfig::disabled()
}

fn hdrs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

const NOW: u64 = 1_788_288_860;

/// epoch 秒を HTTP の日付 (IMF-fixdate。曜日は見ないので `Mon` 固定) にする。
fn http_date(epoch: u64) -> String {
    let (y, mo, d, h, mi, s) = crate::log::civil_from_epoch(epoch);
    format!(
        "Mon, {:02} {} {} {:02}:{:02}:{:02} GMT",
        d,
        [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"
        ][mo as usize - 1],
        y,
        h,
        mi,
        s
    )
}

#[test]
fn default_ttl_without_any_hint() {
    let p = response_policy(200, &hdrs(&[("content-type", "text/html")]), &cfg(), NOW).unwrap();
    assert_eq!(p.ttl, Duration::from_secs(300));
    assert!(!p.validators && !p.must_revalidate);
}

#[test]
fn max_age_s_maxage_and_expires() {
    let c = cfg();
    let p = response_policy(
        200,
        &hdrs(&[("cache-control", "public, max-age=120")]),
        &c,
        NOW,
    );
    assert_eq!(p.unwrap().ttl, Duration::from_secs(120));
    let p = response_policy(
        200,
        &hdrs(&[("cache-control", "max-age=120, s-maxage=600")]),
        &c,
        NOW,
    );
    assert_eq!(p.unwrap().ttl, Duration::from_secs(600));
    let p = response_policy(
        200,
        &hdrs(&[
            ("date", "Mon, 01 Sep 2025 00:00:00 GMT"),
            ("expires", "Mon, 01 Sep 2025 01:00:00 GMT"),
        ]),
        &c,
        NOW,
    );
    assert_eq!(p.unwrap().ttl, Duration::from_secs(3600));
    // 解析できない Expires は「既に古い」→ バリデータが無ければ保存しない
    assert!(response_policy(200, &hdrs(&[("expires", "0")]), &c, NOW).is_none());
}

/// 読めない `max-age` / `s-maxage` は「既に古い」(T28.10 (d))。バリデータが無ければ保存しない、
/// あれば毎回再検証 (TTL 0) で保存する。読める値は今までどおり。
#[test]
fn unreadable_max_age_is_stale() {
    let c = cfg();
    for cc in [
        "max-age=abc",
        "max-age=-1",
        "max-age",
        "s-maxage=x, max-age=60",
    ] {
        assert_eq!(
            response_policy(200, &hdrs(&[("cache-control", cc)]), &c, NOW),
            None,
            "{}",
            cc
        );
        let p = response_policy(
            200,
            &hdrs(&[("cache-control", cc), ("etag", "\"v\"")]),
            &c,
            NOW,
        )
        .unwrap();
        assert_eq!(p.ttl, Duration::ZERO, "{}", cc);
    }
    let p = response_policy(200, &hdrs(&[("cache-control", "max-age=60")]), &c, NOW).unwrap();
    assert_eq!(p.ttl, Duration::from_secs(60));
    // `Expires` より先に見る (読めない max-age があれば Expires で延ばさない)
    assert_eq!(
        response_policy(
            200,
            &hdrs(&[
                ("cache-control", "max-age=abc"),
                ("expires", "Thu, 01 Jan 2099 00:00:00 GMT")
            ]),
            &c,
            NOW
        ),
        None
    );
}

#[test]
fn negative_responses_get_a_short_implicit_ttl() {
    let c = cfg();
    let p = response_policy(404, &hdrs(&[]), &c, NOW).unwrap();
    assert_eq!(p.ttl, c.negative_ttl, "default TTL is capped for 404");
    let p = response_policy(410, &hdrs(&[("cache-control", "max-age=3600")]), &c, NOW).unwrap();
    assert_eq!(p.ttl.as_secs(), 3600, "explicit max-age wins");
    let p = response_policy(200, &hdrs(&[]), &c, NOW).unwrap();
    assert_eq!(p.ttl, c.default_ttl, "success keeps the default TTL");
}

#[test]
fn heuristic_from_last_modified_is_clamped() {
    let c = cfg();
    // 100 日前に更新 → 10% = 10 日 だが上限 7 日
    let lm_str = http_date(NOW - 100 * 86_400);
    let p = response_policy(200, &hdrs(&[("last-modified", &lm_str)]), &c, NOW).unwrap();
    assert_eq!(p.ttl, Duration::from_secs(7 * 86_400));
    assert!(p.validators);
    // 直近の更新なら既定 TTL が下限
    let p = response_policy(
        200,
        &hdrs(&[("last-modified", "Wed, 01 Jan 2025 00:00:00 GMT")]),
        &c,
        1_735_689_600 + 60,
    )
    .unwrap();
    assert_eq!(p.ttl, Duration::from_secs(300));
}

/// 経験則の TTL そのもの: 経過の 10% を既定 TTL (下限) と上限 7 日の間に収める。
/// `heuristic_percent` が 0 と `Last-Modified` 無しは「経験則なし」(`None`)。
#[test]
fn the_heuristic_ttl_is_a_share_of_the_age_between_the_floor_and_the_ceiling() {
    let c = cfg();
    assert_eq!(heuristic_ttl(None, &c, NOW), None);
    // 1,000 秒前 → 10% = 100 秒だが下限 300 秒
    assert_eq!(heuristic_ttl(Some(NOW - 1_000), &c, NOW), Some(300));
    // 10 日前 → 1 日
    assert_eq!(
        heuristic_ttl(Some(NOW - 10 * 86_400), &c, NOW),
        Some(86_400)
    );
    // 100 日前 → 10 日だが上限 7 日
    assert_eq!(
        heuristic_ttl(Some(NOW - 100 * 86_400), &c, NOW),
        Some(7 * 86_400)
    );
    // 未来の `Last-Modified` (時計のずれ) は経過 0 → 下限
    assert_eq!(heuristic_ttl(Some(NOW + 60), &c, NOW), Some(300));
    // 0% は経験則を使わない
    let off = CacheConfig {
        heuristic_percent: 0,
        ..cfg()
    };
    assert_eq!(heuristic_ttl(Some(NOW - 10 * 86_400), &off, NOW), None);
    // 上限が下限より小さい設定でも下限は守る (上限は下限まで引き上げる)
    let tight = CacheConfig {
        heuristic_max: Duration::from_secs(10),
        ..cfg()
    };
    assert_eq!(
        heuristic_ttl(Some(NOW - 10 * 86_400), &tight, NOW),
        Some(300)
    );
    assert_eq!(heuristic_ttl(Some(NOW - 1), &tight, NOW), Some(300));
}

#[test]
fn not_cacheable_cases() {
    let c = cfg();
    let n = |h: &[(&str, &str)]| response_policy(200, &hdrs(h), &c, NOW);
    assert!(n(&[("cache-control", "no-store")]).is_none());
    assert!(n(&[("cache-control", "private")]).is_none());
    assert!(n(&[("cache-control", "max-age=0")]).is_none());
    assert!(n(&[("cache-control", "no-cache")]).is_none());
    assert!(n(&[("set-cookie", "a=b")]).is_none());
    assert!(n(&[("vary", "*")]).is_none());
    assert!(n(&[("vary", "Cookie")]).is_none());
    assert!(n(&[("vary", "Accept-Encoding")]).is_some());
    assert!(response_policy(500, &hdrs(&[]), &c, NOW).is_none());
    assert!(response_policy(302, &hdrs(&[]), &c, NOW).is_none());
}

#[test]
fn zero_ttl_with_validators_is_stored_for_revalidation() {
    let c = cfg();
    let p = response_policy(
        200,
        &hdrs(&[
            ("cache-control", "no-cache, must-revalidate"),
            ("etag", "\"abc\""),
        ]),
        &c,
        NOW,
    )
    .unwrap();
    assert_eq!(p.ttl, Duration::ZERO);
    assert!(p.validators && p.must_revalidate);
    let p = response_policy(
        200,
        &hdrs(&[("cache-control", "max-age=0"), ("etag", "x")]),
        &c,
        NOW,
    );
    assert_eq!(p.unwrap().ttl, Duration::ZERO);
}

/// `Pragma` は `Cache-Control` と同じ扱い (`no-cache` は TTL 0、バリデータがあれば保存)。
/// `proxy-revalidate` も `must-revalidate` と同じ。
#[test]
fn pragma_and_proxy_revalidate_count_like_cache_control() {
    let c = cfg();
    assert!(response_policy(200, &hdrs(&[("pragma", "no-cache")]), &c, NOW).is_none());
    let p = response_policy(
        200,
        &hdrs(&[
            ("pragma", "no-cache"),
            ("last-modified", &http_date(NOW - 60)),
        ]),
        &c,
        NOW,
    )
    .unwrap();
    assert_eq!(p.ttl, Duration::ZERO);
    assert!(p.validators && !p.must_revalidate);
    let p = response_policy(
        200,
        &hdrs(&[("cache-control", "max-age=10, proxy-revalidate")]),
        &c,
        NOW,
    )
    .unwrap();
    assert!(p.must_revalidate);
    assert_eq!(p.ttl, Duration::from_secs(10));
}

#[test]
fn accept_encoding_is_normalized() {
    assert_eq!(accept_encoding_variant(None), "");
    assert_eq!(
        accept_encoding_variant(Some("gzip, deflate, br")),
        "br,deflate,gzip"
    );
    assert_eq!(
        accept_encoding_variant(Some("BR, gzip;q=0.5, identity;q=0")),
        "br,gzip"
    );
    assert_eq!(accept_encoding_variant(Some("gzip, gzip")), "gzip");
}

/// T28.13 (b): 速い道 (無い・空・小文字のトークン 1 つ) も遅い道と同じ正規形。
#[test]
fn accept_encoding_fast_path_matches_the_slow_one() {
    for (v, want) in [
        ("", ""),
        ("  ", ""),
        ("gzip", "gzip"),
        (" br ", "br"),
        ("x-gzip", "x-gzip"),
        ("GZIP", "gzip"),
        ("gzip;q=0", ""),
        (",", ""),
    ] {
        assert_eq!(accept_encoding_variant(Some(v)), want, "{:?}", v);
    }
}

/// T32.35: 要求に `Cookie` があれば、明示の鮮度も `public` も無い応答は保存しない。
#[test]
fn cookie_request_needs_explicit_freshness_or_public() {
    let lm = "Sun, 06 Nov 1994 08:49:37 GMT";
    let with_cookie =
        |h: &[(&str, &str)]| response_policy_lines(200, h.iter().copied(), true, &cfg(), NOW);
    // 経験則 (既定 TTL・Last-Modified)・`no-cache` + validator は保存しない
    assert!(with_cookie(&[("Content-Type", "text/html")]).is_none());
    assert!(with_cookie(&[("Last-Modified", lm)]).is_none());
    assert!(with_cookie(&[("Cache-Control", "no-cache"), ("ETag", "\"a\"")]).is_none());
    // 明示の鮮度か `public` があれば今までどおり
    assert_eq!(
        with_cookie(&[("Cache-Control", "max-age=60")])
            .unwrap()
            .ttl
            .as_secs(),
        60
    );
    assert_eq!(
        with_cookie(&[("Cache-Control", "s-maxage=30")])
            .unwrap()
            .ttl
            .as_secs(),
        30
    );
    assert!(with_cookie(&[("Expires", "Sun, 06 Nov 2094 08:49:37 GMT")]).is_some());
    assert_eq!(
        with_cookie(&[("Cache-Control", "Public")]).unwrap().ttl,
        cfg().default_ttl
    );
    // `Cookie` の無い要求の経験則は変わらない
    assert_eq!(
        response_policy_lines(200, [("Content-Type", "text/html")], false, &cfg(), NOW)
            .unwrap()
            .ttl,
        cfg().default_ttl
    );
}

/// T28.13 (a): 借用の並び (名前の大小を問わない) で読んでも、小文字の組と同じ方針。
#[test]
fn policy_from_borrowed_lines_matches_the_pairs() {
    let cases: &[&[(&str, &str)]] = &[
        &[("Cache-Control", "Max-Age=60"), ("ETag", "\"a\"")],
        &[("cache-control", "no-store")],
        &[
            ("Pragma", "no-cache"),
            ("Last-Modified", "Sun, 06 Nov 1994 08:49:37 GMT"),
        ],
        &[("Set-Cookie", "a=1"), ("Cache-Control", "max-age=60")],
        &[("Vary", "User-Agent")],
        &[
            ("Expires", "0"),
            ("Date", "Sun, 06 Nov 1994 08:49:37 GMT"),
            ("Age", "5"),
        ],
        &[("Content-Type", "text/html")],
    ];
    for case in cases {
        let lower: Vec<(String, String)> = case
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
            .collect();
        for status in [200, 404, 500] {
            assert_eq!(
                response_policy_lines(status, case.iter().copied(), false, &cfg(), NOW),
                response_policy(status, &lower, &cfg(), NOW),
                "{:?} {}",
                case,
                status
            );
        }
    }
}

#[test]
fn cached_head_and_conditionals() {
    let head = b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\nCache-Control: max-age=60, must-revalidate\r\nContent-Length: 2\r\n\r\n";
    let h = parse_cached_head(head);
    assert_eq!(h.status, 200);
    assert_eq!(h.etag.as_deref(), Some("\"v1\""));
    assert!(h.must_revalidate);
    assert!(client_not_modified(&h, Some("\"v1\""), None));
    assert!(client_not_modified(&h, Some("W/\"v1\", \"other\""), None));
    assert!(client_not_modified(&h, Some("*"), None));
    assert!(!client_not_modified(&h, Some("\"v2\""), None));
    assert!(client_not_modified(
        &h,
        None,
        Some("Mon, 07 Nov 1994 00:00:00 GMT")
    ));
    assert!(!client_not_modified(
        &h,
        None,
        Some("Sat, 05 Nov 1994 00:00:00 GMT")
    ));
    // If-None-Match があれば If-Modified-Since は無視
    assert!(!client_not_modified(
        &h,
        Some("\"v2\""),
        Some("Mon, 07 Nov 1994 00:00:00 GMT")
    ));
    let cond = conditional_headers(&h);
    assert_eq!(cond[0], "If-None-Match: \"v1\"");
    assert!(cond[1].starts_with("If-Modified-Since: Sun"));
    let nm = not_modified_headers(&h);
    assert!(nm.iter().any(|l| l.starts_with("ETag:")));
    assert!(!nm.iter().any(|l| l.starts_with("Content-Length")));
}

/// 先頭の読み方の細部: `Cache-Control` が 2 行なら `,` で繋いで小文字にする、状態行が読めなければ 0、
/// `:` の無い行は飛ばす、名前は元の綴りも残す、値は前後の空白を落とす。
#[test]
fn parse_cached_head_joins_cache_control_lines_and_keeps_the_original_names() {
    let h = parse_cached_head(
        b"HTTP/1.1 203 Non-Authoritative\r\nCache-Control: Max-Age=60\r\nCache-Control: Must-Revalidate\r\nX-Odd\r\nETag:   \"e\"  \r\nExpires: E\r\nDate: D\r\n\r\n",
    );
    assert_eq!(h.status, 203);
    assert_eq!(
        h.cache_control.as_deref(),
        Some("max-age=60,must-revalidate")
    );
    assert!(h.must_revalidate && !h.no_stale);
    assert!(!h.may_serve_stale());
    assert_eq!(h.etag.as_deref(), Some("\"e\""));
    assert_eq!(
        (h.expires.as_deref(), h.date.as_deref()),
        (Some("E"), Some("D"))
    );
    assert_eq!(h.headers.len(), 5, "`:` の無い行は数えない");
    assert_eq!(
        h.headers[0],
        (
            "cache-control".to_string(),
            "Cache-Control".to_string(),
            "Max-Age=60".to_string()
        )
    );
    // 空の入力と読めない状態行
    let empty = parse_cached_head(b"");
    assert_eq!(empty.status, 0);
    assert!(empty.headers.is_empty() && empty.cache_control.is_none());
    assert!(empty.may_serve_stale(), "指示が無ければ stale を配ってよい");
    assert_eq!(parse_cached_head(b"HTTP/1.1 abc\r\n\r\n").status, 0);
    // `Cache-Control` が無ければ `None` (空文字列にはしない)
    assert_eq!(
        parse_cached_head(b"HTTP/1.1 200 OK\r\nETag: \"x\"\r\n\r\n").cache_control,
        None
    );
}

/// 304 に載せる行は RFC 9110 §15.4.5 の 7 つだけで、**保存した順と元の綴り**のまま。
#[test]
fn not_modified_headers_keep_the_seven_names_in_stored_order() {
    let h = parse_cached_head(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nETag: \"v\"\r\nVary: Accept-Encoding\r\nDate: D\r\nContent-Location: /x\r\nExpires: E\r\nCache-Control: max-age=1\r\nLast-Modified: L\r\nContent-Length: 2\r\nServer: s\r\n\r\n",
    );
    assert_eq!(
        not_modified_headers(&h),
        [
            "ETag: \"v\"",
            "Vary: Accept-Encoding",
            "Date: D",
            "Content-Location: /x",
            "Expires: E",
            "Cache-Control: max-age=1",
            "Last-Modified: L",
        ]
    );
    // 再検証の条件は持っているものだけ (`Last-Modified` だけ・何も無い)
    let lm_only = parse_cached_head(b"HTTP/1.1 200 OK\r\nLast-Modified: L\r\n\r\n");
    assert_eq!(conditional_headers(&lm_only), ["If-Modified-Since: L"]);
    assert!(conditional_headers(&parse_cached_head(b"HTTP/1.1 200 OK\r\n\r\n")).is_empty());
}

/// `If-None-Match` の比べ方 (弱い比較、`*`、複数)。ETag を持たない表現には `*` だけが当たる。
#[test]
fn etag_lists_match_weakly() {
    assert!(etag_list_matches("\"a\"", "\"a\""));
    assert!(etag_list_matches("W/\"a\"", "\"a\""));
    assert!(etag_list_matches("\"a\"", "W/\"a\""));
    assert!(etag_list_matches("\"x\", \"a\" , \"y\"", "\"a\""));
    assert!(etag_list_matches("*", "\"anything\""));
    assert!(etag_list_matches("  *  ", "\"anything\""));
    assert!(!etag_list_matches("\"a\"", "\"b\""));
    assert!(!etag_list_matches("", "\"b\""));
    assert!(!etag_list_matches("\"a", "\"a\""), "引用符の有無は区別する");
    let no_etag = parse_cached_head(
        b"HTTP/1.1 200 OK\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\n\r\n",
    );
    assert!(client_not_modified(&no_etag, Some("*"), None));
    assert!(!client_not_modified(&no_etag, Some("\"a\""), None));
    // 読めない日付は「変わった」とみなす
    assert!(!client_not_modified(&no_etag, None, Some("yesterday")));
    assert!(!client_not_modified(&no_etag, None, None));
}

/// `Cache-Control` の指示の読み方: 名前だけ・値付き・`=` の前後の空白・引用符・似た名前。
/// 入力は小文字化されている前提なので、大文字の指示は当たらない。
#[test]
fn directives_are_matched_by_token_and_read_with_their_values() {
    assert!(has_directive("no-store", "no-store"));
    assert!(has_directive(" public ,  no-cache ", "no-cache"));
    assert!(has_directive("max-age=60", "max-age"));
    assert!(has_directive("max-age = 60", "max-age"));
    assert!(has_directive("private=\"x\"", "private"));
    assert!(
        !has_directive("no-cachex", "no-cache"),
        "前綴りが同じだけでは当たらない"
    );
    assert!(!has_directive("x-no-cache", "no-cache"));
    assert!(
        !has_directive("No-Store", "no-store"),
        "小文字化は呼ぶ側の仕事"
    );
    assert!(!has_directive("", "no-store"));
    assert!(!has_directive(",,", "no-store"));

    assert_eq!(directive_value("max-age=60", "max-age"), Some(60));
    assert_eq!(directive_value("max-age=\"60\"", "max-age"), Some(60));
    assert_eq!(
        directive_value("public, max-age = 60 , no-cache", "max-age"),
        Some(60)
    );
    assert_eq!(
        directive_value("s-maxage=10, max-age=20", "max-age"),
        Some(20)
    );
    assert_eq!(
        directive_value("max-age=1, max-age=2", "max-age"),
        Some(1),
        "先のものを読む"
    );
    assert_eq!(directive_value("max-age=abc", "max-age"), None);
    assert_eq!(directive_value("max-age=-1", "max-age"), None);
    assert_eq!(directive_value("max-age", "max-age"), None, "値なし");
    assert_eq!(
        directive_value("max-age-x=5", "max-age"),
        None,
        "似た名前は読まない"
    );
    assert_eq!(directive_value("", "max-age"), None);
}

/// 対応できる `Vary` は `Accept-Encoding` だけ (大小は問わず、空のトークンは無視)。
#[test]
fn only_accept_encoding_is_a_supported_vary() {
    assert!(vary_is_supported(""));
    assert!(vary_is_supported("accept-encoding"));
    assert!(vary_is_supported(" ACCEPT-ENCODING "));
    assert!(vary_is_supported("Accept-Encoding, "));
    assert!(vary_is_supported(","));
    assert!(!vary_is_supported("*"));
    assert!(!vary_is_supported("Accept-Encoding, User-Agent"));
    assert!(!vary_is_supported("Cookie"));
}

#[test]
fn revalidated_policy_prefers_304_headers_then_stored_directives() {
    let c = cfg();
    let head = parse_cached_head(
        b"HTTP/1.1 200 OK\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\n\r\n",
    );
    assert_eq!(
        revalidated_policy(&hdrs(&[("cache-control", "max-age=42")]), &head, &c, NOW).ttl,
        Duration::from_secs(42)
    );
    // 304 にヒント無し → 古い Last-Modified からの経験則 (上限 7 日)
    assert_eq!(
        revalidated_policy(&hdrs(&[]), &head, &c, NOW).ttl,
        Duration::from_secs(7 * 86_400)
    );
    let plain = parse_cached_head(b"HTTP/1.1 200 OK\r\nETag: \"e\"\r\n\r\n");
    assert_eq!(
        revalidated_policy(&hdrs(&[]), &plain, &c, NOW).ttl,
        Duration::from_secs(300)
    );
    // 保存済みが no-cache なら、304 が何も言わない限り TTL 0 のまま
    let nc =
        parse_cached_head(b"HTTP/1.1 200 OK\r\nETag: \"e\"\r\nCache-Control: no-cache\r\n\r\n");
    assert_eq!(
        revalidated_policy(&hdrs(&[]), &nc, &c, NOW).ttl,
        Duration::ZERO
    );
    assert!(!nc.may_serve_stale());
    assert_eq!(
        revalidated_policy(&hdrs(&[("cache-control", "max-age=10")]), &nc, &c, NOW).ttl,
        Duration::from_secs(10)
    );
}

/// 304 の `Set-Cookie` は方針に数えない (T29.0)。304 の `Expires` だけでも保存済みの
/// `Cache-Control` は引き継がず、304 が `private` なら保存済みのバリデータで TTL 0 の方針に倒す。
#[test]
fn revalidated_policy_ignores_set_cookie_and_falls_back_on_the_stored_validators() {
    let c = cfg();
    let nc = parse_cached_head(
        b"HTTP/1.1 200 OK\r\nETag: \"e\"\r\nCache-Control: max-age=5, must-revalidate\r\n\r\n",
    );
    // `Set-Cookie` があっても 304 の max-age で延びる
    let p = revalidated_policy(
        &hdrs(&[("set-cookie", "a=b"), ("cache-control", "max-age=42")]),
        &nc,
        &c,
        NOW,
    );
    assert_eq!(p.ttl, Duration::from_secs(42));
    assert!(p.validators);
    // 304 の `private` は `response_policy` が `None` → 保存済みの must_revalidate と
    // バリデータの有無を持った TTL 0
    let p = revalidated_policy(&hdrs(&[("cache-control", "private")]), &nc, &c, NOW);
    assert_eq!(p.ttl, Duration::ZERO);
    assert!(p.validators && p.must_revalidate);
    assert_eq!(p.age, 0);
    // 304 に `Expires` だけがあれば保存済みの `Cache-Control` は引き継がない (`Date` は 304 に無いので今から)
    let p = revalidated_policy(&hdrs(&[("expires", &http_date(NOW + 100))]), &nc, &c, NOW);
    assert_eq!(p.ttl, Duration::from_secs(100));
    assert!(
        !p.must_revalidate,
        "保存済みの must-revalidate は引き継がない"
    );
}

#[test]
fn age_and_date_reduce_remaining_freshness() {
    let c = cfg();
    let p = response_policy(
        200,
        &hdrs(&[("cache-control", "max-age=60"), ("age", "55")]),
        &c,
        NOW,
    )
    .unwrap();
    assert_eq!(p.ttl, Duration::from_secs(60));
    assert_eq!(p.age, 55);
    let date = http_date(NOW - 600);
    let p = response_policy(
        200,
        &hdrs(&[("cache-control", "max-age=900"), ("date", &date)]),
        &c,
        NOW,
    )
    .unwrap();
    assert_eq!(p.age, 600);
    let sm =
        parse_cached_head(b"HTTP/1.1 200 OK\r\nCache-Control: s-maxage=60\r\nETag: \"x\"\r\n\r\n");
    assert!(!sm.may_serve_stale(), "s-maxage forbids stale");
    // 素の LF で終わるヘッダーも解析できる
    let lf = parse_cached_head(b"HTTP/1.1 200 OK\nETag: \"lf\"\n\n");
    assert_eq!(lf.etag.as_deref(), Some("\"lf\""));
}
