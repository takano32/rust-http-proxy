//! 繋がらなかった接続が、確立時間の窓と直近の標本に入らないことの結合テスト (T20.4)。
//!
//! 失敗した CONNECT は「失敗するまでの時間」を統計へ渡していて、`0.18.0` までは成否を
//! 見ずに `/history` の確立の区間 (`connects` / `connect_ms_*` / `connect_buckets`) と
//! `/status` の `recent_quantiles.connect`・`wait` に入っていた。IPv6 だけの宛先への
//! 再試行 (1 本 約 1.3 秒) が接続の 17% を占めた週は、p90 も p95 も秒になって速さの
//! 物差しにならなかった (TODO.md T18.99)。
//!
//! 単体テスト (`crates/metrics-core/src/metrics.rs`) は `record()` の入り口で同じことを
//! 見ている。ここで見るのは**実際に通した接続が口にどう出るか**: 繋がらない宛先への
//! CONNECT を 3 本と、繋がる CONNECT を 1 本送ったあとで、
//!
//! - `/status` の `recent_quantiles.connect.n` と `wait.n` は 1 (繋がった 1 本だけ)
//! - `/history` の `connects`・`connect_buckets` の合計・`waits` は 1
//! - `errors`・`errors_by_cause` の合計は 3、ホスト別の行は 3 件とも数えている
//!
//! 繋がらない宛先は `RefusedPort` (束縛したまま `listen` しないソケット。T20.2)。
//! `--lite` の旗 (`profile`) は処理系で 1 つなので、このファイルのテストは 1 本だけにする。

use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

mod common;
use common::*;

/// history スレッドの周期 (本番は 5 秒)。
const TICK: Duration = Duration::from_millis(100);

/// 繋がらない宛先へ送る CONNECT の本数。
const FAILURES: u64 = 3;

/// `"keys":[…]` の中身を綴りの並びとして取り出す (`tests/history_columns_test.rs` と同じ)。
fn str_array(json: &str, key: &str) -> Vec<String> {
    let pat = format!("\"{}\":[", key);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("no {} in {}", key, &json[..200]))
        + pat.len();
    let end = at + json[at..].find(']').expect("unterminated array");
    json[at..end]
        .split(',')
        .map(|s| s.trim_matches('"').to_string())
        .collect()
}

/// 標本の配列の各行を、**入れ子の配列を 1 列と数えて**切り出す。
fn rows(json: &str) -> Vec<Vec<String>> {
    let at = json.find("\"samples\":[").expect("no samples") + "\"samples\":[".len();
    let mut out = Vec::new();
    let (mut depth, mut cur, mut field) = (0usize, Vec::new(), String::new());
    for c in json[at..].chars() {
        match c {
            '[' if depth == 0 => depth = 1,
            '[' => {
                depth += 1;
                field.push(c);
            }
            ',' if depth == 1 => {
                cur.push(std::mem::take(&mut field));
            }
            ']' if depth == 1 => {
                cur.push(std::mem::take(&mut field));
                out.push(std::mem::take(&mut cur));
                depth = 0;
            }
            ']' if depth == 0 => break,
            ']' => {
                depth -= 1;
                field.push(c);
            }
            _ if depth >= 1 => field.push(c),
            _ => {}
        }
    }
    out
}

/// 列の名前で、全部の標本を足した値 (入れ子の配列は中身を全部足す)。
fn column_sum(json: &str, name: &str) -> u64 {
    let keys = str_array(json, "keys");
    let i = keys
        .iter()
        .position(|k| k == name)
        .unwrap_or_else(|| panic!("no column {}", name));
    rows(json)
        .iter()
        .map(|row| {
            row[i]
                .split(|c: char| !c.is_ascii_digit())
                .filter(|s| !s.is_empty())
                .map(|s| s.parse::<u64>().unwrap())
                .sum::<u64>()
        })
        .sum()
}

/// 列の名前で、全部の標本の最大 (`*_ms_max` 用)。
fn column_max(json: &str, name: &str) -> u64 {
    let keys = str_array(json, "keys");
    let i = keys.iter().position(|k| k == name).unwrap();
    rows(json)
        .iter()
        .map(|row| row[i].parse::<u64>().unwrap())
        .max()
        .unwrap_or(0)
}

/// `"key":{...}` の中身を切り出す (入れ子つき。`tests/quantiles_test.rs` と同じ)。
fn object_of(json: &str, key: &str) -> String {
    let pat = format!("\"{}\":{{", key);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, json))
        + pat.len()
        - 1;
    let rest = &json[at..];
    let mut depth = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return rest[..=i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("{} が閉じていない: {}", key, rest);
}

/// `"key":<数>` を読む。
fn num(obj: &str, key: &str) -> f64 {
    let pat = format!("\"{}\":", key);
    let at = obj
        .find(&pat)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, obj))
        + pat.len();
    obj[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{} が数でない: {}", key, obj))
}

