//! keep-alive とアイドル接続の預かり (epoll) の結合テスト。
//!
//! 末尾の 3 本だけは **TCP の** keepalive (`PROXY_TCP_KEEPALIVE`。T14.52) で、
//! HTTP の keep-alive (`PROXY_KEEPALIVE_SECS`) とは別のもの。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::*;

#[test]
fn test_integration_parked_idle_connection_serves_the_next_request() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let proxy_port = start_test_proxy(park_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, body) = one_keepalive_request(&mut stream, &host, "/p1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");

    // 猶予 (既定 3ms) を過ぎれば監視スレッドに預けられ、スレッドから外れる
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":1"),
        "the idle connection should be parked",
    );
    let status = status_json(proxy_port);
    assert!(status.contains("\"parking\":true"), "{}", status);

    // 預けた接続に要求を送ると、監視スレッドが起こしてワーカーが処理する
    let (head, body) = one_keepalive_request(&mut stream, &host, "/p2");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert!(head.contains("Connection: keep-alive"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    // 2 本目のあとも預けられる (何度でも往復できる)
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":1"),
        "the connection should be parked again",
    );
    let (head, _) = one_keepalive_request(&mut stream, &host, "/p3");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
}

#[test]
fn test_integration_keepalive_without_parking_still_works() {
    // PROXY_PARK_IDLE=off: 「1 接続 = 1 スレッドが専任」の元の動き
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = proxy_config();
    cfg.park_idle = false;
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/n1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    // 猶予より十分長く空けても、預けないので接続はそのまま
    thread::sleep(Duration::from_millis(100));
    let status = status_json(proxy_port);
    assert!(status.contains("\"parked_connections\":0"), "{}", status);
    assert!(status.contains("\"parking\":false"), "{}", status);
    let (head, body) = one_keepalive_request(&mut stream, &host, "/n2");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn test_integration_park_waits_for_a_request_sent_in_pieces() {
    // 猶予は「次の要求がまだ来ていない」ことを読み取りタイムアウトで測る。
    // 要求を送っている最中の細切れ (猶予より長い間隔) で切ってはいけない
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.park_grace = Duration::from_millis(5);
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/s1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":1"),
        "the idle connection should be parked",
    );

    // 猶予 (5ms) の何倍も空けながら、要求行の途中・ヘッダーの途中で区切って送る
    let req = format!(
        "GET http://{}/s2 HTTP/1.1\r\nHost: {}\r\nX-Slow: yes\r\n\r\n",
        host, host
    );
    let bytes = req.as_bytes();
    for chunk in [&bytes[..12], &bytes[12..30], &bytes[30..]] {
        stream.write_all(chunk).unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_millis(40));
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (head, body) = read_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn test_integration_park_with_no_grace_serves_every_request() {
    // 猶予 0 = 要求のたびに必ず預けて戻す。預ける経路を毎回通す設定 (CI 用)
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.park_grace = Duration::ZERO;
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    for i in 0..5 {
        let (head, body) = one_keepalive_request(&mut stream, &host, &format!("/g{}", i));
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
        assert_eq!(body, b"hello from mock origin");
    }
    assert_eq!(counter.load(Ordering::SeqCst), 5);
}

#[test]
fn test_integration_parked_connection_closes_at_keepalive_timeout() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.keepalive = Duration::from_millis(300);
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/t1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);

    // 預けたまま keep-alive の期限が過ぎたら、監視スレッドが閉じる
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut buf = [0u8; 1];
    assert_eq!(stream.read(&mut buf).unwrap(), 0, "closed by the proxy");
    // /status を引く接続それ自体が active に入るので、預かり数の方で見る
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":0"),
        "the expired connection should be released",
    );
}

#[test]
fn test_integration_parked_connection_notices_the_client_going_away() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let proxy_port = start_test_proxy(park_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/c1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":1"),
        "the idle connection should be parked",
    );

    // 預けている間にクライアントが閉じたら、持ち分ごと片付ける
    drop(stream);
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":0"),
        "the closed connection should be released",
    );
}

