//! 中継の詰まりの向き (`/recent` の `stall_ms` と `/history` の `transfer` の
//! `stall_client_ms_sum` / `stall_origin_ms_sum`) の結合テスト (T14.42)。
//!
//! 転送が遅いとき、遅いのは「クライアントの回線 (下り)」か「オリジン」か
//! 「利用者の上り」か。`splice` が `EAGAIN` で止まって `poll` で**書けるのを待つ**
//! 時間を向き別に足せば、トンネル 1 本ごとにそれが読める。ここで見るのは 3 つ:
//!
//! 1. **読まないクライアント** (受信を 2 秒止める) へ流すと `stall_ms.client` が伸び、
//!    `stall_ms.origin` は 0 のまま
//! 2. **読まないオリジン** (受信を 2 秒止める) へ流すと逆になる
//! 3. **普通に流したトンネル**は両方 0 か小さい (待ちに入らないので時計も読まない)
//!
//! 時計を読むのは `poll` で**書けるのを待ちに入る回だけ**で、64 KiB ごとでも
//! `splice` ごとでもない。3 つめはそのことを外から見える形で押さえている
//! (loopback で両方向とも詰まらなければ、この欄は 0 のまま = 1 回も読んでいない)。
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

mod common;
use common::*;

use rust_http_proxy::metrics::Metrics;

/// history スレッドの周期 (本番は 5 秒。窓を早く閉じたいので縮める)。
const TICK: Duration = Duration::from_millis(50);

/// 受信を止めておく時間 (受け入れ基準の 2 秒)。
const PAUSE: Duration = Duration::from_secs(2);

/// 受け入れ基準の下限 (2 秒の停止に対して 1,500 ms)。
const MIN_STALL_MS: u64 = 1_500;

/// 普通に流したトンネルの上限 (「0 か小さい」の小さい)。
const SMALL_MS: u64 = 500;

/// 1 回に動かすバイト数。
const CHUNK: usize = 64 * 1024;
const MIB: usize = 1 << 20;

/// 詰まりを作るのに流すバイト数の上限 (試験が暴走しないための蓋)。
const CAP: u64 = 64 * MIB as u64;

/// 履歴スレッド付きのテスト用プロキシ。
fn stall_proxy() -> (u16, Arc<Metrics>) {
    let mut cfg = park_config();
    // 握ったままの接続が預かり所の期限で閉じないように長くする
    cfg.keepalive = Duration::from_secs(60);
    start_test_proxy_with_history(cfg, TICK)
}

/// CONNECT を張って `200` まで読む。
fn open_tunnel(proxy_port: u16, target_port: u16) -> TcpStream {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(20)))
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

