//! 個票の永続化 (`$HOME/.rust-http-proxy.recent`) の結合テスト (T14.9)。
//!
//! `/recent` `/errors` `/bursts` `/log` のリングはメモリだけだったので、再デプロイの
//! たびに直前の個票が全部消えていた。ここで見るのは「**止めて起こし直しても、前の版で
//! 何が起きたかが読めるか**」の 1 点だけ。
//!
//! 待つのに `/recent` を叩かない: 叩けばそれ自体が接続 1 本になる (自分宛てだけの接続は
//! 個票に残らないが、待ち方としては指標を直に読む方が確か)。
#![cfg(target_os = "linux")]

mod common;

use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use rust_http_proxy::config::Config;

/// history スレッドの周期 (本番は 5 秒。テストは短くする。T14.6 の `spawn_every`)。
const TICK: Duration = Duration::from_millis(50);

/// ファイルの大きさ (固定)。
const RECENT_SIZE: u64 = 4 * 1024 * 1024;

/// 引き継がれるかを見る warn の 1 行。
const WARN_LINE: &str = "t149 individual records must survive a restart";

/// 引き継がれるかを見る出来事 1 件 (T14.11 のリング)。
const EVENT_TEXT: &str = "t149 event must survive a restart";

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rhp-t149-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

/// 受け入れ基準そのもの: 起動 → CONNECT 3 本を閉じる → 周期 1 回ぶん待つ → 止める →
/// 同じ `$HOME` で起動 → `/recent` に 3 件が `restored: 3` として見え、`/errors` (1 件) と
/// `/log` (warn 1 行) も引き継がれる。
#[test]
fn test_integration_individual_records_survive_a_restart() {
    let dir = temp_dir("restart");
    let rrd = dir.join(".rust-http-proxy.rrd");
    let recent = dir.join(".rust-http-proxy.recent");
    let echo_port = start_echo_server();

    // ---- 1 つ目の「プロセス」----
    // 403 で断る宛先を 1 つ用意しておく (403 は `/errors` に乗るが `/recent` には
    // 残らない断り方なので、閉じた接続をちょうど 3 件に保てる)
    let mut cfg = Config::new("0", None, Some("blocked.example"), Duration::from_secs(5)).unwrap();
    cfg.allow_local = true;
    cfg.keepalive = Duration::from_secs(2);
    let (port, metrics, store) = start_test_proxy_with_store(cfg, TICK, rrd.clone());
    let store = store.expect("the state file should open");
    assert!(
        metrics.recent_persisted.load(Ordering::Relaxed),
        "個票の永続化が入っていない"
    );
    assert_eq!(
        std::fs::metadata(&recent).unwrap().len(),
        RECENT_SIZE,
        "起動でファイルができ、大きさは固定"
    );

    // CONNECT 3 本を開いて閉じる
    let target = format!("127.0.0.1:{}", echo_port);
    for _ in 0..3 {
        drop(open_tunnel(port, echo_port));
    }
    wait_until(|| metrics.closed.len() == 3, "three closed connections");

    // エラーを 1 件 (ACL で 403)
    let out = raw_get(
        port,
        "CONNECT blocked.example:443 HTTP/1.1\r\nHost: blocked.example:443\r\n\r\n",
    );
    assert!(out.starts_with("HTTP/1.1 403"), "{}", out);
    wait_until(|| metrics.errors.len() == 1, "one error entry");

    // warn を 1 行と、出来事を 1 件 (T14.11 のリングも永続化の対象)
    rust_http_proxy::log::log_line(rust_http_proxy::log::Level::Warn, None, WARN_LINE);
    rust_http_proxy::events::push(rust_http_proxy::events::EventKind::Reload, EVENT_TEXT);

    // 周期 1 回ぶん待つ (書くのは history スレッドだけ)
    wait_until(
        || status_number(&store.status_json(), "records") >= 6,
        "the history thread to append the records",
    );
    assert_eq!(metrics.closed.len(), 3, "403 は `/recent` に残さない");
    let st = store.status_json();
    assert!(
        st.contains("\"write_errors\":0"),
        "書込エラーが出ている: {}",
        st
    );
    assert!(st.contains("\"dropped\":0"), "個票を落としている: {}", st);
    assert!(
        st.contains(&format!("\"bytes\":{}", RECENT_SIZE)),
        "個票のファイルの大きさ: {}",
        st
    );

    // 止める (停止シグナルで最後の 5 秒ぶんを書くのと同じ呼び出し)
    store.write_recent(&metrics);
    assert_eq!(std::fs::metadata(&recent).unwrap().len(), RECENT_SIZE);

    // ---- 2 つ目の「プロセス」(同じ `$HOME`) ----
    // ログと出来事のリングはプロセスに 1 つなので、起こし直しを再現するために空にする
    rust_http_proxy::log::clear_recent();
    rust_http_proxy::events::clear();
    let (port2, metrics2, store2) = start_test_proxy_with_store(proxy_config(), TICK, rrd.clone());
    assert!(store2.is_some());
    assert!(metrics2.recent_persisted.load(Ordering::Relaxed));

    let json = endpoint_json(port2, "/recent");
    assert!(
        json.contains("\"restored\":3"),
        "引き継いでいない: {}",
        json
    );
    assert!(json.contains("\"persisted\":true"), "{}", json);
    assert!(json.contains("\"count\":3"), "{}", json);
    assert!(json.contains("\"kept\":3"), "{}", json);
    assert_eq!(
        json.matches(&format!("\"target\":\"{}\"", target)).count(),
        3,
        "3 本とも宛先つきで戻っていない: {}",
        json
    );
    assert!(json.contains("\"kind\":\"connect\""), "{}", json);
    assert!(json.contains("\"reason\":\"client_eof\""), "{}", json);
    assert!(json.contains("\"client\":\"127.0.0.1\""), "{}", json);
    assert!(!json.contains("\"truncated\":true"), "{}", json);
    // `?since=` が再起動前の個票に届く (受け入れ基準)
    let at = status_number(&json, "at");
    let older = endpoint_json(port2, &format!("/recent?since={}", at - 1));
    assert!(older.contains("\"matched\":3"), "{}", older);
    let newer = endpoint_json(port2, &format!("/recent?since={}", at + 60));
    assert!(newer.contains("\"matched\":0"), "{}", newer);

    let errors = endpoint_json(port2, "/errors");
    assert!(errors.contains("\"restored\":1"), "{}", errors);
    assert!(errors.contains("\"persisted\":true"), "{}", errors);
    assert!(errors.contains("\"cause\":\"acl\""), "{}", errors);
    assert!(errors.contains("\"status\":403"), "{}", errors);
    assert!(
        errors.contains("\"target\":\"blocked.example:443\""),
        "{}",
        errors
    );

    let log = endpoint_json(port2, "/log");
    assert!(
        log.contains(WARN_LINE),
        "warn の行が引き継がれていない: {}",
        log
    );
    assert!(log.contains("\"persisted\":true"), "{}", log);
    assert!(
        status_number(&log, "restored") >= 1,
        "restored が無い: {}",
        log
    );

    let events = endpoint_json(port2, "/events");
    assert!(
        events.contains(EVENT_TEXT),
        "出来事が引き継がれていない: {}",
        events
    );
    assert!(events.contains("\"persisted\":true"), "{}", events);
    assert!(status_number(&events, "restored") >= 1, "{}", events);

    // 写真は 1 枚も撮っていないが、口は同じ形で答える
    let bursts = endpoint_json(port2, "/bursts");
    assert!(bursts.contains("\"persisted\":true"), "{}", bursts);
    assert!(bursts.contains("\"restored\":0"), "{}", bursts);

    // 2 回目の起動でもファイルは伸びない
    assert_eq!(std::fs::metadata(&recent).unwrap().len(), RECENT_SIZE);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 実バイナリで、**停止シグナルのあと同じ `$HOME` で起こし直す**と個票が残っていること。
///
/// 周期を待たずに (5 秒) 終わるのは、SIGTERM の後始末が最後の 1 回を書くため (T14.9 (4))。
#[test]
fn test_integration_a_stop_signal_writes_the_last_records() {
    let dir = temp_dir("sigterm");
    let recent = dir.join(".rust-http-proxy.recent");
    let echo_port = start_echo_server();
    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\n\
         PROXY_BIND=127.0.0.1\n\
         PROXY_LOG_LEVEL=info\n\
         PROXY_ALLOW_LOCAL=on\n\
         PROXY_CACHE_RESERVE=off\n",
    )
    .unwrap();

    let mut proxy = ProxyProcess::start(&dir);
    assert_eq!(
        std::fs::metadata(&recent).unwrap().len(),
        RECENT_SIZE,
        "起動で 4 MiB 固定のファイルができる"
    );
    drop(open_tunnel(proxy.port, echo_port));
    // 閉じた接続がメモリのリングに載るまで待つ (自分宛ての `/recent` は個票に残らない)
    wait_until(
        || endpoint_json(proxy.port, "/recent").contains("\"recorded\":1"),
        "the closed tunnel to reach the ring",
    );
    // 周期 (5 秒) を待たずに止める: 最後の書き出しはシグナルの後始末が行う
    proxy.stop();
    assert_eq!(std::fs::metadata(&recent).unwrap().len(), RECENT_SIZE);

    let proxy = ProxyProcess::start(&dir);
    let json = endpoint_json(proxy.port, "/recent");
    assert!(json.contains("\"restored\":1"), "{}", json);
    assert!(json.contains("\"persisted\":true"), "{}", json);
    assert!(
        json.contains(&format!("\"target\":\"127.0.0.1:{}\"", echo_port)),
        "{}",
        json
    );
    let status = endpoint_json(proxy.port, "/status");
    assert!(
        status.contains(&format!("\"bytes\":{}", RECENT_SIZE)),
        "state_file に個票のファイルが無い: {}",
        status
    );
    assert!(
        status.matches("\"write_errors\":0").count() >= 2,
        "書込エラーが出ている: {}",
        status
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `PROXY_STATS_PERSIST=off` では**ファイルを作らない** (`persisted: false`)。
#[test]
fn test_integration_nothing_is_written_when_persistence_is_off() {
    let dir = temp_dir("off");
    let echo_port = start_echo_server();
    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\n\
         PROXY_BIND=127.0.0.1\n\
         PROXY_LOG_LEVEL=info\n\
         PROXY_ALLOW_LOCAL=on\n\
         PROXY_STATS_PERSIST=off\n\
         PROXY_CACHE_RESERVE=off\n",
    )
    .unwrap();

    let proxy = ProxyProcess::start(&dir);
    drop(open_tunnel(proxy.port, echo_port));
    let json = endpoint_json(proxy.port, "/recent");
    assert!(json.contains("\"persisted\":false"), "{}", json);
    assert!(json.contains("\"restored\":0"), "{}", json);
    for path in ["/errors", "/log", "/bursts", "/events"] {
        let body = endpoint_json(proxy.port, path);
        assert!(body.contains("\"persisted\":false"), "{} -> {}", path, body);
    }
    assert!(
        !dir.join(".rust-http-proxy.recent").exists(),
        "off なのに個票のファイルができている"
    );
    assert!(!dir.join(".rust-http-proxy.rrd").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 壊れたファイル (先頭を潰す) を置いて起動しても**落ちず、作り直す**。
#[test]
fn test_integration_a_corrupt_recent_file_is_rebuilt() {
    let dir = temp_dir("corrupt");
    let recent = dir.join(".rust-http-proxy.recent");
    let echo_port = start_echo_server();
    // 版の印が違う 4 MiB のファイル (中身は適当なバイト列)
    let mut raw = vec![0u8; RECENT_SIZE as usize];
    raw[..8].copy_from_slice(b"SHPREC00");
    for (i, b) in raw[4096..].iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    std::fs::write(&recent, &raw).unwrap();
    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\n\
         PROXY_BIND=127.0.0.1\n\
         PROXY_LOG_LEVEL=info\n\
         PROXY_ALLOW_LOCAL=on\n\
         PROXY_CACHE_RESERVE=off\n",
    )
    .unwrap();

    let proxy = ProxyProcess::start(&dir);
    assert_eq!(
        std::fs::metadata(&recent).unwrap().len(),
        RECENT_SIZE,
        "作り直しても大きさは同じ"
    );
    let json = endpoint_json(proxy.port, "/recent");
    assert!(json.contains("\"persisted\":true"), "{}", json);
    assert!(json.contains("\"restored\":0"), "中身は捨てる: {}", json);
    // 作り直したファイルにちゃんと書けること
    drop(open_tunnel(proxy.port, echo_port));
    let status = endpoint_json(proxy.port, "/status");
    assert!(
        status.matches("\"write_errors\":0").count() >= 2,
        "{}",
        status
    );
    let _ = std::fs::remove_dir_all(&dir);
}
