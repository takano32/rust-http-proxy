//! 個票 (`/recent`) の段の値が 0 に潰れないこと (T15.0 (1)) の結合テスト。
//!
//! `detail_of` は接続の段を「全体 − 名前解決」で出すが、`PROXY_ALLOW_LOCAL=false`
//! (既定) では入口の ACL が `open()` の時計より**前に**名前を引くので、その費用を
//! 引くと `connect` が 0 に落ちる (デプロイ先の実測: ミスした個票 54 本のうち 40 本)。
//!
//! **ACL の枝そのものは結合テストでは再現できない** (`acl::is_local_ip` は loopback を
//! local と見るので `localhost` 宛ては 403、IP リテラル宛ては名前を引かずに早期 return
//! する)。引き算の中身は単体テスト (`crates/tunnel/src/tunnel.rs` の `mod tests` と
//! `crates/net-dns/src/dns.rs` の `peeking_the_resolve_cost_does_not_take_it`) で見ているので、
//! ここで縛るのは**個票に `connect` と `client_read` の欄が出ること**だけ。
//!
//! `client_read` は 0 のとき JSON に出さない決まり (`STAGE_NAMES`) なので、要求行を
//! 送ってから `Host` を送るまでを空けて、0 でない値を作ってから読む。

use std::io::Write;
use std::net::TcpStream;
use std::thread;
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

/// `"key":<数>` を読む (整数だけ)。
fn num(obj: &str, key: &str) -> u64 {
    let pat = format!("\"{}\":", key);
    let at = obj
        .find(&pat)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, obj))
        + pat.len();
    obj[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{} が数でない: {}", key, obj))
}

/// 要求行と `Host` の間を空けて CONNECT を 1 本張る (`client_read` を 0 でなくする)。
fn slow_tunnel(proxy_port: u16, target: &str, gap: Duration) {
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(format!("CONNECT {} HTTP/1.1\r\n", target).as_bytes())
        .unwrap();
    s.flush().unwrap();
    thread::sleep(gap);
    s.write_all(format!("Host: {}\r\n\r\n", target).as_bytes())
        .unwrap();
    let head = read_connect_response(&mut s);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    drop(s);
}

/// 受け入れ基準: 個票の `ms` に `connect` と `client_read` があり、どちらも
/// 「起きたこと」と矛盾しない値であること (loopback なので `dns` は 0)。
#[test]
fn a_tunnel_records_both_the_connect_and_the_client_read_stage() {
    // 段階の窓と同じ旗 (`--lite` では時計を読まない。T14.3)
    rust_http_proxy::profile::set_enabled(true);

    let echo_port = start_echo_server();
    let proxy_port = start_test_proxy(proxy_config());
    let target = format!("127.0.0.1:{}", echo_port);

    slow_tunnel(proxy_port, &target, Duration::from_millis(40));

    wait_until(
        || endpoint_json(proxy_port, "/recent?n=50").contains("\"kind\":\"connect\""),
        "/recent に CONNECT が 1 本",
    );
    let body = endpoint_json(proxy_port, "/recent?n=50");
    let entry = row_with(&body, "recent", &format!("\"target\":\"{}\"", target));

    // `dns` と `connect` は 0 でも必ず出る欄 (`STAGE_NAMES`)
    assert!(entry.contains("\"dns\":"), "`dns` の欄が無い: {}", entry);
    assert!(
        entry.contains("\"connect\":"),
        "`connect` の欄が無い: {}",
        entry
    );
    // 要求行と `Host` の間を 40 ms 空けたので、この段は 0 でない = JSON に出る
    assert!(
        entry.contains("\"client_read\":"),
        "`client_read` の欄が無い: {}",
        entry
    );
    assert!(
        num(&entry, "client_read") >= 30,
        "空けた 40 ms が入っていない: {}",
        entry
    );
    // loopback は名前を引かないので、引き算に使う値そのものが 0
    assert_eq!(num(&entry, "dns"), 0, "{}", entry);
    // 「`dns` が 0 より大きいのに `connect` が 0」が出ないこと (受け入れ基準)
    assert!(
        num(&entry, "dns") == 0 || num(&entry, "connect") > 0,
        "名前解決を払ったのに接続の段が 0: {}",
        entry
    );
}
