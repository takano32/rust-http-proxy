//! 応答の `Content-Length` を検査する (T19.5)。
//!
//! `0.26.0` の実機では、オリジンが壊れた `Content-Length` を返すと**そのままクライアントへ
//! 届いていた**:
//!
//! - `Content-Length: 5` と `6` の 2 本 → CL は `6` の 1 本になり、本文は 5 バイトだけ
//!   (クライアントは 6 バイト待って止まる)
//! - `Content-Length: 10` を名乗って 3 バイトだけ送って閉じる → CL は `10` のまま 3 バイト
//!
//! ここでは**状態コードと、実際にクライアントへ届いたバイト数**の両方を固定する。
//! 直ったあとはどちらも 502 (本文 0 バイト) になり、`/status` の失敗の数え方に乗る。
//! 正常な応答が 1 バイトも変わらないことも同じ試験で見る。

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

mod common;
use common::*;

/// パスごとに決まった応答を 1 本返して閉じるオリジン。
///
/// `Connection: close` を付けて返すので、プロキシはこの接続をプールに戻さない
/// (次の要求が死んだ接続を掴んで再試行する分の揺れを無くす)。
fn start_scripted_origin() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                stream
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                // 要求の先頭を読み切ってから、パスを見て応答を選ぶ
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
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
                let response: Vec<u8> = match path.as_str() {
                    // 正常: CL 5 と 5 バイト
                    "/ok" => {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
                            .to_vec()
                    }
                    // 正常: 重複した CL が同値 (1 本に畳んでよい)
                    "/dup-same" => b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 5\r\n\
                          Connection: close\r\n\r\nhello"
                        .to_vec(),
                    // 正常: chunked (CL は付かない)
                    "/chunked" => {
                        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\
                          \r\n5\r\nhello\r\n0\r\n\r\n"
                            .to_vec()
                    }
                    // 壊れている: 重複した CL が不一致 (5 と 6)
                    "/dup" => b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 6\r\n\
                          Connection: close\r\n\r\nhello"
                        .to_vec(),
                    // 壊れている: 名乗った 10 バイトに足りない 3 バイトで閉じる
                    "/short" => {
                        let mut v =
                            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc"
                                .to_vec();
                        let _ = stream.write_all(&v);
                        let _ = stream.flush();
                        let _ = stream.shutdown(Shutdown::Both);
                        v.clear();
                        v
                    }
                    // 壊れている: 手元のバッファ (32 KiB) を超えるので素通しで流す大きさ
                    "/big-short" => {
                        let mut v =
                            b"HTTP/1.1 200 OK\r\nContent-Length: 40000\r\nConnection: close\r\n\r\n"
                                .to_vec();
                        v.extend(std::iter::repeat_n(b'x', 1000));
                        let _ = stream.write_all(&v);
                        let _ = stream.flush();
                        let _ = stream.shutdown(Shutdown::Both);
                        v.clear();
                        v
                    }
                    // 壊れている: 数字でない / 負 / u64 に収まらない桁数
                    "/bad-num" => {
                        b"HTTP/1.1 200 OK\r\nContent-Length: abc\r\nConnection: close\r\n\r\nhello"
                            .to_vec()
                    }
                    "/negative" => {
                        b"HTTP/1.1 200 OK\r\nContent-Length: -1\r\nConnection: close\r\n\r\nhello"
                            .to_vec()
                    }
                    "/huge" => b"HTTP/1.1 200 OK\r\nContent-Length: 999999999999999999999\r\n\
                          Connection: close\r\n\r\nhello"
                        .to_vec(),
                    _ => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                if !response.is_empty() {
                    let _ = stream.write_all(&response);
                    let _ = stream.flush();
                    let _ = stream.shutdown(Shutdown::Both);
                }
            });
        }
    });
    port
}

