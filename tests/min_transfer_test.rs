//! 転送中の最小の速さ (`PROXY_MIN_TRANSFER_BPS` / `PROXY_MIN_TRANSFER_WINDOW_SECS`。T.29.34)。
//!
//! 1 B ずつしか読まない (slow-read) / 送らない (slow-POST) 相手は、無通信の締め切り
//! (`PROXY_TIMEOUT_SECS`) では切れない (1 バイト動くたびに待ち直す)。見張りが待った時間の窓で
//! 運べたバイトを見て切ること、少し速い相手は最後まで受け取れること、`0` で前と同じことを見る。
//!
//! 試験を短くするため、速さと窓は既定 (30 秒の窓で 1 KiB/s) より速く短くしてある
//! (同じ形を既定の値で手で再現した結果は TASKS の T.29.34 の結果に書く)。

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rust_http_proxy::minrate::MinRate;

mod common;
use common::*;

/// クライアント側の受信バッファを小さくする (ループバックの自動調整で MB 単位を抱え込むと、
/// 遅い相手でもプロキシが待ち始めるまでが長くなる)。`std` に口が無いので直に宣言する。
fn small_rcvbuf(s: &TcpStream) {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn setsockopt(fd: i32, level: i32, name: i32, value: *const u8, len: u32) -> i32;
    }
    const SOL_SOCKET: i32 = 1;
    const SO_RCVBUF: i32 = 8;
    let v: i32 = 4096;
    // SAFETY: 有効な記述子と 4 バイトの値を渡すだけ
    unsafe {
        setsockopt(
            s.as_raw_fd(),
            SOL_SOCKET,
            SO_RCVBUF,
            (&v as *const i32).cast(),
            4,
        )
    };
}

/// `len` バイトの本文を `Content-Length` 付きで返すオリジン (接続ごとに 1 応答)。
fn start_big_origin(len: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let mut req = Vec::new();
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", len);
                if s.write_all(head.as_bytes()).is_err() {
                    return;
                }
                let chunk = vec![b'B'; 64 * 1024];
                let mut left = len;
                while left > 0 {
                    let n = left.min(chunk.len());
                    if s.write_all(&chunk[..n]).is_err() {
                        return;
                    }
                    left -= n;
                }
            });
        }
    });
    port
}

/// `len` バイトの本文を chunked で返すオリジン (splice ではなく `BufWriter` で書く経路)。
fn start_chunked_origin(len: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let mut req = Vec::new();
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
                if s.write_all(head.as_bytes()).is_err() {
                    return;
                }
                let mut chunk = format!("{:x}\r\n", 64 * 1024).into_bytes();
                chunk.extend(std::iter::repeat_n(b'C', 64 * 1024));
                chunk.extend_from_slice(b"\r\n");
                for _ in 0..len / (64 * 1024) {
                    if s.write_all(&chunk).is_err() {
                        return;
                    }
                }
                let _ = s.write_all(b"0\r\n\r\n");
            });
        }
    });
    port
}

/// 繋いだら `len` バイトを流し続ける相手 (CONNECT の先)。
fn start_blaster(len: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            thread::spawn(move || {
                let chunk = vec![b'T'; 64 * 1024];
                let mut left = len;
                while left > 0 {
                    let n = left.min(chunk.len());
                    if s.write_all(&chunk[..n]).is_err() {
                        return;
                    }
                    left -= n;
                }
            });
        }
    });
    port
}

fn config(rate: MinRate) -> rust_http_proxy::config::Config {
    let mut cfg = proxy_config();
    // 無通信の締め切り。遅い読み手は 0.5 秒ごとに読むので、これには当たらない
    // (= 最小の速さが無ければ切られない形にしてある)
    cfg.timeout = Duration::from_secs(3);
    cfg.min_transfer = rate;
    cfg
}

fn slow_counts(proxy_port: u16) -> (u64, u64, u64) {
    let json = status_json(proxy_port);
    let at = json
        .find("\"slow_transfer_closed\":")
        .expect("slow_transfer_closed");
    let obj = &json[at..];
    (
        status_number(obj, "response"),
        status_number(obj, "request_body"),
        status_number(obj, "tunnel"),
    )
}

/// プロキシから見た同時接続数 (この `/status` を引く 1 本を除く)。
fn active(proxy_port: u16) -> u64 {
    status_number(&status_json(proxy_port), "active_connections").saturating_sub(1)
}

/// 遅い読み手: 受信バッファを小さくして、0.5 秒ごとに 4 KiB だけ読む (= 8 KiB/s)。
/// 毎回少しずつ進むので無通信の締め切りには当たらない。`stop` が立つか切られたら終わる。
fn slow_reader(s: TcpStream, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut s = s;
        s.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut buf = vec![0u8; 4096];
        while !stop.load(Ordering::Relaxed) {
            match s.read(&mut buf) {
                Ok(0) => return,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
            thread::sleep(Duration::from_millis(500));
        }
    })
}

