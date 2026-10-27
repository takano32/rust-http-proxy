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

/// `/metrics` の `sorahost_errors_total{cause="<cause>"}` (起動からの全体の件数)。
fn errors_total(prom: &str, cause: &str) -> u64 {
    let needle = format!("sorahost_errors_total{{cause=\"{}\"}} ", cause);
    let at = prom
        .find(&needle)
        .unwrap_or_else(|| panic!("{} が無い: {}", needle, prom))
        + needle.len();
    prom[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap()
}

/// 名乗った長さ (10 B) に足りないまま閉じるオリジン (要求ごとに 1 本)。
fn short_body_origin() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc",
            );
        }
    });
    port
}

/// **T.29.15**: オリジン側の 502 の WARN も**理由ごとに 1 分に 1 行**。繋がらない宛先 (`refused`) への
/// CONNECT と転送を 100 本ずつ、本文の途中で切るオリジン (`reset`) へ 100 本送っても WARN は理由ごとに
/// 1〜2 行で、件数は `/status` の `errors_by_cause` と `/errors` が全部持つ。
/// 0.28.48 の tokyo の `/log` は 1,000 行がメール宛の `502 Bad Gateway: connect … failed` で埋まっていた。
#[test]
fn test_integration_origin_failures_warn_once_per_reason() {
    let dir = home("origin");
    // 宛先がループバックなので私設宛ての既定 (T.28.41) を外す
    let env = std::fs::read_to_string(dir.join(".env")).unwrap() + "PROXY_ALLOW_LOCAL=on\n";
    std::fs::write(dir.join(".env"), env).unwrap();
    let proxy = ProxyProcess::start(&dir);
    let _ = proxy.drain_log(Duration::from_millis(300));
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let short = short_body_origin();
    const N: usize = 100;
    for i in 0..N {
        let t = format!("127.0.0.1:{}", closed);
        let got = send_raw(
            proxy.port,
            format!("CONNECT {t} HTTP/1.1\r\nHost: {t}\r\n\r\n").as_bytes(),
        );
        assert!(got.starts_with("HTTP/1.1 502"), "{}", got);
        let got = send_raw(
            proxy.port,
            format!("GET http://{t}/{i} HTTP/1.1\r\nHost: {t}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        );
        assert!(got.starts_with("HTTP/1.1 502"), "{}", got);
        let o = format!("127.0.0.1:{}", short);
        let got = send_raw(
            proxy.port,
            format!("GET http://{o}/{i} HTTP/1.1\r\nHost: {o}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        );
        assert!(got.starts_with("HTTP/1.1 502"), "{}", got);
    }
    let prom = endpoint_json(proxy.port, "/metrics");
    let errors = endpoint_json(proxy.port, "/errors");
    let lines = proxy.drain_log(Duration::from_millis(500));
    let warn: Vec<&String> = lines.iter().filter(|l| l.contains(" WARN ")).collect();
    let refused = warn
        .iter()
        .filter(|l| l.contains("502 Bad Gateway: connect"))
        .count();
    let short_lines = warn
        .iter()
        .filter(|l| l.contains("origin body ended"))
        .count();
    println!(
        "T.29.15 繋がらない宛先 {} × 2 + 本文の途中で切るオリジン {}: WARN {} 行 (connect {} 行、本文 {} 行)",
        N,
        N,
        warn.len(),
        refused,
        short_lines
    );
    // CONNECT と転送の繋がらない 200 本は同じ理由 (`refused`) = 1〜2 行、本文の途中は `reset` で 1〜2 行
    assert!((1..=2).contains(&refused), "{:#?}", warn);
    assert!((1..=2).contains(&short_lines), "{:#?}", warn);
    assert!(warn.len() <= 4, "WARN が多すぎる: {:#?}", warn);
    // 件数は全部ある (ログを絞っても数は減らない)
    assert_eq!(errors_total(&prom, "refused"), 2 * N as u64);
    assert_eq!(errors_total(&prom, "reset"), N as u64);
    // `/errors` は同じ失敗の繰り返しを 1 行に畳む (`repeats`) が、足せば同じ本数
    let repeats: u64 = errors
        .split("\"repeats\":")
        .skip(1)
        .map(|r| {
            r.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<u64>()
                .unwrap()
        })
        .sum();
    assert_eq!(repeats, 3 * N as u64, "{}", errors);
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
