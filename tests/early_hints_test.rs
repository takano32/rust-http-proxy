//! 1xx の中間応答 (`103 Early Hints`) をクライアントへ中継する (T19.6)。
//!
//! RFC 9110 §15.2 は、自分が起こした要求でない限り 1xx を中継することを求めている
//! (101 は除く)。`0.26.1` までは読み捨てていたので、オリジンが `103` → `200` と返しても
//! クライアントには `200` だけが届いていた。ここでは**クライアントが受け取った生バイト列**を
//! 固定する:
//!
//! 1. 1xx はヘッダー部だけが中継され、本文を持たない (何本でもよい)
//! 2. 整形は最終応答と同じ (hop-by-hop を落とす・obs-fold は SP 1 個に畳む)
//! 3. `100 Continue` と `101 Switching Protocols` は中継しない
//! 4. 1xx が無いときの応答は 1 バイトも変わらない
//! 5. `/recent` とホスト別の統計に乗るのは最終応答の 1 本だけ

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::thread;
use std::time::{Duration, Instant};

use common::*;

/// オリジンが返す「100 → 103 → 103 (折り返しつき) → 200」の並び。1 回の write でまとめて送る
/// (プロキシが 1 つのバッファから 4 本の先頭を続けて読む形も通す)。
const HINTS: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n\
HTTP/1.1 103 Early Hints\r\n\
Link: </s.css>; rel=preload; as=style\r\n\
Keep-Alive: timeout=5\r\n\
Connection: keep-alive\r\n\r\n\
HTTP/1.1 103 Early Hints\r\n\
X-A: 1\r\n 2\r\n\r\n\
HTTP/1.1 200 OK\r\n\
X-Foo: bar\r\n\
Content-Length: 2\r\n\
Connection: close\r\n\r\nok";

/// 生のバイト列を 1 本の接続へ投げ、閉じるまで読む。
fn raw_bytes(proxy_port: u16, request: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out);
    out
}

/// オリジン形式の GET を 1 本 (`Connection: close`)。
fn get_request(origin_port: u16) -> String {
    format!(
        "GET http://127.0.0.1:{}/x HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        origin_port, origin_port
    )
}

/// 103 → 200 のオリジンで、クライアントが 103 と 200 の 2 本を受け取る (100 は落ちる)。
#[test]
fn test_integration_early_hints_are_relayed() {
    let (origin_port, _handle) = start_origin(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(|_req: &str, _n: usize| HINTS.to_vec()),
    );
    let proxy_port = start_test_proxy(proxy_config());

    let got = raw_bytes(proxy_port, &get_request(origin_port));

    // 1 バイトも違わずに固定する。`100 Continue` は出さず、1xx の `Connection` /
    // `Keep-Alive` は落とし、`X-A: 1` + ` 2` は SP 1 個に畳む
    // T.28.10 (b) から最終応答に自分の `Via` が 1 行付く (1xx には付けない)
    let expected = format!(
        "HTTP/1.1 103 Early Hints\r\n\
Link: </s.css>; rel=preload; as=style\r\n\
\r\n\
HTTP/1.1 103 Early Hints\r\n\
X-A: 1 2\r\n\
\r\n\
HTTP/1.1 200 OK\r\n\
X-Foo: bar\r\n\
Content-Length: 2\r\n\
Via: {}\r\n\
Connection: close\r\n\
\r\n\
ok",
        rust_http_proxy::via::token()
    );
    // T.29.9 から最終応答に `Date` が 1 行付く (オリジンが付けていないので。1xx には付けない)
    let got = without_date(&String::from_utf8_lossy(&got)).into_bytes();
    assert_eq!(
        String::from_utf8_lossy(&got),
        expected,
        "クライアントが受け取ったバイト列が違う: {:?}",
        got
    );
    assert_eq!(got, expected.as_bytes(), "バイト列が違う: {:?}", got);

    // 記録は最終応答の 1 本だけ (1xx で二重に数えない)。
    // 個票は**応答を返したあと**に書かれるので、値で待つ (sleep を伸ばさない)
    let recorded_once = || {
        let recent = endpoint_json(proxy_port, "/recent");
        recent.matches("\"status\":200").count() == 1
            && !recent.contains("\"status\":103")
            && !recent.contains("\"status\":100")
    };
    wait_until(recorded_once, "/recent records the final response once");
    assert!(
        recorded_once(),
        "/recent に最終応答が 1 件だけ乗るはず: {}",
        endpoint_json(proxy_port, "/recent")
    );

    // ホスト別の統計も 1 要求 = 1 本 (1xx は数えない)
    let host_row = format!(
        "\"host\":\"http://127.0.0.1:{}\",\"requests\":1",
        origin_port
    );
    wait_until(
        || endpoint_json(proxy_port, "/status").contains(&host_row),
        "host statistics count the final response once",
    );
    let status = endpoint_json(proxy_port, "/status");
    assert!(
        status.contains(&format!("\"host\":\"http://127.0.0.1:{}\"", origin_port)),
        "オリジンの行が無い: {}",
        status
    );
}

