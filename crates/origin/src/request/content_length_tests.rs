use super::*;

fn parse(lines: &[&str]) -> RequestHeaders {
    let raw: Vec<String> = lines.iter().map(|l| format!("{}\r\n", l)).collect();
    parse_request_headers(&raw, 0)
}

/// `Expect` の読み方 (T.28.3): `100-continue` は大小無視、それ以外が 1 つでもあれば `expect_unknown`。
#[test]
fn expect_continue_and_unknown_expectations() {
    let h = parse(&["Expect: 100-continue"]);
    assert!(h.expect_continue && !h.expect_unknown);
    let h = parse(&["expect: 100-Continue"]);
    assert!(h.expect_continue && !h.expect_unknown);
    let h = parse(&["Expect: 100-continue, foo"]);
    assert!(h.expect_continue && h.expect_unknown);
    let h = parse(&["Expect: fancy"]);
    assert!(!h.expect_continue && h.expect_unknown);
    // 空の値と `Expect` なしはどちらも立たない
    for lines in [&["Expect: "][..], &["Accept: */*"][..]] {
        let h = parse(lines);
        assert!(!h.expect_continue && !h.expect_unknown, "{:?}", lines);
    }
}

/// 受け付ける形 (数字だけ・u64 に収まる・重複は同値なら畳む)。
#[test]
fn accepts_plain_decimal_and_collapses_equal_duplicates() {
    for (value, want) in [
        ("0", 0u64),
        ("5", 5),
        ("0007", 7),
        ("18446744073709551615", u64::MAX),
    ] {
        let h = parse(&[&format!("Content-Length: {}", value)]);
        assert!(!h.content_length_invalid, "value={:?}", value);
        assert_eq!(h.content_length, Some(want), "value={:?}", value);
    }
    let h = parse(&["Content-Length: 5", "Content-Length: 5"]);
    assert!(!h.content_length_invalid);
    assert_eq!(h.content_length, Some(5));
    // 大小の違う綴りでも同じ値なら同じ (名前も値も大小は問わない)
    let h = parse(&["content-length: 5", "CONTENT-LENGTH: 05"]);
    assert!(!h.content_length_invalid);
    assert_eq!(h.content_length, Some(5));
}

/// 断る形 (数字でない・負・記号つき・カンマ・空・u64 に収まらない桁数・重複で不一致)。
#[test]
fn rejects_everything_that_is_not_one_plain_decimal() {
    for value in [
        "-1",
        "+5",
        "5, 5",
        "",
        "0x5",
        "5.",
        "1e3",
        "٥",                    // 非 ASCII の数字
        "99999999999999999999", // 21 桁
        "18446744073709551616", // u64::MAX + 1
    ] {
        let h = parse(&[&format!("Content-Length: {}", value)]);
        assert!(h.content_length_invalid, "value={:?} を通した", value);
    }
    // 前後の OWS は値の側で落とすので `5 ` / ` 5` は 5 として通る (今までどおり)
    let h = parse(&["Content-Length:  5 "]);
    assert!(!h.content_length_invalid);
    assert_eq!(h.content_length, Some(5));
    // 空の値 (`Content-Length:`) も断る
    let h = parse(&["Content-Length:"]);
    assert!(h.content_length_invalid);

    // 重複で不一致
    let h = parse(&["Content-Length: 5", "Content-Length: 6"]);
    assert!(h.content_length_invalid);
    // 1 本でも壊れていれば断る (後ろに正しい値があっても通さない)
    let h = parse(&["Content-Length: x", "Content-Length: 6"]);
    assert!(h.content_length_invalid);
    let h = parse(&["Content-Length: 6", "Content-Length: x"]);
    assert!(h.content_length_invalid);
}

