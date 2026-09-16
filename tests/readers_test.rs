//! 内部エンドポイントの読み手 (`/status` の `readers` と `/readers`) の結合テスト (T14.53)。
//!
//! T14.7 の `clients[]` は**自分宛てだけの接続を数えない** (監視で埋まるため)。こちらは
//! その逆で、**自分宛てだけ**を数える別の表: 認証なしの公開ポートで「誰が個票を読んで
//! いるか」(走査か、自分の監視か) を見分けるためのもの。ここで見るのは 3 つ:
//!
//! 1. `/status` を 3 回引いた接続元が `readers` に `count: 3` / `last_path: "/status"`
//!    (**数えるのは応答を組む前**なので、読んだ応答にはその要求自身が入っている)
//! 2. プロキシとしての要求 (forward / CONNECT) は 1 件も数えない (`clients[]` の側には出る)
//! 3. `/readers` に全部が出て、問い合わせ文字列は落ち、長いパスは 64 バイトで切れ、
//!    表は 256 行で頭打ち (溢れたら最後に引いたのがいちばん古い行から捨てる)
//!
//! **別の接続元は送信元アドレスで作る** (`connect_from([127,0,0,2], ..)`。T14.13 と同じ)。
//! ループバックは `127.0.0.0/8` が丸ごと自分のアドレスなので、1 台で何人でも作れる。
//! 読む側の要求も 1 回として数えられる (**測る行為が状態を変える**) ので、数を突き合わせる
//! ところは「何回目の応答か」を決めてから読む。

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use common::*;

/// `/status` や `/readers` の `readers` の配列だけを切り出す。
///
/// `memory.rings.readers` は**数**なので、配列の方は `"readers":[` で探す。
fn readers_array(json: &str) -> String {
    let pat = "\"readers\":[";
    let at = json
        .find(pat)
        .unwrap_or_else(|| panic!("readers が無い: {}", json))
        + pat.len();
    let end = json[at..]
        .find(']')
        .unwrap_or_else(|| panic!("readers の終わりが無い: {}", json));
    json[at..at + end].to_string()
}

/// `readers` の中の 1 行 (接続元 `client` のもの)。入れ子は無いので `}` まで取る。
fn reader_row(array: &str, client: &str) -> String {
    let head = format!("{{\"client\":\"{}\"", client);
    let at = array
        .find(&head)
        .unwrap_or_else(|| panic!("{} の行が無い: {}", client, array));
    let end = array[at..]
        .find('}')
        .unwrap_or_else(|| panic!("行の終わりが無い: {}", array));
    array[at..at + end + 1].to_string()
}

/// その行に `client` の行があるか (`127.9.0.1` が `127.9.0.10` に当たらないように
/// 閉じ引用符まで見る)。
fn has_reader(array: &str, client: &str) -> bool {
    array.contains(&format!("{{\"client\":\"{}\"", client))
}

/// 1 行の `last_path` (試験で使うパスに escape の要る文字は入れない)。
fn last_path(row: &str) -> String {
    let pat = "\"last_path\":\"";
    let at = row
        .find(pat)
        .unwrap_or_else(|| panic!("last_path が無い: {}", row))
        + pat.len();
    let end = row[at..].find('"').unwrap();
    row[at..at + end].to_string()
}

/// 送信元アドレスを決めて内部エンドポイントを 1 回引く (応答の全文を返す)。
#[cfg(target_os = "linux")]
fn get_from(src: [u8; 4], proxy_port: u16, path: &str) -> String {
    let mut stream = connect_from(src, proxy_port);
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(
            format!(
                "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                path, proxy_port
            )
            .as_bytes(),
        )
        .unwrap();
    let mut out = String::new();
    let _ = stream.read_to_string(&mut out);
    out
}

/// CONNECT を 1 本張って 1 往復して閉じる (プロキシとしての要求)。
fn connect_and_ping(proxy_port: u16, echo_port: u16) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let target = format!("127.0.0.1:{}", echo_port);
    stream
        .write_all(format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n", target, target).as_bytes())
        .unwrap();
    let head = read_connect_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    stream.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
}

