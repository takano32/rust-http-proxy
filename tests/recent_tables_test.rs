//! 個票のエンドポイント `/dns` `/log` `/hosts` と、カーネルの RTT と再送 (T13.4・T14.5) の結合テスト。
//!
//! 初めは `tests/recent_test.rs` にあった (T34.117 で割った)。

mod common;

use std::net::TcpListener;

use common::*;

/// 解決したホストが `/dns` に見え、失敗した名前が**負のキャッシュ**として見えること
/// (T13.4 の受け入れ基準 (c))。
#[test]
fn test_integration_dns_shows_the_resolver_table() {
    let (origin_port, _origin) = start_mock_origin();
    let proxy_port = start_test_proxy(proxy_config());

    // (1) 引ける名前 (`localhost` は表を通る。IP リテラルは通らない)
    let named = format!("localhost:{}", origin_port);
    let r = get_via_proxy(proxy_port, &format!("http://{}/ok", named), &named);
    assert!(r.starts_with("HTTP/1.1 200 OK"), "{}", r);
    // (2) 引けない名前 → 負のキャッシュ (`PROXY_DNS_NEGATIVE_SECS` の既定 60 秒)
    let bogus = "t134-no-such-host.invalid:80";
    let r = get_via_proxy(proxy_port, &format!("http://{}/x", bogus), bogus);
    assert!(r.starts_with("HTTP/1.1 502"), "{}", r);

    let json = endpoint_json(proxy_port, "/dns");
    assert!(json.contains("\"sort\":\"age\""), "{}", json);
    assert!(json.contains("\"ttl_secs\":"), "{}", json);
    assert!(json.contains("\"negative_ttl_secs\":"), "{}", json);
    // 引けた名前: アドレスと残り TTL と「最後に使ってからの秒」がある
    assert!(json.contains("\"host\":\"localhost\""), "{}", json);
    // 答えの並びは OS の resolver しだい (CI の runner は `::1` が先) なので、順番は見ない
    let addrs = json
        .split("\"host\":\"localhost\",\"addrs\":[")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .unwrap_or_else(|| panic!("localhost の addrs が無い: {}", json));
    assert!(
        addrs.contains("\"127.0.0.1\""),
        "addrs に 127.0.0.1 が無い: {}",
        json
    );
    assert!(json.contains("\"ttl_left\":"), "{}", json);
    assert!(json.contains("\"idle_secs\":"), "{}", json);
    assert!(json.contains("\"refreshing\":false"), "{}", json);
    // 引けなかった名前: 負のキャッシュとして理由つきで見える
    assert!(
        json.contains("\"host\":\"t134-no-such-host.invalid\""),
        "{}",
        json
    );
    assert!(json.contains("\"failed\":{\"secs_ago\":"), "{}", json);
    assert!(json.contains("\"error\":\""), "{}", json);
    // IP リテラルは表を通らない (`127.0.0.1` のホスト行は無い)
    assert!(!json.contains("\"host\":\"127.0.0.1\""), "{}", json);

    // 並べ替え: `host` は名前順、知らない値は既定 (`age`) に倒れる
    let by_host = endpoint_json(proxy_port, "/dns?sort=host");
    assert!(by_host.contains("\"sort\":\"host\""), "{}", by_host);
    let hosts: Vec<&str> = by_host
        .split("{\"host\":\"")
        .skip(1)
        .map(|s| s.split('"').next().unwrap_or(""))
        .collect();
    let mut sorted = hosts.clone();
    sorted.sort_unstable();
    assert_eq!(hosts, sorted, "{}", by_host);
    assert!(endpoint_json(proxy_port, "/dns?sort=misses").contains("\"sort\":\"misses\""),);
    assert!(endpoint_json(proxy_port, "/dns?sort=nonsense").contains("\"sort\":\"age\""),);
    // `?limit=` は件数を絞る
    let one = endpoint_json(proxy_port, "/dns?limit=1");
    assert_eq!(one.matches("{\"host\":\"").count(), 1, "{}", one);
    assert!(one.contains("\"shown\":1"), "{}", one);
    // `?offset=` (T27.1): `/hosts` と同じ読み方。表には少なくとも 2 名前ある (localhost と負のキャッシュ)
    let first_host = |json: &str| {
        json.split("{\"host\":\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .unwrap_or_else(|| panic!("host が無い: {}", json))
            .to_string()
    };
    let p1 = endpoint_json(proxy_port, "/dns?sort=host&limit=1");
    assert!(p1.contains("\"offset\":0,\"next_offset\":1}"), "{}", p1);
    let p2 = endpoint_json(proxy_port, "/dns?sort=host&limit=1&offset=1");
    assert!(p2.contains("\"offset\":1,"), "{}", p2);
    assert_eq!(p2.matches("{\"host\":\"").count(), 1, "{}", p2);
    // 2 枚目は 1 枚目の続き (名前順で後ろ。ほかのテストが表に足しても前には戻らない)
    assert!(first_host(&p2) >= first_host(&p1), "{} / {}", p1, p2);
    // 表を越えた offset は 0 件で、続きは無い
    let past = endpoint_json(proxy_port, "/dns?offset=4096");
    assert!(past.contains("\"entries\":[]"), "{}", past);
    assert!(past.contains("\"next_offset\":null}"), "{}", past);
}

/// `log_warn!` の直後に `/log` に同じ行が見え、`info` のアクセスログは見えないこと
/// (T13.4 の受け入れ基準 (d))。
#[test]
fn test_integration_log_shows_warnings_but_not_the_access_log() {
    let (origin_port, _origin) = start_mock_origin();
    let dead_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let proxy_port = start_test_proxy(proxy_config());

    // info のアクセスログが出る要求 (200)
    let ok = format!("127.0.0.1:{}", origin_port);
    let r = get_via_proxy(proxy_port, &format!("http://{}/ok", ok), &ok);
    assert!(r.starts_with("HTTP/1.1 200 OK"), "{}", r);
    // warn が出る要求 (502 Bad Gateway。`crates/http` の `log_warn!`)
    let dead = format!("127.0.0.1:{}", dead_port);
    let r = get_via_proxy(proxy_port, &format!("http://{}/x", dead), &dead);
    assert!(r.starts_with("HTTP/1.1 502"), "{}", r);

    let json = endpoint_json(proxy_port, "/log");
    assert!(json.contains("\"capacity\":1000"), "{}", json);
    assert!(json.contains("\"level\":\"info\""), "{}", json);
    // 出した warn がそのまま 1 行として見える (接続番号つき)
    assert!(json.contains("\"level\":\"warn\""), "{}", json);
    assert!(
        json.contains(&format!("502 Bad Gateway: connect 127.0.0.1:{}", dead_port)),
        "warn の行が無い: {}",
        json
    );
    assert!(json.contains("\"conn\":"), "{}", json);
    let at = status_number(&json, "at");
    assert!(at > 1_700_000_000, "時刻が epoch 秒でない: {}", at);
    // `info` のアクセスログは写していない (熱い経路。T10.10)
    assert!(
        !json.contains("ACCESS"),
        "アクセスログが入っている: {}",
        json
    );
    assert!(
        !json.contains("(internal endpoint)"),
        "info が入っている: {}",
        json
    );
    // 行の側に `info` は 1 つも無い (末尾の `"level":"info"` は今のログ水準の表示)
    assert!(
        !json.contains("\"level\":\"info\",\"conn\""),
        "info の行が入っている: {}",
        json
    );

    // `?n=1` は新しい 1 行だけ
    let one = endpoint_json(proxy_port, "/log?n=1");
    assert_eq!(one.matches("{\"at\":").count(), 1, "{}", one);
    assert!(one.contains("\"count\":1"), "{}", one);
}

/// 60 ホストへ要求したあと `/hosts?limit=1000` が 60 件、`/status` は 50 件のまま
/// (T13.4 の受け入れ基準 (e))。
#[test]
fn test_integration_hosts_lists_every_host_while_status_keeps_fifty() {
    let (origin_port, _origin) = start_mock_origin();
    let dead_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let proxy_port = start_test_proxy(proxy_config());

    // 60 ホスト分の鍵を作る。ホスト別統計の鍵は要求ターゲットの `scheme://host:port`
    // なので、宛先が同じでも `127.0.0.N` を変えれば別のホストとして数えられる
    // (待ち受けは 127.0.0.1 だけなので 2 番以降は 502 になるが、鍵は立つ)
    for i in 0..59 {
        let host = format!("127.0.0.{}:{}", i + 1, origin_port);
        let _ = get_via_proxy(proxy_port, &format!("http://{}/h{}", host, i), &host);
    }
    // 60 ホスト目はエラーを **2 件** 持つホスト (`?sort=errors` の先頭になる。
    // 他のホストは 1 件なので、同点崩しではなくエラー数で先頭に来る)
    let dead = format!("127.0.0.99:{}", dead_port);
    for _ in 0..2 {
        let r = get_via_proxy(proxy_port, &format!("http://{}/x", dead), &dead);
        assert!(r.starts_with("HTTP/1.1 502"), "{}", r);
    }

    let count_hosts = |json: &str| json.matches("{\"host\":\"").count();
    let hosts = endpoint_json(proxy_port, "/hosts?limit=1000");
    assert_eq!(count_hosts(&hosts), 60, "{}", hosts);
    assert!(hosts.contains("\"count\":60"), "{}", hosts);
    assert!(hosts.contains("\"shown\":60"), "{}", hosts);
    assert!(hosts.contains("\"sort\":\"requests\""), "{}", hosts);
    assert!(!hosts.contains("\"truncated\":true"), "{}", hosts);
    // `/status` の 50 は変えない
    let status = status_json(proxy_port);
    assert_eq!(count_hosts(&status), 50, "{}", status);

    // `/status` の `hosts[]` と同じ形 (内訳の列も同じ名前)
    for key in [
        "\"requests\":",
        "\"errors\":",
        "\"timed\":",
        "\"avg_ms\":",
        "\"p50_ms\":",
        "\"p95_ms\":",
        "\"dns_ms_sum\":",
        "\"dns_misses\":",
        "\"connect_ms_sum\":",
        "\"v4_wins\":",
        "\"errors_by_cause\":[",
    ] {
        assert!(hosts.contains(key), "{} が無い: {}", key, hosts);
    }
    // `scripts/status-diff.py` が読む窓の目印も同じ名前で出る
    assert!(hosts.contains("\"uptime_secs\":"), "{}", hosts);
    assert!(hosts.contains("\"total_requests\":"), "{}", hosts);
    assert!(hosts.contains("\"restored_since\":"), "{}", hosts);

    // `?sort=` は `/status?sort=` と同じ鍵。エラー 1 件のホストが先頭に来る
    let by_errors = endpoint_json(proxy_port, "/hosts?sort=errors&limit=1000");
    assert!(by_errors.contains("\"sort\":\"errors\""), "{}", by_errors);
    let first = by_errors
        .split("{\"host\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap_or("");
    assert_eq!(first, format!("http://{}", dead), "{}", by_errors);
    // 知らない値は既定に倒す。`?limit=` は件数を絞る
    assert!(endpoint_json(proxy_port, "/hosts?sort=nonsense").contains("\"sort\":\"requests\""));
    let ten = endpoint_json(proxy_port, "/hosts?limit=10");
    assert_eq!(count_hosts(&ten), 10, "{}", ten);
    assert!(ten.contains("\"count\":60"), "{}", ten);
}

// ---------------------------------------------------------------------------
// カーネルの RTT と再送 (`TCP_INFO`。T14.5)

// ---------------------------------------------------------------------------

/// 閉じたトンネルの個票に**両側**のカーネルの RTT が載り、`/hosts` と `/status` の
/// `clients[]` にも出ること (T14.5)。
///
/// loopback なので RTT は両側とも 1 ms 未満、再送は 0 が期待値。
#[test]
#[cfg(target_os = "linux")]
fn test_integration_recent_records_the_kernel_rtt_of_both_sides() {
    use std::io::{Read, Write};

    let origin_port = start_echo_origin();
    let proxy_port = start_test_proxy(park_config());

    let mut tunnel = open_tunnel(proxy_port, origin_port);
    tunnel.write_all(b"rtt").unwrap();
    let mut back = [0u8; 3];
    tunnel.read_exact(&mut back).unwrap();
    tunnel.shutdown(std::net::Shutdown::Write).unwrap();
    let mut rest = Vec::new();
    let _ = tunnel.read_to_end(&mut rest);
    drop(tunnel);

    wait_until(
        || endpoint_json(proxy_port, "/recent").contains("\"kind\":\"connect\""),
        "閉じたトンネルが /recent に出る",
    );
    let json = endpoint_json(proxy_port, "/recent");

    // ---- 個票: 両側の RTT と再送 ----
    let rtt = json
        .split("\"rtt_ms\":")
        .nth(1)
        .unwrap_or_else(|| panic!("rtt_ms が無い: {}", json));
    let client_rtt = json::number_opt(rtt, "client").expect("クライアント側の RTT が null");
    let origin_rtt = json::number_opt(rtt, "origin").expect("オリジン側の RTT が null");
    assert!(
        client_rtt < 1.0 && client_rtt > 0.0,
        "loopback のクライアント側 RTT が 1 ms 未満でない: {} ms ({})",
        client_rtt,
        json
    );
    assert!(
        origin_rtt < 1.0 && origin_rtt > 0.0,
        "loopback のオリジン側 RTT が 1 ms 未満でない: {} ms ({})",
        origin_rtt,
        json
    );
    assert!(
        json.contains("\"retrans\":{\"client\":0,\"origin\":0}"),
        "loopback で再送が出ている: {}",
        json
    );

    // ---- `/hosts`: オリジン側 (`connect://127.0.0.1:<port>` の行だけを見る) ----
    let hosts = endpoint_json(proxy_port, "/hosts");
    let key = format!("\"host\":\"connect://127.0.0.1:{}\"", origin_port);
    let host_rtt = hosts
        .split(&key)
        .nth(1)
        .unwrap_or_else(|| panic!("{} が /hosts に無い: {}", key, hosts))
        .split("\"rtt_ms\":")
        .nth(1)
        .unwrap_or_else(|| panic!("/hosts に rtt_ms が無い: {}", hosts));
    let avg = json::number_opt(host_rtt, "avg").expect("/hosts の avg が null");
    let min = json::number_opt(host_rtt, "min").expect("/hosts の min が null");
    assert!(avg < 1.0 && min <= avg, "/hosts の RTT が変: {}", hosts);
    // 標本はトンネル 1 本の終わりの 1 つだけ (要求ごとには読まない)
    assert!(host_rtt.contains("\"samples\":1"), "{}", hosts);
    assert!(host_rtt.contains("\"retrans\":0"), "{}", hosts);

    // ---- `/status` の `clients[]`: クライアント側 ----
    let status = status_json(proxy_port);
    let clients = status
        .split("\"clients\":[")
        .nth(1)
        .unwrap_or_else(|| panic!("clients[] が無い: {}", status));
    assert!(clients.contains("\"client\":\"127.0.0.1\""), "{}", status);
    let c_rtt = clients
        .split("\"rtt_ms\":")
        .nth(1)
        .unwrap_or_else(|| panic!("clients[] に rtt_ms が無い: {}", status));
    let c_avg = json::number_opt(c_rtt, "avg").expect("clients[] の avg が null");
    assert!(c_avg < 1.0, "接続元の RTT が 1 ms 未満でない: {}", c_avg);
    assert!(c_rtt.contains("\"retrans\":0"), "{}", status);

    // ---- `/metrics`: 全体の sum / count (ホスト別は出さない) ----
    // `/metrics` は Prometheus 形式だが、本文をそのまま返す口は同じもの。
    // **自分宛てだけの接続は入らない** (`/recent` `/hosts` `/status` を引いただけの接続は
    // `clients[]` に RTT を 1 本も残さない = T15.0。`sorahost_rtt_seconds` の client 側は
    // その表の合計 = `Metrics::rtt_totals`)。ここで数えるのは**トンネル 1 本ぶん**で、
    // オリジン側は接続プールが捨てるときにも残るので、どちらも下限だけを見る
    let prom = endpoint_json(proxy_port, "/metrics");
    let rtt_lines: Vec<&str> = prom
        .lines()
        .filter(|l| l.starts_with("sorahost_rtt_seconds"))
        .collect();
    for side in ["client", "origin"] {
        let count = rtt_lines
            .iter()
            .find(|l| l.starts_with(&format!("sorahost_rtt_seconds_count{{side=\"{}\"}}", side)))
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("{} の _count が無い: {:?}", side, rtt_lines));
        assert!(count >= 1, "{} の標本が 0: {:?}", side, rtt_lines);
        let sum = rtt_lines
            .iter()
            .find(|l| l.starts_with(&format!("sorahost_rtt_seconds_sum{{side=\"{}\"}}", side)))
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("{} の _sum が無い: {:?}", side, rtt_lines));
        assert!(sum > 0.0 && sum < 1.0, "{} の秒が変: {:?}", side, rtt_lines);
    }
    assert!(
        !prom.contains("sorahost_host_rtt_seconds"),
        "ホスト別の RTT は出さない (系列が増えすぎる)"
    );
}

