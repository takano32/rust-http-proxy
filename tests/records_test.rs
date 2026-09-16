//! 記録の一括 off とハッシュ化 `PROXY_RECORDS=on|off|hashed` の結合テスト (T14.41)。
//!
//! 認証を入れない方針 (§0) なので、公開ポートで動かすと個票 (`/recent` `/errors`
//! `/connections` `/clients` `/log` `/events` `/trace` `/bursts`) は誰でも読める。
//! ここで見るのは 3 つだけ:
//!
//! 1. `off` で**その 8 つが空**になり、`/status` が `"records":"off"` で、
//!    **接続元を含まない `/hosts` と `/history` は残る**こと。
//! 2. `hashed` で接続元が**16 桁の 16 進**になり、`/recent` と `/clients` と
//!    `/connections` で**同じ接続元が同じ値**になること (`on` は今までどおり生の IP)。
//! 3. `.env` を `on` → `hashed` に書き換えると、**次の接続から**効くこと。
//!
//! 旗はプロセス全体に 1 つなので、**全部 実バイナリを起こして**確かめる (同じプロセスで
//! 動くテストと旗を取り合わない。`.env` の配線もまとめて見られる)。
#![cfg(target_os = "linux")]

use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod common;
use common::*;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rhp-t1441-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `.env` を置く。**`PROXY_PROFILE=lite` にはしない** (`--lite` はもともと個票を作らないので、
/// 旗が効いているのか `--lite` なのか分からなくなる)。`PROXY_TRACE_CLIENT` は `/trace` に
/// 1 行残すため、`PROXY_STATS_PERSIST` は既定 (on) のまま = `persisted` と `/history` を見る。
fn write_env(dir: &Path, records: &str) {
    std::fs::write(
        dir.join(".env"),
        format!(
            "SERVER_PORT=0\n\
             PROXY_BIND=127.0.0.1\n\
             PROXY_LOG_LEVEL=info\n\
             PROXY_ALLOW_LOCAL=on\n\
             PROXY_TRACE_CLIENT=127.0.0.1\n\
             PROXY_RECORDS={}\n",
            records
        ),
    )
    .unwrap();
}

/// JSON の `"key":"<値>"` を 1 つ読む (最初の 1 つ)。
fn text_field(json: &str, key: &str) -> String {
    let needle = format!("\"{}\":\"", key);
    let at = json
        .find(&needle)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, json));
    let rest = &json[at + needle.len()..];
    rest[..rest.find('"').expect("閉じ引用符")].to_string()
}

