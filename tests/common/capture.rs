//! 届いた要求を貯めるオリジンと、黙るオリジン (T.34.110 で各試験から寄せた)。

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