/// CL の無い要求・他のヘッダーだけの要求は今までどおり (誤検出しない)。
/// T27.6: `Transfer-Encoding` は最後がちょうど 1 回の `chunked` のときだけ chunked。ほかは `te_invalid`。
#[test]
fn transfer_encoding_must_end_in_one_chunked() {
    for lines in [
        &["Transfer-Encoding: chunked\r\n"][..],
        &["Transfer-Encoding: Chunked\r\n"],
        &["Transfer-Encoding: gzip, chunked\r\n"],
        &[
            "Transfer-Encoding: gzip\r\n",
            "Transfer-Encoding: chunked\r\n",
        ],
        &["Transfer-Encoding: chunked,\r\n"],
    ] {
        let h = parse(lines);
        assert!(h.te_present && h.chunked && !h.te_invalid, "{:?}", lines);
    }
    for lines in [
        &["Transfer-Encoding: xchunked\r\n"][..],
        &["Transfer-Encoding: gzip\r\n"],
        &["Transfer-Encoding: chunked, identity\r\n"],
        &["Transfer-Encoding: chunked, chunked\r\n"],
        &[
            "Transfer-Encoding: chunked\r\n",
            "Transfer-Encoding: chunked\r\n",
        ],
        &["Transfer-Encoding: chunked;x=1\r\n"],
        &["Transfer-Encoding: \r\n"],
    ] {
        let h = parse(lines);
        assert!(h.te_present && !h.chunked && h.te_invalid, "{:?}", lines);
    }
    let h = parse(&["Content-Length: 5\r\n"]);
    assert!(!h.te_present && !h.chunked && !h.te_invalid);
}

/// T27.5: 名前とコロンの間の空白と、値の途中の CR / LF / NUL に印が立つ。行末の CRLF と値の HTAB には立たない。
#[test]
fn flags_whitespace_before_colon_and_cr_lf_nul_in_values() {
    for line in [
        "Content-Length : 5\r\n",
        "X-A\t: 1\r\n",
        "X-A: a\rTransfer-Encoding: chunked\r\n",
        "X-A: a\0b\r\n",
        "X-A: a\nb\r\n",
    ] {
        assert!(parse(&[line]).bad_header, "line={:?} を通した", line);
    }
    for line in ["X-A: a\tb\r\n", "X-A: 1\n", "X-A:1", "X-A:  spaced  \r\n"] {
        assert!(!parse(&[line]).bad_header, "line={:?} を断った", line);
    }
}

#[test]
fn leaves_requests_without_content_length_alone() {
    let h = parse(&["Host: example.com", "Accept: */*"]);
    assert!(!h.content_length_invalid);
    assert_eq!(h.content_length, None);
    // `Transfer-Encoding: chunked` 単独もここでは断らない (400 は http 側の判定)
    let h = parse(&["Transfer-Encoding: chunked"]);
    assert!(!h.content_length_invalid);
    assert!(h.chunked);
    assert_eq!(h.content_length, None);
}

/// 使う欄が全部埋まること (名前の大小は問わず、`Connection` と `Proxy-Connection` は同じ扱い。
/// `Cache-Control` と `Pragma` は小文字にして `,` でつなぐ)。
#[test]
fn every_used_field_is_picked_up() {
    let h = parse(&[
        "Host: h.example:8080",
        "AUTHORIZATION: Basic x",
        "Cookie: a=b",
        "Cache-Control: NO-CACHE",
        "Pragma: no-cache",
        "If-None-Match: \"e\"",
        "If-Modified-Since: Mon, 01 Jan 2024 00:00:00 GMT",
        "If-Range: \"e\"",
        "Range: bytes=0-1",
        "Accept-Encoding: gzip",
        "Proxy-Connection: Keep-Alive",
        "Connection: close",
        "Upgrade: websocket",
        "X-Other: ignored",
    ]);
    assert_eq!(h.host.as_deref(), Some("h.example:8080"));
    assert!(!h.dup_host);
    assert!(h.authorization && h.cookie && h.upgrade);
    assert_eq!(h.cache_control, "no-cache,no-cache");
    assert_eq!(h.if_none_match.as_deref(), Some("\"e\""));
    assert_eq!(
        h.if_modified_since.as_deref(),
        Some("Mon, 01 Jan 2024 00:00:00 GMT")
    );
    assert_eq!(h.if_range.as_deref(), Some("\"e\""));
    assert_eq!(h.range.as_deref(), Some("bytes=0-1"));
    assert_eq!(h.accept_encoding.as_deref(), Some("gzip"));
    assert!(h.connection_keep_alive && h.connection_close);
    assert!(!h.obs_fold && !h.bad_header);
    // 2 本目の `Host` は印だけで、値は最初の 1 本のまま。折り返しの行は印
    let h = parse(&["Host: a", "Host: b", " folded"]);
    assert!(h.dup_host && h.obs_fold);
    assert_eq!(h.host.as_deref(), Some("a"));
}
