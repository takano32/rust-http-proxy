//! 確立が続けて時間切れになった宛先を、しばらく待たずに 504 で断る (T36.8。`PROXY_CONNECT_NEGATIVE_SECS`)。
//!
//! 黒穴 (`listen(fd, 0)` + 詰め物 1 本。`tests/connect_timeout_test.rs` と同じ作り) へ、
//! 確立の締め切り 1 秒で 2 回続けて繋ぎに行かせると、3 本目は待たずに 504 になる。
//! 期限が過ぎたら 1 本だけ通して試し、黒穴が受け入れ始めていれば (accept すれば SYN に答える)
//! 今までどおり繋がる。表はプロセスに 1 つなので、このファイルの試験は 1 本ずつ回す。

mod common;

#[cfg(target_os = "linux")]
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(target_os = "linux")]
use std::sync::Mutex;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use common::*;

/// 表と断る長さはプロセスに 1 つなので、このファイルの試験を 1 本ずつにする。
#[cfg(target_os = "linux")]
static SERIAL: Mutex<()> = Mutex::new(());

/// `127.0.0.1` の黒穴 (`listen(fd, 0)` + 詰め物 1 本)。返り値は生かしておくこと。
#[cfg(target_os = "linux")]
fn blackhole_v4() -> Option<(TcpListener, TcpStream, SocketAddr)> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn listen(fd: i32, backlog: i32) -> i32;
    }
    let hole = TcpListener::bind("127.0.0.1:0").ok()?;
    if unsafe { listen(hole.as_raw_fd(), 0) } != 0 {
        return None;
    }
    let hole_addr = hole.local_addr().ok()?;
    // 1 本つないで待ち行列を埋める (accept しない)
    let filler = TcpStream::connect_timeout(&hole_addr, Duration::from_secs(1)).ok()?;
    Some((hole, filler, hole_addr))
}

/// 確立の締め切りを 1 秒にしたプロキシ。
#[cfg(target_os = "linux")]
fn start_proxy() -> u16 {
    let mut cfg = proxy_config();
    cfg.connect_timeout = Duration::from_secs(1);
    start_test_proxy(cfg)
}

/// CONNECT を 1 本送り、応答の 1 行目とかかった時間を返す (200 のあとのトンネルは待たずに閉じる)。
#[cfg(target_os = "linux")]
fn connect_once(proxy_port: u16, target: &str) -> (String, Duration) {
    let started = Instant::now();
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let req = format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n", target, target);
    std::io::Write::write_all(&mut stream, req.as_bytes()).unwrap();
    let head = read_connect_response(&mut stream);
    let line = head.lines().next().unwrap_or("").to_string();
    (line, started.elapsed())
}

/// forward の GET を 1 本送り、応答の 1 行目とかかった時間を返す。
#[cfg(target_os = "linux")]
fn forward_once(proxy_port: u16, target: &str) -> (String, Duration) {
    let started = Instant::now();
    let out = get_via_proxy(proxy_port, &format!("http://{}/x", target), target);
    let line = out.lines().next().unwrap_or("").to_string();
    (line, started.elapsed())
}

/// `/status` の `connect_negative` の 1 つの数。
#[cfg(target_os = "linux")]
fn negative_field(proxy_port: u16, key: &str) -> u64 {
    let json = endpoint_json(proxy_port, "/status");
    json::uint(&json::object(&json, "connect_negative"), key)
}

/// CONNECT と forward の両方で、2 回続けて時間切れになった宛先の 3 本目は待たずに 504。
/// ほかの宛先は断らない。数は `/status` と `/metrics` に出る。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_two_timeouts_in_a_row_make_the_third_a_quick_504() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    rust_http_proxy::net::set_connect_negative(rust_http_proxy::net::DEFAULT_CONNECT_NEGATIVE);
    let Some((_hole, _filler, hole_addr)) = blackhole_v4() else {
        eprintln!("cannot build a v4 blackhole; skipping");
        return;
    };
    let Some((_hole2, _filler2, hole2_addr)) = blackhole_v4() else {
        eprintln!("cannot build a v4 blackhole; skipping");
        return;
    };
    let proxy_port = start_proxy();
    let skipped_before = negative_field(proxy_port, "skipped");

    // CONNECT: 1 本目と 2 本目は締め切り (1 秒) まで待って 502
    let target = hole_addr.to_string();
    for i in 0..2 {
        let (line, took) = connect_once(proxy_port, &target);
        assert!(line.starts_with("HTTP/1.1 502"), "{i}: {line}");
        assert!(
            took >= Duration::from_millis(500),
            "{i}: 黒穴なのに {took:?} で返った"
        );
    }
    // 3 本目は待たずに 504
    let (line, took) = connect_once(proxy_port, &target);
    assert!(line.starts_with("HTTP/1.1 504"), "{line}");
    assert!(took < Duration::from_millis(500), "待たずに断る ({took:?})");

    // forward も同じ: 別の黒穴へ 2 本続けて時間切れなら、3 本目は待たずに 504
    let target2 = hole2_addr.to_string();
    for i in 0..2 {
        let (line, took) = forward_once(proxy_port, &target2);
        assert!(line.starts_with("HTTP/1.1 502"), "{i}: {line}");
        assert!(
            took >= Duration::from_millis(500),
            "{i}: 黒穴なのに {took:?} で返った"
        );
    }
    let (line, took) = forward_once(proxy_port, &target2);
    assert!(line.starts_with("HTTP/1.1 504"), "{line}");
    assert!(took < Duration::from_millis(500), "待たずに断る ({took:?})");

    // 生きている宛先は今までどおり通る
    let (origin_port, _origin) = start_mock_origin();
    let (line, _) = connect_once(proxy_port, &format!("127.0.0.1:{origin_port}"));
    assert!(line.starts_with("HTTP/1.1 200"), "{line}");

    // 数: 断った 2 本 (他の試験と並ばないので差で読める)、いま断っている宛先 2 つ
    assert_eq!(negative_field(proxy_port, "skipped") - skipped_before, 2);
    assert!(negative_field(proxy_port, "blocked") >= 2);
    assert_eq!(negative_field(proxy_port, "secs"), 30);
    let metrics = endpoint_json(proxy_port, "/metrics");
    assert!(
        metrics.contains("connect_negative_skipped_total "),
        "{metrics}"
    );
    assert!(metrics.contains("connect_negative_entries "), "{metrics}");
    // 断った 1 本も `/errors` に残る (原因は今までの時間切れと同じ `timeout`、状態は 504)
    let errors = endpoint_json(proxy_port, "/errors");
    assert!(errors.contains("\"status\":504"), "{errors}");
}