fn get(proxy_port: u16, origin_port: u16, small: bool) -> TcpStream {
    let s = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
    if small {
        small_rcvbuf(&s);
    }
    let req = format!(
        "GET http://127.0.0.1:{origin_port}/big HTTP/1.1\r\nHost: 127.0.0.1:{origin_port}\r\n\r\n"
    );
    (&s).write_all(req.as_bytes()).unwrap();
    s
}

fn get_small(proxy_port: u16, origin_port: u16) -> String {
    raw_get(
        proxy_port,
        &format!(
            "GET http://127.0.0.1:{origin_port}/ HTTP/1.1\r\nHost: 127.0.0.1:{origin_port}\r\nConnection: close\r\n\r\n"
        ),
    )
}

/// `cond` が真になるまで `limit` だけ待ち、かかった時間を返す (真にならなければ `None`)。
fn within(limit: Duration, cond: impl Fn() -> bool) -> Option<Duration> {
    let t0 = Instant::now();
    while t0.elapsed() < limit {
        if cond() {
            return Some(t0.elapsed());
        }
        thread::sleep(Duration::from_millis(100));
    }
    None
}

/// 遅い読み手 8 本で枠 (`PROXY_MAX_CONNS=8`) を埋めると正常な要求は 503 になるが、
/// 窓で全部切られて枠が戻り、正常な要求が通る。
#[test]
fn test_integration_slow_readers_are_closed_and_the_slots_come_back() {
    let origin = start_big_origin(16 << 20);
    let small = start_big_origin(16);
    let mut cfg = config(MinRate {
        bps: 64 * 1024,
        window: Duration::from_secs(3),
    });
    cfg.max_conns = 8;
    let proxy_port = start_test_proxy(cfg);
    let (before, _, _) = slow_counts(proxy_port);
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..8)
        .map(|_| slow_reader(get(proxy_port, origin, true), Arc::clone(&stop)))
        .collect();
    wait_until(|| active(proxy_port) == 8, "8 本が枠を埋める");
    let full = get_small(proxy_port, small);
    assert!(
        full.starts_with("HTTP/1.1 503 "),
        "枠が埋まっているのに通った: {}",
        full
    );
    // 窓 3 秒で 8 KiB/s < 64 KiB/s を見て全部切る (無通信の締め切り 3 秒には当たらない)
    let took = within(Duration::from_secs(20), || active(proxy_port) == 0)
        .expect("遅い読み手が 20 秒たっても切られない");
    assert!(took < Duration::from_secs(15), "{:?}", took);
    let resp = get_small(proxy_port, small);
    assert!(resp.starts_with("HTTP/1.1 200 "), "{}", resp);
    let (after, _, _) = slow_counts(proxy_port);
    assert!(after >= before + 8, "切った本数: {} -> {}", before, after);
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        let _ = r.join();
    }
}

