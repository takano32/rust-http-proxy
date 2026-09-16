//! CONNECT の最初のバイトから SNI を読む (`PROXY_PEEK_SNI`) の結合テスト (T14.38)。
//!
//! `200 Connection Established` のあと、**最初の中継の前に 1 回だけ** `recv(MSG_PEEK)` で
//! ClientHello を覗き、`server_name` を個票 (`/recent` の `sni`) に残す。バイトは
//! 消費しないので、覗いたあとも中継 (`splice`) はそのまま通る (このテストは echo の
//! 相手に送ったバイトが**そっくり返ってくる**ことで確かめる)。
//!
//! 試験のオリジンを 443 に立てられないので、**設定で内蔵オリジンのポートを 443 扱いに
//! する** (`PROXY_PEEK_SNI=on:<port>` = `Config::peek_sni`)。`cfg(test)` の差し替えでは
//! なく設定なので、実バイナリでも同じ道を通る。
//!
//! 覗くポートの旗は**プロセスに 1 つきり**なので、同じプロセスで動くテストは
//! [`SERIAL`] の鍵で 1 つずつ通す。
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::Duration;

mod common;
use common::*;

/// 覗くポートの旗はプロセスに 1 つなので、テストは 1 つずつ通す。
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// 手書きの ClientHello (`server_name` 拡張 1 つだけ)。
///
/// TLS record (`0x16`) → handshake (`0x01`) → extensions → `server_name` (`0x0000`)。
fn client_hello(name: &str) -> Vec<u8> {
    let mut entry = vec![0x00];
    entry.extend_from_slice(&(name.len() as u16).to_be_bytes());
    entry.extend_from_slice(name.as_bytes());
    let mut list = (entry.len() as u16).to_be_bytes().to_vec();
    list.extend_from_slice(&entry);
    let mut ext = vec![0x00, 0x00];
    ext.extend_from_slice(&(list.len() as u16).to_be_bytes());
    ext.extend_from_slice(&list);

    let mut body = vec![0x03, 0x03];
    body.extend_from_slice(&[0x11; 32]);
    body.push(0);
    body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]);
    body.extend_from_slice(&[0x01, 0x00]);
    body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    body.extend_from_slice(&ext);

    let mut hs = vec![0x01];
    hs.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    hs.extend_from_slice(&body);

    let mut rec = vec![0x16, 0x03, 0x01];
    rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
    rec.extend_from_slice(&hs);
    rec
}

/// CONNECT を 1 本張り、ClientHello を送って echo が返るのを見てから閉じる。
///
/// 返ってきたバイトが送ったものと同じであること = **覗いてもバイトを消費していない**。
fn tunnel_with_client_hello(proxy_port: u16, target: &str, sni: &str) {
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n", target, target).as_bytes())
        .unwrap();
    let head = read_connect_response(&mut s);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);

    let hello = client_hello(sni);
    s.write_all(&hello).unwrap();
    let mut back = vec![0u8; hello.len()];
    s.read_exact(&mut back).unwrap();
    assert_eq!(back, hello, "覗いたバイトを消費している");
    drop(s);
}

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

