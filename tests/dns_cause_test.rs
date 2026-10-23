//! 名前解決の失敗を `nxdomain` / `dns_temp` に割ることの結合テスト (T25.0)。
//!
//! 2026-10-23 の雪像では `/errors` の 500 件のうち 447 件が `dns` で、`/log` の文言で
//! 割ると `Name or service not known` (`EAI_NONAME`) が大半、`Temporary failure in
//! name resolution` (`EAI_AGAIN`) が 16 件だった。1 つの `dns` にまとまっていると
//! 「名前が無い (引き直しても変わらない)」のか「リゾルバが落ちている (引き直せば
//! 変わりうる)」のかが読めない。
//!
//! ここで見るのは**実際に通した CONNECT が口にどう出るか**: 解決できない名前
//! (`.invalid`) への CONNECT を 1 本送ったあとで、
//!
//! - `/status` のホスト別 `errors_by_cause` が **10 要素**で、先頭 8 つは今までどおり
//!   (`dns` = 1)、9・10 番目 (`nxdomain` / `dns_temp`) のどちらかが 1
//! - `/errors` の `cause` が `nxdomain` / `dns_temp`
//! - `/recent` の `reason` が `error:nxdomain` / `error:dns_temp`
//!
//! どちらになるかは**この機械のリゾルバの答え方で変わる** (DNS の無い環境では
//! `EAI_AGAIN` = `dns_temp`、`.invalid` を NXDOMAIN で返す環境では `nxdomain`) ので、
//! 先に同じ名前を引いて期待する側を決める。判定は実装と同じ [`DnsCause::of_text`] を
//! 使う (試験のために規則を書き写さない)。

use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

mod common;
use common::*;

/// 解決できない名前 (RFC 2606 の `.invalid`)。**キャッシュに当たらないよう**毎回同じでも
/// 1 本しか送らない (`PROXY_DNS_TTL_SECS` の中で 2 本目は答えを覚えている)。
const HOST: &str = "t250b.invalid";

/// CONNECT を 1 本送り、応答の先頭行を返す (接続はすぐ閉じる)。
fn connect_name(proxy_port: u16, host: &str) -> String {
    let mut t = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    t.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    t.write_all(format!("CONNECT {host}:443 HTTP/1.1\r\nHost: {host}:443\r\n\r\n").as_bytes())
        .unwrap();
    read_connect_response(&mut t)
}

/// `/status` のホスト別の行の `errors_by_cause` (10 要素) を数として読む。
fn host_causes(status: &str, key: &str) -> Vec<u64> {
    let pat = format!("{{\"host\":\"{}\",", key);
    let at = status
        .find(&pat)
        .unwrap_or_else(|| panic!("ホスト別の行が無い: {}", key));
    let row = &status[at..];
    let head = "\"errors_by_cause\":[";
    let start = row.find(head).expect("errors_by_cause が無い") + head.len();
    let end = start + row[start..].find(']').expect("errors_by_cause が閉じない");
    row[start..end]
        .split(',')
        .map(|v| v.trim().parse::<u64>().expect("数でない"))
        .collect()
}

/// **受け入れ基準 (T25.0)**: `dns` の内訳が `/status`・`/errors`・`/recent` から読める。
#[test]
fn test_integration_dns_failures_show_their_split_cause() {
    // この機械がこの名前をどう断るかを先に決める (答え方が違えば期待も変わる)
    let want = match (HOST, 443u16).to_socket_addrs() {
        Err(e) => {
            let f = rust_http_proxy::metrics::Failure::from_io(&e);
            match f.dns {
                Some(d) => d,
                None => panic!("`{}` の失敗が DNS にならない: {}", HOST, e),
            }
        }
        Ok(_) => {
            // ワイルドカードで何でも引ける環境。ここで見たいのは DNS の失敗なので、
            // 解決できてしまうなら何も確かめられない (黙って通さず、理由を出す)
            println!(
                "t250b: {} が引けてしまうので DNS の内訳は確かめられない",
                HOST
            );
            return;
        }
    };
    let want_name = want.name();

    let proxy_port = start_test_proxy(proxy_config());
    let head = connect_name(proxy_port, HOST);
    assert!(head.starts_with("HTTP/1.1 502"), "{}", head);

    // 失敗の集計は 502 を返した直後に書かれる (history スレッドは要らない)
    let key = format!("connect://{}:443", HOST);
    let status = status_json(proxy_port);
    let causes = host_causes(&status, &key);
    // **先頭 8 つは今までどおり**、後ろに 2 つ (T25.0)
    assert_eq!(
        causes.len(),
        10,
        "errors_by_cause が 10 要素でない: {:?}",
        causes
    );
    assert_eq!(causes[0], 1, "`dns` は合計のまま: {:?}", causes);
    assert_eq!(
        causes[8] + causes[9],
        1,
        "内訳はどちらか片方だけ (1 本しか送っていない): {:?}",
        causes
    );
    let got = if causes[8] == 1 { 8 } else { 9 };
    assert_eq!(
        causes[got], 1,
        "`{}` の欄が 1 でない: {:?}",
        want_name, causes
    );
    // 内訳の合計は `dns` と一致する
    assert_eq!(causes[0], causes[8] + causes[9], "{:?}", causes);
    println!("t250b: errors_by_cause={:?} (期待は {})", causes, want_name);

    // `/errors` の個票も内訳の名前で出る (合計の `dns` ではない)
    let errors = endpoint_json(proxy_port, "/errors?n=10");
    assert!(
        errors.contains(&format!("\"cause\":\"{}\"", want_name)),
        "{}",
        errors
    );
    assert!(
        errors.contains(&format!("\"target\":\"{}:443\"", HOST)),
        "{}",
        errors
    );
    assert!(
        !errors.contains(&format!("\"target\":\"{}:443\",\"cause\":\"dns\"", HOST)),
        "内訳があるのに `dns` で出ている: {}",
        errors
    );

    // `/recent` の `reason` は接続が閉じたあとに載る (1 本だけなので待つ)
    let reason = format!("\"reason\":\"error:{}\"", want_name);
    let target = format!("\"target\":\"{}:443\"", HOST);
    let mut recent = String::new();
    for _ in 0..500 {
        recent = endpoint_json(proxy_port, "/recent?n=20");
        if recent.contains(&reason) && recent.contains(&target) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(recent.contains(&reason), "{}", recent);
    assert!(recent.contains(&target), "{}", recent);
    println!("t250b: /recent に {} が出た", reason);
}