#[test]
fn test_integration_keepalive_serves_multiple_requests_per_connection() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-keepalive"));
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    for i in 0..3 {
        let req = format!(
            "GET http://{}/ka{} HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, i, host
        );
        stream.write_all(req.as_bytes()).unwrap();
        let (head, body) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
        assert!(head.contains("Connection: keep-alive"), "{}", head);
        assert_eq!(body, b"hello from mock origin");
    }
    // 同じ接続でキャッシュヒットも返る
    let req = format!("GET http://{}/ka0 HTTP/1.1\r\nHost: {}\r\n\r\n", host, host);
    stream.write_all(req.as_bytes()).unwrap();
    let (head, body) = read_response(&mut stream);
    assert!(head.contains("X-Cache: HIT"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 3);

    // Connection: close で終わる
    let req = format!(
        "GET http://{}/ka1 HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        host, host
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    let text = String::from_utf8_lossy(&rest);
    assert!(text.contains("Connection: close"), "{}", text);
    assert!(text.ends_with("hello from mock origin"));

    // HTTP/1.0 の要求は応答後に閉じられる
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!("GET http://{}/ka2 HTTP/1.0\r\nHost: {}\r\n\r\n", host, host);
    stream.write_all(req.as_bytes()).unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    assert!(String::from_utf8_lossy(&rest).contains("Connection: close"));
}

#[test]
fn test_integration_keepalive_requests_are_not_delayed_by_nagle() {
    // TCP_NODELAY が立っていないと、応答ヘッダーと本文を別々に write したときに
    // Nagle + delayed ACK で 1 要求あたり約 40 ms 止まる (5 要求で 200 ms 以上)。
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        start_keepalive_origin(Arc::clone(&connections), Arc::clone(&requests));
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    // 1 要求目は接続確立を含むので測定から外す
    let warmup = format!("GET http://{}/w HTTP/1.1\r\nHost: {}\r\n\r\n", host, host);
    stream.write_all(warmup.as_bytes()).unwrap();
    read_response(&mut stream);

    let started = std::time::Instant::now();
    for i in 0..5 {
        let req = format!(
            "GET http://{}/nagle{} HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, i, host
        );
        stream.write_all(req.as_bytes()).unwrap();
        let (head, _body) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(100),
        "5 keep-alive requests took {:?} (Nagle would need 200ms or more)",
        elapsed
    );
}

#[test]
fn test_integration_zero_timeout_proxies_and_still_parks_idle_connections() {
    // PROXY_TIMEOUT_SECS=0 = 無期限 (T10.6)。以前はこの設定だと `Conn::new` の
    // `set_write_timeout(Some(ZERO))` が必ず失敗し、**1 本も代理できなかった**。
    // 預ける仕組み (T6.5) は「2 回目以降の読み取りタイムアウトを猶予の長さにして
    // 空振りを『暇だ』と解釈する」形なので、待ち方が無期限になっても壊れないことを見る
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.timeout = Duration::ZERO;
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, body) = one_keepalive_request(&mut stream, &host, "/z1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");

    // 猶予は `park_grace` の長さで `timeout` とは別物なので、無期限でも預けられる
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":1"),
        "the idle connection should be parked even with an unlimited timeout",
    );

    // 預けた接続はそのまま次の要求に使える
    let (head, body) = one_keepalive_request(&mut stream, &host, "/z2");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn test_integration_zero_timeout_never_closes_a_silent_connection() {
    // 「無期限」の意味を見る (T10.6): 何も送ってこない接続を閉じない。
    // 対比のため、短いタイムアウトなら同じ接続が閉じられることも同時に見る
    let mut quick = proxy_config();
    quick.timeout = Duration::from_millis(200);
    let quick_port = start_test_proxy(quick);

    let mut forever = proxy_config();
    forever.timeout = Duration::ZERO;
    let forever_port = start_test_proxy(forever);

    let mut buf = [0u8; 1];
    // 200 ms のタイムアウト: 黙っていると閉じられる
    let mut closed = TcpStream::connect(format!("127.0.0.1:{}", quick_port)).unwrap();
    closed
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    assert_eq!(
        closed.read(&mut buf).unwrap(),
        0,
        "PROXY_TIMEOUT_SECS が有限なら黙っている接続は閉じられる"
    );

    // 無期限: 何倍の時間待っても閉じられない
    let mut kept = TcpStream::connect(format!("127.0.0.1:{}", forever_port)).unwrap();
    kept.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let err = kept.read(&mut buf).unwrap_err();
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "閉じられずに読み取りが空振りするはず: {:?}",
        err
    );

    // 生きているので、そのまま普通に代理できる
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let host = format!("127.0.0.1:{}", origin_port);
    kept.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let (head, body) = one_keepalive_request(&mut kept, &host, "/late");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
}
/// 生きているスレッドの上限に達しても、要求は待たされるだけで捨てられないこと (T10.5)。
///
/// 上限 2 本に対して 12 本の接続を同時に張る。上限を超えたぶんは `Workers` の待ち行列で
/// 待ち、空いたスレッドが順に引き取る。1 本でも落とされたら (= 上限のときに `Err(job)` を
/// 返して `serve` が接続を閉じる作りに戻ったら) このテストが落ちる。
#[test]
fn test_integration_requests_wait_instead_of_being_dropped_at_the_thread_limit() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.max_threads = 2;
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    const CLIENTS: usize = 12;
    let (tx, rx) = std::sync::mpsc::channel();
    for i in 0..CLIENTS {
        let (tx, host) = (tx.clone(), host.clone());
        thread::spawn(move || {
            let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            let (head, body) = one_keepalive_request(&mut stream, &host, &format!("/t{}", i));
            let _ = tx.send((head, body));
        });
    }
    for i in 0..CLIENTS {
        let (head, body) = rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|e| panic!("{} 本目の応答が来ない: {}", i + 1, e));
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
        assert_eq!(body, b"hello from mock origin");
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        CLIENTS,
        "全部オリジンへ届く"
    );
}

