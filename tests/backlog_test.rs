//! 待ち受けの backlog (`PROXY_LISTEN_BACKLOG`。T14.47) の結合テスト。
//!
//! `std` の `TcpListener::bind` は backlog **128 固定**で、accept ループは 1 本 (§4 の T4.3)。
//! ブラウザがページを 1 枚開くと数十本の CONNECT が同時に来るので、受け入れ待ち行列が溢れると
//! SYN は黙って捨てられ、クライアントは 1 秒後に再送する (T14.16 の手元の実測で max 1,011 ms、
//! T14.12 の `ListenOverflows` が直近 5 分で +122)。
//!
//! **実バイナリ**を起こして 3 つを見る:
//!
//! 1. 起動ログの待ち受けの行に `backlog N` が出る
//! 2. `/config` の `PROXY_LISTEN_BACKLOG` が**実効値**で、出どころが追随する
//! 3. `ss -ltn` の `Send-Q` がその値になっている (= カーネルに届いている)
//!
//! 3 は `ss` があるときだけ。**`/proc/net/tcp` は backlog を出さない**ので、無い機械では
//! 1 と 2 で確かめる (`ss` は netlink の `sock_diag` で `sk_max_ack_backlog` を読んでいる)。

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::raw_get;

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `.env` を置いてプロキシを起こし、`(子, 待ち受けポート, 起動の 1 行)` を返す。
fn start_proxy(dir: &std::path::Path, extra: &str) -> (KillOnDrop, u16, String) {
    // 待ち受けを 1 本にする (`ss -ltn` の行を取り違えないため)。ログは info で起動行を読む
    std::fs::write(
        dir.join(".env"),
        format!(
            "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\nPROXY_CACHE_RESERVE=off\n{}",
            extra
        ),
    )
    .unwrap();
    let mut child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
            .env("HOME", dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("could not start the proxy binary"),
    );
    let stdout = child.0.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let mut port = None;
    let mut banner = String::new();
    while let Ok(line) = rx.recv_timeout(Duration::from_secs(30)) {
        if let Some(rest) = line.split_once("listening on ") {
            port = rest
                .1
                .split_whitespace()
                .next()
                .and_then(|a| a.rsplit(':').next())
                .and_then(|p| p.parse::<u16>().ok());
            banner = line;
            break;
        }
    }
    (
        child,
        port.expect("the proxy did not log its listening port"),
        banner,
    )
}

/// `ss -ltn` の `Send-Q` (= `listen(2)` に渡した backlog)。`ss` が無ければ `None`。
fn send_q(port: u16) -> Option<u32> {
    let out = Command::new("ss").arg("-ltn").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let want = format!(":{}", port);
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        // State Recv-Q Send-Q Local:Port Peer:Port
        (f.len() >= 4 && f[3].ends_with(&want)).then(|| f[2].parse().ok())?
    })
}

/// `/config` の 1 つの設定の `{"value":…,"source":"…"}` を取り出す。
fn setting(port: u16, key: &str) -> String {
    let body = raw_get(
        port,
        &format!(
            "GET /config HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            port
        ),
    );
    let pat = format!("\"{}\":{{", key);
    let at = body
        .find(&pat)
        .unwrap_or_else(|| panic!("{} が /config に無い: {}", key, body));
    let end = body[at..]
        .find('}')
        .unwrap_or_else(|| panic!("{} の値が閉じていない", key));
    body[at..at + end + 1].to_string()
}

/// `/proc/sys/net/core/somaxconn`。読めなければ `None`。
fn somaxconn() -> Option<u32> {
    std::fs::read_to_string("/proc/sys/net/core/somaxconn")
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

#[test]
fn test_integration_listen_backlog_env_reaches_the_listening_socket() {
    let dir = tempdir("backlog-env");
    // 5 は `somaxconn` より確実に小さい (カーネルの頭打ちに当たらない値)
    let (_child, port, banner) = start_proxy(&dir, "PROXY_LISTEN_BACKLOG=5\n");

    assert!(
        banner.contains("backlog 5"),
        "起動ログに backlog が出ていない: {}",
        banner
    );
    assert_eq!(
        setting(port, "PROXY_LISTEN_BACKLOG"),
        "\"PROXY_LISTEN_BACKLOG\":{\"value\":5,\"source\":\"env_file\"}"
    );
    match send_q(port) {
        Some(q) => assert_eq!(q, 5, "ss -ltn の Send-Q が設定値ではない"),
        // `ss` が無い機械では起動ログと `/config` で確かめる (`/proc/net/tcp` は backlog を出さない)
        None => eprintln!("ss が使えないので Send-Q は見ない (起動ログと /config で確認済み)"),
    }
    // 待ち受けを自分で作っても、今までどおり要求を捌ける
    let health = raw_get(
        port,
        &format!(
            "GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            port
        ),
    );
    assert!(health.starts_with("HTTP/1.1 200 OK"), "{}", health);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_integration_default_listen_backlog_is_somaxconn_capped_at_1024() {
    let dir = tempdir("backlog-default");
    let (_child, port, banner) = start_proxy(&dir, "");

    // 既定は min(1024, somaxconn)。somaxconn が読めない機械では 1024
    let want = somaxconn().unwrap_or(1024).min(1024);
    assert!(
        banner.contains(&format!("backlog {}", want)),
        "既定の backlog が {} ではない: {}",
        want,
        banner
    );
    assert_eq!(
        setting(port, "PROXY_LISTEN_BACKLOG"),
        format!(
            "\"PROXY_LISTEN_BACKLOG\":{{\"value\":{},\"source\":\"default\"}}",
            want
        )
    );
    if let Some(q) = send_q(port) {
        assert_eq!(q, want, "ss -ltn の Send-Q が既定値ではない");
        assert!(q > 128, "std の 128 のままになっている");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 使い捨ての `HOME` (`.env` と状態ファイルの置き場)。
fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rhp-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