/// `/status` のホスト別の行の `errors_by_cause` (10 要素) を数として読む。
fn host_causes(status: &str, key: &str) -> Vec<u64> {
    let pat = format!("{{\"host\":\"{}\",", key);
    let at = status
        .find(&pat)
        .unwrap_or_else(|| panic!("ホスト別の行が無い: {} / {}", key, status));
    let row = &status[at..];
    let head = "\"errors_by_cause\":[";
    let start = row.find(head).expect("errors_by_cause が無い") + head.len();
    let end = start + row[start..].find(']').expect("errors_by_cause が閉じない");
    row[start..end]
        .split(',')
        .map(|v| v.trim().parse::<u64>().expect("数でない"))
        .collect()
}

/// プロキシ経由で 1 本取りに行く (`Connection: close`)。
fn get(proxy_port: u16, origin_port: u16, path: &str) -> String {
    get_via_proxy(
        proxy_port,
        &format!("http://127.0.0.1:{}{}", origin_port, path),
        &format!("127.0.0.1:{}", origin_port),
    )
}

/// 応答ヘッダーと、そこに書いてある `Content-Length` を分けて返す。
fn split_response(resp: &str) -> (&str, Option<usize>, &str) {
    match resp.split_once("\r\n\r\n") {
        Some((head, body)) => {
            let cl = head.lines().find_map(|l| {
                l.strip_prefix("Content-Length: ")
                    .and_then(|v| v.trim().parse::<usize>().ok())
            });
            (head, cl, body)
        }
        None => (resp, None, ""),
    }
}

