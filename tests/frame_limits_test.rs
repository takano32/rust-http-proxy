//! 応答の先頭・chunk の大きさの行・trailer の長さの上限 (T.29.1)。
//!
//! 要求の先頭は 64 KiB / 128 KiB で 431 にしていたのに、**応答の先頭**と **chunk の大きさの行・
//! trailer の行**は無制限だった。手元で: 空行なしで 300 MB のヘッダーを返すオリジン → RSS 18 → 907 MB、
//! クライアントが chunked の POST で `f` を改行なしに 796 MiB → +793 MiB (オリジン不要)、
//! オリジンの chunk 行 400 MiB → +702 MiB。フォワードプロキシなので宛先 (= 悪いオリジン) は
//! クライアントが選べる。
//!
//! 直した後: 応答の先頭は 128 KiB を越えたら 502、chunk の大きさの行は 4 KiB、trailer は行 8 KiB・
//! 合計 64 KiB を越えたら、クライアントの本文なら 400、オリジンの本文なら切る (先頭はもう出ている)。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

mod common;
use common::*;

/// 悪い形で流す量 (直す前はこのぶん以上 RSS が増えた)。
const FLOOD: usize = 64 * 1024 * 1024;

/// パスごとに悪い応答を流すオリジン。書けなくなる (プロキシが閉じる) か `FLOOD` を流し終えたら閉じる。
fn start_flood_origin() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                let (prefix, unit): (&[u8], &[u8]) = match path.as_str() {
                    // 空行の来ないヘッダー
                    "/head" => (
                        b"HTTP/1.1 200 OK\r\n",
                        b"X-Pad: 0123456789abcdef0123456789abcdef0123456789abcdef\r\n",
                    ),
                    // 改行の来ない chunk の大きさの行
                    "/chunk" => (
                        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
                        b"ffffffffffffffff",
                    ),
                    // 改行の来ない trailer の行
                    "/trailer" => (
                        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                          5\r\nhello\r\n0\r\nX-Trailer: ",
                        b"aaaaaaaaaaaaaaaa",
                    ),
                    // 64 KiB のヘッダーは正当 (通る)
                    "/big-ok" => {
                        let mut v = b"HTTP/1.1 200 OK\r\nX-Big: ".to_vec();
                        v.extend(std::iter::repeat_n(b'a', 64 * 1024));
                        v.extend_from_slice(
                            b"\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                        let _ = stream.write_all(&v);
                        return;
                    }
                    // 本文を最後まで読まずに 200 を返す (クライアントの本文の試験用)
                    _ => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                        thread::sleep(Duration::from_secs(2));
                        return;
                    }
                };
                if stream.write_all(prefix).is_err() {
                    return;
                }
                let block: Vec<u8> = unit.iter().copied().cycle().take(64 * 1024).collect();
                let mut sent = 0usize;
                while sent < FLOOD {
                    if stream.write_all(&block).is_err() {
                        return;
                    }
                    sent += block.len();
                }
            });
        }
    });
    port
}

fn request(origin_port: u16, path: &str) -> String {
    format!(
        "GET http://127.0.0.1:{origin_port}{path} HTTP/1.1\r\nHost: 127.0.0.1:{origin_port}\r\n\
         Connection: close\r\n\r\n"
    )
}

