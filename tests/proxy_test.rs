//! 中継そのものの結合テスト (転送・ACL・エンドポイント・上限・壊れた要求)。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use std::sync::atomic::{AtomicUsize, Ordering};

use rust_http_proxy::config::Config;

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
fn test_integration_acl_denied() {
    let (origin_port, _origin_handle) = start_mock_origin();
    let config = Config::new(
        "0",
        None,
        Some("blocked.com, 127.0.0.1"),
        Duration::from_secs(5),
    )
    .unwrap();
    let proxy_port = start_test_proxy(config);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!(
        "GET http://127.0.0.1:{}/test HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        origin_port, origin_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
}

#[test]
fn test_integration_healthz() {
    let proxy_port = start_test_proxy(proxy_config());

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .write_all(
            format!(
                "GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                proxy_port
            )
            .as_bytes(),
        )
        .unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("\"status\":\"ok\""));
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
fn test_integration_status_embeds_the_parts_from_the_upper_layers() {
    // `/status` の組み立ては endpoints の仕事で、指標 (proxy-metrics) は部品を並べるだけ。
    // 受け渡しが切れると、その部品が丸ごと null になる。
    // テスト用のプロキシは再読込スレッドも状態ファイルも持たないので、
    // settings と state_file はもともと null。生きているのは blocklist と dns の 2 つ
    let proxy_port = start_test_proxy(proxy_config());
    let status = status_json(proxy_port);
    for (key, must_contain) in [
        ("\"blocklist\":", "\"entries\":"),
        ("\"dns\":", "\"ttl_secs\":"),
    ] {
        let at = status
            .find(key)
            .unwrap_or_else(|| panic!("{} が無い: {}", key, status));
        let tail = &status[at + key.len()..];
        assert!(
            tail[..200.min(tail.len())].contains(must_contain),
            "{} の中身が空 (上の層からの受け渡しが切れている): {}",
            key,
            &tail[..80.min(tail.len())]
        );
    }
    // 部品の位置 (キーの並び) も変わっていないこと
    let order: Vec<&str> = [
        "\"log_level\":",
        "\"settings\":",
        "\"dns\":",
        "\"blocklist\":",
        "\"state_file\":",
        "\"cache\":",
    ]
    .into_iter()
    .filter(|k| status.contains(k))
    .collect();
    assert_eq!(order.len(), 6, "{}", status);
    let mut at = 0;
    for k in order {
        let i = status[at..]
            .find(k)
            .unwrap_or_else(|| panic!("{} の位置が違う: {}", k, status));
        at += i;
    }
}

/// `PROXY_MAX_CONNS` / `PROXY_MAX_THREADS` の `auto` で決まった値と、いまのスレッドの数が
/// `/status` から見えること (T10.7。以前は起動ログにしか出ていなかった)。
#[test]
fn test_integration_status_shows_the_limits_and_the_thread_counts() {
    let mut cfg = proxy_config();
    cfg.max_conns = 137;
    cfg.max_threads = 41;
    let proxy_port = start_test_proxy(cfg);
    let status = status_json(proxy_port);
    assert!(
        status.contains("\"max_conns\":137"),
        "決まった上限が出ていない: {}",
        status
    );
    assert!(
        status.contains("\"max_threads\":41"),
        "スレッドの上限が出ていない: {}",
        status
    );
    // `/status` を引いている接続そのものがワーカースレッドを 1 本使っているので、
    // 生きているスレッドは 1 本以上。待ち行列は空 (上限に当たっていない)
    let live: usize = field(&status, "\"live_threads\":");
    let idle: usize = field(&status, "\"idle_threads\":");
    assert!(
        live >= 1,
        "生きているスレッドが数えられていない: {}",
        status
    );
    assert!(idle <= live, "空きは生きている数を超えない: {}", status);
    assert!(status.contains("\"queued_jobs\":0"), "{}", status);
}

/// `/status` に出した上限といまのスレッド数が、`/metrics` にも gauge として出ること (T11.5)。
/// 運用で見るのは `/metrics` の方なので、`/status` だけだと片肺になる。
#[test]
fn test_integration_metrics_shows_the_limits_and_the_thread_counts() {
    let mut cfg = proxy_config();
    cfg.max_conns = 137;
    cfg.max_threads = 41;
    let proxy_port = start_test_proxy(cfg);
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .write_all(
            format!(
                "GET /metrics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                proxy_port
            )
            .as_bytes(),
        )
        .unwrap();
    let mut metrics = String::new();
    stream.read_to_string(&mut metrics).unwrap();
    for name in [
        "max_connections",
        "max_threads",
        "live_threads",
        "idle_threads",
        "queued_jobs",
    ] {
        assert!(
            metrics.contains(&format!("# TYPE sorahost_{} gauge\n", name)),
            "sorahost_{} の gauge が無い: {}",
            name,
            metrics
        );
    }
    assert!(
        metrics.contains("sorahost_max_connections 137\n"),
        "決まった上限が出ていない: {}",
        metrics
    );
    assert!(
        metrics.contains("sorahost_max_threads 41\n"),
        "スレッドの上限が出ていない: {}",
        metrics
    );
    assert!(
        metrics.contains("sorahost_queued_jobs 0\n"),
        "待ち行列は空のはず: {}",
        metrics
    );
    // `/metrics` を引いている接続そのものがワーカースレッドを 1 本使っている
    let live: usize = field(&metrics, "\nsorahost_live_threads ");
    let idle: usize = field(&metrics, "\nsorahost_idle_threads ");
    assert!(
        live >= 1,
        "生きているスレッドが数えられていない: {}",
        metrics
    );
    assert!(idle <= live, "空きは生きている数を超えない: {}", metrics);
}

/// `/status` の JSON や `/metrics` の本文から `key` に続く数を取る (テスト用の雑な取り出し)。
fn field(status: &str, key: &str) -> usize {
    let at = status
        .find(key)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, status))
        + key.len();
    let end = status[at..]
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(status.len() - at);
    status[at..at + end].parse().expect("数字")
}

