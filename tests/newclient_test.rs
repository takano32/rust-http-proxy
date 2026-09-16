//! 初めて見た接続元 `/events` の `new_client` の結合テスト (T14.54 = T14.23 の規則 6)。
//!
//! T14.0 では見知らぬ接続元 (`161.33.196.121`) に**3 日後**に気づいた。`/clients` の
//! `first_seen` (T14.7) は「いつからか」を答えるが、読みに行かないと分からない。
//! ここで見るのは「**要求を 1 本通したら、5 秒 (テストでは 50 ms) 以内に `/events` へ
//! 1 件出る**」「**同じ接続元では増えない**」の 2 つだけで、判定そのもの (窓・上限・
//! 読み戻した接続元) は `crates/metrics/src/anomaly.rs` の単体テストが見る。
//!
//! 単体では見られないのは「history スレッドの周期 → `/clients` → 判定 → 出来事のリング
//! → `/events`」の配線で、**周期 (50 ms) が秒より短いので同じ秒の窓を何度も見る**
//! (覚えていないと同じ接続元が何十件も出る) のもここでしか出ない。
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

mod common;
use common::*;

use rust_http_proxy::metrics::Metrics;

/// history スレッドの周期 (本番は 5 秒。テストは短くする。`tests/bursts_test.rs` と同じ作法)。
const TICK: Duration = Duration::from_millis(50);

/// `User-Agent` 付きの CONNECT を 1 本張って、1 往復してから閉じる。
fn connect_with_agent(proxy_port: u16, target: &str, agent: &str) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let req = format!(
        "CONNECT {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\n\r\n",
        target, target, agent
    );
    stream.write_all(req.as_bytes()).unwrap();
    let head = read_connect_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{} -> {}", target, head);
    stream.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
}

/// `/events` の中の `new_client` の件数 (`kinds` の一覧に出る綴りは数えない)。
fn new_client_events(json: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = json;
    while let Some(at) = rest.find("{\"at\":") {
        rest = &rest[at..];
        let end = match rest.find('}') {
            Some(i) => i + 1,
            None => break,
        };
        let one = &rest[..end];
        if one.contains("\"kind\":\"new_client\"") {
            out.push(one.to_string());
        }
        rest = &rest[end..];
    }
    out
}

/// history スレッドが `n` 周期ぶん回るまで待つ (`tests/bursts_test.rs` と同じ)。
fn wait_ticks(metrics: &Metrics, n: usize) {
    let want = metrics.history.len() + n;
    wait_until(|| metrics.history.len() >= want, "history thread to tick");
}

/// 新しい接続元の最初の要求のあと、`/events` に `new_client` が 1 件だけ出ること。
#[test]
fn test_integration_new_client_shows_up_once_with_the_agent() {
    let echo = start_echo_server();
    let (port, metrics) = start_test_proxy_with_history(proxy_config(), TICK);

    // まだ誰も通していない (自分宛ての `/events` を引いただけでは接続元にならない。T14.7)
    let before = endpoint_json(port, "/events");
    assert!(new_client_events(&before).is_empty(), "{}", before);
    // 綴りは `kinds` の 12 種目に並ぶ
    assert!(before.contains("\"new_client\""), "{}", before);

    // (1) 初めての接続元 (127.0.0.1) が 1 本 CONNECT を通す
    let started = Instant::now();
    connect_with_agent(port, &format!("localhost:{}", echo), "t1454/1.0");
    wait_until(
        || !new_client_events(&endpoint_json(port, "/events")).is_empty(),
        "new_client event",
    );
    let waited = started.elapsed();
    // 本番の周期は 5 秒なので、ここもその中に収まること (テストの周期は 50 ms)
    assert!(waited < Duration::from_secs(5), "{:?} かかった", waited);

    let json = endpoint_json(port, "/events");
    let events = new_client_events(&json);
    assert_eq!(events.len(), 1, "{}", json);
    let one = &events[0];
    // 接続元・要求数・最初の宛先の種類・`User-Agent` が 1 行に入る
    assert!(
        one.contains("new_client: 127.0.0.1 first seen ("),
        "{}",
        one
    );
    assert!(one.contains("agent \\\"t1454/1.0\\\""), "{}", one);
    assert!(
        one.contains(&format!("first target port {} (name)", echo)),
        "{}",
        one
    );

    // (2) 同じ接続元の 2 本目では増えない (周期は 50 ms なので、この間に
    //     同じ秒の窓を何十回も見ている)
    connect_with_agent(port, &format!("localhost:{}", echo), "t1454/1.0");
    wait_ticks(&metrics, 3);
    let after = endpoint_json(port, "/events");
    assert_eq!(new_client_events(&after).len(), 1, "{}", after);
    // 要求は 2 本とも数えられている (増えていないのは出来事の方だけ)
    let clients = endpoint_json(port, "/clients");
    assert!(status_number(&clients, "requests") >= 2, "{}", clients);
}
