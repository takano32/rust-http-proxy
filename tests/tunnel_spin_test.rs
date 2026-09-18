//! 中継の輪が空回りしないことの結合テスト (T15.5)。
//!
//! `run_until_idle` の `poll` には**両方の記述子**を渡していた (関心 `events` が 0 の
//! ものも)。`poll(2)` は `events` が 0 でも `POLLERR` / `POLLHUP` を必ず返すので、
//! その記述子を**誰も読まない**状態になると「面倒を見る人のいない起床」が残る:
//! `poll` から戻っても 1 バイトも進まず (`progressed = false`)、すぐ次の `poll` に入って
//! またすぐ返る。`poll` が 0 を返さないのでアイドル打ち切り (`PROXY_TUNNEL_IDLE_SECS`)
//! にも永久に当たらない = **1 本のトンネルが 1 スレッドを回し続ける**
//! (2026-09-18 のデプロイ先はこの形のトンネル 2 本で CPU 割り当て 0.5 コアを
//! 27 時間食い切っていた)。
//!
//! 直しは「**関心の無い記述子は `poll` に渡さない** (`-1` を入れる)」の 1 か所だけ。
//! これで「`poll` が返した起床は必ず `fill` か `drain` の 1 回につながる」が成り立ち、
//! 死んだトンネルは今までどおりアイドル打ち切りで閉じる。
//!
//! ここで作る入力は 3 つ。どれも「**受信バッファに未読のバイトがあるソケットを
//! 閉じると RST が出る**」ことで片側だけを殺す (`set_linger` は安定版の std に無い)。
//!
//! - テスト 1 = クライアントが先に FIN → そのあとクライアント側に RST (読む向きが `done`)
//! - テスト 2 = 対称 (オリジンが先に FIN → そのあとオリジン側に RST)
//! - テスト 3 = 読む向きは生きているが `pending > 0` で塞がっている記述子に RST
//!   (関心 `events` が 0 なのは同じ。読む向きは `done` でも `src_eof` でもない)
//! - テスト 4 = 退行の見張り。正常な半閉じでは全バイトが届き、今までどおり閉じる
//!
//! 判定は 2 つ: (a) **トンネルが `/connections` から消える** (直す前は永久に閉じない)。
//! (b) 入力を作ってからの 500 ms で**自プロセスの CPU がほとんど増えない**。
//! このテスト用のプロキシだけ `tunnel_idle` を 1 秒にして、直したあとは
//! アイドル打ち切りで閉じるのを待つ。
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

mod common;
use common::*;

use rust_http_proxy::config::Config;

/// CPU を測る窓。この間は `/connections` を叩かない (叩くと自分で CPU を使う)。
const CPU_WINDOW: Duration = Duration::from_millis(500);

/// 窓のあいだに増えてよい CPU (us)。空回り 1 本は 1 コアを丸ごと使うので
/// 窓の長さそのもの (500,000 us) 近くまで増える。ここは十分に離れた境目。
const MAX_CPU_US: u64 = 100_000;

/// 死んだトンネルを閉じるアイドル打ち切り。既定 (300 秒) ではテストが待てないので短くする。
const TUNNEL_IDLE: Duration = Duration::from_secs(1);

/// 詰まりを作るテスト用の打ち切り。**詰まった時点で待ちに入る**ので、これが短いと
/// 相手が RST を出す前にアイドル打ち切りで閉じてしまう (実測: 緩衝が埋まるまで約 0.6 秒)。
const STALLED_TUNNEL_IDLE: Duration = Duration::from_secs(5);

/// **プロセス全体の CPU** を見るので、同じテストバイナリの隣のテストと重なると
/// 数字が混ざる。このファイルのテストは直列にする (`tests/history_depth_test.rs` と同じ作法)
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ------------------------------------------------------------------
// 補助
// ------------------------------------------------------------------

/// 空回りを見るためのプロキシ設定 (アイドル打ち切りだけ縮める)。
fn spin_config(idle: Duration) -> Config {
    let mut cfg = park_config();
    cfg.tunnel_idle = idle;
    cfg
}

/// CONNECT を張って `200` まで読む。
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

/// この試験用プロキシに CONNECT のトンネルが 1 本でも残っているか。
///
/// テストごとに別のプロキシを起こしているので、`connect` の行はこのトンネルだけ。
fn tunnel_is_open(proxy_port: u16) -> bool {
    endpoint_json(proxy_port, "/connections").contains("\"kind\":\"connect\"")
}

/// 自プロセスの CPU 時間 (utime + stime、us)。
fn cpu_us() -> u64 {
    rust_http_proxy::profile::process_cpu_us().expect("/proc/self/stat が読めない")
}

