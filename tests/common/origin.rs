//! モックのオリジン (要求を読み切ってから応答する HTTP のサーバーと、CONNECT の相手のエコー)。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// リクエスト全文と通し番号 (1 始まり) を受け取って応答のバイト列を返す。
pub type Handler = dyn Fn(&str, usize) -> Vec<u8> + Send + Sync;

/// ヘッダー部から `Content-Length` の値を取る (無ければ 0)。名前の大小は無視する。
fn content_length(head: &str) -> usize {
    head.lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0)
}

pub fn start_mock_origin() -> (u16, thread::JoinHandle<()>) {
    start_counting_origin(Arc::new(AtomicUsize::new(0)), "")
}

/// オリジンへの到達回数を数えるモックサーバー。`extra_headers` は追加のレスポンスヘッダー。
pub fn start_counting_origin(
    counter: Arc<AtomicUsize>,
    extra_headers: &'static str,
) -> (u16, thread::JoinHandle<()>) {
    start_origin(
        counter,
        Arc::new(move |_req, _n| {
            let body = "hello from mock origin";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
                body.len(),
                extra_headers,
                body
            )
            .into_bytes()
        }),
    )
}

/// リクエスト全文と通し番号 (1 始まり) を受け取って応答を返すモックサーバー。
pub fn start_origin(
    counter: Arc<AtomicUsize>,
    handler: Arc<Handler>,
) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let counter = Arc::clone(&counter);
            let handler = Arc::clone(&handler);
            thread::spawn(move || {
                // ヘッダー終端まで読み切ってから応答する (読み残しがあると close 時に
                // RST が飛び、プロキシ側でレスポンスが「途中で切れた」扱いになる)
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                // 本文も `Content-Length` ぶん読み切ってからハンドラに渡す。ここで止めると、
                // ヘッダーと本文が別のセグメントで届いたとき (機械が混むと起きる) に
                // 全文から本文が落ちて、エコーするハンドラが空を返す (T8.7)。
                if let Some(head_end) = req.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
                {
                    let head = String::from_utf8_lossy(&req[..head_end]).to_ascii_lowercase();
                    if head.contains("transfer-encoding:") && head.contains("chunked") {
                        // chunked の本文は終端 (0 サイズのチャンク + 空行) まで読み切る。
                        // 読み残すと close のときに RST が飛び、プロキシ側で 502 になる (T19.1)。
                        // 終端は**本文の中だけ**で探す: ヘッダーの最後の行が `0` で終わると
                        // (プロキシの `Via` の印は 16 進なので 16 回に 1 回)、ヘッダーの終わりの
                        // 空行を終端と取り違えて本文の途中で返していた (T.28.37)
                        while !req[head_end..].windows(5).any(|w| w == b"0\r\n\r\n") {
                            match stream.read(&mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => req.extend_from_slice(&buf[..n]),
                            }
                        }
                    } else {
                        let want = head_end + content_length(&head);
                        while req.len() < want {
                            match stream.read(&mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => req.extend_from_slice(&buf[..n]),
                            }
                        }
                    }
                }
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let resp = handler(&String::from_utf8_lossy(&req), n);
                let _ = stream.write_all(&resp);
            });
        }
    });

    (port, handle)
}

/// HTTP/1.1 keep-alive で複数の要求に応答するモックオリジン。接続数と要求数を数える。
pub fn start_keepalive_origin(
    connections: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            connections.fetch_add(1, Ordering::SeqCst);
            let requests = Arc::clone(&requests);
            thread::spawn(move || {
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut lines = Vec::new();
                    loop {
                        let mut line = String::new();
                        if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line.trim().is_empty() {
                            break;
                        }
                        lines.push(line);
                    }
                    if lines.is_empty() {
                        return;
                    }
                    let n = requests.fetch_add(1, Ordering::SeqCst) + 1;
                    let body = format!("response #{}", n);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    if stream.write_all(resp.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (port, handle)
}

/// ETag 付きで常に再検証が必要 (max-age=0) な表現を返し、If-None-Match が一致すれば 304 を返す。
pub fn revalidating_origin(
    counter: Arc<AtomicUsize>,
    fail_after_first: bool,
) -> (u16, thread::JoinHandle<()>) {
    start_origin(
        counter,
        Arc::new(move |req, n| {
            if fail_after_first && n > 1 {
                return b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
            }
            if req.contains("If-None-Match: \"v1\"") {
                return b"HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n"
                    .to_vec();
            }
            let body = "revalidated body";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"v1\"\r\nCache-Control: max-age=0\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    )
}

/// 受け取ったバイトをそのまま返す TCP サーバー (CONNECT トンネルの相手)。
pub fn start_echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut buf = [0u8; 64 * 1024];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = stream.shutdown(std::net::Shutdown::Write);
            });
        }
    });
    port
}