#[test]
fn test_integration_metrics_purge_and_lookup_endpoints() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        start_counting_origin(Arc::clone(&counter), "Cache-Control: max-age=60\r\n");
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-endpoints"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/asset", host);

    get_via_proxy(proxy_port, &url, &host);
    let hit = get_via_proxy(proxy_port, &url, &host);
    assert!(hit.contains("X-Cache: HIT"));

    let endpoint = |req: &str| {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        stream.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        out
    };

    // /lookup はエントリを報告する (LRU には触らない)
    let looked = endpoint(&format!(
        "GET /lookup?url={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        url, proxy_port
    ));
    assert!(looked.starts_with("HTTP/1.1 200 OK"), "{}", looked);
    assert!(
        looked.contains("\"found\":true") && looked.contains("\"memory\":true"),
        "{}",
        looked
    );

    // /metrics は Prometheus 形式でヒット数とホスト別統計を出す
    let metrics = endpoint(&format!(
        "GET /metrics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        proxy_port
    ));
    assert!(metrics.contains("text/plain; version=0.0.4"), "{}", metrics);
    assert!(
        metrics.contains("sorahost_cache_hits_total{tier=\"memory\"} 1"),
        "{}",
        metrics
    );
    assert!(
        metrics.contains(&format!(
            "sorahost_host_hits_total{{host=\"http://{}\"}} 1",
            host
        )),
        "{}",
        metrics
    );
    assert!(
        metrics.contains(&format!(
            "sorahost_host_misses_total{{host=\"http://{}\"}} 1",
            host
        )),
        "{}",
        metrics
    );

    // /status にもホスト別が入る
    let status = endpoint(&format!(
        "GET /status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        proxy_port
    ));
    assert!(
        status.contains(&format!(
            "\"host\":\"http://{}\",\"requests\":2,\"hits\":1,\"misses\":1",
            host
        )),
        "{}",
        status
    );

    // PURGE メソッドで消える → 次は MISS
    let purged = endpoint(&format!("PURGE {} HTTP/1.1\r\nHost: {}\r\n\r\n", url, host));
    assert!(
        purged.starts_with("HTTP/1.1 200 OK") && purged.contains("\"purged\":1"),
        "{}",
        purged
    );
    let after = get_via_proxy(proxy_port, &url, &host);
    assert!(!after.contains("X-Cache"), "{}", after);
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    // /purge?all=1 で全消去、/lookup は 404
    let all = endpoint(&format!(
        "GET /purge?all=1 HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        proxy_port
    ));
    assert!(all.contains("\"all\":true"), "{}", all);
    let gone = endpoint(&format!(
        "GET /lookup?url={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        url, proxy_port
    ));
    assert!(
        gone.starts_with("HTTP/1.1 404") && gone.contains("\"found\":false"),
        "{}",
        gone
    );
    let bad = endpoint(&format!(
        "GET /purge HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        proxy_port
    ));
    assert!(bad.starts_with("HTTP/1.1 400"), "{}", bad);
}

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