/// 片側を殺したあと、(a) CPU がほとんど増えないこと と (b) トンネルが閉じること。
///
/// CPU は**黙って待つ 500 ms** で測る (`/connections` を叩くと自分で CPU を使う)。
/// そのあとアイドル打ち切り (1 秒) で閉じるのを待つ。直す前は `poll` が 0 を返さないので
/// 打ち切りに永久に届かず、`wait_until` が時間切れで落ちる = 空回りの再現。
fn assert_no_spin_and_closes(proxy_port: u16, what: &str) {
    let started = Instant::now();
    let before = cpu_us();
    thread::sleep(CPU_WINDOW);
    let spent = cpu_us().saturating_sub(before);
    let spin = format!(
        "{}: the proxy used {} us of CPU in the quiet {:?} after the reset (budget {} us)",
        what, spent, CPU_WINDOW, MAX_CPU_US
    );
    println!("{}", spin);

    wait_until(
        || !tunnel_is_open(proxy_port),
        &format!("the tunnel to close ({}) -- {}", what, spin),
    );
    println!("{}: the tunnel is gone after {:?}", what, started.elapsed());
    assert!(spent < MAX_CPU_US, "{}", spin);
}

// ------------------------------------------------------------------
// テスト 1 = クライアントが先に FIN → そのあとクライアント側に RST
// ------------------------------------------------------------------

/// EOF を読んだら 1 バイト書き、**閉じずに黙る**オリジン。
///
/// ソケットはテストが終わるまで持つ (落とすと FIN が出て、別の道で輪が終わってしまう)。
fn start_mute_after_eof_origin() -> (u16, mpsc::Receiver<TcpStream>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let tx = tx.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                // クライアントの FIN がプロキシ越しに届くまで読む
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                }
                // 半閉じのまま 1 バイトだけ返す (このバイトがクライアントの受信
                // バッファに未読で残り、閉じたときに RST になる)
                let _ = stream.write_all(b"x");
                // 呼び出し側へ渡して持たせる (ここで落とすと FIN が出てしまう)
                let _ = tx.send(stream);
            });
        }
    });
    (port, rx)
}

#[test]
fn test_integration_tunnel_does_not_spin_after_the_client_is_reset() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (origin_port, held) = start_mute_after_eof_origin();
    let proxy_port = start_test_proxy(spin_config(TUNNEL_IDLE));

    let mut client = open_tunnel(proxy_port, origin_port);
    client.write_all(b"hello").unwrap();
    client.flush().unwrap();
    // クライアントが先に FIN (プロキシは dirs[0] を done にしてオリジンへ shutdown(Write))
    client.shutdown(Shutdown::Write).unwrap();

    // オリジンの 1 バイトが**未読のまま**受信バッファに入るのを待つ (`peek` は
    // 取り出さないので、そのまま閉じれば RST が出る)
    let mut byte = [0u8; 1];
    assert_eq!(
        client.peek(&mut byte).unwrap(),
        1,
        "オリジンの 1 バイトが来ない"
    );
    assert_eq!(&byte, b"x");

    // 未読のバイトを抱えたまま閉じる = RST。プロキシのクライアント側記述子は
    // `TCP_CLOSE` になり、`POLLERR | POLLHUP` が立ち続ける
    drop(client);

    assert_no_spin_and_closes(proxy_port, "client reset after the client's FIN");
    // オリジン側のソケットは最後まで持っておく (先に落とすと FIN で輪が終わる)
    drop(held);
}

// ------------------------------------------------------------------
// テスト 2 = オリジンが先に FIN → そのあとオリジン側に RST
// ------------------------------------------------------------------

/// 受けたらすぐ `shutdown(Write)` し、届いたバイトを**読まずに**閉じるオリジン。
///
/// RST を出せたときだけ `ready` に 1 回流す (出せなければ普通の FIN になるので、
/// 黙ってテストを待たせて落とす)。
fn start_fin_then_reset_origin() -> (u16, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            thread::spawn(move || {
                // オリジンが先に FIN (プロキシは dirs[1] を done にする)
                let _ = stream.shutdown(Shutdown::Write);
                // クライアントの 1 バイトが**未読のまま**溜まるのを待つ
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut byte = [0u8; 1];
                let peeked = stream.peek(&mut byte).ok();
                // 未読のバイトを抱えたまま閉じる = RST
                drop(stream);
                if peeked == Some(1) {
                    let _ = tx.send(());
                }
            });
        }
    });
    (port, rx)
}

#[test]
fn test_integration_tunnel_does_not_spin_after_the_origin_is_reset() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (origin_port, reset) = start_fin_then_reset_origin();
    let proxy_port = start_test_proxy(spin_config(TUNNEL_IDLE));

    let client = open_tunnel(proxy_port, origin_port);
    let mut writer = client.try_clone().unwrap();
    // クライアントは 1 バイト送って黙る (閉じない)
    writer.write_all(b"y").unwrap();
    writer.flush().unwrap();

    // オリジンが RST を出すまで待つ
    reset
        .recv_timeout(Duration::from_secs(10))
        .expect("オリジンが RST を出さない");

    assert_no_spin_and_closes(proxy_port, "origin reset after the origin's FIN");
    // クライアント側は最後まで黙って持っておく (閉じると別の道で輪が終わる)
    drop(client);
}

