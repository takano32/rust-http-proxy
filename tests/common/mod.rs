//! 結合テストの共通の道具立て (モックのオリジン、テスト用プロキシの起動、応答の読み取り)。
//!
//! `tests/*.rs` はそれぞれ別のテストバイナリなので、使わない道具が出るのは普通。
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use std::sync::atomic::{AtomicUsize, Ordering};

use rust_http_proxy::Upstream;
use rust_http_proxy::cache::{Cache, CacheConfig, MIB};
use rust_http_proxy::config::Config;
use rust_http_proxy::metrics::Metrics;
use rust_http_proxy::pool::Pool;
use rust_http_proxy::tls::TlsClient;

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
                // このリポジトリのテストは chunked の要求本文を送らないので扱わない
                if let Some(head_end) = req.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
                {
                    let want =
                        head_end + content_length(&String::from_utf8_lossy(&req[..head_end]));
                    while req.len() < want {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
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

pub fn start_test_proxy(config: Config) -> u16 {
    start_test_proxy_with_cache(config, CacheConfig::disabled())
}

pub fn start_test_proxy_with_cache(config: Config, cache_cfg: CacheConfig) -> u16 {
    start_test_proxy_full(config, cache_cfg, None)
}

/// `ca_file` を渡すと、その証明書だけを信頼する TLS クライアント付きで起動する。
pub fn start_test_proxy_full(
    config: Config,
    cache_cfg: CacheConfig,
    ca_file: Option<std::path::PathBuf>,
) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let cfg = Arc::new(config);
    let metrics = Arc::new(Metrics::new());
    let cache = Arc::new(Cache::new(cache_cfg));
    let tls = TlsClient::load(true, ca_file.as_deref()).ok().flatten();
    let pool = Arc::new(Upstream {
        pool: Pool::new(cfg.pool_per_host, Duration::from_secs(30)),
        tls,
    });

    // 上限は設定から (テストでは既定のまま = コア数から決まる値)
    let workers = Arc::new(rust_http_proxy::workers::Workers::new(cfg.max_threads));
    // park_idle が立っている設定なら、アイドル接続を預ける監視スレッドも起こす
    let park = cfg.park_idle.then(|| {
        rust_http_proxy::idle::IdleWatch::start(Arc::clone(&workers), Arc::clone(&metrics))
            .expect("idle watcher")
    });

    thread::spawn(move || {
        rust_http_proxy::serve(
            listener,
            || Arc::clone(&cfg),
            rust_http_proxy::Limiter::new(),
            workers,
            metrics,
            cache,
            pool,
            park,
        )
    });

    port
}

/// `.env` の再読込のように**設定を差し替えられる**テスト用プロキシ (T11.6)。
///
/// 返した `RwLock` の中身を入れ替えると、`serve` が次に受ける接続から新しい設定を引く
/// (本番の `reload::Live::config()` と同じ形)。
pub fn start_test_proxy_with_live_config(
    config: Config,
) -> (u16, Arc<std::sync::RwLock<Arc<Config>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let live = Arc::new(std::sync::RwLock::new(Arc::new(config)));
    let cfg = live.read().unwrap().clone();
    let metrics = Arc::new(Metrics::new());
    let cache = Arc::new(Cache::new(CacheConfig::disabled()));
    let pool = Arc::new(Upstream {
        pool: Pool::new(cfg.pool_per_host, Duration::from_secs(30)),
        tls: None,
    });
    let workers = Arc::new(rust_http_proxy::workers::Workers::new(cfg.max_threads));
    let park = cfg.park_idle.then(|| {
        rust_http_proxy::idle::IdleWatch::start(Arc::clone(&workers), Arc::clone(&metrics))
            .expect("idle watcher")
    });
    let shared = Arc::clone(&live);
    thread::spawn(move || {
        rust_http_proxy::serve(
            listener,
            || Arc::clone(&shared.read().unwrap()),
            rust_http_proxy::Limiter::new(),
            workers,
            metrics,
            cache,
            pool,
            park,
        )
    });
    (port, live)
}

pub fn proxy_config() -> Config {
    let mut cfg = Config::new("0", None, None, Duration::from_secs(5)).unwrap();
    cfg.keepalive = Duration::from_secs(2);
    // テストのオリジンは 127.0.0.1 なので、ローカル宛ての既定の拒否を外す
    cfg.allow_local = true;
    cfg
}

/// 1 本の接続で複数の要求を送るための、Content-Length 付き応答の読み取り。
pub fn read_response(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).unwrap() == 0 {
            break;
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).unwrap();
    (head, body)
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

/// `/status` の JSON を取る (テストが「状態が落ち着いたか」を見るため)。
pub fn status_json(proxy_port: u16) -> String {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .write_all(b"GET /status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut out = String::new();
    let _ = stream.read_to_string(&mut out);
    out
}

/// `cond` が真になるまで最大 10 秒待つ (負荷の高い CI でも落ちない幅)。
pub fn wait_until(cond: impl Fn() -> bool, what: &str) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
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

/// 固定上限 (メモリ 200 MiB / ディスク 2048 MiB) のテスト用キャッシュ設定。
pub fn cache_cfg(dir: &str) -> CacheConfig {
    let dir = std::env::temp_dir().join(dir);
    let _ = std::fs::remove_dir_all(&dir);
    CacheConfig {
        default_ttl: Duration::from_secs(60),
        ..CacheConfig::fixed(200 * MIB, 2048 * MIB, dir)
    }
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

/// アイドル接続を監視スレッド (epoll) に預ける設定 (既定なので `proxy_config` と
/// 同じだが、そのテストが何を見ているかを名前で示すために分けておく)。
pub fn park_config() -> Config {
    let mut cfg = proxy_config();
    cfg.park_idle = true;
    cfg
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

/// 自己署名証明書を作り、python の https サーバーを立てる。道具が無ければ None (テストはスキップ)。
pub fn start_tls_origin(
    dir: &std::path::Path,
) -> Option<(u16, std::path::PathBuf, std::process::Child)> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    std::fs::create_dir_all(dir.join("www")).ok()?;
    std::fs::write(dir.join("www/hello.txt"), b"hello over tls").ok()?;
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let ok = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-keyout",
            key.to_str()?,
            "-out",
            cert.to_str()?,
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=IP:127.0.0.1,DNS:localhost",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?
        .success();
    if !ok {
        return None;
    }
    let script = format!(
        "import http.server, ssl, functools, sys\n\
         class H(http.server.SimpleHTTPRequestHandler):\n\
             protocol_version = 'HTTP/1.1'\n\
             def log_message(self, *a): pass\n\
         ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)\n\
         ctx.load_cert_chain({cert:?}, {key:?})\n\
         srv = http.server.ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(H, directory={www:?}))\n\
         srv.socket = ctx.wrap_socket(srv.socket, server_side=True)\n\
         print(srv.server_address[1], flush=True)\n\
         srv.serve_forever()\n",
        cert = cert.to_str()?,
        key = key.to_str()?,
        www = dir.join("www").to_str()?,
    );
    let mut child = Command::new("python3")
        .args(["-c", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take()?)
        .read_line(&mut line)
        .ok()?;
    let port: u16 = line.trim().parse().ok()?;
    Some((port, cert, child))
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

/// CONNECT の 200 応答を読み切る。
pub fn read_connect_response(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).unwrap() == 0 {
            break;
        }
        buf.push(byte[0]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// 1 本の接続に生のバイト列を投げ、応答 (あれば) と接続が閉じたかを返す。
pub fn raw_request(proxy_port: u16, bytes: &[u8]) -> String {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(bytes).unwrap();
    let mut out = Vec::new();
    // 相手が閉じるまで読む (閉じなければタイムアウトで抜ける)
    let _ = stream.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
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