/// **全速で送り続ける**オリジン (`stop` が立つか相手が閉じるまで)。
///
/// 読まないクライアントを作るには、緩衝 (クライアントの受信・プロキシの送信・
/// 中継パイプ) を埋めきるまで送り込む必要がある。`tests/rate_test.rs` の
/// 1 MiB/s では 2 秒で埋まりきらないので、ここは刻みを入れずに送る。
fn start_blasting_origin(stop: Arc<AtomicBool>, sent: Arc<AtomicU64>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let stop = Arc::clone(&stop);
            let sent = Arc::clone(&sent);
            thread::spawn(move || {
                let buf = vec![0x5au8; CHUNK];
                while !stop.load(Ordering::Relaxed) && sent.load(Ordering::Relaxed) < CAP {
                    if stream.write_all(&buf).is_err() {
                        break;
                    }
                    sent.fetch_add(CHUNK as u64, Ordering::Relaxed);
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    port
}

/// **`delay` のあいだ 1 バイトも読まない**オリジン (そのあと EOF まで飲み干す)。
///
/// 読まないオリジンは「プロキシがオリジンへ書けない」状態で、`/recent` の
/// `stall_ms.origin` に出る。待っているあいだに受信の緩衝が埋まり、プロキシの
/// `splice` が `EAGAIN` で止まる。
fn start_deaf_origin(delay: Duration, got: Arc<AtomicU64>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let got = Arc::clone(&got);
            thread::spawn(move || {
                thread::sleep(delay);
                let mut buf = vec![0u8; CHUNK];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            got.fetch_add(n as u64, Ordering::Relaxed);
                        }
                    }
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    port
}

/// EOF まで読み切る (読んだバイト数)。
fn drain_to_eof(stream: &mut TcpStream) -> u64 {
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n as u64,
        }
    }
    total
}

/// `/recent` から `"kind":"connect"` の 1 件を取り出す。
fn recent_tunnel(proxy_port: u16) -> String {
    let json = endpoint_json(proxy_port, "/recent");
    let head = "\"recent\":[";
    let at = json.find(head).expect("recent がある") + head.len();
    let end = at + json[at..].find(']').expect("配列が閉じている");
    json[at..end]
        .split("},{")
        .find(|r| r.contains("\"kind\":\"connect\""))
        .unwrap_or_else(|| panic!("CONNECT の個票が無い: {}", json))
        .to_string()
}

/// `"stall_ms":{"client":N,"origin":M}` を読む。
fn stall(entry: &str) -> (u64, u64) {
    let head = "\"stall_ms\":";
    let at = entry.find(head).expect("stall_ms がある") + head.len();
    let block = &entry[at..];
    (
        status_number(block, "client"),
        status_number(block, "origin"),
    )
}

/// トンネルが閉じるまで待つ (個票はその瞬間に 1 件書かれる)。
fn wait_for_close(metrics: &Metrics) {
    wait_until(
        || metrics.active_connections.load(Ordering::Relaxed) == 0,
        "the tunnel to close",
    );
}

/// `/history` の `transfer` の 1 行から、末尾 2 列 (向き別の合計 ms) を読む。
fn transfer_stall_sums(proxy_port: u16) -> (u64, u64) {
    let json = endpoint_json(proxy_port, "/history?res=5");
    let at = json.find("\"transfer\":").expect("transfer がある");
    let rest = &json[at..];
    let rows_at = rest.find("\"samples\":[[").expect("窓が 1 つ以上ある") + "\"samples\":[".len();
    let row = &rest[rows_at..];
    let end = row.find("]]").expect("行が閉じている");
    // 行の末尾 2 つ (`…,half_close_ms_sum,stall_client_ms_sum,stall_origin_ms_sum`)
    let tail: Vec<&str> = row[..end].rsplit(',').take(2).collect();
    (
        tail[1].trim().parse().expect("stall_client_ms_sum"),
        tail[0].trim().parse().expect("stall_origin_ms_sum"),
    )
}

/// 受信を 2 秒止めるクライアントへ流すと `stall_ms.client` が伸びること (受け入れ基準)。
#[test]
fn test_integration_a_client_that_stops_reading_stalls_the_client_side() {
    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicU64::new(0));
    let origin_port = start_blasting_origin(Arc::clone(&stop), Arc::clone(&sent));
    let (proxy_port, metrics) = stall_proxy();

    let mut tunnel = open_tunnel(proxy_port, origin_port);
    // **2 秒間 1 バイトも読まない**。緩衝が埋まるとプロキシはクライアントへ
    // 書けなくなり、`poll` で `POLLOUT` を待つ = そこで初めて時計を読む
    let paused = Instant::now();
    thread::sleep(PAUSE);
    let blocked = sent.load(Ordering::Relaxed);
    // 読み始めればオリジンの `write_all` も進むので、止めてから飲み干す
    stop.store(true, Ordering::Relaxed);
    let got = drain_to_eof(&mut tunnel);
    let _ = tunnel.shutdown(std::net::Shutdown::Both);
    drop(tunnel);
    wait_for_close(&metrics);

    let entry = recent_tunnel(proxy_port);
    let (client_ms, origin_ms) = stall(&entry);
    println!(
        "読まないクライアント: {:.2} 秒止めて {} B が緩衝で止まり、そのあと {} B 読んだ",
        paused.elapsed().as_secs_f64(),
        blocked,
        got
    );
    println!("  個票: {{{}}}", entry);

    assert!(
        blocked >= MIB as u64,
        "緩衝が埋まるほど流れていない: {} B",
        blocked
    );
    assert!(
        client_ms >= MIN_STALL_MS,
        "クライアント側で待った ms が足りない: {} ({})",
        client_ms,
        entry
    );
    assert!(
        origin_ms <= SMALL_MS,
        "オリジン側は詰まっていないはず: {} ({})",
        origin_ms,
        entry
    );

    // 窓 (T14.6 / T14.25) の末尾 2 列にも同じ向きで足されること
    wait_until(
        || metrics.history.transfer.counts().0 >= 1,
        "the 5s transfer window to close",
    );
    let (sum_client, sum_origin) = transfer_stall_sums(proxy_port);
    println!(
        "  窓の合計: stall_client_ms_sum {} / stall_origin_ms_sum {}",
        sum_client, sum_origin
    );
    assert_eq!(sum_client, client_ms, "窓の合計は個票と同じ 1 本ぶん");
    assert_eq!(sum_origin, origin_ms);
}

