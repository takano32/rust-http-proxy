//! プールの接続に遅れて届いたバイトを、次の要求の応答として配らない (T.29.2)。
//!
//! T26.0 で GET / HEAD (やり直せる要求) はプールから取り出すときの `recv(MSG_PEEK)` を省いた。
//! `put` は `BufReader` の読み残ししか見ないので、**応答を読み切って戻したあとに**オリジンが
//! 送ったバイトはカーネルの受信キューに残り、次の要求がそれを自分の応答として読んでいた
//! (手元の再現: `GET /a` の `CL: 2` の 0.3 秒後に `HTTP/1.1 200 OK … X-Injected: yes … EVIL` →
//! 次の `GET /b` に `EVIL` が返り、その次は `HIT`)。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

mod common;
use common::*;

/// 1 本の接続で要求を待ち続けるオリジン。`/a` には応答の 0.3 秒後に頼まれていない応答を足す。
fn start_late_bytes_origin(seen: Arc<Mutex<Vec<String>>>, conns: Arc<AtomicUsize>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            conns.fetch_add(1, Ordering::SeqCst);
            let seen = Arc::clone(&seen);
            thread::spawn(move || {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                    buf.drain(..end);
                    let line = head.lines().next().unwrap_or("").to_string();
                    seen.lock().unwrap().push(line.clone());
                    if line.starts_with("GET /a ") {
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nCache-Control: no-store\r\n\r\nok",
                        );
                        thread::sleep(Duration::from_millis(300));
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nCache-Control: max-age=600\r\n\
                              X-Injected: yes\r\n\r\nEVIL",
                        );
                    } else {
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nCache-Control: max-age=600\r\n\r\nreal_b",
                        );
                    }
                }
            });
        }
    });
    port
}

/// **受け入れ基準**: 遅れて届いた応答は次の要求に配られず、キャッシュにも入らない。
/// `/b` はオリジンが本当に返した `real_b` で、その次は `real_b` の HIT。
#[test]
fn test_integration_late_bytes_on_a_pooled_connection_are_not_served() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let conns = Arc::new(AtomicUsize::new(0));
    let origin_port = start_late_bytes_origin(Arc::clone(&seen), Arc::clone(&conns));
    let proxy_port =
        start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-pool-late-bytes"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = |p: &str| format!("http://{}{}", host, p);

    let a = get_via_proxy(proxy_port, &url("/a"), &host);
    assert!(a.ends_with("\r\n\r\nok"), "{:?}", a);
    // オリジンが頼まれていない応答を送り終えるまで待つ (接続はプールに戻っている)
    thread::sleep(Duration::from_millis(600));

    let b = get_via_proxy(proxy_port, &url("/b"), &host);
    assert!(
        b.ends_with("\r\n\r\nreal_b"),
        "/b に別の応答が配られた: {:?}",
        b
    );
    assert!(!b.contains("X-Injected"), "{:?}", b);
    let b2 = get_via_proxy(proxy_port, &url("/b"), &host);
    assert!(b2.ends_with("\r\n\r\nreal_b"), "{:?}", b2);
    assert!(b2.contains("X-Cache: HIT"), "{:?}", b2);
    assert!(!b2.contains("X-Injected"), "{:?}", b2);

    // `/b` はオリジンへ 1 回だけ行き、読み残しのある接続は捨てて張り直した
    let lines = seen.lock().unwrap().clone();
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("GET /b ")).count(),
        1,
        "{:?}",
        lines
    );
    assert_eq!(conns.load(Ordering::SeqCst), 2);
}