/// chunked の本文 (`BufWriter` の経路) を読まない相手も切られる。
#[test]
fn test_integration_a_slow_reader_of_a_chunked_body_is_closed() {
    let origin = start_chunked_origin(16 << 20);
    let proxy_port = start_test_proxy(config(MinRate {
        bps: 64 * 1024,
        window: Duration::from_secs(2),
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let r = slow_reader(get(proxy_port, origin, true), Arc::clone(&stop));
    wait_until(|| active(proxy_port) == 1, "繋がる");
    let took = within(Duration::from_secs(20), || active(proxy_port) == 0)
        .expect("chunked の遅い読み手が 20 秒たっても切られない");
    assert!(took < Duration::from_secs(15), "{:?}", took);
    stop.store(true, Ordering::Relaxed);
    let _ = r.join();
}

/// 下限を少し上回る相手は最後まで受け取れる (送信バッファ 4 MiB を越える本文で、プロキシが
/// 実際に書けずに待つ形。splice の経路)。
#[test]
fn test_integration_a_client_just_above_the_minimum_gets_the_whole_body() {
    let len = 12 << 20;
    let origin = start_big_origin(len);
    let proxy_port = start_test_proxy(config(MinRate {
        bps: 384 * 1024,
        window: Duration::from_secs(2),
    }));
    let mut s = get(proxy_port, origin, false);
    // 64 KiB を読んでは 40 ms 休む。読み手が待つぶんもあって、プロキシから見た速さ (届いた量 ÷
    // 書けずに待った時間) は実測で約 0.95 MB/s = 下限 384 KiB/s の 2.4 倍 (下限 1 MiB/s では
    // 0.93 MiB/s で切られた。境目そのものは `proxy_base::minrate` の単体試験で見る)
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    let mut got = 0usize;
    let t0 = Instant::now();
    'outer: loop {
        let mut filled = 0;
        while filled < buf.len() {
            match s.read(&mut buf[filled..]) {
                Ok(0) | Err(_) => {
                    got += filled;
                    break 'outer;
                }
                Ok(n) => filled += n,
            }
        }
        got += filled;
        thread::sleep(Duration::from_millis(40));
    }
    // 応答ヘッダーのぶんだけ多い
    assert!(
        got >= len,
        "途中で切られた: {} / {} B ({:?})",
        got,
        len,
        t0.elapsed()
    );
}

/// `0` なら切らない (前と同じ振る舞い)。窓 (2 秒) の 4 倍たっても開いたまま。
#[test]
fn test_integration_zero_never_closes_a_slow_reader() {
    let origin = start_big_origin(16 << 20);
    let proxy_port = start_test_proxy(config(MinRate {
        bps: 0,
        window: Duration::from_secs(2),
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let r = slow_reader(get(proxy_port, origin, true), Arc::clone(&stop));
    wait_until(|| active(proxy_port) == 1, "繋がる");
    thread::sleep(Duration::from_secs(8));
    assert_eq!(active(proxy_port), 1, "0 なのに切られた");
    stop.store(true, Ordering::Relaxed);
    let _ = r.join();
}

/// 本文を 1 B / 200 ms でしか送らない要求 (slow-POST) は切られる。
#[test]
fn test_integration_a_slow_request_body_is_closed() {
    let origin = start_body_echo_origin();
    let proxy_port = start_test_proxy(config(MinRate {
        bps: 4 * 1024,
        window: Duration::from_secs(2),
    }));
    let (_, before, _) = slow_counts(proxy_port);
    let mut s = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
    let head = format!(
        "POST http://127.0.0.1:{origin}/up HTTP/1.1\r\nHost: 127.0.0.1:{origin}\r\nContent-Length: 100000\r\n\r\n"
    );
    s.write_all(head.as_bytes()).unwrap();
    let t0 = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let stop = Arc::clone(&stop);
        let mut s = s.try_clone().unwrap();
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if s.write_all(b"x").is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(200));
            }
        })
    };
    let took = within(Duration::from_secs(15), || {
        slow_counts(proxy_port).1 > before
    })
    .expect("遅い本文が 15 秒たっても切られない");
    assert!(took < Duration::from_secs(10), "{:?}", took);
    // クライアントには閉じたことが分かる (読みが 0 か応答で終わる)
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut rest = Vec::new();
    let _ = s.read_to_end(&mut rest);
    assert!(t0.elapsed() < Duration::from_secs(15));
    stop.store(true, Ordering::Relaxed);
    let _ = feeder.join();
}

fn connect_tunnel(proxy_port: u16, target: u16) -> TcpStream {
    let mut s = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
    small_rcvbuf(&s);
    s.write_all(
        format!("CONNECT 127.0.0.1:{target} HTTP/1.1\r\nHost: 127.0.0.1:{target}\r\n\r\n")
            .as_bytes(),
    )
    .unwrap();
    let head = read_connect_response(&mut s);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    s
}

/// トンネルの書き側: 読まないクライアントへ流し続けるトンネルは切られる。
/// 片側が黙っているだけのトンネル (half-idle) は窓を何倍過ぎても切られない。
#[test]
fn test_integration_a_slow_tunnel_reader_is_closed_but_a_quiet_tunnel_stays() {
    let blaster = start_blaster(64 << 20);
    let echo = start_echo_server();
    let proxy_port = start_test_proxy(config(MinRate {
        bps: 64 * 1024,
        window: Duration::from_secs(2),
    }));
    let (_, _, before) = slow_counts(proxy_port);
    let mut quiet = connect_tunnel(proxy_port, echo);
    let stop = Arc::new(AtomicBool::new(false));
    let r = slow_reader(connect_tunnel(proxy_port, blaster), Arc::clone(&stop));
    wait_until(|| active(proxy_port) == 2, "2 本が繋がる");
    let took = within(Duration::from_secs(20), || active(proxy_port) == 1)
        .expect("読まないトンネルが 20 秒たっても切られない");
    assert!(took < Duration::from_secs(15), "{:?}", took);
    assert!(
        slow_counts(proxy_port).2 > before,
        "tunnel の本数が増えない"
    );
    // 黙っていたトンネルは生きていて、まだ運べる
    quiet.write_all(b"ping").unwrap();
    quiet
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = [0u8; 4];
    quiet.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
    stop.store(true, Ordering::Relaxed);
    let _ = r.join();
}
