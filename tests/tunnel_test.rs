//! CONNECT トンネルの結合テスト。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

mod common;
use common::*;

#[test]
fn test_integration_connect_tunnel_forwards_prefix_and_both_directions() {
    let echo_port = start_echo_server();
    let proxy_port = start_test_proxy(proxy_config());

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    // 要求と、その直後に続くバイト (TLS ClientHello 相当) を 1 回で送る。
    // プロキシは先読みしてしまった分をトンネルの先頭で送り直さなければならない
    let req = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\nCLIENT-HELLO",
        echo_port, echo_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let head = read_connect_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);

    let mut got = [0u8; 12];
    stream.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"CLIENT-HELLO", "prefix must reach the origin first");

    // 双方向に流れる (splice の経路)
    let payload = vec![b'z'; 1 << 20];
    let mut sender = stream.try_clone().unwrap();
    let sent = payload.clone();
    let writer = thread::spawn(move || {
        sender.write_all(&sent).unwrap();
        sender.shutdown(std::net::Shutdown::Write).unwrap();
    });
    let mut back = Vec::new();
    stream.read_to_end(&mut back).unwrap();
    writer.join().unwrap();
    assert_eq!(back.len(), payload.len());
    assert_eq!(back, payload);
}

#[test]
fn test_integration_idle_tunnel_is_closed_after_the_idle_timeout() {
    let echo_port = start_echo_server();
    let mut cfg = proxy_config();
    cfg.tunnel_idle = Duration::from_secs(1);
    let proxy_port = start_test_proxy(cfg);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        echo_port, echo_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    assert!(read_connect_response(&mut stream).starts_with("HTTP/1.1 200"));

    // 無通信のまま放っておくと約 1 秒で閉じられる
    let started = std::time::Instant::now();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    let elapsed = started.elapsed();
    assert!(rest.is_empty(), "no data was sent");
    assert!(
        elapsed >= Duration::from_millis(700) && elapsed < Duration::from_secs(5),
        "closed after {:?}",
        elapsed
    );
}

#[test]
fn test_integration_connect_port_restriction() {
    let echo_port = start_echo_server();
    let mut cfg = proxy_config();
    // 443 だけ許す設定なので、テスト用オリジンのポートは弾かれる
    cfg.connect_ports = rust_http_proxy::acl::PortSet::parse("443");
    let proxy_port = start_test_proxy(cfg);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        echo_port, echo_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 403 Forbidden"), "{}", resp);
}

/// CONNECT を張って `200` まで読む (テストの定型)。
fn open_tunnel(proxy_port: u16, target_port: u16) -> TcpStream {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let req = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        target_port, target_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let head = read_connect_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    stream
}

/// 接続を受けて EOF まで読み、読み終わったことを知らせるだけのリスナー。
fn start_eof_reporter() -> (u16, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let tx = tx.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while !matches!(stream.read(&mut buf), Ok(0) | Err(_)) {}
                let _ = tx.send(());
            });
        }
    });
    (port, rx)
}

/// 両方向とも暇なトンネルは監視スレッドに預けられ、そのあとに送っても通る (T8.1)。
#[test]
fn test_integration_parked_tunnel_still_passes_data() {
    let echo_port = start_echo_server();
    let proxy_port = start_test_proxy(proxy_config());
    let mut stream = open_tunnel(proxy_port, echo_port);

    // 何も送らなければ猶予 (既定 3ms) のあとに預けられ、スレッドは解放される
    wait_until(
        || status_json(proxy_port).contains("\"parked_tunnels\":1"),
        "the idle tunnel should be parked",
    );

    // 預けたあとに送っても、起こされてそのまま通る
    stream.write_all(b"after-park").unwrap();
    let mut got = [0u8; 10];
    stream.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"after-park");

    // 通し終わるとまた暇になるので、もう一度預けられる
    wait_until(
        || status_json(proxy_port).contains("\"parked_tunnels\":1"),
        "the tunnel should be parked again",
    );
}

/// 預けているトンネルも「開いている接続」として数える (`PROXY_MAX_CONNS` の意味を保つ)。
#[test]
fn test_integration_parked_tunnel_still_counts_against_max_conns() {
    let echo_port = start_echo_server();
    let mut cfg = proxy_config();
    cfg.max_conns = 1;
    let proxy_port = start_test_proxy(cfg);
    let mut stream = open_tunnel(proxy_port, echo_port);

    // 預けられるまで待つ (猶予は 3ms。/status は上限に当たって使えないので時間で待つ)
    thread::sleep(Duration::from_millis(300));
    let resp = raw_request(
        proxy_port,
        b"GET http://127.0.0.1/ HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
    );
    assert!(
        resp.starts_with("HTTP/1.1 503"),
        "a parked tunnel must keep its slot: {}",
        resp
    );

    // 預けたトンネルはまだ生きている
    stream.write_all(b"ping").unwrap();
    let mut got = [0u8; 4];
    stream.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"ping");
}

/// 預けているトンネルの片側が閉じたら、相手にも伝わる。
#[test]
fn test_integration_parked_tunnel_closes_the_other_side() {
    let (origin_port, eof) = start_eof_reporter();
    let proxy_port = start_test_proxy(proxy_config());
    let stream = open_tunnel(proxy_port, origin_port);

    wait_until(
        || status_json(proxy_port).contains("\"parked_tunnels\":1"),
        "the idle tunnel should be parked",
    );

    // クライアントが閉じると、預かり所が起こしてワーカーが相手にも伝える
    drop(stream);
    eof.recv_timeout(Duration::from_secs(10))
        .expect("the origin side must see EOF");
    wait_until(
        || status_json(proxy_port).contains("\"parked_tunnels\":0"),
        "the tunnel should be gone",
    );
}

/// 接続を受けて握り、合図が来たら全部いっせいに閉じるリスナー。
fn start_closing_origin() -> (u16, mpsc::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let held: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
    let accepting = Arc::clone(&held);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            accepting.lock().unwrap().push(stream);
        }
    });
    let (tx, rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        if rx.recv().is_ok() {
            // ここで一斉に close される
            held.lock().unwrap().clear();
        }
    });
    (port, tx)
}

/// 両端が同時に閉じても、預かり所は 1 本のトンネルを 1 回だけ引き取る。
///
/// トンネルは記述子 2 本を同じ鍵で epoll に入れているので、1 回の `epoll_wait` で
/// 同じトンネルの事象が 2 つ来る。2 つ目は引き取りが空振り (`take` が `None`) して
/// 無視されること — 取りこぼしも二重の引き取りも無いことを、数を揃えて確かめる。
#[test]
fn test_integration_parked_tunnels_survive_both_sides_closing_at_once() {
    let (origin_port, close_origin) = start_closing_origin();
    let proxy_port = start_test_proxy(proxy_config());

    let tunnels = 30;
    let clients: Vec<TcpStream> = (0..tunnels)
        .map(|_| open_tunnel(proxy_port, origin_port))
        .collect();
    wait_until(
        || status_json(proxy_port).contains(&format!("\"parked_tunnels\":{}", tunnels)),
        "every idle tunnel should be parked",
    );

    // 両端を同時に閉じる
    close_origin.send(()).unwrap();
    drop(clients);

    wait_until(
        || {
            let status = status_json(proxy_port);
            // 自分 (/status の接続) だけが残る = 持ち分が漏れずに 1 回だけ返された
            status.contains("\"parked_tunnels\":0") && status.contains("\"active_connections\":1")
        },
        "all tunnels should be closed exactly once",
    );
}
