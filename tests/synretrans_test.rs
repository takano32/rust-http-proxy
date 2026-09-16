//! 接続確立時の SYN の再送を数える (T14.46) の結合テスト。
//!
//! `crates/net` の確立点で `getsockopt(TCP_INFO)` を **1 回**だけ読み、確立直後の
//! `tcpi_total_retrans` (= SYN の再送回数) を個票 (`/recent` の `syn_retrans`)、
//! ホスト別 (`/hosts` の `syn_retrans`)、全体 (`/status` の `syn_retrans_total`) に出す。
//!
//! loopback では SYN が落ちないので**どれも 0 のまま**で、「欄があること」と
//! 「0 のときに 0 と出ること」を縛る (再送が 1 以上になる条件は
//! `scripts/deployed-like.sh` の中の `--only connect-multi` でしか作れない)。
#![cfg(target_os = "linux")]

use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

mod common;
use common::*;

/// `"<key>":[ ... ]` を 1 件ずつに割る (`ms` や `rtt_ms` が入れ子なので括弧を数える)。
fn rows(body: &str, key: &str) -> Vec<String> {
    let head = format!("\"{}\":[", key);
    let at = body
        .find(&head)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, body))
        + head.len();
    let rest = &body[at..];
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    out.push(rest[start..=i].to_string());
                }
            }
            ']' if depth == 0 => break,
            _ => {}
        }
    }
    out
}

/// `needle` を含む 1 件 (無ければ落ちる)。
fn row_with(body: &str, key: &str, needle: &str) -> String {
    rows(body, key)
        .into_iter()
        .find(|r| r.contains(needle))
        .unwrap_or_else(|| panic!("{} が無い: {}", needle, body))
}

/// CONNECT を 1 本張って、少しだけ流してから閉じる。
fn tunnel_once(proxy_port: u16, target: &str) {
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n", target, target).as_bytes())
        .unwrap();
    let head = read_connect_response(&mut s);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    drop(s);
}

/// 受け入れ基準: loopback の CONNECT では個票の `syn_retrans` が 0、
/// ホスト別も 0、`/status` の `syn_retrans_total` が 0 で**欄がある**こと。
#[test]
fn a_loopback_connect_records_zero_syn_retransmissions() {
    let echo_port = start_echo_server();
    let proxy_port = start_test_proxy(proxy_config());
    let target = format!("127.0.0.1:{}", echo_port);

    tunnel_once(proxy_port, &target);

    wait_until(
        || endpoint_json(proxy_port, "/recent?n=50").contains("\"kind\":\"connect\""),
        "/recent に CONNECT が 1 本",
    );
    let body = endpoint_json(proxy_port, "/recent?n=50");
    let entry = row_with(&body, "recent", &format!("\"target\":\"{}\"", target));
    assert!(
        entry.contains("\"syn_retrans\":0"),
        "loopback で SYN の再送が出ている: {}",
        entry
    );

    // ホスト別 (`/hosts`) と全体 (`/status`) の欄。どちらもメモリだけの欄
    let hosts = endpoint_json(proxy_port, "/hosts?limit=50");
    let host = row_with(&hosts, "hosts", &format!("connect://{}", target));
    assert!(host.contains("\"syn_retrans\":0"), "{}", host);

    let status = status_json(proxy_port);
    assert!(
        status.contains("\"syn_retrans_total\":"),
        "`/status` に欄が無い: {}",
        status
    );
    assert_eq!(status_number(&status, "syn_retrans_total"), 0, "{}", status);
}

/// forward (http) の個票にも欄があり、loopback では 0 のままであること。
///
/// forward が確立点を通るのは**プールが接続を張るときだけ**なので、要求を 2 本
/// 送っても値は 0 のまま (使い回した 2 本目は `getsockopt` を 1 度も呼ばない)。
#[test]
fn forwarded_requests_carry_the_field_too() {
    let (origin_port, _origin) = start_mock_origin();
    let proxy_port = start_test_proxy(proxy_config());
    let url = format!("http://127.0.0.1:{}/", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    for _ in 0..2 {
        let resp = get_via_proxy(proxy_port, &url, &host);
        assert!(resp.starts_with("HTTP/1.1 200"), "{}", resp);
    }

    wait_until(
        || endpoint_json(proxy_port, "/recent?n=50").contains("\"kind\":\"http\""),
        "/recent に http が 1 本",
    );
    let body = endpoint_json(proxy_port, "/recent?n=50");
    let entry = row_with(&body, "recent", "\"kind\":\"http\"");
    assert!(
        entry.contains("\"syn_retrans\":0"),
        "loopback で SYN の再送が出ている: {}",
        entry
    );
    assert_eq!(
        status_number(&status_json(proxy_port), "syn_retrans_total"),
        0
    );
}