/// 期限が過ぎたら 1 本だけ通して試す。まだ黙っていれば断り直し、答えるようになっていれば忘れる。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_after_the_hold_one_probe_passes_and_recovery_is_noticed() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // 30 秒は待てないので 2 秒にする (`PROXY_CONNECT_NEGATIVE_SECS=2` と同じ)
    rust_http_proxy::net::set_connect_negative(Duration::from_secs(2));
    let Some((hole, _filler, hole_addr)) = blackhole_v4() else {
        eprintln!("cannot build a v4 blackhole; skipping");
        rust_http_proxy::net::set_connect_negative(rust_http_proxy::net::DEFAULT_CONNECT_NEGATIVE);
        return;
    };
    let proxy_port = start_proxy();
    let target = hole_addr.to_string();
    for _ in 0..2 {
        assert!(
            connect_once(proxy_port, &target)
                .0
                .starts_with("HTTP/1.1 502")
        );
    }
    assert!(
        connect_once(proxy_port, &target)
            .0
            .starts_with("HTTP/1.1 504")
    );

    // 期限が過ぎた: 試しの 1 本は繋ぎに行き、まだ黙っているので締め切りまで待って 502。
    // そのすぐ後は (また 2 秒) 待たずに 504
    std::thread::sleep(Duration::from_millis(2_100));
    let (line, took) = connect_once(proxy_port, &target);
    assert!(line.starts_with("HTTP/1.1 502"), "{line}");
    assert!(
        took >= Duration::from_millis(500),
        "試しの 1 本は繋ぎに行く ({took:?})"
    );
    assert!(
        connect_once(proxy_port, &target)
            .0
            .starts_with("HTTP/1.1 504")
    );

    // 黒穴が受け入れ始めた (生き返った)。期限が過ぎたら試しの 1 本が繋がり、その後は普通に通る
    std::thread::spawn(move || {
        for s in hole.incoming() {
            std::mem::forget(s);
        }
    });
    std::thread::sleep(Duration::from_millis(2_100));
    let (line, _) = connect_once(proxy_port, &target);
    assert!(
        line.starts_with("HTTP/1.1 200"),
        "試しの 1 本が繋がる: {line}"
    );
    let (line, _) = connect_once(proxy_port, &target);
    assert!(
        line.starts_with("HTTP/1.1 200"),
        "忘れたので普通に通る: {line}"
    );
    assert!(negative_field(proxy_port, "recovered") >= 1);
    assert!(negative_field(proxy_port, "probes") >= 2);

    rust_http_proxy::net::set_connect_negative(rust_http_proxy::net::DEFAULT_CONNECT_NEGATIVE);
}

/// `0` なら覚えない: 何本続けて時間切れになっても、毎回締め切りまで待って 502 (今までどおり)。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_zero_turns_the_quick_504_off() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    rust_http_proxy::net::set_connect_negative(Duration::ZERO);
    let Some((_hole, _filler, hole_addr)) = blackhole_v4() else {
        eprintln!("cannot build a v4 blackhole; skipping");
        rust_http_proxy::net::set_connect_negative(rust_http_proxy::net::DEFAULT_CONNECT_NEGATIVE);
        return;
    };
    let proxy_port = start_proxy();
    let target = hole_addr.to_string();
    for i in 0..3 {
        let (line, took) = connect_once(proxy_port, &target);
        assert!(line.starts_with("HTTP/1.1 502"), "{i}: {line}");
        assert!(took >= Duration::from_millis(500), "{i}: {took:?}");
    }
    assert_eq!(negative_field(proxy_port, "secs"), 0);
    assert_eq!(negative_field(proxy_port, "entries"), 0);
    rust_http_proxy::net::set_connect_negative(rust_http_proxy::net::DEFAULT_CONNECT_NEGATIVE);
}
