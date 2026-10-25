//! keep-alive とアイドル接続の預かり (epoll) の結合テスト。
//!
//! 末尾の 3 本だけは **TCP の** keepalive (`PROXY_TCP_KEEPALIVE`。T14.52) で、
//! HTTP の keep-alive (`PROXY_KEEPALIVE_SECS`) とは別のもの。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::*;

#[test]
fn test_integration_parked_idle_connection_serves_the_next_request() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let proxy_port = start_test_proxy(park_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, body) = one_keepalive_request(&mut stream, &host, "/p1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");

    // 猶予 (既定 3ms) を過ぎれば監視スレッドに預けられ、スレッドから外れる
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":1"),
        "the idle connection should be parked",
    );
    let status = status_json(proxy_port);
    assert!(status.contains("\"parking\":true"), "{}", status);

    // 預けた接続に要求を送ると、監視スレッドが起こしてワーカーが処理する
    let (head, body) = one_keepalive_request(&mut stream, &host, "/p2");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert!(head.contains("Connection: keep-alive"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    // 2 本目のあとも預けられる (何度でも往復できる)
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":1"),
        "the connection should be parked again",
    );
    let (head, _) = one_keepalive_request(&mut stream, &host, "/p3");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
}

#[test]
fn test_integration_keepalive_without_parking_still_works() {
    // PROXY_PARK_IDLE=off: 「1 接続 = 1 スレッドが専任」の元の動き
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = proxy_config();
    cfg.park_idle = false;
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/n1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    // 猶予より十分長く空けても、預けないので接続はそのまま
    thread::sleep(Duration::from_millis(100));
    let status = status_json(proxy_port);
    assert!(status.contains("\"parked_connections\":0"), "{}", status);
    assert!(status.contains("\"parking\":false"), "{}", status);
    let (head, body) = one_keepalive_request(&mut stream, &host, "/n2");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn test_integration_park_waits_for_a_request_sent_in_pieces() {
    // 猶予は「次の要求がまだ来ていない」ことを読み取りタイムアウトで測る。
    // 要求を送っている最中の細切れ (猶予より長い間隔) で切ってはいけない
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.park_grace = Duration::from_millis(5);
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/s1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":1"),
        "the idle connection should be parked",
    );

    // 猶予 (5ms) の何倍も空けながら、要求行の途中・ヘッダーの途中で区切って送る
    let req = format!(
        "GET http://{}/s2 HTTP/1.1\r\nHost: {}\r\nX-Slow: yes\r\n\r\n",
        host, host
    );
    let bytes = req.as_bytes();
    for chunk in [&bytes[..12], &bytes[12..30], &bytes[30..]] {
        stream.write_all(chunk).unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_millis(40));
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (head, body) = read_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn test_integration_park_with_no_grace_serves_every_request() {
    // 猶予 0 = 要求のたびに必ず預けて戻す。預ける経路を毎回通す設定 (CI 用)
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.park_grace = Duration::ZERO;
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    for i in 0..5 {
        let (head, body) = one_keepalive_request(&mut stream, &host, &format!("/g{}", i));
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
        assert_eq!(body, b"hello from mock origin");
    }
    assert_eq!(counter.load(Ordering::SeqCst), 5);
}

#[test]
fn test_integration_parked_connection_closes_at_keepalive_timeout() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.keepalive = Duration::from_millis(300);
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/t1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);

    // 預けたまま keep-alive の期限が過ぎたら、監視スレッドが閉じる
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut buf = [0u8; 1];
    assert_eq!(stream.read(&mut buf).unwrap(), 0, "closed by the proxy");
    // /status を引く接続それ自体が active に入るので、預かり数の方で見る
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":0"),
        "the expired connection should be released",
    );
}

#[test]
fn test_integration_parked_connection_notices_the_client_going_away() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let proxy_port = start_test_proxy(park_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/c1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":1"),
        "the idle connection should be parked",
    );

    // 預けている間にクライアントが閉じたら、持ち分ごと片付ける
    drop(stream);
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":0"),
        "the closed connection should be released",
    );
}

