//! 届いた要求を貯めるオリジンと、黙るオリジン (T34.110 で各試験から寄せた)。

use std::io::Read;
use std::net::TcpListener;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::thread;

use super::{Handler, start_origin};

/// 届いた要求の全文 (届いた順)。
pub type Captured = Arc<Mutex<Vec<String>>>;

/// `Content-Length: 2` の `ok` を返して閉じる応答 (要求を貯めるオリジンの既定)。
pub const OK_CLOSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

/// `handler` の前に、届いた要求の全文を `seen` に積むハンドラを作る (`start_origin` に渡す)。
pub fn capturing(
    seen: Captured,
    handler: impl Fn(&str, usize) -> Vec<u8> + Send + Sync + 'static,
) -> Arc<Handler> {
    Arc::new(move |req, n| {
        seen.lock().unwrap().push(req.to_string());
        handler(req, n)
    })
}

/// 受け取った要求の全文を貯め、[`OK_CLOSE`] を返すオリジン。
pub fn capture_origin() -> (u16, Captured) {
    capture_origin_replying(OK_CLOSE)
}

/// 受け取った要求の全文を貯め、いつも `response` を返すオリジン。
pub fn capture_origin_replying(response: &'static [u8]) -> (u16, Captured) {
    let seen: Captured = Arc::new(Mutex::new(Vec::new()));
    let (port, _handle) = start_origin(
        Arc::new(AtomicUsize::new(0)),
        capturing(Arc::clone(&seen), move |_req, _n| response.to_vec()),
    );
    (port, seen)
}

/// 印の要求の道 ([`settle_with_mark`])。
pub const SETTLE_MARK: &str = "/t34114-settle-mark";

/// 「断った要求がオリジンへ届いていない」を見る前の合図 (T34.114。固定の sleep の代わり)。
///
/// 同じオリジンへ印の要求 (`GET /t34114-settle-mark`) を 1 本通し、その 200 を受け取る。
/// 要求を貯めるオリジンは応答を返す前に積むので、200 が返った時点で印は `seen` にある。
/// 断った要求が誤って転送されていたなら、プロキシはそれをクライアントへ返す前に送り終えて
/// いるので、往復ぶん遅れて届く印より先にオリジンへ着いて積まれている。
/// 印の行は `seen` から取り除くので、呼んだあとの `seen` は前と同じ形で見られる。
pub fn settle_with_mark(proxy_port: u16, origin_port: u16, seen: &Mutex<Vec<String>>) {
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}{}", host, SETTLE_MARK);
    let resp = super::get_via_proxy(proxy_port, &url, &host);
    assert!(
        resp.starts_with("HTTP/1.1 200 "),
        "印の要求が通らない: {:?}",
        resp
    );
    let mut seen = seen.lock().unwrap();
    let at = seen
        .iter()
        .position(|r| r.contains(SETTLE_MARK))
        .unwrap_or_else(|| panic!("印の要求がオリジンに積まれていない: {:?}", *seen));
    seen.remove(at);
}

/// 読むだけで何も返さず、閉じもしないリスナー。
///
/// ここへ張ったトンネルでクライアントが送信側だけ閉じると、トンネルは**片方向だけ EOF**
/// になる。この形は預けられない (預かり所は両方向とも暇なものだけ預かる) ので、
/// 「全部のトンネルが忙しい = 閉じるものが無い」状態や、ずっと `relaying` のトンネルを
/// 待ち時間なしに作れる (初めは overload・kernel・recent の試験に 1 つずつあった)。
pub fn start_quiet_origin() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while !matches!(stream.read(&mut buf), Ok(0) | Err(_)) {}
                // EOF を見ても**閉じない**。記述子ごと手放してこのスレッドは終わる
                // (テストが終わるまで開いたまま = トンネルは片方向 EOF のまま生き続ける)
                std::mem::forget(stream);
            });
        }
    });
    port
}