/// 受け入れ基準 (1): `PROXY_PEEK_SNI=on:<内蔵オリジンのポート>` のとき、CONNECT の
/// あとに送った ClientHello の SNI が個票に出て、CONNECT のホストと違えば
/// `sni_mismatches` が +1 されること。
#[test]
fn the_sni_of_the_first_bytes_lands_in_the_record_and_counts_the_mismatch() {
    let _g = serial();
    let echo_port = start_echo_server();
    let mut cfg = proxy_config();
    // 試験のオリジンを 443 扱いにする (`PROXY_PEEK_SNI=on:<port>`)
    cfg.peek_sni = Some(echo_port);
    let proxy_port = start_test_proxy(cfg);
    let target = format!("127.0.0.1:{}", echo_port);

    // (a) CONNECT のホスト (IP リテラル) と SNI が違う = 食い違い 1 件
    tunnel_with_client_hello(proxy_port, &target, "example.test");
    // (b) SNI が CONNECT のホストと同じなら食い違いは増えない
    tunnel_with_client_hello(proxy_port, &target, "127.0.0.1");

    wait_until(
        || {
            endpoint_json(proxy_port, "/recent?n=50")
                .matches("\"kind\":\"connect\"")
                .count()
                == 2
        },
        "/recent に CONNECT が 2 本",
    );
    let body = endpoint_json(proxy_port, "/recent?n=50");
    let mismatched = row_with(&body, "recent", "\"sni\":\"example.test\"");
    assert!(
        mismatched.contains(&format!("\"target\":\"{}\"", target)),
        "宛先が違う: {}",
        mismatched
    );
    assert!(
        body.contains("\"sni\":\"127.0.0.1\""),
        "同じ名前の 1 本が無い: {}",
        body
    );

    // 食い違いは (a) の 1 本だけ (`/status` の合計と、ホスト別の欄)
    let status = status_json(proxy_port);
    assert_eq!(status_number(&status, "sni_mismatches"), 1, "{}", status);
    let hosts = endpoint_json(proxy_port, "/hosts?limit=50");
    let host = row_with(&hosts, "hosts", &format!("connect://{}", target));
    assert!(host.contains("\"sni_mismatch\":1"), "{}", host);
}

/// 受け入れ基準 (2): `off` なら覗かないので個票の `sni` は `null` のままで、
/// 食い違いも数えないこと。
#[test]
fn off_never_peeks() {
    let _g = serial();
    let echo_port = start_echo_server();
    let mut cfg = proxy_config();
    cfg.peek_sni = None;
    let proxy_port = start_test_proxy(cfg);

    tunnel_with_client_hello(
        proxy_port,
        &format!("127.0.0.1:{}", echo_port),
        "example.test",
    );
    wait_until(
        || endpoint_json(proxy_port, "/recent?n=50").contains("\"kind\":\"connect\""),
        "/recent に CONNECT が 1 本",
    );
    let body = endpoint_json(proxy_port, "/recent?n=50");
    assert!(body.contains("\"sni\":null"), "覗いている: {}", body);
    assert!(!body.contains("example.test"), "覗いている: {}", body);
    let status = status_json(proxy_port);
    assert_eq!(status_number(&status, "sni_mismatches"), 0, "{}", status);
}

/// 443 以外のポートでは覗かないこと (既定の `on` = 443 だけ。TLS とは限らないため)。
#[test]
fn other_ports_are_not_peeked() {
    let _g = serial();
    let echo_port = start_echo_server();
    let cfg = proxy_config(); // 既定 = `on` (443 だけ)
    assert_eq!(cfg.peek_sni, Some(443), "既定は on");
    let proxy_port = start_test_proxy(cfg);

    tunnel_with_client_hello(
        proxy_port,
        &format!("127.0.0.1:{}", echo_port),
        "example.test",
    );
    wait_until(
        || endpoint_json(proxy_port, "/recent?n=50").contains("\"kind\":\"connect\""),
        "/recent に CONNECT が 1 本",
    );
    let body = endpoint_json(proxy_port, "/recent?n=50");
    assert!(
        body.contains("\"sni\":null"),
        "443 以外で覗いている: {}",
        body
    );
    assert_eq!(status_number(&status_json(proxy_port), "sni_mismatches"), 0);
}

/// `/config` に効いている値が出ること (`off` / `on` / `on:<port>` の 3 通り)。
#[test]
fn the_effective_setting_is_visible() {
    let mut cfg = proxy_config();
    assert_eq!(cfg.peek_sni_spec(), "on");
    cfg.peek_sni = None;
    assert_eq!(cfg.peek_sni_spec(), "off");
    cfg.peek_sni = Some(9443);
    assert_eq!(cfg.peek_sni_spec(), "on:9443");
    let found = cfg
        .settings()
        .into_iter()
        .find(|s| s.key == "PROXY_PEEK_SNI")
        .expect("PROXY_PEEK_SNI が /config に無い");
    assert_eq!(found.value, "\"on:9443\"");
}