/// **受け入れ基準**: 重複した CL が不一致なら 502 で、中途半端な CL を出さない。
#[test]
fn test_integration_conflicting_content_length_is_502() {
    let origin_port = start_scripted_origin();
    let proxy_port = start_test_proxy(proxy_config());

    let resp = get(proxy_port, origin_port, "/dup");
    let (head, cl, body) = split_response(&resp);
    assert!(head.starts_with("HTTP/1.1 502 "), "{:?}", resp);
    // 名乗る長さと実バイトが食い違う応答を残さない (0 バイトだけ)
    assert_eq!(cl, Some(0), "{:?}", head);
    assert_eq!(body.len(), 0, "本文が残っている: {:?}", body);
    assert!(
        !resp.contains("hello"),
        "オリジンの本文が漏れた: {:?}",
        resp
    );

    // 失敗の数え方に乗る (`/status` の errors_by_cause の 8 番目 = other)
    let key = format!("http://127.0.0.1:{}", origin_port);
    let status = status_json(proxy_port);
    let causes = host_causes(&status, &key);
    assert_eq!(causes.len(), 10, "{:?}", causes);
    assert_eq!(causes[7], 1, "other が 1 でない: {:?}", causes);
    // 個票 (`/errors`) にも 502 と原因が 1 件残る
    let errors = endpoint_json(proxy_port, "/errors?n=10");
    assert!(errors.contains("\"status\":502"), "{}", errors);
    assert!(errors.contains("\"cause\":\"other\""), "{}", errors);
    let prom = endpoint_json(proxy_port, "/metrics");
    assert!(
        prom.contains(r#"sorahost_errors_total{cause="other"} 1"#),
        "{}",
        prom
    );
}

/// **受け入れ基準**: 名乗った CL より短い本文で閉じたら 502 (ヘッダーはまだ出していない)。
#[test]
fn test_integration_short_body_is_502() {
    let origin_port = start_scripted_origin();
    let proxy_port = start_test_proxy(proxy_config());

    let resp = get(proxy_port, origin_port, "/short");
    let (head, cl, body) = split_response(&resp);
    assert!(head.starts_with("HTTP/1.1 502 "), "{:?}", resp);
    assert_eq!(cl, Some(0), "{:?}", head);
    assert_eq!(
        body.len(),
        0,
        "読めた 3 バイトを出してはいけない: {:?}",
        body
    );
    assert!(!resp.contains("abc"), "{:?}", resp);

    // 原因は既存の 8 つのうち `reset` (EOF で切られた)
    let key = format!("http://127.0.0.1:{}", origin_port);
    let status = status_json(proxy_port);
    let causes = host_causes(&status, &key);
    assert_eq!(causes[4], 1, "reset が 1 でない: {:?}", causes);
    let prom = endpoint_json(proxy_port, "/metrics");
    assert!(
        prom.contains(r#"sorahost_errors_total{cause="reset"} 1"#),
        "{}",
        prom
    );
}

/// 数字でない・負・u64 に収まらない桁数の CL も、素通しせず 502 にする。
#[test]
fn test_integration_broken_content_length_forms_are_502() {
    let origin_port = start_scripted_origin();
    let proxy_port = start_test_proxy(proxy_config());

    for path in ["/bad-num", "/negative", "/huge"] {
        let resp = get(proxy_port, origin_port, path);
        let (head, cl, body) = split_response(&resp);
        assert!(head.starts_with("HTTP/1.1 502 "), "{}: {:?}", path, resp);
        assert_eq!(cl, Some(0), "{}: {:?}", path, head);
        assert_eq!(body.len(), 0, "{}: {:?}", path, body);
    }
}

/// **受け入れ基準**: 正常な応答は 1 バイトも変わらない (CL・重複同値・chunked)。
#[test]
fn test_integration_good_responses_are_unchanged() {
    let origin_port = start_scripted_origin();
    let proxy_port = start_test_proxy(proxy_config());

    // CL 1 本の応答: 状態行・ヘッダー・本文まで含めて今までどおりのバイト列
    assert_eq!(
        get(proxy_port, origin_port, "/ok"),
        "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
    );
    // 重複した CL が同値なら 1 本に畳んで、同じバイト列になる
    assert_eq!(
        get(proxy_port, origin_port, "/dup-same"),
        "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
    );
    // chunked は今までどおり再 chunk (CL は付けない)
    assert_eq!(
        get(proxy_port, origin_port, "/chunked"),
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
         5\r\nhello\r\n0\r\n\r\n"
    );

    // クライアントが keep-alive を求めたら、今までどおりそのまま使い回せる
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let url = format!("http://127.0.0.1:{}/ok", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);
    for round in 0..2 {
        stream
            .write_all(format!("GET {} HTTP/1.1\r\nHost: {}\r\n\r\n", url, host).as_bytes())
            .unwrap();
        let (head, body) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200 "), "{}: {:?}", round, head);
        assert_eq!(body, b"hello", "{} 本目", round);
        assert!(
            head.contains("\r\nConnection: keep-alive\r\n"),
            "{} 本目: {:?}",
            round,
            head
        );
    }
}

/// 手元のバッファ (32 KiB) を超える本文は素通しで流すので、ヘッダーのあとに切れたら
/// **接続を閉じる**しかない。中途半端な CL のまま keep-alive を続けないことを見る。
#[test]
fn test_integration_truncated_large_body_closes_the_connection() {
    let origin_port = start_scripted_origin();
    let proxy_port = start_test_proxy(proxy_config());

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(
            format!(
                "GET http://127.0.0.1:{}/big-short HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                origin_port, origin_port
            )
            .as_bytes(),
        )
        .unwrap();
    // プロキシが閉じるまで読む (閉じないとタイムアウトでここが失敗する)
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("プロキシが閉じない");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, cl, body) = split_response(&text);
    assert!(head.starts_with("HTTP/1.1 200 "), "{:?}", head);
    // 宣言は 40000 のまま (ヘッダーはもう出ている) だが、届いたのは 1000 バイトだけ。
    // クライアントは「閉じたのに足りない」で切れたと分かる
    assert_eq!(cl, Some(40000), "{:?}", head);
    assert_eq!(body.len(), 1000, "届いた本文: {}", body.len());
}
