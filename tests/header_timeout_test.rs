//! 要求行とヘッダーの総時間の締め切り (`PROXY_HEADER_TIMEOUT_SECS`) の結合テスト (T.28.0)。
//!
//! `SO_RCVTIMEO` は読めるたびに待ち直すので、黙ったままの接続も 1 バイトずつ送る接続
//! (slowloris) も、それだけでワーカー 1 本と接続枠 1 つを握れた。締め切りを越えたら
//! `408 Request Timeout` で閉じること、上限ぶんの沈黙を張られても `/status` が返ること、
//! 本文の転送は締め切りの外であることを見る。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

mod common;
use common::*;

/// 締め切りを `secs` 秒にした設定 (`PROXY_TIMEOUT_SECS` は 30 秒 = 本番の既定)。
fn config_with_deadline(secs: u64, lite: bool) -> rust_http_proxy::config::Config {
    let mut cfg = proxy_config();
    cfg.timeout = Duration::from_secs(30);
    cfg.header_timeout = Duration::from_secs(secs);
    cfg.lite = lite;
    cfg
}

/// 受け付けのループが待ち受けに読み取りタイムアウトを当て終わるまで待つ。
///
/// 待ち受けの `SO_RCVTIMEO` は `serve` の頭で当てるので、その前に握手を終えた接続は
/// 継承できない (無期限のまま)。テストは起動直後に繋ぐので、`/healthz` を 1 本通してから始める
fn ready(port: u16) -> u16 {
    let got = raw_get(
        port,
        &format!(
            "GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            port
        ),
    );
    assert!(got.starts_with("HTTP/1.1 200"), "{}", got);
    port
}

/// 相手が閉じるまで読み、届いたバイト列と閉じるまでの時間を返す。
fn read_until_closed(stream: &mut TcpStream, started: Instant) -> (String, Duration) {
    stream
        .set_read_timeout(Some(Duration::from_secs(40)))
        .unwrap();
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out);
    (
        String::from_utf8_lossy(&out).into_owned(),
        started.elapsed(),
    )
}

/// (a) 黙ったままの接続は締め切り + 1 秒以内に 408 で閉じる (`--lite` でも)。
#[test]
fn test_integration_silent_connection_gets_408_within_the_deadline() {
    for lite in [false, true] {
        let port = ready(start_test_proxy(config_with_deadline(2, lite)));
        let started = Instant::now();
        let mut s = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
        let (got, took) = read_until_closed(&mut s, started);
        assert!(
            got.starts_with("HTTP/1.1 408 Request Timeout"),
            "lite={}: {:?} after {:?}",
            lite,
            got,
            took
        );
        assert!(
            took >= Duration::from_millis(1800) && took < Duration::from_secs(3),
            "lite={}: 締め切り 2 秒 + 1 秒以内に閉じる: {:?}",
            lite,
            took
        );
    }
}

/// 1 秒に 1 バイトずつ要求を送り、閉じられるまで続ける。閉じられたら届いた応答と時間を返す。
fn trickle(port: u16, prefix: &[u8], limit: Duration) -> (String, Duration) {
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
    s.write_all(prefix).unwrap();
    let started = Instant::now();
    let tail = b"GET http://127.0.0.1:9/ HTTP/1.1\r\nHost: 127.0.0.1:9\r\nX-Pad: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let mut out = Vec::new();
    for b in tail.iter() {
        if s.write_all(&[*b]).is_err() {
            break;
        }
        // 1 秒待つ間に応答が来れば (閉じられれば) 終わり
        let mut buf = [0u8; 256];
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                let _ = s.read_to_end(&mut out);
                break;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
        if started.elapsed() > limit {
            break;
        }
    }
    (
        String::from_utf8_lossy(&out).into_owned(),
        started.elapsed(),
    )
}

/// (b) 1 バイト / 1 秒の trickle も締め切り + 1 秒以内に 408 で閉じる (`--lite` でも)。
/// 今までは読めるたびに `SO_RCVTIMEO` が待ち直すので切れなかった。
#[test]
fn test_integration_trickled_headers_get_408_within_the_deadline() {
    for lite in [false, true] {
        let port = ready(start_test_proxy(config_with_deadline(3, lite)));
        let (got, took) = trickle(port, b"", Duration::from_secs(15));
        assert!(
            got.starts_with("HTTP/1.1 408 Request Timeout"),
            "lite={}: {:?} after {:?}",
            lite,
            got,
            took
        );
        assert!(
            took < Duration::from_secs(4) + Duration::from_millis(500),
            "lite={}: 締め切り 3 秒 + 1 秒以内に閉じる: {:?}",
            lite,
            took
        );
    }
}

