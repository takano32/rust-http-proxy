//! 接続元の個票 `/clients` の結合テスト (T14.7)。
//!
//! `/status` の `clients[]` は要求数と応答時間しか無く、2026-09-16 に現れた見知らぬ
//! 接続元 (`161.33.196.121`) が「誰のどのプログラムで、何をしているか」を答えられなかった。
//! ここで見るのは「実際に通した要求が、その形で個票に出てくるか」だけ。

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use common::*;

/// `User-Agent` 付きの CONNECT を 1 本張って、閉じる (閉じたところで統計に入る)。
fn connect_with_agent(proxy_port: u16, target: &str, agent: &str) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let req = format!(
        "CONNECT {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\n\r\n",
        target, target, agent
    );
    stream.write_all(req.as_bytes()).unwrap();
    let head = read_connect_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{} -> {}", target, head);
    // トンネル越しに 1 往復してから閉じる (中継が動いたことを確かめてから終わる)
    stream.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
}

/// `/clients` (または `/status`) の 1 行 (接続元 `127.0.0.1` のもの) を切り出す。
///
/// `ports` が入れ子の `{...}` を持つので、**対応する閉じ括弧まで**を数えて取る。
fn client_row(json: &str) -> String {
    let at = json
        .find("{\"client\":\"127.0.0.1\"")
        .unwrap_or_else(|| panic!("127.0.0.1 の行が無い: {}", json));
    let mut depth = 0usize;
    for (i, c) in json[at..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return json[at..at + i + 1].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("行の終わりが無い: {}", json)
}

/// CONNECT の `User-Agent`・宛先の種類・ポート・IP リテラル宛てが `/clients` に出ること。
#[test]
fn test_integration_clients_shows_the_agent_targets_and_ports() {
    let echo = start_echo_server();
    let proxy_port = start_test_proxy(proxy_config());

    // まだ誰も通っていない (自分宛ての `/clients` を引いただけでは増えない)
    let empty = endpoint_json(proxy_port, "/clients");
    assert!(empty.contains("\"clients\":[]"), "{}", empty);
    assert!(empty.contains("\"count\":0"), "{}", empty);
    // 新しい欄は状態ファイルに残らない (`.rrd` のスロットに余白が無い)
    assert!(empty.contains("\"persisted\":false"), "{}", empty);
    assert!(empty.contains("\"max_targets\":256"), "{}", empty);

    // (1) 名前宛ての CONNECT を 1 本 (`localhost` は IP リテラルではない)
    connect_with_agent(proxy_port, &format!("localhost:{}", echo), "t147/1.0");
    wait_until(
        || endpoint_json(proxy_port, "/clients").contains("t147/1.0"),
        "/clients に User-Agent が出る",
    );
    let row = client_row(&endpoint_json(proxy_port, "/clients"));
    assert!(row.contains("\"agents\":[\"t147/1.0\"]"), "{}", row);
    assert_eq!(status_number(&row, "distinct_targets"), 1, "{}", row);
    assert_eq!(status_number(&row, "literal_targets"), 0, "{}", row);
    assert_eq!(status_number(&row, "nonstandard_ports"), 1, "{}", row);
    assert!(
        row.contains(&format!("{{\"port\":{},\"requests\":1}}", echo)),
        "試験のポートが出ていない: {}",
        row
    );
    assert!(row.contains("\"distinct_targets_capped\":false"), "{}", row);
    assert!(row.contains("\"agents_dropped\":0"), "{}", row);
    let first_seen = status_number(&row, "first_seen");
    assert!(first_seen > 1_700_000_000, "時刻が epoch 秒でない: {}", row);

    // (2) IP リテラル宛てを 1 本足すと `literal_targets` が 1 になる
    connect_with_agent(proxy_port, &format!("127.0.0.1:{}", echo), "t147/1.0");
    wait_until(
        || {
            status_number(
                &client_row(&endpoint_json(proxy_port, "/clients")),
                "requests",
            ) >= 2
        },
        "2 本目が数えられる",
    );
    let row = client_row(&endpoint_json(proxy_port, "/clients"));
    assert_eq!(status_number(&row, "literal_targets"), 1, "{}", row);
    assert_eq!(status_number(&row, "distinct_targets"), 2, "{}", row);
    assert_eq!(status_number(&row, "nonstandard_ports"), 2, "{}", row);
    assert!(
        row.contains(&format!("{{\"port\":{},\"requests\":2}}", echo)),
        "同じポートは 1 行にまとまる: {}",
        row
    );
    // `first_seen` は最初に見た時刻のまま
    assert_eq!(status_number(&row, "first_seen"), first_seen, "{}", row);

    // (3) `/status` の `clients[]` は**今までの欄の後ろに** 4 つ足しただけ
    let status = status_json(proxy_port);
    let srow = client_row(&status);
    let at = |k: &str| {
        srow.find(k)
            .unwrap_or_else(|| panic!("{} が無い: {}", k, srow))
    };
    assert!(at("\"requests\":") < at("\"first_seen\":"), "{}", srow);
    assert!(at("\"last_seen\":") < at("\"first_seen\":"), "{}", srow);
    assert!(srow.contains("\"agent\":\"t147/1.0\""), "{}", srow);
    assert!(srow.contains("\"distinct_targets\":2"), "{}", srow);
    assert!(srow.contains("\"literal_targets\":1"), "{}", srow);
    // `/status` は今までどおり上位 50 と 4 つの欄だけ (個票の欄は出さない)
    assert!(!srow.contains("\"ports\""), "{}", srow);
}

/// `?sort=` の 4 つの鍵と `?limit=` が効くこと (知らない値は既定に倒す)。
#[test]
fn test_integration_clients_honours_sort_and_limit() {
    let echo = start_echo_server();
    let proxy_port = start_test_proxy(proxy_config());
    connect_with_agent(proxy_port, &format!("127.0.0.1:{}", echo), "t147/1.0");
    wait_until(
        || endpoint_json(proxy_port, "/clients").contains("t147/1.0"),
        "/clients に 1 件出る",
    );
    for (q, want) in [
        ("", "requests"),
        ("?sort=requests", "requests"),
        ("?sort=recent", "recent"),
        ("?sort=targets", "targets"),
        ("?sort=literal", "literal"),
        ("?sort=nonsense", "requests"),
        ("?sort=", "requests"),
    ] {
        let json = endpoint_json(proxy_port, &format!("/clients{}", q));
        assert!(
            json.contains(&format!("\"sort\":\"{}\"", want)),
            "{} -> {}",
            q,
            json
        );
        assert_eq!(status_number(&json, "count"), 1, "{} -> {}", q, json);
    }
    // `?limit=` は端に倒す (`/errors?n=` と同じ方針)
    let one = endpoint_json(proxy_port, "/clients?limit=1");
    assert!(one.contains("\"limit\":1"), "{}", one);
    let big = endpoint_json(proxy_port, "/clients?limit=99999&sort=targets");
    assert!(big.contains("\"limit\":1000"), "{}", big);
    let bad = endpoint_json(proxy_port, "/clients?limit=abc");
    assert!(bad.contains("\"limit\":200"), "{}", bad);
}

/// `User-Agent` を読むのは**接続の最初の要求だけ** (2 要求目からは旗で飛ばす)。
#[test]
fn test_integration_clients_reads_the_user_agent_only_on_the_first_request() {
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        start_keepalive_origin(Arc::clone(&connections), Arc::clone(&requests));
    let proxy_port = start_test_proxy(proxy_config());

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    for (path, agent) in [("/a", "first/1.0"), ("/b", "second/2.0")] {
        let req = format!(
            "GET http://127.0.0.1:{}{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUser-Agent: {}\r\n\r\n",
            origin_port, path, origin_port, agent
        );
        stream.write_all(req.as_bytes()).unwrap();
        let (head, _) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    }
    wait_until(
        || {
            status_number(
                &client_row(&endpoint_json(proxy_port, "/clients")),
                "requests",
            ) >= 2
        },
        "2 要求とも数えられる",
    );
    let row = client_row(&endpoint_json(proxy_port, "/clients"));
    assert!(row.contains("\"agents\":[\"first/1.0\"]"), "{}", row);
    assert!(!row.contains("second/2.0"), "2 要求目は見ない: {}", row);
    assert_eq!(status_number(&row, "agents_dropped"), 0, "{}", row);
    // 宛先は 1 種 (同じホストの 2 要求)、IP リテラル宛てが 2
    assert_eq!(status_number(&row, "distinct_targets"), 1, "{}", row);
    assert_eq!(status_number(&row, "literal_targets"), 2, "{}", row);
}
