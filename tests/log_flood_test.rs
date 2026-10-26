//! ログの量を攻撃者が決められないことの結合テスト (T.28.9)。
//!
//! 0.27.8 までは、不正な要求 1 本につき WARN が 1 行 (20 本 → 20 行) で上限が無く、
//! 自分宛ての要求 (`/status` など) も既定の INFO で 1 本 1 行出ていた (ダッシュボードを
//! 開いたままにすると約 1.2 行/秒で Pterodactyl のコンソールが埋まる)。
//!
//! 今は (1) 400 系の WARN は**理由ごとに 1 分に 1 回** (抑えた本数は次の行に添える。
//! 本数そのものは `/status` の `rejected_requests` が持つ)、(2) 自分宛ての要求の行は debug。
//! 実バイナリの標準出力を全部引き取って数える。
//!
//! 「1 分たつと抑えた本数を添えてもう 1 行」は 1 分待てないので
//! `proxy_base::log::Throttle` の単体テストで見ている。

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use common::{ProxyProcess, endpoint_json, status_json};

fn home(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rhp-t2809-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\nPROXY_CACHE_RESERVE=off\n\
         PROXY_STATS_PERSIST=off\nPROXY_CANARY=off\n",
    )
    .unwrap();
    dir
}

fn send_raw(port: u16, bytes: &[u8]) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let _ = s.write_all(bytes);
    // 書き終えたと伝える (向こうが読み捨ての待ち (`linger`) で 200 ms 止まらないように)
    let _ = s.shutdown(std::net::Shutdown::Write);
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// `/status` の `rejected_requests` の中の 1 つ。
fn rejected(status: &str, reason: &str) -> u64 {
    let block = status
        .split("\"rejected_requests\":{")
        .nth(1)
        .expect("rejected_requests が無い");
    let needle = format!("\"{}\":", reason);
    let at = block.find(&needle).expect("理由が無い") + needle.len();
    block[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap()
}

#[test]
fn test_integration_bad_requests_warn_once_per_reason_and_self_requests_stay_quiet() {
    let dir = home("flood");
    let proxy = ProxyProcess::start(&dir);
    // 起動のログを読み捨てる
    let _ = proxy.drain_log(Duration::from_millis(300));

    const N: usize = 100;
    // 3 つの理由を 100 本ずつ: 要求行が 1 語 (`request_line`)、メソッドが token でない
    // (`method`)、CONNECT の `Host` が 2 本 (`dup_host`。本体クレートの入口で断る)
    let connect_dup = b"CONNECT example.invalid:443 HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n";
    for _ in 0..N {
        assert!(send_raw(proxy.port, b"GET\r\n\r\n").starts_with("HTTP/1.1 400 "));
        assert!(send_raw(proxy.port, b"@@@\r\n\r\n").starts_with("HTTP/1.1 400 "));
        assert!(send_raw(proxy.port, connect_dup).starts_with("HTTP/1.1 400 "));
    }
    // 自分宛ての要求 (ダッシュボードが引くもの)
    for _ in 0..5 {
        let _ = endpoint_json(proxy.port, "/healthz");
    }
    let status = status_json(proxy.port);
    let lines = proxy.drain_log(Duration::from_millis(500));

    let warn: Vec<&String> = lines.iter().filter(|l| l.contains(" WARN ")).collect();
    let malformed = warn
        .iter()
        .filter(|l| l.contains("malformed request line"))
        .count();
    let dup = warn
        .iter()
        .filter(|l| l.contains("dup_host in CONNECT"))
        .count();
    println!(
        "T.28.9 不正な要求 {} 本 × 3 種: WARN {} 行 (要求行の 2 種 {} 行、dup_host {} 行)",
        N,
        warn.len(),
        malformed,
        dup
    );
    // 要求行が 1 語のものとメソッドが token でないものは同じ文言で、理由は別 = 1〜2 行ずつ
    assert!((2..=4).contains(&malformed), "{:#?}", warn);
    assert!((1..=2).contains(&dup), "{:#?}", warn);
    assert!(warn.len() <= 6, "WARN が多すぎる: {:#?}", warn);

    // 本数は `/status` が全部持っている
    assert_eq!(rejected(&status, "request_line"), N as u64, "{}", status);
    assert_eq!(rejected(&status, "method"), N as u64, "{}", status);
    assert_eq!(rejected(&status, "dup_host"), N as u64, "{}", status);

    // 自分宛ての要求は info で 0 行
    let internal: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("internal endpoint") || l.contains("/healthz"))
        .collect();
    assert!(internal.is_empty(), "自分宛ての行がある: {:#?}", internal);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 壊れた要求行の WARN に載るのは先頭 256 バイトと全体の長さだけ (T.28.38)。
/// 行の数は T.28.9 で 1 分 1 行にしたが、1 行の長さは要求行 (最大 64 KiB) のままだった。
#[test]
fn test_integration_a_long_malformed_request_line_is_clipped_in_the_log() {
    let dir = home("clip");
    let proxy = ProxyProcess::start(&dir);
    let _ = proxy.drain_log(Duration::from_millis(300));
    // 空白の無い 60 KiB の 1 語 (`request_line`)
    let mut req = vec![b'A'; 60 * 1024];
    req.extend_from_slice(b"\r\n\r\n");
    assert!(send_raw(proxy.port, &req).starts_with("HTTP/1.1 400 "));
    let lines = proxy.drain_log(Duration::from_millis(500));
    let warn: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("malformed request line"))
        .collect();
    assert_eq!(warn.len(), 1, "{:#?}", lines);
    assert!(warn[0].len() <= 512, "WARN が {} B", warn[0].len());
    assert!(warn[0].contains("(61440 bytes)"), "{}", warn[0]);
    let _ = std::fs::remove_dir_all(&dir);
}