/// 受信を 2 秒止めるオリジンへ流すと `stall_ms.origin` が伸びること (受け入れ基準)。
#[test]
fn test_integration_an_origin_that_stops_reading_stalls_the_origin_side() {
    let got = Arc::new(AtomicU64::new(0));
    let origin_port = start_deaf_origin(PAUSE + Duration::from_millis(200), Arc::clone(&got));
    let (proxy_port, metrics) = stall_proxy();

    let tunnel = open_tunnel(proxy_port, origin_port);
    // **書き続ける**クライアント (オリジンが読み始めるまで緩衝が埋まる)。
    // `write_all` は詰まると止まるので、止める合図は 1 刻みごとに見る
    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicU64::new(0));
    let mut writer = tunnel.try_clone().unwrap();
    let (wstop, wsent) = (Arc::clone(&stop), Arc::clone(&sent));
    let sender = thread::spawn(move || {
        let buf = vec![0x5au8; CHUNK];
        while !wstop.load(Ordering::Relaxed) && wsent.load(Ordering::Relaxed) < CAP {
            if writer.write_all(&buf).is_err() {
                break;
            }
            wsent.fetch_add(CHUNK as u64, Ordering::Relaxed);
        }
    });

    thread::sleep(PAUSE + Duration::from_secs(1));
    stop.store(true, Ordering::Relaxed);
    sender.join().unwrap();
    tunnel.shutdown(std::net::Shutdown::Write).unwrap();
    let mut reader = tunnel.try_clone().unwrap();
    let back = drain_to_eof(&mut reader);
    drop(tunnel);
    wait_for_close(&metrics);

    let entry = recent_tunnel(proxy_port);
    let (client_ms, origin_ms) = stall(&entry);
    println!(
        "読まないオリジン: 送った {} B / オリジンが受けた {} B / 返ってきた {} B",
        sent.load(Ordering::Relaxed),
        got.load(Ordering::Relaxed),
        back
    );
    println!("  個票: {{{}}}", entry);

    assert!(
        sent.load(Ordering::Relaxed) >= MIB as u64,
        "緩衝が埋まるほど送れていない"
    );
    assert!(
        origin_ms >= MIN_STALL_MS,
        "オリジン側で待った ms が足りない: {} ({})",
        origin_ms,
        entry
    );
    assert!(
        client_ms <= SMALL_MS,
        "クライアント側は詰まっていないはず: {} ({})",
        client_ms,
        entry
    );
}

/// 普通に流したトンネルは両方 0 か小さいこと (= 待ちに入っていない)。
///
/// **待ちに入らない中継では時計を 1 回も読まない**ことの外から見える証拠。
/// `poll` に `POLLOUT` が立つのは `splice` が `EAGAIN` で止まった方向だけなので、
/// 書けば必ず入る相手 (loopback の echo と、読み続けるクライアント) では
/// `Instant::now()` を呼ぶ枝に 1 度も入らない。
#[test]
fn test_integration_a_healthy_tunnel_does_not_stall() {
    let echo_port = start_echo_server();
    let (proxy_port, metrics) = stall_proxy();

    // 1 MiB 上げて 1 MiB 下ろす (echo なので運ぶのは合計 2 MiB)。
    // 下りを読みながら上げるので、どちらの向きも詰まらない
    let tunnel = open_tunnel(proxy_port, echo_port);
    let mut writer = tunnel.try_clone().unwrap();
    let sender = thread::spawn(move || {
        writer.write_all(&vec![0x5au8; MIB]).unwrap();
        writer.flush().unwrap();
    });
    let mut reader = tunnel.try_clone().unwrap();
    let mut back = vec![0u8; MIB];
    reader.read_exact(&mut back).unwrap();
    sender.join().unwrap();
    assert!(back.iter().all(|&b| b == 0x5a), "運んだ中身が違う");

    tunnel.shutdown(std::net::Shutdown::Write).unwrap();
    let _ = drain_to_eof(&mut reader);
    drop(tunnel);
    wait_for_close(&metrics);

    let entry = recent_tunnel(proxy_port);
    let (client_ms, origin_ms) = stall(&entry);
    println!("普通のトンネル (2 MiB): {{{}}}", entry);
    assert!(
        client_ms <= SMALL_MS && origin_ms <= SMALL_MS,
        "詰まっていないのに待った ms が出ている: client {} / origin {} ({})",
        client_ms,
        origin_ms,
        entry
    );
}