/// 2 要求目以降の trickle も、1 バイト目からの締め切りで 408 になる。
/// 2 要求目の 1 バイト目までの待ち (keep-alive のアイドル) は締め切りに入れない。
#[test]
fn test_integration_trickled_second_request_gets_408() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = config_with_deadline(3, false);
    // 2 要求目の前の沈黙は締め切り (3 秒) より長くても切らない (アイドルは 8 秒)
    cfg.keepalive = Duration::from_secs(8);
    let port = ready(start_test_proxy(cfg));
    let host = format!("127.0.0.1:{}", origin_port);
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let (head, _) = one_keepalive_request(&mut s, &host, "/first");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);

    // 締め切りより長く黙ってから 2 要求目を普通に送っても通る
    std::thread::sleep(Duration::from_millis(3500));
    let (head, _) = one_keepalive_request(&mut s, &host, "/second");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);

    // 3 要求目を 1 バイトずつ送ると、1 バイト目から 3 秒 + 1 秒以内に 408
    s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let started = Instant::now();
    let tail =
        b"GET http://127.0.0.1:9/ HTTP/1.1\r\nHost: 127.0.0.1:9\r\nX-Pad: aaaaaaaaaaaaaaaaaaaa";
    let mut out = Vec::new();
    for b in tail.iter() {
        if s.write_all(&[*b]).is_err() {
            break;
        }
        let mut buf = [0u8; 256];
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                break;
            }
            Err(_) => {}
        }
        if started.elapsed() > Duration::from_secs(15) {
            break;
        }
    }
    let got = String::from_utf8_lossy(&out);
    assert!(
        got.starts_with("HTTP/1.1 408 Request Timeout"),
        "{:?} after {:?}",
        got,
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_millis(4500),
        "{:?}",
        started.elapsed()
    );
}

/// (c) `PROXY_MAX_CONNS` ぶんの沈黙を張っても、`/status` が 3 秒以内に 200 で返る。
/// 締め切りは既定の 10 秒のまま (締め切りで沈黙が放されるのを待っているのではない)。
#[test]
fn test_integration_status_answers_while_silent_connections_fill_the_limit() {
    let mut cfg = config_with_deadline(10, false);
    cfg.max_conns = 8;
    // 本番の既定 (`PROXY_MAX_THREADS=auto`) と同じく、ワーカーも 8 本まで
    cfg.max_threads = 8;
    let (port, metrics) = start_test_proxy_with_metrics(cfg);
    ready(port);
    let silent: Vec<TcpStream> = (0..8)
        .map(|_| TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap())
        .collect();
    wait_until(
        || {
            metrics
                .active_connections
                .load(std::sync::atomic::Ordering::Relaxed)
                >= 8
        },
        "8 silent connections should be open",
    );
    let started = Instant::now();
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(
        format!(
            "GET /status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            port
        )
        .as_bytes(),
    )
    .unwrap();
    let mut head = [0u8; 15];
    let got = s.read_exact(&mut head).map(|_| head);
    let took = started.elapsed();
    assert_eq!(
        got.as_ref().map(|h| &h[..]).ok(),
        Some(&b"HTTP/1.1 200 OK"[..]),
        "{:?} after {:?}",
        got,
        took
    );
    assert!(took < Duration::from_secs(3), "{:?}", took);
    drop(silent);
}

/// (d) 本文の転送は締め切りの外: 10 MiB の応答も、締め切りより長く待たせてから
/// 送る 10 MiB の要求の本文も、今までどおり通る。
#[test]
fn test_integration_large_bodies_are_not_cut_by_the_header_deadline() {
    let port = ready(start_test_proxy(config_with_deadline(1, false)));

    // 10 MiB の応答
    let counter = Arc::new(AtomicUsize::new(0));
    let origin = start_sized_origin(Arc::clone(&counter), false);
    let body = get_body_via_proxy(port, origin, 10 * 1024 * 1024);
    assert_eq!(body.len(), 10 * 1024 * 1024);

    // 10 MiB の要求の本文。ヘッダーを送ってから締め切り (1 秒) より長く待ち、それから本文
    let echo = start_body_echo_origin();
    let len = 10 * 1024 * 1024;
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(
        format!(
            "POST http://127.0.0.1:{}/up HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            echo, echo, len
        )
        .as_bytes(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let chunk = vec![7u8; 64 * 1024];
    for _ in 0..(len / chunk.len()) {
        s.write_all(&chunk).unwrap();
    }
    let mut all = Vec::new();
    s.read_to_end(&mut all).unwrap();
    let text = String::from_utf8_lossy(&all);
    assert!(text.starts_with("HTTP/1.1 200 OK"), "{}", text);
    assert!(text.ends_with(&format!("{}:7:7", len)), "{}", text);
}
