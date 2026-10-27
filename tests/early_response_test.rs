//! 要求の本文を送る途中でオリジンが返した応答 (413 など) をクライアントへ渡す (T.29.7)。
//!
//! 8 MiB の POST に、オリジンが先頭だけ読んで `413` + `Connection: close` を返して閉じると、
//! 今までは本文の書き込みが `EPIPE` / `ECONNRESET` で落ちたところで 502 にしていた
//! (RFC 9112 §9.5 は、本文を送り終える前に届いた応答も読むよう求めている)。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

mod common;
use common::*;

/// 要求の先頭だけ読み、本文を読まずに 413 を返して閉じるオリジン。
fn start_early_413_origin(conns: Arc<AtomicUsize>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            conns.fetch_add(1, Ordering::SeqCst);
            thread::spawn(move || {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 3\r\n\
                      Connection: close\r\n\r\nbig",
                );
                thread::sleep(Duration::from_millis(50));
                // 本文を読まずに閉じる (読み残しがあるので相手には RST が行く)
            });
        }
    });
    port
}

/// 本文を別のスレッドで送りながら、応答を閉じるまで読む。
fn post_big(proxy_port: u16, origin_port: u16, chunked: bool) -> String {
    const BODY: usize = 32 * 1024 * 1024;
    let mut stream = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let framing = if chunked {
        "Transfer-Encoding: chunked".to_string()
    } else {
        format!("Content-Length: {BODY}")
    };
    stream
        .write_all(
            format!(
                "POST http://127.0.0.1:{origin_port}/up HTTP/1.1\r\n\
                 Host: 127.0.0.1:{origin_port}\r\n{framing}\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let sender = thread::spawn(move || {
        let block = vec![b'x'; 64 * 1024];
        let mut sent = 0usize;
        while sent < BODY {
            let ok = if chunked {
                writer
                    .write_all(format!("{:x}\r\n", block.len()).as_bytes())
                    .and_then(|_| writer.write_all(&block))
                    .and_then(|_| writer.write_all(b"\r\n"))
            } else {
                writer.write_all(&block)
            };
            if ok.is_err() {
                return;
            }
            sent += block.len();
        }
        if chunked {
            let _ = writer.write_all(b"0\r\n\r\n");
        }
    });
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    drop(stream);
    let _ = sender.join();
    String::from_utf8_lossy(&out).into_owned()
}

/// **受け入れ基準**: 本文の途中でオリジンが返した 413 がそのまま届く (Content-Length の本文と
/// `Connection: close` つき)。chunked の本文でも同じ。
#[test]
fn test_integration_early_response_during_the_request_body_is_relayed() {
    let conns = Arc::new(AtomicUsize::new(0));
    let origin_port = start_early_413_origin(Arc::clone(&conns));
    let proxy_port = start_test_proxy(proxy_config());

    for chunked in [false, true] {
        let resp = post_big(proxy_port, origin_port, chunked);
        assert!(
            resp.starts_with("HTTP/1.1 413 "),
            "chunked={}: {:?}",
            chunked,
            &resp[..resp.len().min(300)]
        );
        assert!(
            resp.ends_with("\r\n\r\nbig"),
            "chunked={}: {:?}",
            chunked,
            resp
        );
        assert!(
            resp.contains("\r\nConnection: close\r\n"),
            "本文の残りを読んでいないので閉じる: {:?}",
            resp
        );
    }
}
