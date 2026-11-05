//! `PURGE <url>` を引いた接続元は、`/readers` に URL ではなく `/purge` として残る (T.34.160)。
//!
//! PURGE の要求は自分宛て判定 (`local_path`) を通らず、要求ターゲット (URL) をそのまま持つ。
//! 前はそれが読み手の表の `last_path` に入り、`http://host/path` (他所の URL) が記録に残っていた。
//! 読み手の表は「どの口を読んでいるか」なので、同じ仕事をする `/purge?url=` と同じ `/purge` にする。
//! 別の接続元は送信元アドレスで作る (`connect_from`。`tests/readers_test.rs` と同じ)。

mod common;

use std::io::{Read, Write};
use std::time::Duration;

use common::*;

/// 1 行の `last_path` (`/readers` の `client` の行から)。
fn last_path_of(json: &str, client: &str) -> String {
    let head = format!("{{\"client\":\"{}\"", client);
    let at = json
        .find(&head)
        .unwrap_or_else(|| panic!("{} の行が無い: {}", client, json));
    let row = &json[at..at + json[at..].find('}').unwrap()];
    let pat = "\"last_path\":\"";
    let p = row
        .find(pat)
        .unwrap_or_else(|| panic!("last_path が無い: {}", row))
        + pat.len();
    row[p..p + row[p..].find('"').unwrap()].to_string()
}

/// 送信元を決めて生の要求を 1 本投げ、応答の全文を返す。
#[cfg(target_os = "linux")]
fn send_from(src: [u8; 4], proxy_port: u16, request: &str) -> String {
    let mut stream = connect_from(src, proxy_port);
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = stream.read_to_string(&mut out);
    out
}

#[cfg(target_os = "linux")]
#[test]
fn test_integration_purge_is_recorded_as_the_purge_endpoint_not_the_url() {
    let proxy_port = start_test_proxy(proxy_config());

    let resp = send_from(
        [127, 0, 0, 160],
        proxy_port,
        "PURGE http://example.com/secret/file?token=abc HTTP/1.1\r\nHost: example.com\r\n\
         Connection: close\r\n\r\n",
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{}", resp);
    assert!(
        resp.contains("\"url\":\"http://example.com:80/secret/file?token=abc\""),
        "{}",
        resp
    );

    // 比べる相手: `/purge?url=` を引いた接続元も `/purge` (問い合わせは落とす)
    let get = send_from(
        [127, 0, 0, 161],
        proxy_port,
        &format!(
            "GET /purge?url=http://example.com/x HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
             Connection: close\r\n\r\n",
            proxy_port
        ),
    );
    assert!(get.starts_with("HTTP/1.1 200 "), "{}", get);

    let readers = endpoint_json(proxy_port, "/readers");
    assert_eq!(
        last_path_of(&readers, "127.0.0.160"),
        "/purge",
        "{}",
        readers
    );
    assert_eq!(
        last_path_of(&readers, "127.0.0.161"),
        "/purge",
        "{}",
        readers
    );
    assert!(
        !readers.contains("example.com"),
        "URL が記録に残っている: {}",
        readers
    );
}
