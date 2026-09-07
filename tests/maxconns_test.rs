//! `PROXY_MAX_CONNS=auto` (既定) の結合テスト。
//!
//! **このファイルにはテストを 1 つしか置かないこと。** `setrlimit(RLIMIT_NOFILE)` は
//! プロセス全体に効くので、同じテストバイナリの他のテストを道連れにしてしまう
//! (`tests/*.rs` は 1 ファイル 1 バイナリ = 1 プロセスなので、分けておけば影響しない)。
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

mod common;
use common::*;

/// 下げる記述子の上限。`ulimit -n 256` 相当で、auto は (256 - 64) / 4 = 48 本になる。
const NOFILE: u64 = 256;

#[test]
fn test_integration_auto_max_conns_rejects_with_503_before_descriptors_run_out() {
    // 記述子の少ない環境を作ってから設定を組み立てる (auto はここで決まる)
    rust_http_proxy::sys::set_max_open_files(NOFILE).expect("setrlimit(RLIMIT_NOFILE)");
    let expected = rust_http_proxy::config::auto_max_conns(NOFILE);
    assert_eq!(expected, 48, "(256 - 予備 64) / 4");

    let (origin_port, _origin) = start_mock_origin();
    let mut cfg = proxy_config();
    // 握ったままの接続がアイドルで閉じられないように長くする (数える対象から外れてしまう)
    cfg.keepalive = Duration::from_secs(60);
    assert_eq!(
        cfg.max_conns, expected,
        "既定は auto (RLIMIT_NOFILE から決める)"
    );
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    // 上限ぶん握る。1 要求ずつ流して「accept されて数え始めた」ことを応答で確かめる。
    // モックのオリジンは `Connection: close` を返すので、握っている間の記述子は
    // クライアント側 1 + プロキシが accept した 1 の 2 本だけ (48 × 2 = 96)
    let mut held = Vec::new();
    for i in 0..expected {
        let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        let req = format!(
            "GET http://{}/hold{} HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, i, host
        );
        s.write_all(req.as_bytes()).unwrap();
        let (head, _) = read_response(&mut s);
        assert!(head.starts_with("HTTP/1.1 200"), "{}: {}", i, head);
        held.push(s);
    }

    // 上限に当たっても記述子はまだ余っている = accept は EMFILE で失敗しない
    let open_fds = std::fs::read_dir("/proc/self/fd").unwrap().count() as u64;
    assert!(
        open_fds < NOFILE,
        "上限 {} 本を握った時点で記述子 {} / {} を使っている",
        expected,
        open_fds,
        NOFILE
    );

    // 49 本目。記述子切れの ECONNRESET ではなく、ちゃんと 503 が返ること
    let mut extra = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let mut resp = String::new();
    extra.read_to_string(&mut resp).unwrap();
    assert!(
        resp.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{}",
        resp
    );
    assert!(resp.contains("Retry-After: 1"), "{}", resp);

    // 握っていたぶんを返せば、また通る (記述子を使い切っていない証拠)
    drop(held);
    for _ in 0..100 {
        let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        let req = format!(
            "GET http://{}/after HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, host
        );
        s.write_all(req.as_bytes()).unwrap();
        let (head, _) = read_response(&mut s);
        if head.starts_with("HTTP/1.1 200") {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("接続を閉じても上限が下がらない");
}