#[test]
fn test_integration_keepalive_serves_multiple_requests_per_connection() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-keepalive"));
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    for i in 0..3 {
        let req = format!(
            "GET http://{}/ka{} HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, i, host
        );
        stream.write_all(req.as_bytes()).unwrap();
        let (head, body) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
        assert!(head.contains("Connection: keep-alive"), "{}", head);
        assert_eq!(body, b"hello from mock origin");
    }
    // 同じ接続でキャッシュヒットも返る
    let req = format!("GET http://{}/ka0 HTTP/1.1\r\nHost: {}\r\n\r\n", host, host);
    stream.write_all(req.as_bytes()).unwrap();
    let (head, body) = read_response(&mut stream);
    assert!(head.contains("X-Cache: HIT"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 3);

    // Connection: close で終わる
    let req = format!(
        "GET http://{}/ka1 HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        host, host
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    let text = String::from_utf8_lossy(&rest);
    assert!(text.contains("Connection: close"), "{}", text);
    assert!(text.ends_with("hello from mock origin"));

    // HTTP/1.0 の要求は応答後に閉じられる
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!("GET http://{}/ka2 HTTP/1.0\r\nHost: {}\r\n\r\n", host, host);
    stream.write_all(req.as_bytes()).unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    assert!(String::from_utf8_lossy(&rest).contains("Connection: close"));
}

/// 受け入れ基準: 5 本の束はこれ未満で返る。
const LIMIT: Duration = Duration::from_millis(100);

/// Nagle + delayed ACK なら 1 要求あたり約 40 ms 止まるので、5 本の束はこれ以上になる。
const NAGLE: Duration = Duration::from_millis(200);

/// 束を測り直す回数。
const ATTEMPTS: usize = 5;

/// 5 本の束を 1 回測る (要求を書いて応答を読み切るまで)。
fn measure_five(stream: &mut TcpStream, host: &str, tag: &str) -> Duration {
    let started = std::time::Instant::now();
    for i in 0..5 {
        let req = format!(
            "GET http://{}/{}{} HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, tag, i, host
        );
        stream.write_all(req.as_bytes()).unwrap();
        let (head, _body) = read_response(stream);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    }
    started.elapsed()
}

/// 束の測定値を判定する: **1 回でも 100 ms 未満なら通す** (機械が混んだ回を拾わない)。
///
/// ただし Nagle + delayed ACK は毎回 1 要求あたり約 40 ms 止まるので、束は**毎回**
/// 200 ms 以上になる。だから 200 ms 以上の回が 2 つ以上あれば、速い回が混ざっていても
/// Nagle と見て落とす (「遅い機械」を捨てる仕掛けが「Nagle の見逃し」に化けないための縛り)。
fn judge(samples: &[Duration]) -> Result<Duration, String> {
    let slow = samples.iter().filter(|s| **s >= NAGLE).count();
    if slow >= 2 {
        return Err(format!(
            "Nagle delay: {} of {} bundles took {:?} or more: {:?}",
            slow,
            samples.len(),
            NAGLE,
            samples
        ));
    }
    match samples.iter().filter(|s| **s < LIMIT).min() {
        Some(best) => Ok(*best),
        None => Err(format!(
            "no bundle under {:?} in {} attempts: {:?}",
            LIMIT,
            samples.len(),
            samples
        )),
    }
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// 応答ヘッダーと本文のあいだに `delay` あけるオリジン (Nagle + delayed ACK の代役)。
///
/// **はじめの `delayed` 本だけ**あける (残りはすぐ返す)。1 要求あたり `delay` 止まるので、
/// 5 本の束は `5 * delay` になる。実際に遅れた束を測り方が拾えること (測っている物差し
/// そのもの) と、遅れた束を捨てて測り直せることを縛るために使う。
fn start_delayed_origin(delay: Duration, delayed: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(AtomicUsize::new(0));
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let served = Arc::clone(&served);
            thread::spawn(move || {
                // このオリジン自身の Nagle を切る。切らないと「ヘッダーと本文を別々に
                // 書いた」だけで 40 ms 止まり、`delayed` を過ぎても遅いままになる
                // (そちらは本番のプロキシ側で見ている現象)
                let _ = stream.set_nodelay(true);
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut seen_any = false;
                    loop {
                        let mut line = String::new();
                        if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line.trim().is_empty() {
                            break;
                        }
                        seen_any = true;
                    }
                    if !seen_any {
                        return;
                    }
                    let n = served.fetch_add(1, Ordering::SeqCst);
                    let body = "delayed";
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n",
                        body.len()
                    );
                    if stream.write_all(head.as_bytes()).is_err() {
                        return;
                    }
                    let _ = stream.flush();
                    if n < delayed {
                        thread::sleep(delay);
                    }
                    if stream.write_all(body.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    port
}

/// 5 本の束を、速い回が出るまで最大 `ATTEMPTS` 回**測り直す**。
///
/// 判定は `judge` (1 回でも 100 ms 未満なら通す)。Nagle なら毎回 200 ms 以上なので落ちる。
fn measure_with_retries(
    stream: &mut TcpStream,
    host: &str,
    tag: &str,
) -> (Result<Duration, String>, Vec<Duration>) {
    let mut samples: Vec<Duration> = Vec::new();
    for attempt in 0..ATTEMPTS {
        let took = measure_five(stream, host, &format!("{}{}_", tag, attempt));
        samples.push(took);
        if took < LIMIT {
            // 速い回が 1 つあれば足りる (残りは測らない)
            break;
        }
    }
    (judge(&samples), samples)
}

#[test]
fn test_integration_keepalive_requests_are_not_delayed_by_nagle() {
    // TCP_NODELAY が立っていないと、応答ヘッダーと本文を別々に write したときに
    // Nagle + delayed ACK で 1 要求あたり約 40 ms 止まる (5 要求で 200 ms 以上)。
    //
    // 負荷の下では束が 1 回だけ 100 ms を超えることがある (T24.18 の段 1 の実測:
    // 10 回に 1 回 158.7 ms。Nagle の 200 ms ではない = 機械が混んだだけの回)。
    // sleep を伸ばさず**測り直し**、1 回でも 100 ms 未満なら通す。Nagle なら毎回
    // 200 ms 以上になるので、`judge` が取り違えない (下の 2 本で縛ってある)。
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        start_keepalive_origin(Arc::clone(&connections), Arc::clone(&requests));
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    // 1 要求目は接続確立を含むので測定から外す
    let warmup = format!("GET http://{}/w HTTP/1.1\r\nHost: {}\r\n\r\n", host, host);
    stream.write_all(warmup.as_bytes()).unwrap();
    read_response(&mut stream);

    let (verdict, samples) = measure_with_retries(&mut stream, &host, "nagle");
    let best = verdict.unwrap_or_else(|why| panic!("{}", why));
    println!(
        "bundle of 5 keep-alive requests: {:?} (best {:?} in {} attempts)",
        samples,
        best,
        samples.len()
    );
    assert!(best < LIMIT, "best bundle should be under {:?}", LIMIT);
}

/// 「遅い機械」と「Nagle」を取り違えないこと (測り直しで Nagle を見逃さない縛り)。
#[test]
fn test_the_nagle_judgment_still_fails_a_delayed_bundle() {
    // 実際に落ちた回 (T24.18 の段 1、158.7 ms) は Nagle ではない。速い回が 1 つあれば通す
    let a_slow_machine = [ms(158), ms(96), ms(155), ms(151), ms(147)];
    assert!(
        judge(&a_slow_machine).is_ok(),
        "a slow machine should be retried, not failed: {:?}",
        judge(&a_slow_machine)
    );
    // 1 回だけの引っかかりでも、100 ms 以上なら Nagle ではないので通す
    assert!(judge(&[ms(18), ms(205), ms(21)]).is_ok());
    // 全部 100 ms 以上なら、測り直しで隠さずに落とす
    let why = judge(&[ms(158); 5]).expect_err("five slow bundles must fail");
    assert!(why.contains("no bundle under"), "{}", why);
    // Nagle + delayed ACK = 毎回 200 ms 以上。速い回が混ざっていても落とす
    let nagle = [ms(212), ms(205), ms(209), ms(214), ms(207)];
    let why = judge(&nagle).expect_err("a Nagle bundle must fail");
    assert!(why.contains("Nagle"), "{}", why);
    let why = judge(&[ms(18), ms(205), ms(21), ms(210)]).expect_err("two slow bundles");
    assert!(why.contains("Nagle"), "{}", why);
}

/// 実際に 200 ms 以上かかる束を、プロキシ越しの測定が拾って落とすこと (物差しの裏取り)。
#[test]
fn test_the_nagle_measurement_fails_a_bundle_that_is_always_delayed() {
    // 1 要求 40 ms x 5 本 = 200 ms の遅れを**毎回**入れる (Nagle + delayed ACK と同じ)。
    // 測り直しても遅いままなので、5 回とも測って落とす
    let origin_port = start_delayed_origin(ms(40), usize::MAX);
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let (verdict, samples) = measure_with_retries(&mut stream, &host, "delayed");
    println!("always delayed bundles: {:?}", samples);
    assert_eq!(
        samples.len(),
        ATTEMPTS,
        "速い回が無いので全部測るはず: {:?}",
        samples
    );
    assert!(
        samples.iter().all(|s| *s >= NAGLE),
        "each delayed bundle should reach {:?}: {:?}",
        NAGLE,
        samples
    );
    let why = verdict.expect_err("an always-delayed bundle must fail");
    assert!(why.contains("Nagle"), "{}", why);
}

/// 1 束だけ遅れた回は、**測り直して**通すこと (「遅い機械」を拾わない)。
///
/// はじめの 5 本 (= 1 束ぶん) だけ 40 ms あける。1 束目は 200 ms で落とす判定だが、
/// 2 束目は速いので通る。ここで「測り直す」が効いていないと 1 束目で落ちる。
#[test]
fn test_the_nagle_measurement_retries_a_single_delayed_bundle() {
    let origin_port = start_delayed_origin(ms(40), 5);
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let (verdict, samples) = measure_with_retries(&mut stream, &host, "retry");
    println!("bundles with only the first one delayed: {:?}", samples);
    assert_eq!(
        samples.len(),
        2,
        "1 束目が遅く 2 束目が速い並びになるはず: {:?}",
        samples
    );
    assert!(samples[0] >= LIMIT, "1 束目は遅れているはず: {:?}", samples);
    let best = verdict.unwrap_or_else(|why| panic!("{}", why));
    assert!(best < LIMIT, "2 束目は速いはず: {:?}", samples);
}

#[test]
fn test_integration_zero_timeout_proxies_and_still_parks_idle_connections() {
    // PROXY_TIMEOUT_SECS=0 = 無期限 (T10.6)。以前はこの設定だと `Conn::new` の
    // `set_write_timeout(Some(ZERO))` が必ず失敗し、**1 本も代理できなかった**。
    // 預ける仕組み (T6.5) は「2 回目以降の読み取りタイムアウトを猶予の長さにして
    // 空振りを『暇だ』と解釈する」形なので、待ち方が無期限になっても壊れないことを見る
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.timeout = Duration::ZERO;
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, body) = one_keepalive_request(&mut stream, &host, "/z1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");

    // 猶予は `park_grace` の長さで `timeout` とは別物なので、無期限でも預けられる
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":1"),
        "the idle connection should be parked even with an unlimited timeout",
    );

    // 預けた接続はそのまま次の要求に使える
    let (head, body) = one_keepalive_request(&mut stream, &host, "/z2");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn test_integration_zero_timeout_never_closes_a_silent_connection() {
    // 「無期限」の意味を見る (T10.6): 何も送ってこない接続を閉じない。
    // 対比のため、短いタイムアウトなら同じ接続が閉じられることも同時に見る
    let mut quick = proxy_config();
    quick.timeout = Duration::from_millis(200);
    let quick_port = start_test_proxy(quick);

    let mut forever = proxy_config();
    forever.timeout = Duration::ZERO;
    // 要求行とヘッダーの締め切り (T.28.0) は未設定なら `PROXY_TIMEOUT_SECS=0` でも 10 秒残る。
    // 黙った接続を無期限に置くのは両方 0 のとき
    forever.header_timeout = Duration::ZERO;
    let forever_port = start_test_proxy(forever);

    let mut buf = [0u8; 1];
    // 200 ms のタイムアウト: 黙っていると閉じられる (T.28.0 から 408 を返してから閉じる)
    let mut closed = TcpStream::connect(format!("127.0.0.1:{}", quick_port)).unwrap();
    closed
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut got = Vec::new();
    closed.read_to_end(&mut got).unwrap();
    let got = String::from_utf8_lossy(&got);
    assert!(
        got.is_empty() || got.starts_with("HTTP/1.1 408 Request Timeout"),
        "PROXY_TIMEOUT_SECS が有限なら黙っている接続は閉じられる: {:?}",
        got
    );

    // 無期限: 何倍の時間待っても閉じられない。
    // 混んだ機械では、この接続が別の理由 (受け入れの取りこぼし等) で閉じられることが
    // 稀にある (2026-10-23、負荷の下で 10 回に 1 回)。**閉じられたら張り直して引き直す**
    // (上限 3 回)。判定は弱めない — 3 回とも閉じられたら落とす
    let mut kept = None;
    for _ in 0..3 {
        let mut conn = TcpStream::connect(format!("127.0.0.1:{}", forever_port)).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        match conn.read(&mut buf) {
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                kept = Some(conn);
                break;
            }
            Ok(0) => continue, // 閉じられた: 張り直して引き直す
            Ok(n) => panic!("黙っている接続から {} バイト届いた", n),
            Err(e) => panic!("読み取りが思いも寄らない形で失敗した: {:?}", e),
        }
    }
    let mut kept = kept.expect("PROXY_TIMEOUT_SECS=0 なのに 3 回とも黙っている接続が閉じられた");

    // 生きているので、そのまま普通に代理できる
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let host = format!("127.0.0.1:{}", origin_port);
    kept.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let (head, body) = one_keepalive_request(&mut kept, &host, "/late");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
}
/// 生きているスレッドの上限に達しても、要求は待たされるだけで捨てられないこと (T10.5)。
///
/// 上限 2 本に対して 12 本の接続を同時に張る。上限を超えたぶんは `Workers` の待ち行列で
/// 待ち、空いたスレッドが順に引き取る。1 本でも落とされたら (= 上限のときに `Err(job)` を
/// 返して `serve` が接続を閉じる作りに戻ったら) このテストが落ちる。
#[test]
fn test_integration_requests_wait_instead_of_being_dropped_at_the_thread_limit() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.max_threads = 2;
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    const CLIENTS: usize = 12;
    let (tx, rx) = std::sync::mpsc::channel();
    for i in 0..CLIENTS {
        let (tx, host) = (tx.clone(), host.clone());
        thread::spawn(move || {
            let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            let (head, body) = one_keepalive_request(&mut stream, &host, &format!("/t{}", i));
            let _ = tx.send((head, body));
        });
    }
    for i in 0..CLIENTS {
        let (head, body) = rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|e| panic!("{} 本目の応答が来ない: {}", i + 1, e));
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
        assert_eq!(body, b"hello from mock origin");
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        CLIENTS,
        "全部オリジンへ届く"
    );
}