/// 上限の要求 (既定 1,000 本目) の応答には `Connection: close` が付き、そのあと閉じる (T14.2)。
///
/// 以前は上限に当たった応答にも `Connection: keep-alive` が付いたまま閉じていたので、
/// その応答を読んだ直後に次の要求を送ったクライアントは**入れ違いで取りこぼしていた**。
/// 上限は設定 (`Config::max_requests_per_conn`) で渡せるので、ここでは 3 に下げて見る
/// (環境変数では変えられない値。1,000 本流すと遅いのでテストのために口を開けてある)。
#[test]
fn test_integration_the_last_request_gets_connection_close_before_the_close() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.max_requests_per_conn = 3;
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    // 上限の 1 つ手前までは今までどおり keep-alive
    for i in 1..3 {
        let (head, body) = one_keepalive_request(&mut stream, &host, &format!("/k{}", i));
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{} 本目: {}", i, head);
        assert!(
            head.contains("Connection: keep-alive"),
            "{} 本目: {}",
            i,
            head
        );
        assert_eq!(body, b"hello from mock origin");
    }

    // 上限の要求。応答は普通に返り、そこに `Connection: close` が付く
    let (head, body) = one_keepalive_request(&mut stream, &host, "/k3");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert!(head.contains("Connection: close"), "{}", head);
    assert!(!head.contains("Connection: keep-alive"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 3);

    // そのあと接続は閉じる (入れ違いを見るために、閉じるのを待たずに次の要求を送る)。
    // 書けてしまうことはある (相手の受信バッファに入るだけ) が、読めるのは EOF だけ
    let _ = stream
        .write_all(format!("GET http://{}/k4 HTTP/1.1\r\nHost: {}\r\n\r\n", host, host).as_bytes());
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut rest = Vec::new();
    match stream.read_to_end(&mut rest) {
        // 閉じ終わっていれば EOF、こちらの書込が先に届いていれば RST。どちらも「閉じた」
        Ok(_) => assert!(
            rest.is_empty(),
            "上限のあとは何も返さずに閉じる: {:?}",
            String::from_utf8_lossy(&rest)
        ),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!("上限のあとは閉じているはず: {}", e),
    }
    // 上限を越えた要求はオリジンへ行っていない
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

// ------------------------------------------------------------------
// 消えたクライアントの検知 (`PROXY_TCP_KEEPALIVE`。T14.52)
// ------------------------------------------------------------------

/// `sockaddr_in` (16 B)。プロキシ側の記述子を探すために自分で宣言する
/// (外部クレートは足さない方針なので `tests/common` の `connect_from` と同じ作法)。
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SockAddrIn {
    family: u16,
    port: u16,
    addr: [u8; 4],
    zero: [u8; 8],
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn getsockname(fd: i32, addr: *mut SockAddrIn, len: *mut u32) -> i32;
    fn getpeername(fd: i32, addr: *mut SockAddrIn, len: *mut u32) -> i32;
}

/// その記述子の (自分のポート, 相手のポート)。IPv4 の繋がったソケットでなければ `None`。
#[cfg(target_os = "linux")]
fn port_pair(fd: i32) -> Option<(u16, u16)> {
    let mut local = SockAddrIn::default();
    let mut peer = SockAddrIn::default();
    let mut n = std::mem::size_of::<SockAddrIn>() as u32;
    // SAFETY: どちらも `sockaddr_in` 1 つぶんの領域とその長さを対で渡している
    // (IPv6 なら切り詰められるが、族を見て弾く)。
    if unsafe { getsockname(fd, &mut local, &mut n) } != 0 {
        return None;
    }
    let mut n = std::mem::size_of::<SockAddrIn>() as u32;
    if unsafe { getpeername(fd, &mut peer, &mut n) } != 0 {
        return None;
    }
    (local.family == 2).then(|| (u16::from_be(local.port), u16::from_be(peer.port)))
}

/// この接続の**プロキシ側** (accept した方) の記述子を探す。
///
/// 結合テストのプロキシは同じプロセスの中で動いているので、`/proc/self/fd` を走査して
/// 「自分のポート = 待ち受け、相手のポート = クライアント」のソケットを 1 つ見つければ
/// それが accept された側 (クライアント側はちょうど逆なので取り違えない)。
#[cfg(target_os = "linux")]
fn proxy_side_fd(proxy_port: u16, client_port: u16) -> Option<i32> {
    for entry in std::fs::read_dir("/proc/self/fd").ok()? {
        let Ok(entry) = entry else { continue };
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if port_pair(fd) == Some((proxy_port, client_port)) {
            return Some(fd);
        }
    }
    None
}

/// 要求を 1 本通してから、プロキシ側の記述子に当たっている keepalive を読み戻す。
#[cfg(target_os = "linux")]
fn keepalive_after_one_request(
    cfg: rust_http_proxy::config::Config,
) -> rust_http_proxy::sys::Keepalive {
    let (origin_port, _origin) = start_mock_origin();
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let client_port = stream.local_addr().unwrap().port();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/p1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);

    let fd = proxy_side_fd(proxy_port, client_port)
        .expect("accept された側の記述子が自分のプロセスに見つからない");
    let k = rust_http_proxy::sys::keepalive_of(fd).expect("getsockopt");
    drop(stream);
    k
}

