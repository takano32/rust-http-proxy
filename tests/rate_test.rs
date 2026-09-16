//! いまの転送速度 (`/connections` の各行の `rate_bps` と `/status` の `rate_bps_total`) の
//! 結合テスト (T14.39)。
//!
//! `/connections` の `bytes` は接続を受けてからの**累計**なので、「いま誰が帯域を
//! 使っているか」が読めない。history スレッドが周期ごとに全 slot の `bytes` を控えれば、
//! 差分で直近の周期の速さが出る。ここで見るのは 2 つ:
//!
//! 1. **流し続けているトンネル**の `rate_bps` が実際の速さ (1 MiB/s) の前後に出ること
//! 2. **流し終えて暇になったトンネル**は `bytes` が残ったまま `rate_bps` が 0 に戻ること
//!    (「累計」と「いま」が別物であることの裏返し)
//!
//! 速さを書くのは history スレッドなので、周期を 1 秒に縮めて起こす
//! (`history::spawn_every`。`tests/transfer_test.rs` と同じ作法)。
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

/// history スレッドの周期 (本番は 5 秒)。12 秒の間に何度も控えたいので 1 秒にする。
const TICK: Duration = Duration::from_secs(1);

/// 流し続ける刻み (64 KiB を 1/16 秒ごと = **1 MiB/s**)。
const CHUNK: usize = 64 * 1024;
const CHUNKS_PER_SEC: u64 = 16;
const MIB: u64 = 1 << 20;

/// トンネルを握っている時間 (受け入れ基準の 12 秒)。
const HOLD: Duration = Duration::from_secs(12);

/// 速さを見始める時刻 (最初の 1 周期は控えるだけで速さが出ない)。
const SETTLE: Duration = Duration::from_secs(3);

/// 受け入れ基準の幅 (0.5〜2 MiB/s)。
const LOW: u64 = MIB / 2;
const HIGH: u64 = MIB * 2;

/// 履歴スレッド付きのテスト用プロキシ。
fn rate_proxy() -> (u16, Arc<Metrics>) {
    let mut cfg = proxy_config();
    // 12 秒握ったままにするので、預かり所の期限で閉じられないように長くする
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
    let req = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        target_port, target_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let head = read_connect_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    stream
}

/// **1 MiB/s で流し続ける**オリジン (`stop` が立つか相手が閉じるまで)。
///
/// 内蔵の echo (`start_echo_server`) は送った分しか返さないので「流し続ける」が作れない。
/// 刻みの遅れを持ち越さないように、**開始からの経過で次の刻みの時刻を決める**
/// (1 回ごとに `sleep(62.5ms)` を積むと、書き込みの時間のぶんだけ実測が遅くなる)。
fn start_streaming_origin(stop: Arc<AtomicBool>, sent: Arc<AtomicU64>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let stop = Arc::clone(&stop);
            let sent = Arc::clone(&sent);
            thread::spawn(move || {
                let buf = vec![0x5au8; CHUNK];
                let start = Instant::now();
                let mut n: u64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    if stream.write_all(&buf).is_err() {
                        break;
                    }
                    sent.fetch_add(CHUNK as u64, Ordering::Relaxed);
                    n += 1;
                    let due = Duration::from_micros(1_000_000 / CHUNKS_PER_SEC * n);
                    if let Some(rest) = due.checked_sub(start.elapsed()) {
                        thread::sleep(rest);
                    }
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    port
}

/// トンネルから読み続ける (読まないと緩衝が埋まって流れが止まる)。
fn drain(mut stream: TcpStream, got: Arc<AtomicU64>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    got.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
        }
    })
}

/// `/connections` の配列から `"kind":"connect"` の行を 1 つ取り出す。
fn connect_row(json: &str) -> String {
    let head = "\"connections\":[";
    let at = json.find(head).expect("connections がある") + head.len();
    let end = at + json[at..].find(']').expect("配列が閉じている");
    json[at..end]
        .split("},{")
        .find(|r| r.contains("\"kind\":\"connect\""))
        .unwrap_or_else(|| panic!("CONNECT の行が無い: {}", json))
        .to_string()
}

/// `/connections` と `/status` から、いまの速さを 2 通りで読む。
fn read_rates(proxy_port: u16) -> (u64, u64, String) {
    let row = connect_row(&endpoint_json(proxy_port, "/connections"));
    let rate = status_number(&row, "rate_bps");
    let total = status_number(&status_json(proxy_port), "rate_bps_total");
    (rate, total, row)
}