/// 上限の要求 (既定 1,000 本目) の応答には `Connection: close` が付き、そのあと閉じる (T14.2)。
///
/// 以前は上限に当たった応答にも `Connection: keep-alive` が付いたまま閉じていたので、
/// その応答を読んだ直後に次の要求を送ったクライアントは**入れ違いで取りこぼしていた**。
/// 上限は設定 (`Config::max_requests_per_conn`) で渡せるので、ここでは 3 に下げて見る
/// (環境変数では変えられない値。1,000 本流すと遅いのでテストのために口を開けてある)。
#[test]
fn test_integration_the_last_request_gets_connection_close_before_the_close() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.max_requests_per_conn = 3;
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    // 上限の 1 つ手前までは今までどおり keep-alive
    for i in 1..3 {
        let (head, body) = one_keepalive_request(&mut stream, &host, &format!("/k{}", i));
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{} 本目: {}", i, head);
        assert!(
            head.contains("Connection: keep-alive"),
            "{} 本目: {}",
            i,
            head
        );
        assert_eq!(body, b"hello from mock origin");
    }

    // 上限の要求。応答は普通に返り、そこに `Connection: close` が付く
    let (head, body) = one_keepalive_request(&mut stream, &host, "/k3");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert!(head.contains("Connection: close"), "{}", head);
    assert!(!head.contains("Connection: keep-alive"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 3);

    // そのあと接続は閉じる (入れ違いを見るために、閉じるのを待たずに次の要求を送る)。
    // 書けてしまうことはある (相手の受信バッファに入るだけ) が、読めるのは EOF だけ
    let _ = stream
        .write_all(format!("GET http://{}/k4 HTTP/1.1\r\nHost: {}\r\n\r\n", host, host).as_bytes());
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut rest = Vec::new();
    match stream.read_to_end(&mut rest) {
        // 閉じ終わっていれば EOF、こちらの書込が先に届いていれば RST。どちらも「閉じた」
        Ok(_) => assert!(
            rest.is_empty(),
            "上限のあとは何も返さずに閉じる: {:?}",
            String::from_utf8_lossy(&rest)
        ),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!("上限のあとは閉じているはず: {}", e),
    }
    // 上限を越えた要求はオリジンへ行っていない
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

// ------------------------------------------------------------------
// 消えたクライアントの検知 (`PROXY_TCP_KEEPALIVE`。T14.52)
// ------------------------------------------------------------------

/// `sockaddr_in` (16 B)。プロキシ側の記述子を探すために自分で宣言する
/// (外部クレートは足さない方針なので `tests/common` の `connect_from` と同じ作法)。
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SockAddrIn {
    family: u16,
    port: u16,
    addr: [u8; 4],
    zero: [u8; 8],
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn getsockname(fd: i32, addr: *mut SockAddrIn, len: *mut u32) -> i32;
    fn getpeername(fd: i32, addr: *mut SockAddrIn, len: *mut u32) -> i32;
}

/// その記述子の (自分のポート, 相手のポート)。IPv4 の繋がったソケットでなければ `None`。
#[cfg(target_os = "linux")]
fn port_pair(fd: i32) -> Option<(u16, u16)> {
    let mut local = SockAddrIn::default();
    let mut peer = SockAddrIn::default();
    let mut n = std::mem::size_of::<SockAddrIn>() as u32;
    // SAFETY: どちらも `sockaddr_in` 1 つぶんの領域とその長さを対で渡している
    // (IPv6 なら切り詰められるが、族を見て弾く)。
    if unsafe { getsockname(fd, &mut local, &mut n) } != 0 {
        return None;
    }
    let mut n = std::mem::size_of::<SockAddrIn>() as u32;
    if unsafe { getpeername(fd, &mut peer, &mut n) } != 0 {
        return None;
    }
    (local.family == 2).then(|| (u16::from_be(local.port), u16::from_be(peer.port)))
}

/// この接続の**プロキシ側** (accept した方) の記述子を探す。
///
/// 結合テストのプロキシは同じプロセスの中で動いているので、`/proc/self/fd` を走査して
/// 「自分のポート = 待ち受け、相手のポート = クライアント」のソケットを 1 つ見つければ
/// それが accept された側 (クライアント側はちょうど逆なので取り違えない)。
#[cfg(target_os = "linux")]
fn proxy_side_fd(proxy_port: u16, client_port: u16) -> Option<i32> {
    for entry in std::fs::read_dir("/proc/self/fd").ok()? {
        let Ok(entry) = entry else { continue };
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if port_pair(fd) == Some((proxy_port, client_port)) {
            return Some(fd);
        }
    }
    None
}

/// 要求を 1 本通してから、プロキシ側の記述子に当たっている keepalive を読み戻す。
#[cfg(target_os = "linux")]
fn keepalive_after_one_request(
    cfg: rust_http_proxy::config::Config,
) -> rust_http_proxy::sys::Keepalive {
    let (origin_port, _origin) = start_mock_origin();
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let client_port = stream.local_addr().unwrap().port();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/p1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);

    let fd = proxy_side_fd(proxy_port, client_port)
        .expect("accept された側の記述子が自分のプロセスに見つからない");
    let k = rust_http_proxy::sys::keepalive_of(fd).expect("getsockopt");
    drop(stream);
    k
}