/// 応答を閉じるまで (多くても `cap` バイト) 読む。
fn read_capped(stream: &mut TcpStream, cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    while out.len() < cap {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    out
}

/// **受け入れ基準 (状態コード)**: 先頭が長すぎる応答は 502、chunk 行・trailer が長すぎる応答は
/// 先頭のあとで切れる (終端の chunk が来ない)、クライアントの長すぎる chunk 行は 400。
/// 64 KiB の正当な先頭は通る。
#[test]
fn test_integration_oversized_frames_get_fixed_status_codes() {
    let origin_port = start_flood_origin();
    let proxy_port = start_test_proxy(proxy_config());

    // 先頭が 128 KiB を越える → 502 (本文なし)
    let resp = raw_get(proxy_port, &request(origin_port, "/head"));
    assert!(
        resp.starts_with("HTTP/1.1 502 "),
        "{:?}",
        &resp[..resp.len().min(200)]
    );
    assert!(!resp.contains("X-Pad"), "オリジンのヘッダーが漏れた");

    // chunk の大きさの行・trailer が長すぎる → 先頭は出ているので切るだけ (終端の chunk は来ない)
    for path in ["/chunk", "/trailer"] {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(request(origin_port, path).as_bytes())
            .unwrap();
        let raw = read_capped(&mut stream, 1024 * 1024);
        let text = String::from_utf8_lossy(&raw);
        assert!(text.starts_with("HTTP/1.1 200 "), "{}: {:?}", path, text);
        assert!(
            !text.ends_with("0\r\n\r\n"),
            "{}: 切れた本文が完結して見える: {:?}",
            path,
            text
        );
        assert!(raw.len() < 64 * 1024, "{}: {} B 届いた", path, raw.len());
    }

    // 64 KiB の先頭は正当なので通る
    let resp = raw_get(proxy_port, &request(origin_port, "/big-ok"));
    assert!(
        resp.starts_with("HTTP/1.1 200 "),
        "{:?}",
        &resp[..resp.len().min(200)]
    );
    assert!(
        resp.ends_with("\r\n\r\nok"),
        "{:?}",
        &resp[resp.len().saturating_sub(64)..]
    );

    // クライアントの chunked の本文で、改行の無い大きさの行 (4 KiB を越える) → 400
    let mut req = format!(
        "POST http://127.0.0.1:{origin_port}/post HTTP/1.1\r\nHost: 127.0.0.1:{origin_port}\r\n\
         Transfer-Encoding: chunked\r\n\r\n"
    )
    .into_bytes();
    req.extend(std::iter::repeat_n(b'f', 4200));
    let resp = raw_request(proxy_port, &req);
    assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);
}

/// 今のプロセスの RSS の山 (`VmHWM`、KiB)。
fn peak_rss_kib() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    s.lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap()
}

/// **受け入れ基準 (メモリ)**: 4 つの形で 64 MiB ずつ流しても、プロキシ (= この試験のプロセス) の
/// RSS の山が数 MiB しか増えない。直す前はどれも 64 MiB 以上増えた。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_oversized_frames_do_not_grow_memory() {
    let origin_port = start_flood_origin();
    let proxy_port = start_test_proxy(proxy_config());
    // 待ち受けとオリジンのスレッドが立ち上がったぶんを先に払う
    let _ = raw_get(proxy_port, &request(origin_port, "/big-ok"));
    let before = peak_rss_kib();

    for path in ["/head", "/chunk", "/trailer"] {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        stream
            .write_all(request(origin_port, path).as_bytes())
            .unwrap();
        let _ = read_capped(&mut stream, FLOOD);
    }
    // クライアントが chunked の POST で `f` を改行なしに流す (オリジンは本文を読まない)
    let mut stream = TcpStream::connect(format!("127.0.0.1:{proxy_port}")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream
        .write_all(
            format!(
                "POST http://127.0.0.1:{origin_port}/post HTTP/1.1\r\n\
                 Host: 127.0.0.1:{origin_port}\r\nTransfer-Encoding: chunked\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let block = vec![b'f'; 64 * 1024];
    let mut sent = 0usize;
    while sent < FLOOD {
        if stream.write_all(&block).is_err() {
            break;
        }
        sent += block.len();
    }
    let _ = read_capped(&mut stream, 1024 * 1024);

    let grown = peak_rss_kib().saturating_sub(before);
    eprintln!("peak RSS grew by {} KiB", grown);
    assert!(
        grown < 16 * 1024,
        "RSS の山が {} KiB 増えた (上限が効いていない)",
        grown
    );
}
