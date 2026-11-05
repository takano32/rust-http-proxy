//! 自分宛ての要求 (ダッシュボード・オリジン形式・`Via` の印) がループしないことの結合テスト。
//!
//! 初めは `tests/proxy_test.rs` にあった (T.34.116 で割った)。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

mod common;
use common::*;

#[test]
fn test_integration_dashboard_and_self_addressed_requests() {
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-dashboard"));
    let me = format!("127.0.0.1:{}", proxy_port);
    // ブラウザがプロキシ経由で自分自身の /status を開いたとき (絶対形式) も自分宛てとして応答する
    let r = get_via_proxy(proxy_port, &format!("http://{}/status", me), &me);
    assert!(r.starts_with("HTTP/1.1 200 OK"), "{}", r);
    assert!(r.contains("\"status\":\"ok\""), "{}", r);
    let r = get_via_proxy(proxy_port, &format!("http://{}/dashboard", me), &me);
    assert!(
        r.starts_with("HTTP/1.1 200 OK") && r.contains("text/html"),
        "{}",
        r
    );
    assert!(r.contains("<canvas"), "dashboard html");
    let r = get_via_proxy(proxy_port, &format!("http://{}/history", me), &me);
    assert!(
        r.starts_with("HTTP/1.1 200 OK") && r.contains("\"samples\":["),
        "{}",
        r
    );
    // PAC は自分の名前 (Host / authority) とポートでプロキシを指す
    let r = get_via_proxy(proxy_port, &format!("http://{}/proxy.pac", me), &me);
    assert!(
        r.starts_with("HTTP/1.1 200 OK")
            && r.contains("application/x-ns-proxy-autoconfig")
            && r.contains(&format!("PROXY {}; DIRECT", me)),
        "{}",
        r
    );
    // 履歴は解像度を選べる
    let r = get_via_proxy(proxy_port, &format!("http://{}/history?res=60", me), &me);
    assert!(r.contains("\"interval_secs\":60,"), "{}", r);
    // ブロックリストの判定と手動の上書き
    let r = get_via_proxy(
        proxy_port,
        &format!("http://{}/blocklist?host=ads.test.invalid&action=block", me),
        &me,
    );
    assert!(
        r.contains("\"blocked\":true,\"verdict\":\"override:block\""),
        "{}",
        r
    );
    let r = get_via_proxy(
        proxy_port,
        "http://sub.ads.test.invalid/x",
        "sub.ads.test.invalid",
    );
    assert!(r.starts_with("HTTP/1.1 403"), "{}", r);
    let r = get_via_proxy(
        proxy_port,
        &format!("http://{}/blocklist?host=ads.test.invalid&action=clear", me),
        &me,
    );
    assert!(r.contains("\"verdict\":\"clear\""), "{}", r);
    // 自分宛てで知らないパスは転送せず 404
    let r = get_via_proxy(proxy_port, &format!("http://{}/nope", me), &me);
    assert!(r.starts_with("HTTP/1.1 404"), "{}", r);
}

// --- T12.3: 自分宛てのオリジン形式でループしない ---------------------------------

/// 待ち受けアドレス:ポートを `Host` にしたオリジン形式の知らないパスは、その場で 404。
/// 直す前は `Host` 宛て = 自分自身へ転送して `max_conns` 本つないでいた (手元で 33 本)。
#[test]
fn test_integration_self_addressed_origin_form_does_not_loop() {
    let proxy_port = start_test_proxy(proxy_config());
    assert_eq!(status_number(&status_json(proxy_port), "new"), 0);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let req = format!(
        "GET /x HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        proxy_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    assert!(
        response.starts_with("HTTP/1.1 404 Not Found"),
        "{}",
        response
    );
    // 自分へ転送していない証拠は**時間ではなく接続の数で見る** (T20.2)。前は「要求 1 本の往復が
    // 10 ms 未満」で見ていて、混んだ機械ではスレッドを起こすだけで超えた。自分へ 1 本でも
    // つなげば、それはオリジンへの新しい接続として `new` に数えられる (自分の待ち受けでも同じ。
    // `test_integration_via_mark_stops_a_loop_between_two_listeners` の 1 段目 → 2 段目が 1)
    let status = status_json(proxy_port);
    assert_eq!(
        status_number(&status, "new"),
        0,
        "オリジンへの接続は 1 本も張らない: {}",
        status
    );
    // 残っている接続が無いこと (ループしていれば自分宛ての接続が積み上がったまま残る)。
    // **閉じ終わるのを待ってから数える**: プロキシはソケットを閉じたあとで
    // `active_connections` を引く (`Conn` の欄の順でソケットが先に落ちる) ので、こちらが
    // EOF を読んだ時点ではまだ引かれていないことがある (上の要求の 1 本も、その前の
    // `/status` の 1 本も)。混んだ機械ではその隙間に次の `/status` が入って 2 や 3 に見えた
    wait_until(
        || status_number(&status_json(proxy_port), "active_connections") <= 1,
        "/status を取っているこの 1 本だけになる",
    );
    // 待っている間にも増えていない
    let status = status_json(proxy_port);
    assert_eq!(
        status_number(&status, "new"),
        0,
        "オリジンへの接続は 1 本も張らない: {}",
        status
    );
}

/// 自分宛ての `/` は 200 でエンドポイントの一覧 (ブラウザで開いた人への案内)。
#[test]
fn test_integration_self_addressed_root_lists_endpoints() {
    let proxy_port = start_test_proxy(proxy_config());
    let response = raw_get(
        proxy_port,
        &format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            proxy_port
        ),
    );
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{}", response);
    assert!(
        response.contains("Content-Type: text/plain; charset=utf-8"),
        "{}",
        response
    );
    assert!(response.contains("/dashboard"), "{}", response);
    assert!(response.contains("/proxy.pac"), "{}", response);

    // lite でも一覧は出す (ただし持っていない /dashboard は載せない)
    let mut lite = proxy_config();
    lite.lite = true;
    let lite_port = start_test_proxy(lite);
    let response = raw_get(
        lite_port,
        &format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            lite_port
        ),
    );
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{}", response);
    assert!(response.contains("/status"), "{}", response);
    assert!(!response.contains("/dashboard"), "{}", response);
}

/// ポートの違う自分へ回った要求は、2 段目が自分の `Via` の印を見て 508 で閉じる。
/// (待ち受けを 2 つ立てる = 同じプロセス = 同じ印。別プロセスの 2 段重ねは誤検出しない)
#[test]
fn test_integration_via_mark_stops_a_loop_between_two_listeners() {
    let first = start_test_proxy(proxy_config());
    let second = start_test_proxy(proxy_config());

    // 1 段目は `Host` のポートが自分と違うので転送する → 2 段目が印を見つけて 508
    let response = raw_get(
        first,
        &format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            second
        ),
    );
    assert!(
        response.starts_with("HTTP/1.1 508 Loop Detected"),
        "{}",
        response
    );

    // 使った接続は client→1 段目 と 1 段目→2 段目 の 2 本だけ
    assert_eq!(status_number(&status_json(first), "new"), 1);
    assert_eq!(
        status_number(&status_json(second), "new"),
        0,
        "2 段目は転送していない"
    );

    // 別プロセスの印 (違う 8 桁) は誤検出しない
    let response = raw_get(
        second,
        &format!(
            "GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nVia: 1.1 rust-http-proxy/deadbeef\r\nConnection: close\r\n\r\n",
            second
        ),
    );
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{}", response);
}