/// 既定 (`on`) では accept した接続に 60 / 10 / 3 が当たっていること (T14.52)。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_tcp_keepalive_is_set_on_the_accepted_socket() {
    let k = keepalive_after_one_request(proxy_config());
    assert_eq!(
        k,
        rust_http_proxy::sys::Keepalive {
            on: true,
            idle_secs: 60,
            intvl_secs: 10,
            count: 3,
        },
        "既定の PROXY_TCP_KEEPALIVE が当たっていない"
    );
}

/// テスト用に短くした指定 (`on:1:1:2`) がそのままカーネルへ届くこと (T14.52)。
///
/// この設定なら消えた相手は約 3 秒で `ETIMEDOUT` になる (既定は約 90 秒)。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_tcp_keepalive_takes_the_three_numbers() {
    let mut cfg = proxy_config();
    cfg.tcp_keepalive =
        rust_http_proxy::config::TcpKeepalive::parse("on:1:1:2").expect("on:1:1:2 が読めない");
    assert_eq!(cfg.tcp_keepalive.map(|k| k.dead_after_secs()), Some(3));
    let k = keepalive_after_one_request(cfg);
    assert_eq!(
        k,
        rust_http_proxy::sys::Keepalive {
            on: true,
            idle_secs: 1,
            intvl_secs: 1,
            count: 2,
        }
    );
}

/// `off` なら `setsockopt` を 1 回も呼ばない (ソケットは素のまま。T14.52)。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_tcp_keepalive_off_leaves_the_socket_alone() {
    let mut cfg = proxy_config();
    cfg.tcp_keepalive = None;
    let k = keepalive_after_one_request(cfg);
    assert!(!k.on, "off なのに SO_KEEPALIVE が立っている: {:?}", k);
}