/// 受け入れ基準そのもの: `/status` を 3 回引いた接続元が `count: 3` /
/// `last_path: "/status"`、プロキシとしての要求は数えない、`/readers` に全部。
#[test]
fn test_integration_readers_counts_internal_endpoints_only() {
    let echo = start_echo_server();
    let hits = Arc::new(AtomicUsize::new(0));
    let (origin, _origin) = start_counting_origin(hits, "");
    let proxy_port = start_test_proxy(proxy_config());

    // (1) `/status` を 3 回。**3 回目の応答**に自分の行が `count: 3` で出る
    //     (数えるのは `handle` の入口 = 応答を組む前なので、その要求自身が入っている)
    endpoint_json(proxy_port, "/status");
    endpoint_json(proxy_port, "/status");
    let third = endpoint_json(proxy_port, "/status");
    let row = reader_row(&readers_array(&third), "127.0.0.1");
    assert_eq!(status_number(&row, "count"), 3, "{}", row);
    assert_eq!(last_path(&row), "/status", "{}", row);
    assert!(status_number(&row, "last_at") > 0, "{}", row);

    // (2) プロキシとしての要求 (forward 1 本 + CONNECT 1 本) を通す
    let body = get_via_proxy(
        proxy_port,
        &format!("http://127.0.0.1:{}/x", origin),
        &format!("127.0.0.1:{}", origin),
    );
    assert!(body.starts_with("HTTP/1.1 200"), "{}", body);
    connect_and_ping(proxy_port, echo);

    //     4 回目の `/status` は `count: 4` (2 本のプロキシ要求は 1 件も入っていない)
    let fourth = endpoint_json(proxy_port, "/status");
    let row = reader_row(&readers_array(&fourth), "127.0.0.1");
    assert_eq!(
        status_number(&row, "count"),
        4,
        "プロキシとしての要求が混ざっている: {}",
        row
    );
    assert_eq!(last_path(&row), "/status", "{}", row);
    // `memory.rings` にも 1 行 (表が満杯のときの見積もり。T14.21 の作法)
    let rings = fourth
        .split("\"rings\":{")
        .nth(1)
        .unwrap_or_else(|| panic!("rings が無い: {}", fourth));
    assert!(
        status_number(rings, "readers") > 0,
        "rings に readers が無い"
    );

    // (3) `/readers` は全部を出す。この要求自身も 1 回 (5 回目) として入り、
    //     `last_path` は `/readers` になっている
    let all = endpoint_json(proxy_port, "/readers");
    let row = reader_row(&readers_array(&all), "127.0.0.1");
    assert_eq!(status_number(&row, "count"), 5, "{}", row);
    assert_eq!(last_path(&row), "/readers", "{}", row);
    assert!(all.contains("\"count\":1,\"shown\":1"), "{}", all);
    assert!(all.contains("\"truncated\":false"), "{}", all);
    assert!(all.contains("\"max_readers\":256"), "{}", all);
    assert!(all.contains("\"max_path\":64"), "{}", all);
    // 表はメモリだけ (状態ファイルには残らない)
    assert!(all.contains("\"persisted\":false"), "{}", all);
    assert!(all.len() <= 256 * 1024, "{} B", all.len());

    // (4) 同じ接続元は `clients[]` にも居る (**別の表**: あちらは通した要求だけ、
    //     こちらは自分宛てだけ)。CONNECT はトンネルが閉じたときに数えられる
    wait_until(
        || endpoint_json(proxy_port, "/clients").contains("\"client\":\"127.0.0.1\""),
        "clients に 127.0.0.1 が出る",
    );
    let clients = endpoint_json(proxy_port, "/clients");
    // `/clients` には読み手の表は出さない (口が違う)
    assert!(!clients.contains("\"last_path\""), "{}", clients);
}

/// 問い合わせ文字列は落とし、長いパスは 64 バイトで切る。知らないパス (404) も数える
/// (走査を見つけるのがこの表の仕事なので、200 だけを数えても意味が無い)。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_readers_drops_the_query_and_clips_the_path() {
    let proxy_port = start_test_proxy(proxy_config());

    // 別の接続元 2 人に引かせる (自分で `/status` を読むと `last_path` が動くため)
    let ok = get_from([127, 0, 0, 2], proxy_port, "/clients?sort=recent&limit=5");
    assert!(ok.starts_with("HTTP/1.1 200 "), "{}", ok);
    let long = format!("/{}", "a".repeat(200));
    let missing = get_from([127, 0, 0, 3], proxy_port, &long);
    assert!(missing.starts_with("HTTP/1.1 404 "), "{}", missing);

    let array = readers_array(&endpoint_json(proxy_port, "/status"));
    let row2 = reader_row(&array, "127.0.0.2");
    assert_eq!(
        last_path(&row2),
        "/clients",
        "問い合わせが残っている: {}",
        row2
    );
    let row3 = reader_row(&array, "127.0.0.3");
    let clipped = last_path(&row3);
    assert!(clipped.len() <= 64, "{} B: {}", clipped.len(), clipped);
    assert!(clipped.starts_with("/aaaa"), "{}", clipped);
    assert!(clipped.ends_with('…'), "{}", clipped);
    assert_eq!(status_number(&row3, "count"), 1, "{}", row3);
}

/// 表は 256 行で頭打ちで、溢れたら**最後に引いたのがいちばん古い行**から捨てる。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_readers_keep_256_and_drop_the_oldest() {
    let proxy_port = start_test_proxy(proxy_config());

    // 260 人に 1 回ずつ引かせる (`127.9.0.1` … `127.9.1.10`)
    let addr = |i: usize| [127, 9, (i / 250) as u8, (i % 250 + 1) as u8];
    for i in 0..260 {
        let out = get_from(addr(i), proxy_port, "/status");
        assert!(out.starts_with("HTTP/1.1 200 "), "{} 人目: {}", i, out);
    }

    // 読む側 (127.0.0.1) も 1 人として入るので、表は 256 行のまま
    let all = endpoint_json(proxy_port, "/readers");
    let tail = all.rsplit(']').next().unwrap_or("");
    assert!(all.contains("\"count\":256,\"shown\":256"), "{}", tail);
    let array = readers_array(&all);
    assert!(has_reader(&array, "127.0.0.1"), "読む側が居ない");
    // いちばん古い (= いちばん先に引いた) 行は押し出され、最後の 1 人は残っている
    assert!(!has_reader(&array, "127.9.0.1"), "最古が残っている");
    assert!(has_reader(&array, "127.9.1.10"), "最後の 1 人が居ない");
    assert!(all.len() <= 256 * 1024, "{} B", all.len());
}