/// 1 MiB/s で流し続けるトンネルを 12 秒握ると、`/connections` の `rate_bps` と
/// `/status` の `rate_bps_total` が 0.5〜2 MiB/s に入ること (受け入れ基準)。
#[test]
fn test_integration_a_streaming_tunnel_shows_its_current_rate() {
    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicU64::new(0));
    let origin_port = start_streaming_origin(Arc::clone(&stop), Arc::clone(&sent));
    let (proxy_port, metrics) = rate_proxy();

    let tunnel = open_tunnel(proxy_port, origin_port);
    let got = Arc::new(AtomicU64::new(0));
    let reader = drain(tunnel.try_clone().unwrap(), Arc::clone(&got));

    // 12 秒握ったまま、1 秒ごとに速さを読む (最初の 1 周期は控えるだけなので飛ばす)
    let start = Instant::now();
    let mut seen: Vec<(u64, u64, u64)> = Vec::new();
    while start.elapsed() < HOLD {
        thread::sleep(TICK);
        if start.elapsed() < SETTLE {
            continue;
        }
        let (rate, total, _) = read_rates(proxy_port);
        seen.push((start.elapsed().as_secs(), rate, total));
    }
    let (rate, total, row) = read_rates(proxy_port);
    let bytes = status_number(&row, "bytes");

    stop.store(true, Ordering::Relaxed);
    let _ = tunnel.shutdown(std::net::Shutdown::Both);
    drop(tunnel);
    let _ = reader.join();

    let secs = start.elapsed().as_secs_f64();
    let measured = got.load(Ordering::Relaxed) as f64 / secs;
    println!(
        "流した {} B / 受けた {} B / {:.1} 秒 = {:.0} B/s、最後の rate_bps {} ({:.2} MiB/s)、\
         rate_bps_total {}、累計 bytes {}",
        sent.load(Ordering::Relaxed),
        got.load(Ordering::Relaxed),
        secs,
        measured,
        rate,
        rate as f64 / MIB as f64,
        total,
        bytes,
    );
    for (at, r, t) in &seen {
        println!("  {:>2} 秒: rate_bps {} / rate_bps_total {}", at, r, t);
    }
    println!("  /connections の 1 行: {{{}}}", row);

    // まず**測っている物差し**を確かめる: 実際に 1 MiB/s 前後で流れていたこと
    assert!(
        (LOW as f64..=HIGH as f64).contains(&measured),
        "試験そのものが 1 MiB/s で流せていない: {:.0} B/s",
        measured
    );
    // 受け入れ基準: 直近 1 周期の速さが 0.5〜2 MiB/s
    assert!(
        (LOW..=HIGH).contains(&rate),
        "rate_bps が範囲外: {} ({:?})",
        rate,
        seen
    );
    assert!(
        (LOW..=HIGH).contains(&total),
        "rate_bps_total が範囲外: {} ({:?})",
        total,
        seen
    );
    // 落ち着いてからの窓はどれも範囲に入ること (1 周期だけまぐれで当たったのではない)
    assert!(seen.len() >= 8, "窓が足りない: {:?}", seen);
    for (at, r, t) in &seen {
        assert!(
            (LOW..=HIGH).contains(r),
            "{} 秒の rate_bps が範囲外: {}",
            at,
            r
        );
        assert!(
            (LOW..=HIGH).contains(t),
            "{} 秒の rate_bps_total が範囲外: {}",
            at,
            t
        );
    }
    // 累計は 12 秒ぶん (速さと違って減らない)
    assert!(bytes >= 8 * MIB, "累計が少なすぎる: {}", bytes);

    wait_until(
        || metrics.active_connections.load(Ordering::Relaxed) == 0,
        "the tunnel to close",
    );
}

/// 流し終えて暇になったトンネルは、`bytes` が残ったまま `rate_bps` が 0 に戻ること。
///
/// 「累計」と「いま」が別物であることの裏返しで、`bytes` だけ見ていたときに
/// 読めなかったもの (T14.39 の目的) がこれ。
#[test]
fn test_integration_an_idle_tunnel_falls_back_to_zero() {
    let echo_port = start_echo_server();
    let (proxy_port, _metrics) = rate_proxy();

    // 1 MiB 上げて 1 MiB 下ろす (echo なので運ぶのは合計 2 MiB)。下りを読まないまま
    // 上りを書くと緩衝が埋まって止まるので、書くのは別スレッド
    let tunnel = open_tunnel(proxy_port, echo_port);
    let mut writer = tunnel.try_clone().unwrap();
    let sender = thread::spawn(move || {
        writer.write_all(&vec![0x5au8; MIB as usize]).unwrap();
        writer.flush().unwrap();
    });
    let mut reader = tunnel.try_clone().unwrap();
    let mut back = vec![0u8; MIB as usize];
    reader.read_exact(&mut back).unwrap();
    sender.join().unwrap();

    // 流し終えてから 3 周期 (トンネルは握ったまま暇にする)
    thread::sleep(TICK * 3);
    let (rate, total, row) = read_rates(proxy_port);
    let bytes = status_number(&row, "bytes");
    println!(
        "暇なトンネル: bytes {} / rate_bps {} / 和 {}",
        bytes, rate, total
    );

    assert!(bytes >= 2 * MIB, "累計は残ること: {} ({})", bytes, row);
    assert_eq!(rate, 0, "暇なら速さは 0: {}", row);
    assert_eq!(total, 0, "和も 0");

    let _ = tunnel.shutdown(std::net::Shutdown::Both);
}