/// 読んだぶんをそのまま返し、クライアントが送信側を閉じたら自分も閉じるオリジン。
///
/// トンネルの「ふつうの終わり方」(クライアントが先に EOF) を待ち時間なしに作るための道具
/// (初めは `tests/recent_test.rs` にあった。T.34.117 で割ったときに、2 つの試験が使うので移した)。
pub fn start_echo_origin() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
                // クライアントが送信側を閉じたので、こちらも閉じる (トンネルが終わる)
            });
        }
    });
    port
}

/// Content-Length ぶんの本文を最後まで読んでから、その要約を返すオリジン。
pub fn start_body_echo_origin() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 64 * 1024];
                loop {
                    // ヘッダー終端まで
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    let len = content_length(&String::from_utf8_lossy(&buf[..split]));
                    let mut body: Vec<u8> = buf[split..].to_vec();
                    while body.len() < len {
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => body.extend_from_slice(&chunk[..n]),
                        }
                    }
                    buf = body.split_off(len);
                    let summary = format!(
                        "{}:{}:{}",
                        body.len(),
                        body.first().copied().unwrap_or(0),
                        body.last().copied().unwrap_or(0)
                    );
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n{}",
                        summary.len(),
                        summary
                    );
                    if stream.write_all(resp.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    port
}

/// keep-alive で受け、同じ接続の 2 本目の要求にだけ 408 を返すオリジン
/// (アイドルタイムアウトで 408 を積んでから閉じる実装を模した形)。
pub fn start_408_on_second_request_origin(requests: Arc<AtomicUsize>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let requests = Arc::clone(&requests);
            thread::spawn(move || {
                let mut stream = stream;
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut on_this_conn = 0usize;
                loop {
                    let mut saw_request = false;
                    loop {
                        let mut line = String::new();
                        if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line.trim().is_empty() {
                            break;
                        }
                        saw_request = true;
                    }
                    if !saw_request {
                        return;
                    }
                    on_this_conn += 1;
                    let n = requests.fetch_add(1, Ordering::SeqCst) + 1;
                    let resp = if on_this_conn == 2 {
                        "HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\n\r\n".to_string()
                    } else {
                        let body = format!("ok {}", n);
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    };
                    if stream.write_all(resp.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    port
}

/// 指定サイズの決まった中身を Content-Length 付きで返すオリジン。
pub fn start_sized_origin(counter: Arc<AtomicUsize>, cacheable: bool) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let counter = Arc::clone(&counter);
            thread::spawn(move || {
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut path = String::new();
                    let mut first = String::new();
                    if std::io::BufRead::read_line(&mut reader, &mut first).unwrap_or(0) == 0 {
                        return;
                    }
                    loop {
                        let mut l = String::new();
                        if std::io::BufRead::read_line(&mut reader, &mut l).unwrap_or(0) == 0 {
                            return;
                        }
                        if l.trim().is_empty() {
                            break;
                        }
                    }
                    path.push_str(first.split_whitespace().nth(1).unwrap_or("/"));
                    let size: usize = path
                        .rsplit('/')
                        .next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    counter.fetch_add(1, Ordering::SeqCst);
                    let body: Vec<u8> = (0..size).map(|i| ((i * 7 + 13) % 251) as u8).collect();
                    let cc = if cacheable { "max-age=60" } else { "no-store" };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: {}\r\n\r\n",
                        size, cc
                    );
                    if stream.write_all(head.as_bytes()).is_err()
                        || stream.write_all(&body).is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    port
}