/// 既定 (`on`) では accept した接続に 60 / 10 / 3 が当たっていること (T14.52)。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_tcp_keepalive_is_set_on_the_accepted_socket() {
    let k = keepalive_after_one_request(proxy_config());
    assert_eq!(
        k,
        rust_http_proxy::sys::Keepalive {
            on: true,
            idle_secs: 60,
            intvl_secs: 10,
            count: 3,
        },
        "既定の PROXY_TCP_KEEPALIVE が当たっていない"
    );
}

/// テスト用に短くした指定 (`on:1:1:2`) がそのままカーネルへ届くこと (T14.52)。
///
/// この設定なら消えた相手は約 3 秒で `ETIMEDOUT` になる (既定は約 90 秒)。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_tcp_keepalive_takes_the_three_numbers() {
    let mut cfg = proxy_config();
    cfg.tcp_keepalive =
        rust_http_proxy::config::TcpKeepalive::parse("on:1:1:2").expect("on:1:1:2 が読めない");
    assert_eq!(cfg.tcp_keepalive.map(|k| k.dead_after_secs()), Some(3));
    let k = keepalive_after_one_request(cfg);
    assert_eq!(
        k,
        rust_http_proxy::sys::Keepalive {
            on: true,
            idle_secs: 1,
            intvl_secs: 1,
            count: 2,
        }
    );
}

/// `off` なら `setsockopt` を 1 回も呼ばない (ソケットは素のまま。T14.52)。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_tcp_keepalive_off_leaves_the_socket_alone() {
    let mut cfg = proxy_config();
    cfg.tcp_keepalive = None;
    let k = keepalive_after_one_request(cfg);
    assert!(!k.on, "off なのに SO_KEEPALIVE が立っている: {:?}", k);
}