/// CONNECT を 1 本送り、応答の先頭行を返す (接続はすぐ閉じる)。
fn connect_once(proxy_port: u16, target_port: u16) -> String {
    let mut t = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    t.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    t.write_all(
        format!(
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            target_port, target_port
        )
        .as_bytes(),
    )
    .unwrap();
    read_connect_response(&mut t)
}

/// **受け入れ基準 (T20.4)**: 繋がらない宛先への CONNECT 3 本は `errors` とホスト別の行に
/// 残り、確立の窓・直近の標本・`wait` には繋がった 1 本だけが入る。
#[test]
fn test_integration_failed_connects_stay_out_of_the_connect_windows() {
    // 既定のプロファイル (= `--lite` ではない)。`wait` と直近の標本はこの旗の内側
    rust_http_proxy::profile::set_enabled(true);

    let echo_port = start_echo_server();
    let dead = RefusedPort::reserve();
    let (proxy_port, metrics) = start_test_proxy_with_history(proxy_config(), TICK);

    for _ in 0..FAILURES {
        let head = connect_once(proxy_port, dead.port());
        assert!(head.starts_with("HTTP/1.1 502"), "{}", head);
    }
    let head = connect_once(proxy_port, echo_port);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);

    // 統計を書くのは失敗は 502 の直後、成功はトンネルの終わり (`tunnel::report`)。
    // 指標を直に見て、4 本とも数え終わるのを待つ (ホスト別の行は成否を問わず増える)
    let dead_key = format!("connect://127.0.0.1:{}", dead.port());
    let ok_key = format!("connect://127.0.0.1:{}", echo_port);
    let requests_of = |key: &str| {
        metrics
            .hosts_sorted()
            .iter()
            .find(|(h, _)| h == key)
            .map_or(0, |(_, s)| s.requests)
    };
    wait_until(
        || requests_of(&dead_key) == FAILURES && requests_of(&ok_key) == 1,
        "the four connects to be counted",
    );
    // history スレッドが区間を標本に移し終わるのを待つ。標本は周期ごとに 1 本積まれる
    // (空でも積む) ので、**4 本を数え終えたあとに 2 本積まれれば**区間は必ず移っている
    // (1 本目は、区間を読んだあと積む前の周期に当たっているかもしれない)。
    // エラー 3 件と成功の 1 本がどの標本に入るかは周期しだいなので、全部の標本を足して見る
    let history = || endpoint_json(proxy_port, "/history?res=5");
    let before = rows(&history()).len();
    wait_until(
        || rows(&history()).len() >= before + 2,
        "the history thread to take the interval",
    );
    let h = history();
    let status = status_json(proxy_port);
    let q = object_of(&status, "recent_quantiles");
    let (qc, qw) = (object_of(&q, "connect"), object_of(&q, "wait"));
    let host_row = {
        let pat = format!("{{\"host\":\"{}\",", dead_key);
        let at = status
            .find(&pat)
            .unwrap_or_else(|| panic!("ホスト別の行が無い: {}", status));
        status[at..].to_string()
    };
    // 前後の表に貼る 1 行 (`--nocapture` で読む)
    println!(
        "t204: connects={} connect_buckets={} connect_ms_max={} waits={} errors={} \
         errors_by_cause={} recent.connect.n={} recent.wait.n={} host.requests={} host.errors={}",
        column_sum(&h, "connects"),
        column_sum(&h, "connect_buckets"),
        column_max(&h, "connect_ms_max"),
        column_sum(&h, "waits"),
        column_sum(&h, "errors"),
        column_sum(&h, "errors_by_cause"),
        num(&qc, "n"),
        num(&qw, "n"),
        num(&host_row, "requests"),
        num(&host_row, "errors"),
    );

    // 残る方: エラーと原因別、ホスト別の行
    assert_eq!(column_sum(&h, "errors"), FAILURES, "{}", h);
    assert_eq!(column_sum(&h, "errors_by_cause"), FAILURES, "{}", h);
    assert_eq!(num(&host_row, "requests"), FAILURES as f64, "{}", host_row);
    assert_eq!(num(&host_row, "errors"), FAILURES as f64, "{}", host_row);
    let (_, dead_stats) = metrics
        .hosts_sorted()
        .into_iter()
        .find(|(h, _)| *h == dead_key)
        .unwrap();
    assert_eq!(
        dead_stats.timed, FAILURES,
        "ホスト別の行は失敗に掛かった時間も数えたまま"
    );

    // 入らない方: `/history` の確立の区間と `wait`
    assert_eq!(column_sum(&h, "connects"), 1, "{}", h);
    assert_eq!(column_sum(&h, "connect_buckets"), 1, "{}", h);
    assert_eq!(column_sum(&h, "waits"), 1, "{}", h);
    assert_eq!(column_sum(&h, "wait_buckets"), 1, "{}", h);
    // 入らない方: `/status` の直近の標本
    assert_eq!(num(&qc, "n"), 1.0, "{}", q);
    assert_eq!(num(&qw, "n"), 1.0, "{}", q);
}
