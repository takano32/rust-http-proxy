//! クライアント側の読み書き (要求を投げて応答を読む、自分宛ての口を引く、待つ)。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

/// 待つループの刻み。[`POLL_MAX`] 回で 10 秒 (負荷の高い CI でも落ちない幅)。
pub const POLL_STEP: Duration = Duration::from_millis(20);
/// 待つループの回数の上限 ([`wait_until`] と [`endpoint_json`] の引き直しが共有する)。
pub const POLL_MAX: usize = 500;

/// ヘッダーの終わり (空行) まで 1 バイトずつ読む (本文を 1 バイトも読み過ごさないため)。
///
/// 相手が先に閉じたら、そこまでの分を返す。
pub fn read_head(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).unwrap() == 0 {
            break;
        }
        buf.push(byte[0]);
    }
    buf
}

/// 1 本の接続で複数の要求を送るための、Content-Length 付き応答の読み取り。
pub fn read_response(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let head = String::from_utf8_lossy(&read_head(stream)).into_owned();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).unwrap();
    (head, body)
}

/// `/status` の JSON を取る (テストが「状態が落ち着いたか」を見るため)。
pub fn status_json(proxy_port: u16) -> String {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    // オリジン形式が自分宛てになるのは `Host` のポートが待ち受けと同じときだけ (T12.3)
    stream
        .write_all(
            format!(
                "GET /status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                proxy_port
            )
            .as_bytes(),
        )
        .unwrap();
    let mut out = String::new();
    let _ = stream.read_to_string(&mut out);
    out
}

/// 自分宛ての GET を 1 本投げて**本文だけ**を返す (`/errors` `/connections` `/dns` …。T13.4)。
///
/// オリジン形式が自分宛てになるのは `Host` のポートが待ち受けと同じときだけ (T12.3)。
pub fn endpoint_json(proxy_port: u16, path: &str) -> String {
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        path, proxy_port
    );
    // **重い口は 1 本ずつ** (T14.51) なので、同じテストバイナリで並行に走っている別のテストが
    // `/snapshot` や `/explain` を引いている間は `503 {"error":"busy"}` が返る。
    // 実装の誤りではないので、`wait_until` と同じ幅だけ待って引き直す (T14.55)
    let mut out = raw_get(proxy_port, &req);
    for _ in 0..POLL_MAX {
        if !(out.starts_with("HTTP/1.1 503 ") && out.contains("\"busy\"")) {
            break;
        }
        thread::sleep(POLL_STEP);
        out = raw_get(proxy_port, &req);
    }
    assert!(out.starts_with("HTTP/1.1 200 "), "{} -> {}", path, out);
    match out.split_once("\r\n\r\n") {
        Some((_, body)) => body.to_string(),
        None => String::new(),
    }
}

/// `cond` が真になるまで最大 10 秒待つ (負荷の高い CI でも落ちない幅)。
pub fn wait_until(cond: impl Fn() -> bool, what: &str) {
    for _ in 0..POLL_MAX {
        if cond() {
            return;
        }
        thread::sleep(POLL_STEP);
    }
    panic!("timed out waiting: {}", what);
}

pub fn get_via_proxy(proxy_port: u16, url: &str, host: &str) -> String {
    get_via_proxy_with(proxy_port, url, host, "")
}

/// `extra` は追加のリクエストヘッダー行 (CRLF 終端)。
pub fn get_via_proxy_with(proxy_port: u16, url: &str, host: &str, extra: &str) -> String {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n{}\r\n",
        url, host, extra
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

/// `host` 宛ての要求を 1 本送って応答を読む (keep-alive のまま接続は開けておく)。
pub fn one_keepalive_request(stream: &mut TcpStream, host: &str, path: &str) -> (String, Vec<u8>) {
    let req = format!(
        "GET http://{}{} HTTP/1.1\r\nHost: {}\r\n\r\n",
        host, path, host
    );
    stream.write_all(req.as_bytes()).unwrap();
    read_response(stream)
}

/// CONNECT の 200 応答を読み切る。
pub fn read_connect_response(stream: &mut TcpStream) -> String {
    String::from_utf8_lossy(&read_head(stream)).into_owned()
}

/// `127.0.0.1:<target_port>` 宛ての CONNECT の要求文 (T.34.108 で 1 つにした)。
pub fn connect_request(target_port: u16) -> String {
    format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        target_port, target_port
    )
}