// ------------------------------------------------------------------
// テスト 3 = 読む向きが `pending > 0` で塞がっている記述子に RST
// ------------------------------------------------------------------

/// クライアントの 1 バイトを**読まずに**大量に書き、書けなくなったら閉じるオリジン。
///
/// クライアントが 1 バイトも読まないので、プロキシは `dirs[1]` (オリジン → クライアント)
/// に `pending > 0` を抱えたまま詰まる。**その間 `dirs[1]` はオリジン側の記述子を読まない**
/// ので、その記述子への関心 (`events[1]`) は 0 になる — そこへ RST が来る形。
/// 読む向きは `done` でも `src_eof` でもないので、「読む向きが終わった記述子」を見る
/// 直し方では止まらない。
fn start_flooding_then_reset_origin(wrote: mpsc::Sender<usize>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let wrote = wrote.clone();
            thread::spawn(move || {
                // 書けなくなったら諦める (クライアントが読まないので必ず詰まる)
                stream
                    .set_write_timeout(Some(Duration::from_millis(200)))
                    .unwrap();
                let chunk = vec![0x5au8; 256 * 1024];
                let mut total = 0usize;
                let t0 = Instant::now();
                // 上限 64 MiB。パイプ (1 MiB) と両側のソケット緩衝を埋め切るのが目的
                while total < 64 * 1024 * 1024 {
                    match stream.write(&chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => total += n,
                    }
                }
                println!("origin blocked after {:?} ({} B)", t0.elapsed(), total);
                // クライアントの 1 バイトは**読んでいない**ので、ここで閉じると RST
                drop(stream);
                let _ = wrote.send(total);
            });
        }
    });
    port
}

#[test]
fn test_integration_tunnel_does_not_spin_when_a_stalled_side_is_reset() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (tx, wrote) = mpsc::channel();
    let origin_port = start_flooding_then_reset_origin(tx);
    let proxy_port = start_test_proxy(spin_config(STALLED_TUNNEL_IDLE));

    // クライアントは 1 バイト送って、そのあと**1 バイトも読まない**
    let client = open_tunnel(proxy_port, origin_port);
    let mut writer = client.try_clone().unwrap();
    writer.write_all(b"z").unwrap();
    writer.flush().unwrap();

    let total = wrote
        .recv_timeout(Duration::from_secs(20))
        .expect("オリジンが詰まらない");
    // 詰まった = パイプ (1 MiB) より多く書けてから止まった。これで `dirs[1]` は
    // `pending > 0` を抱えており、オリジン側の記述子への関心は 0 になっている
    assert!(
        total > 1024 * 1024,
        "オリジンが詰まる前に終わった ({} B)",
        total
    );
    println!(
        "stalled relay: the origin wrote {} B before it blocked",
        total
    );

    assert_no_spin_and_closes(proxy_port, "origin reset while the relay is stalled");
    drop(client);
}

// ------------------------------------------------------------------
// テスト 4 = 退行の見張り。正常な半閉じ
// ------------------------------------------------------------------

/// クライアント FIN → オリジンが残りを送り切って FIN、で全バイトが届いて閉じること。
///
/// 関心の無い記述子を `poll` から外したことで、**正常な半閉じの取りこぼしが起きて
/// いない**ことの見張り (どの向きも `done` でなければ `POLLIN` か `POLLOUT` を 1 つは
/// 立てるので、外れるのは「全部 `done`」= 輪を抜ける手前だけ)。
#[test]
fn test_integration_normal_half_close_still_delivers_every_byte() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let echo_port = start_echo_server();
    let proxy_port = start_test_proxy(spin_config(TUNNEL_IDLE));

    let payload: Vec<u8> = (0..1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let mut client = open_tunnel(proxy_port, echo_port);
    let mut writer = client.try_clone().unwrap();
    let sent = payload.clone();
    let sender = thread::spawn(move || {
        writer.write_all(&sent).unwrap();
        writer.flush().unwrap();
        // 送り終わったら FIN (echo は残りを返してから閉じる)
        writer.shutdown(Shutdown::Write).unwrap();
    });

    let mut got = Vec::new();
    client.read_to_end(&mut got).unwrap();
    sender.join().unwrap();
    assert_eq!(got.len(), payload.len(), "半閉じで取りこぼした");
    assert_eq!(got, payload, "半閉じで中身が変わった");
    drop(client);

    // 今までどおり閉じる (`/connections` から消える)
    wait_until(|| !tunnel_is_open(proxy_port), "the tunnel to close");
}