#[test]
fn test_integration_connection_limit_returns_503() {
    // 上限 8 で起動し、9 本目が 503 になること
    let (origin_port, _origin) = start_mock_origin();
    let mut cfg = proxy_config();
    cfg.max_conns = 8;
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    // 上限ぶん keep-alive で握ったままにする (1 要求ずつ流して接続を確立させる)
    let mut held = Vec::new();
    for i in 0..8 {
        let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        let req = format!(
            "GET http://{}/hold{} HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, i, host
        );
        s.write_all(req.as_bytes()).unwrap();
        read_response(&mut s);
        held.push(s);
    }

    let mut extra = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let mut resp = String::new();
    extra.read_to_string(&mut resp).unwrap();
    assert!(
        resp.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{}",
        resp
    );
    assert!(resp.contains("Retry-After: 1"), "{}", resp);

    // 1 本閉じれば次は通る
    held.pop();
    for _ in 0..50 {
        let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        let req = format!(
            "GET http://{}/after HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, host
        );
        s.write_all(req.as_bytes()).unwrap();
        let (head, _) = read_response(&mut s);
        if head.starts_with("HTTP/1.1 200") {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("a slot should have been freed");
}

#[test]
fn test_integration_metadata_address_is_forbidden_by_default() {
    // 既定 (PROXY_ALLOW_LOCAL 無し) ではクラウドのメタデータ宛ては 403
    let mut cfg = proxy_config();
    cfg.allow_local = false;
    let proxy_port = start_test_proxy(cfg);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .write_all(
            b"GET http://169.254.169.254/latest/meta-data/ HTTP/1.1\r\nHost: 169.254.169.254\r\n\r\n",
        )
        .unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 403 Forbidden"), "{}", resp);
}

#[test]
fn test_integration_malformed_requests_do_not_panic() {
    let proxy_port = start_test_proxy(proxy_config());
    let status =
        |resp: &str| -> Option<u16> { resp.split_whitespace().nth(1).and_then(|s| s.parse().ok()) };

    // 要求行が壊れている: 応答なしで閉じるか 400
    for bad in [
        &b"GARBAGE\r\n\r\n"[..],
        &b"GET\r\n\r\n"[..],
        &b"\x00\x01\x02\r\n\r\n"[..],
        &b"GET http://127.0.0.1:9/ HTTP/1.1\r\nHost\r\n\r\n"[..], // 区切りのないヘッダー
    ] {
        let resp = raw_request(proxy_port, bad);
        assert!(
            resp.is_empty() || matches!(status(&resp), Some(400..=599)),
            "unexpected response: {:?}",
            resp
        );
    }

    // 巨大な要求行 -> 414
    let mut long = b"GET http://example.invalid/".to_vec();
    long.extend(std::iter::repeat_n(b'a', 70 * 1024));
    long.extend_from_slice(b" HTTP/1.1\r\n\r\n");
    assert_eq!(status(&raw_request(proxy_port, &long)), Some(414));

    // ヘッダー行が多すぎる -> 431
    let mut many = b"GET http://example.invalid/ HTTP/1.1\r\nHost: example.invalid\r\n".to_vec();
    for i in 0..300 {
        many.extend_from_slice(format!("X-Pad-{}: 1\r\n", i).as_bytes());
    }
    many.extend_from_slice(b"\r\n");
    assert_eq!(status(&raw_request(proxy_port, &many)), Some(431));

    // ヘッダー 1 行が長すぎる -> 431
    let mut long_header = b"GET http://example.invalid/ HTTP/1.1\r\nX-Long: ".to_vec();
    long_header.extend(std::iter::repeat_n(b'b', 70 * 1024));
    long_header.extend_from_slice(b"\r\n\r\n");
    assert_eq!(status(&raw_request(proxy_port, &long_header)), Some(431));

    // CR 無しの行 (LF だけ) でも解釈できる
    let resp = raw_request(
        proxy_port,
        b"GET http://127.0.0.1:9/ HTTP/1.1\nHost: 127.0.0.1:9\n\n",
    );
    assert!(
        resp.is_empty() || matches!(status(&resp), Some(400..=599)),
        "{:?}",
        resp
    );

    // CONNECT の宛先にパスが付いている / スキームが付いている
    for target in [
        "http://example.invalid:443/path",
        "example.invalid:443/path",
    ] {
        let req = format!(
            "CONNECT {} HTTP/1.1\r\nHost: example.invalid\r\n\r\n",
            target
        );
        let resp = raw_request(proxy_port, req.as_bytes());
        assert!(
            resp.is_empty() || matches!(status(&resp), Some(400..=599)),
            "{:?}",
            resp
        );
    }

    // IPv6 リテラル (到達しないので 502 か 403。パニックしないことが要点)
    let resp = raw_request(
        proxy_port,
        b"GET http://[2001:db8::1]:8080/ HTTP/1.1\r\nHost: [2001:db8::1]:8080\r\n\r\n",
    );
    assert!(
        resp.is_empty() || matches!(status(&resp), Some(400..=599)),
        "{:?}",
        resp
    );

    // 最後に正常な要求が通ること (プロキシが生きている)
    let (origin_port, _origin) = start_mock_origin();
    let host = format!("127.0.0.1:{}", origin_port);
    let ok = get_via_proxy(proxy_port, &format!("http://{}/alive", host), &host);
    assert!(ok.contains("200 OK"), "{}", ok);
}

#[test]
fn test_integration_request_body_on_a_reused_connection() {
    // 同じ接続の 2 本目以降で本文付きの要求を送る。要求行とヘッダーは keep-alive の
    // アイドル時間で待つようにしたので、本文を読む前にタイムアウトが戻ることの確認
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(|req, _n| {
            let body = req.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n{}",
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
fn test_integration_total_header_size_is_capped() {
    // 1 行の上限だけだと、正常な形の要求でも 64 KiB × 256 行 = 16 MiB を送れてしまい、
    // 認証なしの開放プロキシでは数接続でメモリを食い潰せる (実測で RSS 15 → 272 MiB)。
    // 合計にも上限があること、上限内の大きめのヘッダーは通ることを固定する
    let (origin_port, _origin) = start_mock_origin();
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let request_with_headers = |kib: usize| -> String {
        let mut req = format!("GET http://{}/pad HTTP/1.1\r\nHost: {}\r\n", host, host);
        // 1 行 8 KiB のヘッダーを並べる
        for i in 0..(kib / 8) {
            req.push_str(&format!(
                "X-Pad-{:03}: {}\r\n",
                i,
                "a".repeat(8 * 1024 - 15)
            ));
        }
        req.push_str("\r\n");
        req
    };

    // 上限内 (64 KiB) は通る
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(request_with_headers(64).as_bytes())
        .unwrap();
    let (head, _) = read_response(&mut stream);
    assert!(
        head.starts_with("HTTP/1.1 200 OK"),
        "上限内は通る: {}",
        head
    );

    // 上限超え (256 KiB) は 431
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(request_with_headers(256).as_bytes())
        .unwrap();
    let mut resp = String::new();
    let _ = stream.read_to_string(&mut resp);
    assert!(
        resp.starts_with("HTTP/1.1 431"),
        "合計の上限を超えたら 431: {}",
        &resp[..resp.len().min(80)]
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

#[test]
fn test_integration_connection_named_framing_headers_do_not_smuggle() {
    // `Connection: Content-Length` のように枠組みのヘッダーを hop-by-hop として指名されると、
    // 「ヘッダーからは Content-Length を落とすが本文は送る」というずれが起きうる。
    // オリジンはその本文を次の要求の先頭として読むので、共有のオリジン接続に別要求を
    // 注入できてしまう (要求スマグリング)。ヘッダーと本文が必ず整合することを固定する。
    let received = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = listener.local_addr().unwrap().port();
    {
        let received = Arc::clone(&received);
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let received = Arc::clone(&received);
                thread::spawn(move || {
                    stream
                        .set_read_timeout(Some(Duration::from_millis(500)))
                        .unwrap();
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() >= 10 {
                                    break;
                                }
                            }
                        }
                    }
                    received
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf).into_owned());
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                });
            }
        });
    }
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    for named in ["Content-Length", "Transfer-Encoding"] {
        let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let req = format!(
            "POST http://{}/x HTTP/1.1\r\nHost: {}\r\nConnection: {}\r\n\
             Content-Length: 10\r\n\r\n0123456789",
            host, host, named
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut resp = String::new();
        let _ = s.read_to_string(&mut resp);

        thread::sleep(Duration::from_millis(300));
        let seen = received.lock().unwrap();
        let last = seen.last().cloned().unwrap_or_default();
        let (head, body) = last.split_once("\r\n\r\n").unwrap_or((last.as_str(), ""));
        let has_framing = head
            .lines()
            .any(|l| l.to_ascii_lowercase().starts_with("content-length:"));
        assert!(
            has_framing || body.is_empty(),
            "Connection: {} で枠組みが落ちたのに本文が届いている (要求スマグリング):\n{}",
            named,
            last
        );
    }
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
    // 自分へ 1 本でもつなぐと loopback でも桁が変わるので、要求 1 本の往復を測る
    let start = std::time::Instant::now();
    stream.write_all(req.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let elapsed = start.elapsed();

    assert!(
        response.starts_with("HTTP/1.1 404 Not Found"),
        "{}",
        response
    );
    assert!(
        elapsed < Duration::from_millis(10),
        "自分へ転送していない証拠: {:?}",
        elapsed
    );
    let status = status_json(proxy_port);
    assert_eq!(
        status_number(&status, "new"),
        0,
        "オリジンへの接続は 1 本も張らない: {}",
        status
    );
    assert!(
        status_number(&status, "active_connections") <= 1,
        "/status を取っているこの 1 本だけ: {}",
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