/// CONNECT を張って `200` まで読む (テストの定型。読みの締め切りは 10 秒)。
pub fn open_tunnel(proxy_port: u16, target_port: u16) -> TcpStream {
    open_tunnel_with(proxy_port, target_port, Duration::from_secs(10), None)
}

/// [`open_tunnel`] の締め切りを選べる版 (長く握る試験は読みを 20 秒に、送りっぱなしの
/// 試験は書きの締め切りも付ける)。`200` でなければ落ちる。
pub fn open_tunnel_with(
    proxy_port: u16,
    target_port: u16,
    read_timeout: Duration,
    write_timeout: Option<Duration>,
) -> TcpStream {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    stream.set_read_timeout(Some(read_timeout)).unwrap();
    if write_timeout.is_some() {
        stream.set_write_timeout(write_timeout).unwrap();
    }
    stream
        .write_all(connect_request(target_port).as_bytes())
        .unwrap();
    let head = read_connect_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    stream
}

/// 1 本の接続に生のバイト列を投げ、応答 (あれば) と接続が閉じたかを返す (読みの締め切り 3 秒)。
pub fn raw_request(proxy_port: u16, bytes: &[u8]) -> String {
    raw_request_with(proxy_port, bytes, Duration::from_secs(3), false)
}

/// 生のバイト列を 1 本の接続に投げて、相手が閉じるまで (閉じなければ `timeout` まで) 読む
/// (T.34.109 で各試験の `send_raw` をここへ寄せた)。
///
/// 書き込みの失敗は無視する: 長すぎるヘッダーや壊れた要求は、相手が途中で閉じることがある。
/// `half_close` なら書き終えたと伝える (向こうが読み捨ての待ち (`linger`) で止まらないように)。
pub fn raw_request_with(port: u16, bytes: &[u8], timeout: Duration, half_close: bool) -> String {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
    stream.set_read_timeout(Some(timeout)).unwrap();
    let _ = stream.write_all(bytes);
    if half_close {
        let _ = stream.shutdown(std::net::Shutdown::Write);
    }
    let mut out = Vec::new();
    // 相手が閉じるまで読む (閉じなければタイムアウトで抜ける)
    let _ = stream.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

pub fn get_body_via_proxy(proxy_port: u16, origin_port: u16, size: usize) -> Vec<u8> {
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let req = format!(
        "GET http://127.0.0.1:{}/b/{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        origin_port, size, origin_port
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut all = Vec::new();
    s.read_to_end(&mut all).unwrap();
    let split = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert!(
        String::from_utf8_lossy(&all[..split]).starts_with("HTTP/1.1 200 OK"),
        "{}",
        String::from_utf8_lossy(&all[..split.min(200)])
    );
    all[split..].to_vec()
}

/// `/status` の JSON から整数の指標を 1 つ取る (`"key":123` の 123)。
pub fn status_number(json: &str, key: &str) -> u64 {
    let pat = format!("\"{}\":", key);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("no {} in {}", key, json))
        + pat.len();
    json[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{} is not a number in {}", key, json))
}

/// 1 本の接続で 1 要求を送り、応答を全部読む (`Connection: close`)。
pub fn raw_get(proxy_port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = stream.read_to_string(&mut out);
    out
}

/// 応答からプロキシが付けた `Date: <IMF-fixdate>` の行を 1 本だけ取り除く (T.29.9)。
///
/// `Date` の無い応答には転送のときに今の時刻の `Date` が付くので、バイト列で固定する試験は
/// これを通してから比べる。**ちょうど 1 本あること**もここで確かめる。
#[allow(dead_code)]
pub fn without_date(resp: &str) -> String {
    let n = resp.matches("\r\nDate: ").count();
    assert_eq!(n, 1, "Date の行がちょうど 1 本でない: {:?}", resp);
    let at = resp.find("\r\nDate: ").unwrap();
    let line_end = at + 2 + "Date: ".len() + 29;
    assert!(
        rust_http_proxy::httpdate::parse(&resp[at + 8..line_end]).is_some(),
        "読めない Date: {:?}",
        resp
    );
    format!("{}{}", &resp[..at], &resp[line_end..])
}
