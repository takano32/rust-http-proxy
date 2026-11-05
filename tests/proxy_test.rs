//! 中継そのものの結合テスト (転送・オリジン接続のプール・要求と応答の本文・付け足すヘッダー)。
//!
//! 自分宛ての口は `proxy_endpoints_test.rs`、ACL と上限は `proxy_acl_test.rs`、壊れた要求と
//! 枠組みは `proxy_framing_test.rs`、自分宛てのループは `proxy_loop_test.rs` に割った (T34.116)。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::*;

#[test]
fn test_integration_http_forwarding() {
    let (origin_port, _origin_handle) = start_mock_origin();
    let proxy_port = start_test_proxy(proxy_config());

    // Send HTTP proxy request without any authentication
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!(
        "GET http://127.0.0.1:{}/test HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        origin_port, origin_port
    );
    stream.write_all(req.as_bytes()).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("hello from mock origin"));
}

#[test]
fn test_integration_origin_connections_are_pooled() {
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        start_keepalive_origin(Arc::clone(&connections), Arc::clone(&requests));
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-pool"));
    let host = format!("127.0.0.1:{}", origin_port);

    for i in 0..4 {
        let resp = get_via_proxy(proxy_port, &format!("http://{}/p{}", host, i), &host);
        assert!(resp.contains(&format!("response #{}", i + 1)), "{}", resp);
    }
    assert_eq!(requests.load(Ordering::SeqCst), 4);
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "one pooled origin connection"
    );

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .write_all(
            format!(
                "GET /status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                proxy_port
            )
            .as_bytes(),
        )
        .unwrap();
    let mut status = String::new();
    stream.read_to_string(&mut status).unwrap();
    assert!(
        status
            .contains("\"origin_connections\":{\"new\":1,\"reused\":3,\"pool_hit_ratio\":0.7500}"),
        "{}",
        status
    );
}

#[test]
fn test_integration_request_body_on_a_reused_connection() {
    // 同じ接続の 2 本目以降で本文付きの要求を送る。要求行とヘッダーは keep-alive の
    // アイドル時間で待つようにしたので、本文を読む前にタイムアウトが戻ることの確認
    //
    // **オリジンは `Connection: close` を付けて返す** (T18.4)。`start_origin` は 1 接続で
    // 1 要求だけ答えて閉じるので、付けないとプロキシはその接続をプールに戻す。ふだんは
    // 次の要求の前に FIN が届いて生存確認 (`Pool::get` の peek) が捨てるが、機械が混んで
    // オリジンのスレッドが「応答を書いた」と「閉じる」の間で止まると、2 本目の POST が
    // まだ生きて見える接続に乗り、直後の RST で 502 になる (本文付きの要求は送り直さない
    // 決まり。全体テストの負荷の下で 1 回、6 本並列で 3 / 48 回)。ここで見たいのは
    // **クライアント側の**接続の 2 本目以降なので、オリジン側は毎回張り直させる
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(|req, _n| {
            let body = req.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
                 Connection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    );
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    // 1 本目: 本文なし
    let req = format!(
        "GET http://{}/first HTTP/1.1\r\nHost: {}\r\n\r\n",
        host, host
    );
    stream.write_all(req.as_bytes()).unwrap();
    read_response(&mut stream);

    // 2 本目: 本文あり (オリジンは受け取った本文をそのまま返す)
    let payload = "name=value&x=1";
    let req = format!(
        "POST http://{}/second HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\n\r\n{}",
        host,
        host,
        payload.len(),
        payload
    );
    stream.write_all(req.as_bytes()).unwrap();
    let (head, body) = read_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(String::from_utf8_lossy(&body), payload, "本文が転送される");

    // 3 本目: 本文なしに戻っても続く
    let req = format!(
        "GET http://{}/third HTTP/1.1\r\nHost: {}\r\n\r\n",
        host, host
    );
    stream.write_all(req.as_bytes()).unwrap();
    let (head, _) = read_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
}

#[test]
fn test_integration_forwarded_headers_are_added_on_every_request() {
    // 接続元 IP は接続ごとに 1 回だけ文字列にして使い回す。同じ接続の 2 本目以降でも
    // X-Forwarded-For と Via がオリジンに届くこと (使い回しを壊すと 2 本目で消える)
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(|req, _n| {
            // 受け取った要求の先頭をそのまま本文にして返す
            let head = req.split("\r\n\r\n").next().unwrap_or("").to_string();
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n{}",
                head.len(),
                head
            )
            .into_bytes()
        }),
    );
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    for path in ["/first", "/second"] {
        let req = format!(
            "GET http://{}{} HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, path, host
        );
        stream.write_all(req.as_bytes()).unwrap();
        let (head, body) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
        let seen = String::from_utf8_lossy(&body).to_string();
        assert!(
            seen.contains("X-Forwarded-For: 127.0.0.1\r\n"),
            "{}: {}",
            path,
            seen
        );
        // Via はプロキシが最後に足すので、エコーされた先頭では行末の CRLF が付かない。
        // 印は起動ごとの 8 桁 16 進 (T12.3)
        assert!(
            seen.ends_with(&format!("Via: {}", rust_http_proxy::via::token())),
            "{}: {}",
            path,
            seen
        );
        assert!(
            seen.contains(&format!("Host: {}\r\n", host)),
            "{}: {}",
            path,
            seen
        );
    }
}