/// 16 桁の 16 進か (ハッシュにした接続元の形)。
fn is_hashed(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// CONNECT を 1 本張って `200` まで読む (閉じずに返す = `/connections` に 1 行残る)。
fn open_tunnel(proxy_port: u16, target_port: u16) -> TcpStream {
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(
        format!(
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            target_port, target_port
        )
        .as_bytes(),
    )
    .unwrap();
    let head = read_connect_response(&mut s);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    s
}

/// 個票が残りうることを 1 通り起こす: 転送 1 要求 (閉じる) → `/recent` `/clients` `/hosts`
/// `/trace`、壊れた要求行 1 本 → `/errors` と `/log` の warn、CONNECT 1 本 (開いたまま) →
/// `/connections`。戻すのは開いたままのトンネル (呼ぶ側が持っている間だけ 1 行に出る)。
fn make_records(proxy_port: u16, origin_port: u16) -> TcpStream {
    let body = get_via_proxy(
        proxy_port,
        &format!("http://127.0.0.1:{}/t1441", origin_port),
        &format!("127.0.0.1:{}", origin_port),
    );
    assert!(body.starts_with("HTTP/1.1 200"), "{}", body);
    // 要求行として読めない 1 本 (語が 3 つ揃わない) = 400。`/errors` に 1 件、
    // `/log` に warn が 1 行残る
    let bad = raw_get(proxy_port, "BROKEN\r\n\r\n");
    assert!(bad.starts_with("HTTP/1.1 400"), "{}", bad);
    open_tunnel(proxy_port, origin_port)
}

/// `off` で 8 つの口が空になり、`/hosts` と `/history` は残ること (受け入れ基準の 1 つ目)。
#[test]
fn test_integration_records_off_empties_every_ring_but_keeps_the_statistics() {
    let dir = temp_dir("off");
    let (origin_port, _origin) = start_mock_origin();
    write_env(&dir, "off");
    let mut proxy = ProxyProcess::start(&dir);
    let port = proxy.port;

    let _tunnel = make_records(port, origin_port);

    let status = endpoint_json(port, "/status");
    assert!(status.contains("\"records\":\"off\""), "{}", status);

    // 個票の 8 つ (`/bursts` は「写真」が 0 枚) が空
    for path in [
        "/recent",
        "/errors",
        "/connections",
        "/clients",
        "/log",
        "/events",
        "/trace",
        "/bursts",
    ] {
        let json = endpoint_json(port, path);
        assert!(
            json.contains("\"count\":0"),
            "{} が空になっていない: {}",
            path,
            json
        );
    }
    // `/recent` は状態ファイルにも 1 バイトも書かない
    let recent = endpoint_json(port, "/recent");
    assert!(recent.contains("\"persisted\":false"), "{}", recent);
    assert!(
        !recent.contains("127.0.0.1"),
        "接続元が残っている: {}",
        recent
    );
    // `--lite` ではない (空なのは旗のせいだと読めること)
    assert!(recent.contains("\"lite\":false"), "{}", recent);
    // `/events` は起動の 1 件すら残らない
    let events = endpoint_json(port, "/events");
    assert!(!events.contains("\"kind\":\"start\""), "{}", events);

    // **接続元を含まない統計は残る**: ホスト別 (`/hosts`) と `/status` の数字と `/history`
    let hosts = endpoint_json(port, "/hosts");
    assert!(
        hosts.contains(&format!("127.0.0.1:{}", origin_port)),
        "ホスト別の統計まで消えている: {}",
        hosts
    );
    assert!(status_number(&status, "total_requests") > 0, "{}", status);
    let history = endpoint_json(port, "/history");
    assert!(history.contains("\"interval_secs\":5"), "{}", history);
    assert!(
        history.contains("\"samples\":[["),
        "時系列まで消えている: {}",
        history
    );

    proxy.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `on` (既定) は今までどおり生の IP、`hashed` は 16 桁の 16 進で、`/recent` と `/clients` と
/// `/connections` の 3 つで同じ値になること (受け入れ基準の 2 つ目)。
#[test]
fn test_integration_records_hashed_replaces_the_client_everywhere_with_one_value() {
    let dir = temp_dir("hashed");
    let (origin_port, _origin) = start_mock_origin();

    // ---- `on` (今までどおり) ----
    write_env(&dir, "on");
    let mut proxy = ProxyProcess::start(&dir);
    let port = proxy.port;
    let tunnel = make_records(port, origin_port);
    let status = endpoint_json(port, "/status");
    assert!(status.contains("\"records\":\"on\""), "{}", status);
    for path in ["/recent", "/clients", "/connections", "/errors", "/trace"] {
        let json = endpoint_json(port, path);
        assert!(
            json.contains("\"client\":\"127.0.0.1\""),
            "{} が生の IP になっていない: {}",
            path,
            json
        );
    }
    drop(tunnel);
    proxy.stop();

    // ---- `hashed` (同じ `$HOME` で起こし直す) ----
    write_env(&dir, "hashed");
    let mut proxy = ProxyProcess::start(&dir);
    let port = proxy.port;
    let tunnel = make_records(port, origin_port);

    let status = endpoint_json(port, "/status");
    assert!(status.contains("\"records\":\"hashed\""), "{}", status);

    let hashed = text_field(&endpoint_json(port, "/recent"), "client");
    assert!(is_hashed(&hashed), "16 桁の 16 進でない: {}", hashed);
    for path in ["/clients", "/connections", "/errors", "/trace"] {
        let json = endpoint_json(port, path);
        assert!(
            json.contains(&format!("\"client\":\"{}\"", hashed)),
            "{} が `/recent` と同じ値になっていない ({}): {}",
            path,
            hashed,
            json
        );
        assert!(
            !json.contains("\"client\":\"127.0.0.1\""),
            "{} に生の IP が残っている: {}",
            path,
            json
        );
    }
    // 前の起動 (`on`) で状態ファイルに残った個票も、読み戻すときに同じ形に直る
    let recent = endpoint_json(port, "/recent");
    assert!(
        !recent.contains("\"client\":\"127.0.0.1\""),
        "読み戻した個票に生の IP が残っている: {}",
        recent
    );
    assert!(recent.contains("\"persisted\":true"), "{}", recent);
    // ハッシュは起動ごとの塩を混ぜるので、2 回の起動で同じ接続元でも値が変わる
    let mut again = String::new();
    drop(tunnel);
    proxy.stop();
    write_env(&dir, "hashed");
    let mut proxy = ProxyProcess::start(&dir);
    let tunnel = make_records(proxy.port, origin_port);
    again.push_str(&text_field(
        &endpoint_json(proxy.port, "/clients"),
        "client",
    ));
    assert!(is_hashed(&again), "{}", again);
    assert_ne!(again, hashed, "起動をまたいで同じ値になっている");
    drop(tunnel);
    proxy.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `.env` を `on` → `hashed` に書き換えると、**次の接続から**効くこと (受け入れ基準の 3 つ目)。
#[test]
fn test_integration_records_follows_the_env_file_from_the_next_connection() {
    let dir = temp_dir("env");
    let (origin_port, _origin) = start_mock_origin();
    write_env(&dir, "on");
    let mut proxy = ProxyProcess::start(&dir);
    let port = proxy.port;

    let first = make_records(port, origin_port);
    let json = endpoint_json(port, "/recent");
    assert!(json.contains("\"client\":\"127.0.0.1\""), "{}", json);
    drop(first);

    write_env(&dir, "hashed");
    proxy.wait_for_log("PROXY_RECORDS");
    let status = endpoint_json(port, "/status");
    assert!(status.contains("\"records\":\"hashed\""), "{}", status);

    // 書き換える前に残った個票はそのまま (消しはしない)。**次の接続**からハッシュになる
    let second = make_records(port, origin_port);
    let json = endpoint_json(port, "/clients");
    let hashed = json
        .split("\"client\":\"")
        .skip(1)
        .map(|s| s[..s.find('"').unwrap()].to_string())
        .find(|c| is_hashed(c))
        .unwrap_or_else(|| panic!("ハッシュになった接続元が無い: {}", json));
    assert!(
        json.contains("\"client\":\"127.0.0.1\""),
        "再読込より前の行は残る: {}",
        json
    );
    let conns = endpoint_json(port, "/connections");
    assert!(
        conns.contains(&format!("\"client\":\"{}\"", hashed)),
        "{}",
        conns
    );
    drop(second);

    proxy.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