/// T.29.3: HTTP/1.0 のクライアントへは 1xx を送らない (RFC 9110 §15.2 の MUST NOT。1.0 の
/// クライアントは 103 を最終応答と読む)。読み捨てて、最初の状態行が最終応答の 200 になる。
#[test]
fn test_integration_interim_responses_are_not_sent_to_http10_clients() {
    let (origin_port, _handle) = start_origin(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(|_req: &str, _n: usize| HINTS.to_vec()),
    );
    let proxy_port = start_test_proxy(proxy_config());

    let got = raw_bytes(
        proxy_port,
        &format!(
            "GET http://127.0.0.1:{}/x HTTP/1.0\r\nHost: 127.0.0.1:{}\r\n\r\n",
            origin_port, origin_port
        ),
    );
    let text = String::from_utf8_lossy(&got);
    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{:?}", text);
    assert!(!text.contains(" 103 "), "1xx が届いた: {:?}", text);
    assert!(text.ends_with("\r\n\r\nok"), "{:?}", text);
}

/// 1xx が無い応答は 1 バイトも変わらない (生バイト列で固定)。
#[test]
fn test_integration_response_without_interim_is_unchanged() {
    let plain: &[u8] =
        b"HTTP/1.1 200 OK\r\nX-Foo: bar\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";
    let (origin_port, _handle) = start_origin(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(move |_req: &str, _n: usize| plain.to_vec()),
    );
    let proxy_port = start_test_proxy(proxy_config());

    let got = raw_bytes(proxy_port, &get_request(origin_port));
    // T.29.9 から `Date` の無い応答には `Date` が 1 行付く (外して比べる)
    let got = without_date(&String::from_utf8_lossy(&got)).into_bytes();
    // T.28.10 (b) から自分の `Via` が 1 行だけ増える (ほかは 1 バイトも変わらない)
    let expected = format!(
        "HTTP/1.1 200 OK\r\nX-Foo: bar\r\nContent-Length: 5\r\nVia: {}\r\nConnection: close\r\n\r\nhello",
        rust_http_proxy::via::token()
    );
    assert_eq!(got, expected.as_bytes(), "バイト列が違う: {:?}", got);
}

/// `Connection: keep-alive` のオリジンでは、103 は 200 を待たずにクライアントへ届く。
///
/// 1 つの `BufWriter` に溜めたまま 200 と一緒に出すと、`103` の意味 (先に資源を
/// 取りに行かせる) が消える。ここではオリジンが 103 と 200 の間に 700 ms 置くので、
/// **最初に読める先頭が 103 であること**で flush を固定する。
#[test]
fn test_integration_early_hints_are_flushed_before_the_final_response() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let _ = stream
                    .write_all(b"HTTP/1.1 103 Early Hints\r\nLink: </a.css>; rel=preload\r\n\r\n");
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(700));
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            });
        }
    });
    let proxy_port = start_test_proxy(proxy_config());

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(get_request(origin_port).as_bytes())
        .unwrap();

    // 最初の 1 本 (空行まで) を読む。200 は 700 ms 後にしか来ないので、ここが
    // 103 でなければ「溜めてから出した」ことになる
    let started = Instant::now();
    let mut first = Vec::new();
    let mut byte = [0u8; 1];
    while !first.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).unwrap() == 0 {
            break;
        }
        first.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&first).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 103 Early Hints\r\n"),
        "最初に届いたのが 103 でない: {:?}",
        head
    );
    assert!(
        !head.contains(" 200 "),
        "200 と一緒に出てはいけない: {:?}",
        head
    );
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "103 が 200 を待っている ({:?})",
        started.elapsed()
    );

    // 続きの 200 もちゃんと届く
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest);
    let rest = String::from_utf8_lossy(&rest).into_owned();
    assert!(rest.starts_with("HTTP/1.1 200 OK\r\n"), "{:?}", rest);
    assert!(rest.ends_with("\r\n\r\nok"), "{:?}", rest);
}