#[test]
fn test_integration_interim_100_continue_is_not_forwarded() {
    // オリジンが 100 Continue を先に送っても、クライアントには本物の応答が届く
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(|_req, _n| {
            let body = "created";
            format!(
                "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Length: {}\r\n\
                 Cache-Control: no-store\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    );
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let payload = "x=1";
    let req = format!(
        "POST http://{}/create HTTP/1.1\r\nHost: {}\r\nExpect: 100-continue\r\n\
         Content-Length: {}\r\n\r\n{}",
        host,
        host,
        payload.len(),
        payload
    );
    stream.write_all(req.as_bytes()).unwrap();
    // T28.3 から、本文つきの `Expect: 100-continue` にはプロキシが自分で 100 を 1 回返す。
    // オリジンの 100 は今までどおり中継しないので、100 は 1 回だけで、次が本物の応答
    let (interim, _) = read_response(&mut stream);
    assert_eq!(interim, "HTTP/1.1 100 Continue\r\n\r\n");
    let (head, body) = read_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 201 Created"), "{}", head);
    assert_eq!(String::from_utf8_lossy(&body), "created");
}

#[test]
fn test_integration_large_request_body_is_forwarded_intact() {
    // 本文の転送は 64 KiB 単位で読む。境界をまたいでも長さも中身も欠けないこと
    let origin_port = start_body_echo_origin();
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    for size in [200 * 1024, 64 * 1024, 65 * 1024] {
        let mut payload = vec![b'a'; size];
        payload[0] = b'S';
        let last = payload.len() - 1;
        payload[last] = b'E';
        let head = format!(
            "POST http://{}/big HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\n\r\n",
            host,
            host,
            payload.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(&payload).unwrap();
        let (head, body) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
        assert_eq!(
            String::from_utf8_lossy(&body),
            format!("{}:{}:{}", size, b'S', b'E'),
            "{} バイトの本文が欠けずに届く",
            size
        );
    }
}

#[test]
fn test_integration_pooled_connection_returning_408_is_retried() {
    // 再利用した接続に「積まれていた」408 は、そのまま中継せずに新しい接続でやり直す
    let requests = Arc::new(AtomicUsize::new(0));
    let origin_port = start_408_on_second_request_origin(Arc::clone(&requests));
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/p", host);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(
        first.contains("200 OK") && first.ends_with("ok 1"),
        "{}",
        first
    );

    // 2 本目はプールの接続を再利用して 408 を受けるが、新しい接続でやり直して 200 が返る
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(
        second.contains("200 OK"),
        "408 が中継されてはいけない: {}",
        second
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        3,
        "408 を受けた 1 回ぶん余計にオリジンへ行く"
    );
}

#[test]
fn test_integration_large_response_bodies_are_delivered_intact() {
    // 素通しできる本文は splice(2) でカーネル内を運ぶ。閾値の前後と、
    // 保存する応答 (splice を通さない経路) の両方で中身が一致すること
    let expect =
        |size: usize| -> Vec<u8> { (0..size).map(|i| ((i * 7 + 13) % 251) as u8).collect() };

    let counter = Arc::new(AtomicUsize::new(0));
    let origin_port = start_sized_origin(Arc::clone(&counter), false);
    let proxy_port = start_test_proxy(proxy_config());
    // 閾値 (128 KiB) の前後をまたぐ大きさ。並列実行の邪魔をしないよう最大は 1 MiB に留める
    for size in [1024, 100 * 1024, 128 * 1024, 512 * 1024, 1024 * 1024] {
        let got = get_body_via_proxy(proxy_port, origin_port, size);
        assert_eq!(got.len(), size, "{} バイトの本文の長さ", size);
        assert_eq!(got, expect(size), "{} バイトの本文の中身", size);
    }

    // キャッシュに保存する応答は splice を通らない (保存と配信が両方正しいこと)
    let counter = Arc::new(AtomicUsize::new(0));
    let origin_port = start_sized_origin(Arc::clone(&counter), true);
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-splice"));
    let size = 512 * 1024;
    let first = get_body_via_proxy(proxy_port, origin_port, size);
    assert_eq!(first, expect(size), "保存しながらの配信");
    let second = get_body_via_proxy(proxy_port, origin_port, size);
    assert_eq!(second, expect(size), "キャッシュからの配信");
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "2 回目はオリジンに行かない"
    );
}