/// http の keep-alive 接続も、閉じるときにクライアント側の RTT を 1 回だけ読むこと (T14.5)。
#[test]
#[cfg(target_os = "linux")]
fn test_integration_a_closed_http_connection_records_the_client_rtt() {
    use std::io::Write;

    let (origin_port, _origin) = common::start_keepalive_origin(
        std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    );
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut s = std::net::TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    common::one_keepalive_request(&mut s, &host, "/a");
    s.write_all(
        format!(
            "GET http://{}/b HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            host, host
        )
        .as_bytes(),
    )
    .unwrap();
    let _ = common::read_response(&mut s);
    drop(s);

    wait_until(
        || endpoint_json(proxy_port, "/recent").contains("\"kind\":\"http\""),
        "閉じた http 接続が /recent に出る",
    );
    let json = endpoint_json(proxy_port, "/recent");
    let entry = json
        .split("\"kind\":\"http\"")
        .nth(1)
        .unwrap_or_else(|| panic!("http の 1 件が無い: {}", json));
    let rtt = entry
        .split("\"rtt_ms\":")
        .nth(1)
        .unwrap_or_else(|| panic!("rtt_ms が無い: {}", json));
    let client_rtt = json::number_opt(rtt, "client").expect("クライアント側の RTT が null");
    assert!(client_rtt < 1.0, "{} ms: {}", client_rtt, json);
    // オリジン側はトンネルではないので個票には載らない (プールが捨てるときにホスト別へ)
    assert!(
        rtt.starts_with(&format!("{{\"client\":{},\"origin\":null}}", client_rtt)),
        "http の個票にオリジン側が載っている: {}",
        &rtt[..60.min(rtt.len())]
    );
}
