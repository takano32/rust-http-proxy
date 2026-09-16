//! 個票のエンドポイントの結合テスト (T13.4): `/errors` `/connections` `/dns` `/log` `/hosts`。
//!
//! 集計 (`/status`) と時系列 (`/history`) では読めない「**誰が・いつ・なぜ**」を
//! 出す口なので、見るのは「実際に起きたことがその形で出てくるか」だけ。

mod common;

use std::net::TcpListener;

use common::*;

/// 閉じたポートへの CONNECT が `/errors` に 1 件 (原因 `refused`、宛先、接続 ms) 残ること。
#[test]
fn test_integration_errors_records_a_refused_connect() {
    let dead_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let proxy_port = start_test_proxy(proxy_config());

    // 空の `/errors` (まだ何も起きていない)
    let empty = endpoint_json(proxy_port, "/errors");
    assert!(empty.contains("\"errors\":[]"), "{}", empty);
    assert!(empty.contains("\"recorded\":0"), "{}", empty);
    assert!(empty.contains("\"capacity\":500"), "{}", empty);

    let target = format!("127.0.0.1:{}", dead_port);
    let out = raw_get(
        proxy_port,
        &format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n", target, target),
    );
    assert!(out.starts_with("HTTP/1.1 502"), "{}", out);

    let json = endpoint_json(proxy_port, "/errors");
    assert!(json.contains("\"recorded\":1"), "{}", json);
    assert!(json.contains("\"count\":1"), "{}", json);
    assert!(json.contains("\"kind\":\"connect\""), "{}", json);
    assert!(json.contains("\"cause\":\"refused\""), "{}", json);
    assert!(json.contains("\"status\":502"), "{}", json);
    assert!(
        json.contains(&format!("\"target\":\"{}\"", target)),
        "宛先が無い: {}",
        json
    );
    assert!(json.contains("\"client\":\"127.0.0.1\""), "{}", json);
    // 名前解決と接続の ms、時刻 (epoch 秒) の欄があること
    assert!(json.contains("\"dns_ms\":"), "{}", json);
    assert!(json.contains("\"connect_ms\":"), "{}", json);
    let at = status_number(&json, "at");
    assert!(at > 1_700_000_000, "時刻が epoch 秒でない: {}", at);
    assert!(!json.contains("\"truncated\":true"), "{}", json);
}

/// forward の 502 も `/errors` に残り、`?n=` が件数を絞ること (新しい順)。
#[test]
fn test_integration_errors_keeps_the_newest_first_and_honours_n() {
    let dead_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let proxy_port = start_test_proxy(proxy_config());

    // 2 つの宛先へ順に失敗させる (2 件目が新しい)
    for path in ["first", "second"] {
        let dead = format!("127.0.0.1:{}", dead_port);
        let r = get_via_proxy(proxy_port, &format!("http://{}/{}", dead, path), &dead);
        assert!(r.starts_with("HTTP/1.1 502"), "{}", r);
    }
    let json = endpoint_json(proxy_port, "/errors");
    assert!(json.contains("\"recorded\":2"), "{}", json);
    assert!(json.contains("\"kind\":\"forward\""), "{}", json);
    assert!(json.contains("\"cause\":\"refused\""), "{}", json);
    // forward の宛先はホスト別統計と同じ鍵 (`scheme://host:port`)
    assert!(
        json.contains(&format!("\"target\":\"http://127.0.0.1:{}\"", dead_port)),
        "{}",
        json
    );

    // `?n=1` は 1 件だけ (リングに 2 件あっても)
    let one = endpoint_json(proxy_port, "/errors?n=1");
    assert!(one.contains("\"count\":1"), "{}", one);
    assert!(one.contains("\"kept\":2"), "{}", one);
    assert_eq!(one.matches("\"cause\":").count(), 1, "{}", one);
    // 知らない / 壊れた問い合わせは既定に倒す
    let bad = endpoint_json(proxy_port, "/errors?n=abc&x=1");
    assert!(bad.contains("\"count\":2"), "{}", bad);
}

/// 読むだけで何も返さず、閉じもしないリスナー (`tests/overload_test.rs` と同じ道具)。
///
/// ここへ張ったトンネルでクライアントが送信側だけ閉じると**片方向だけ EOF** になり、
/// 預けられない = ずっと `relaying` のトンネルが作れる。
fn start_quiet_origin() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while !matches!(std::io::Read::read(&mut stream, &mut buf), Ok(0) | Err(_)) {}
                std::mem::forget(stream);
            });
        }
    });
    port
}

/// CONNECT を張って `200` まで読む。
fn open_tunnel(proxy_port: u16, target_port: u16) -> std::net::TcpStream {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let req = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        target_port, target_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).unwrap() == 0 {
            break;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    stream
}

/// 握ったトンネルが `/connections` に `parked` として、中継中のトンネルが
/// `relaying` として見えること (T13.4 の受け入れ基準 (b))。
#[test]
#[cfg(target_os = "linux")]
fn test_integration_connections_shows_parked_and_relaying_tunnels() {
    let origin_port = start_quiet_origin();
    let proxy_port = start_test_proxy(park_config());

    // (1) 両方向とも暇なトンネル → 猶予が過ぎたら預かり所へ (`parked`)
    let _idle = open_tunnel(proxy_port, origin_port);
    // (2) 送信側だけ閉じた (片方向 EOF) トンネル → 預けられないので `relaying` のまま
    let busy = open_tunnel(proxy_port, origin_port);
    busy.shutdown(std::net::Shutdown::Write).unwrap();

    wait_until(
        || endpoint_json(proxy_port, "/connections").contains("\"state\":\"parked\""),
        "トンネルが預けられる",
    );
    let json = endpoint_json(proxy_port, "/connections");
    assert!(json.contains("\"state\":\"parked\""), "{}", json);
    assert!(json.contains("\"state\":\"relaying\""), "{}", json);
    // どちらも CONNECT で、記述子は 2 本、宛先はオリジン
    assert_eq!(json.matches("\"kind\":\"connect\"").count(), 2, "{}", json);
    assert_eq!(json.matches("\"fds\":2").count(), 2, "{}", json);
    assert_eq!(
        json.matches(&format!("\"target\":\"127.0.0.1:{}\"", origin_port))
            .count(),
        2,
        "{}",
        json
    );
    // `/connections` を取りに来たこの接続自身も 1 行になる (keep-alive の HTTP、記述子 1 本)
    assert!(json.contains("\"kind\":\"http\""), "{}", json);
    assert!(json.contains("\"state\":\"serving\""), "{}", json);
    assert!(json.contains("\"fds\":1"), "{}", json);
    assert!(json.contains("\"client\":\"127.0.0.1\""), "{}", json);
    assert!(json.contains("\"lite\":false"), "{}", json);
    assert!(!json.contains("\"truncated\":true"), "{}", json);
    // 通し番号の小さい順 (古い順)
    let ids: Vec<u64> = json
        .split("{\"id\":")
        .skip(1)
        .map(|s| {
            s.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap()
        })
        .collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "{:?}", ids);
    assert!(
        json.contains(&format!("\"count\":{}", ids.len())),
        "{}",
        json
    );
}

/// `--lite` では登録しないので空の一覧 (T1.4 の方針)。
#[test]
fn test_integration_connections_is_empty_in_lite_mode() {
    let mut cfg = proxy_config();
    cfg.lite = true;
    let proxy_port = start_test_proxy(cfg);
    let json = endpoint_json(proxy_port, "/connections");
    assert!(json.contains("\"connections\":[]"), "{}", json);
    assert!(json.contains("\"count\":0"), "{}", json);
    assert!(json.contains("\"lite\":true"), "{}", json);
}

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
    assert!(json.contains("\"addrs\":[\"127.0.0.1\""), "{}", json);
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

/// ACL で拒否した CONNECT が `/errors` に `acl` として見えること (T14.2 (4))。
///
/// 403 は 5xx ではないので集計 (`errors_by_cause`) には乗らない。**誰が何を拒否されたか**を
/// 読めるのは個票だけなので、そこに出ることを見る。
#[test]
fn test_integration_errors_records_a_403_with_its_reason() {
    use rust_http_proxy::config::Config;
    use std::time::Duration;

    let mut cfg = Config::new(
        "0",
        None,
        Some("blocked.example, 127.0.0.1"),
        Duration::from_secs(5),
    )
    .unwrap();
    cfg.keepalive = Duration::from_secs(2);
    let proxy_port = start_test_proxy(cfg);

    // ACL で拒否される CONNECT (127.0.0.1 は deny_hosts に入れてある)
    let target = "127.0.0.1:443";
    let out = raw_get(
        proxy_port,
        &format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n", target, target),
    );
    assert!(out.starts_with("HTTP/1.1 403"), "{}", out);

    let json = endpoint_json(proxy_port, "/errors");
    assert!(json.contains("\"recorded\":1"), "{}", json);
    assert!(json.contains("\"cause\":\"acl\""), "{}", json);
    assert!(json.contains("\"status\":403"), "{}", json);
    assert!(json.contains("\"kind\":\"connect\""), "{}", json);
    assert!(
        json.contains(&format!("\"target\":\"{}\"", target)),
        "宛先が無い: {}",
        json
    );
    assert!(json.contains("\"client\":\"127.0.0.1\""), "{}", json);

    // 集計の方は今までどおり: 403 は `blocked` に数え、`errors_by_cause` は全部 0 のまま
    let status = endpoint_json(proxy_port, "/status");
    assert!(status.contains("\"blocked\":1"), "{}", status);
    assert!(
        status.contains("\"errors_by_cause\":[0,0,0,0,0,0,0,0]"),
        "403 が集計の原因別に混ざっている: {}",
        status
    );
}

/// keep-alive の HTTP 接続が `/connections` に**最初の要求の宛先つき**で見えること (T14.2 (5))。
///
/// T13.4 では `http` の行だけ宛先が空で、占有の内訳を読むときに何に使われている接続か
/// 分からなかった。**書くのは接続の最初の要求のとき 1 回だけ**なので、2 本目に別のホストへ
/// 要求しても宛先は変わらない (要求ごとに表を触らないという方針はそのまま)。
#[test]
fn test_integration_connections_shows_the_first_target_of_a_keepalive_http_connection() {
    let (first_port, _first) = start_mock_origin();
    let (second_port, _second) = start_mock_origin();
    let proxy_port = start_test_proxy(park_config());

    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let first_host = format!("127.0.0.1:{}", first_port);
    let (head, _) = one_keepalive_request(&mut stream, &first_host, "/one");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);

    let json = endpoint_json(proxy_port, "/connections");
    assert!(
        json.contains(&format!("\"target\":\"{}\",\"kind\":\"http\"", first_host)),
        "http の行に最初の宛先が無い: {}",
        json
    );
    // 記述子は 1 本 (クライアント側だけ。オリジンへの接続はプールが持つ)
    assert!(json.contains("\"fds\":1"), "{}", json);
    // `/connections` を取りに来た接続自身は自分宛てなので宛先を持たない
    assert!(
        json.contains("\"target\":\"\",\"kind\":\"http\""),
        "{}",
        json
    );

    // 2 本目は別のホストへ。宛先は最初のままで、行は増えない
    let second_host = format!("127.0.0.1:{}", second_port);
    let (head, _) = one_keepalive_request(&mut stream, &second_host, "/two");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    let json = endpoint_json(proxy_port, "/connections");
    assert!(
        json.contains(&format!("\"target\":\"{}\",\"kind\":\"http\"", first_host)),
        "2 本目で宛先が書き換わった: {}",
        json
    );
    assert!(
        !json.contains(&format!("\"target\":\"{}\"", second_host)),
        "要求ごとに宛先を書いている: {}",
        json
    );
}

// ---------------------------------------------------------------------------
// `/recent` — 閉じた接続の個票 (T14.4)
// ---------------------------------------------------------------------------

/// 読んだぶんをそのまま返し、クライアントが送信側を閉じたら自分も閉じるオリジン。
///
/// トンネルの「ふつうの終わり方」(クライアントが先に EOF) を待ち時間なしに作るための道具。
fn start_echo_origin() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
                // クライアントが送信側を閉じたので、こちらも閉じる (トンネルが終わる)
            });
        }
    });
    port
}

/// `/recent` の JSON から、`needle` を含む 1 件だけを切り出す。
fn one_entry(json: &str, needle: &str) -> String {
    json.split("{\"id\":")
        .skip(1)
        .map(|s| s.split('}').next().unwrap_or(s).to_string())
        .find(|s| s.contains(needle))
        .unwrap_or_else(|| panic!("{} の 1 件が無い: {}", needle, json))
}

/// `/recent` の JSON から `"id":N` の並びを取る (出てくる順 = 並べ替えの結果)。
fn recent_ids(json: &str) -> Vec<u64> {
    json.split("{\"id\":")
        .skip(1)
        .map(|s| {
            s.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap()
        })
        .collect()
}

/// (a) 閉じた CONNECT が `/recent` に 1 件 (理由 `client_eof`、寿命、バイト、段階の ms)。
#[test]
#[cfg(target_os = "linux")]
fn test_integration_recent_records_a_closed_tunnel() {
    use std::io::{Read, Write};

    let origin_port = start_echo_origin();
    let proxy_port = start_test_proxy(park_config());

    // まだ何も閉じていない
    let empty = endpoint_json(proxy_port, "/recent");
    assert!(empty.contains("\"recent\":[]"), "{}", empty);
    assert!(empty.contains("\"recorded\":0"), "{}", empty);
    assert!(empty.contains("\"capacity\":2000"), "{}", empty);
    assert!(empty.contains("\"lite\":false"), "{}", empty);

    let mut tunnel = open_tunnel(proxy_port, origin_port);
    tunnel.write_all(b"hello tunnel").unwrap();
    let mut back = [0u8; 12];
    tunnel.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"hello tunnel");
    // クライアントが先に EOF を出す = `client_eof`
    tunnel.shutdown(std::net::Shutdown::Write).unwrap();
    let mut rest = Vec::new();
    let _ = tunnel.read_to_end(&mut rest);
    drop(tunnel);

    wait_until(
        || endpoint_json(proxy_port, "/recent").contains("\"kind\":\"connect\""),
        "閉じたトンネルが /recent に出る",
    );
    let json = endpoint_json(proxy_port, "/recent");
    assert!(json.contains("\"kind\":\"connect\""), "{}", json);
    assert!(json.contains("\"reason\":\"client_eof\""), "{}", json);
    assert!(json.contains("\"client\":\"127.0.0.1\""), "{}", json);
    assert!(
        json.contains(&format!("\"target\":\"127.0.0.1:{}\"", origin_port)),
        "{}",
        json
    );
    // 運んだバイトは上り / 下り別 (12 B ずつエコーした)
    assert!(json.contains("\"up\":12"), "{}", json);
    assert!(json.contains("\"down\":12"), "{}", json);
    // 段階の ms は `dns` と `connect` が必ず出る (IP リテラルなので名前解決は 0)
    assert!(json.contains("\"ms\":{\"dns\":0,\"connect\":"), "{}", json);
    // 開いた時刻は epoch 秒、寿命は秒
    let at = status_number(&json, "at");
    assert!(at > 1_700_000_000, "開いた時刻が epoch 秒でない: {}", at);
    assert!(json.contains("\"secs\":"), "{}", json);
    // CONNECT なので要求数と状態コードは 0
    assert!(json.contains("\"reqs\":0"), "{}", json);
    assert!(json.contains("\"status\":0"), "{}", json);
    assert!(!json.contains("\"truncated\":true"), "{}", json);
}

/// (b-1) 1 接続あたりの要求数の上限で閉じた http 接続が `limit` として見えること。
///
/// 上限は設定で渡せる (`Config::max_requests_per_conn`。T14.2) ので 3 に下げて見る。
#[test]
fn test_integration_recent_shows_the_request_limit() {
    use std::io::Write;

    let (origin_port, _origin) = common::start_keepalive_origin(
        std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    );
    let mut cfg = proxy_config();
    cfg.max_requests_per_conn = 3;
    let proxy_port = start_test_proxy(cfg);

    let host = format!("127.0.0.1:{}", origin_port);
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    for i in 0..3 {
        let (head, _) = one_keepalive_request(&mut stream, &host, "/x");
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{} 本目: {}", i, head);
    }
    // 3 本目の応答には `Connection: close` が付いている (T14.2) ので、ここで閉じる
    let _ = stream.write_all(b"");
    drop(stream);

    wait_until(
        || endpoint_json(proxy_port, "/recent").contains("\"reason\":\"limit\""),
        "1,000 要求 (ここでは 3) の上限で閉じた接続が /recent に出る",
    );
    let json = endpoint_json(proxy_port, "/recent");
    assert!(json.contains("\"reason\":\"limit\""), "{}", json);
    assert!(json.contains("\"kind\":\"http\""), "{}", json);
    assert!(json.contains("\"reqs\":3"), "{}", json);
    assert!(json.contains("\"status\":200"), "{}", json);
    // 宛先は接続の最初の要求のもの (T14.2 (5))、バイトは上り (要求) / 下り (応答) 別
    assert!(
        json.contains(&format!("\"target\":\"{}\"", host)),
        "{}",
        json
    );
    let entry = one_entry(&json, "\"reason\":\"limit\"");
    let up = status_number(&entry, "up");
    let down = status_number(&entry, "down");
    assert!(up > 0 && down > 0, "up={} down={} in {}", up, down, entry);
}

/// (b-2) アイドルで閉じたトンネルが `idle_timeout` として見えること。
#[test]
#[cfg(target_os = "linux")]
fn test_integration_recent_shows_an_idle_tunnel_timeout() {
    let origin_port = start_quiet_origin();
    let mut cfg = park_config();
    // 預かり所の期限 = 預けた時刻 + このアイドル期限
    cfg.tunnel_idle = std::time::Duration::from_millis(300);
    let proxy_port = start_test_proxy(cfg);

    let tunnel = open_tunnel(proxy_port, origin_port);
    wait_until(
        || endpoint_json(proxy_port, "/recent").contains("\"reason\":\"idle_timeout\""),
        "アイドルのトンネルが期限で閉じて /recent に出る",
    );
    let json = endpoint_json(proxy_port, "/recent");
    assert!(json.contains("\"reason\":\"idle_timeout\""), "{}", json);
    assert!(json.contains("\"kind\":\"connect\""), "{}", json);
    // 預けられていた回数と秒が残ること
    assert!(json.contains("\"parks\":1"), "{}", json);
    drop(tunnel);
}

/// (b-3) T13.2 の追い出しで閉じたトンネルが `evicted` として見えること。
#[test]
#[cfg(target_os = "linux")]
fn test_integration_recent_shows_an_evicted_tunnel() {
    use std::sync::atomic::Ordering;

    let origin_port = start_quiet_origin();
    let mut cfg = park_config();
    cfg.max_conns = 4;
    cfg.keepalive = std::time::Duration::from_secs(60);
    let (proxy_port, metrics) = common::start_test_proxy_with_metrics(cfg);

    // 上限ぴったりまで暇なトンネルを張り、全部が預けられるのを待つ
    let held: Vec<_> = (0..4)
        .map(|_| open_tunnel(proxy_port, origin_port))
        .collect();
    wait_until(
        || metrics.parked_tunnels.load(Ordering::Relaxed) == 4,
        "4 本とも預かり所に入る",
    );
    // 5 本目: 上限に当たるので最古の暇なトンネルが 1 本閉じられる (T13.2)
    let extra = open_tunnel(proxy_port, origin_port);
    wait_until(
        || metrics.evicted_idle.load(Ordering::Relaxed) >= 1,
        "暇なトンネルが 1 本追い出される",
    );
    wait_until(
        || endpoint_json(proxy_port, "/recent").contains("\"reason\":\"evicted\""),
        "追い出されたトンネルが /recent に出る",
    );
    // `/recent` を引きに来る接続自身も上限に当たるので、追い出しは 1 本とは限らない
    // (T13.2 の「上限に当たった accept が暇なトンネルを 1 本閉じる」がそのまま見える)
    let json = endpoint_json(proxy_port, "/recent");
    assert!(json.contains("\"reason\":\"evicted\""), "{}", json);
    let entry = one_entry(&json, "\"reason\":\"evicted\"");
    assert!(entry.contains("\"kind\":\"connect\""), "{}", entry);
    assert!(entry.contains("\"parks\":1"), "{}", entry);
    drop(held);
    drop(extra);
}

/// (c) `?client=` と `?since=` で絞れ、`?sort=slow` が確立の遅い順に並ぶこと。
#[test]
fn test_integration_recent_filters_and_sorts() {
    let (origin_port, _origin) = common::start_mock_origin();
    let proxy_port = start_test_proxy(proxy_config());

    // 3 本の http 接続を開いて閉じる (`Connection: close` なので 1 本 1 要求)
    let host = format!("127.0.0.1:{}", origin_port);
    for _ in 0..3 {
        let r = get_via_proxy(proxy_port, &format!("http://{}/x", host), &host);
        assert!(r.starts_with("HTTP/1.1 200"), "{}", r);
    }
    wait_until(
        || status_number(&endpoint_json(proxy_port, "/recent"), "recorded") >= 3,
        "3 本ぶんの個票が残る",
    );

    let all = endpoint_json(proxy_port, "/recent");
    let ids = recent_ids(&all);
    assert_eq!(
        ids.len(),
        3,
        "自分宛ての `/recent` 自身は残らないはず: {}",
        all
    );
    // 新しい順 (通し番号は増える一方なので降順になる)
    assert!(ids.windows(2).all(|w| w[0] > w[1]), "{:?}", ids);
    assert_eq!(
        all.matches(&format!("\"target\":\"{}\"", host)).count(),
        3,
        "{}",
        all
    );

    // 接続元で絞る: 自分は 127.0.0.1 なので全部残り、別の IP なら 0 件
    let mine = endpoint_json(proxy_port, "/recent?client=127.0.0.1");
    assert_eq!(recent_ids(&mine).len(), 3, "{}", mine);
    let none = endpoint_json(proxy_port, "/recent?client=198.51.100.1");
    assert!(none.contains("\"recent\":[]"), "{}", none);
    assert!(none.contains("\"client\":\"198.51.100.1\""), "{}", none);

    // 時刻で絞る: 未来を渡せば 0 件、過去を渡せば全部
    let future = endpoint_json(proxy_port, "/recent?since=9999999999");
    assert!(future.contains("\"recent\":[]"), "{}", future);
    let past = endpoint_json(proxy_port, "/recent?since=1");
    assert_eq!(recent_ids(&past).len(), 3, "{}", past);

    // `?sort=slow` は確立の遅い順 (`ms.connect` の降順)
    let slow = endpoint_json(proxy_port, "/recent?sort=slow");
    assert!(slow.contains("\"sort\":\"slow\""), "{}", slow);
    let connect_ms: Vec<u64> = slow
        .split("\"connect\":")
        .skip(1)
        .map(|s| {
            s.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap()
        })
        .collect();
    assert!(
        connect_ms.windows(2).all(|w| w[0] >= w[1]),
        "確立の遅い順になっていない: {:?}",
        connect_ms
    );
    // `?n=` で件数を絞る。知らない値は既定に倒す
    assert_eq!(
        recent_ids(&endpoint_json(proxy_port, "/recent?n=1")).len(),
        1
    );
    assert_eq!(
        recent_ids(&endpoint_json(proxy_port, "/recent?n=abc&sort=nope")).len(),
        3
    );
}

// ---------------------------------------------------------------------------
// `/snapshot` — 1 要求で全部取る (T14.4)
// ---------------------------------------------------------------------------

/// JSON として括弧と引用符の釣り合いが取れているか (外部クレートを足さずに形だけ見る)。
///
/// `/snapshot` は部品の文字列を手で並べて組むので、**入れ子の閉じ忘れ**が最も起きやすい。
/// 本当の構文検査は `scripts/collect-deployed.sh` が Python で通すが、回帰はここで止める。
fn json_is_balanced(s: &str) -> bool {
    let mut depth: i64 = 0;
    let (mut in_str, mut escaped) = (false, false);
    for c in s.chars() {
        if in_str {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && !in_str
}

/// (d) `/snapshot` が 1 要求で全部を含み、4 MiB 以下であること。
#[test]
fn test_integration_snapshot_has_every_part_in_one_request() {
    let (origin_port, _origin) = common::start_mock_origin();
    let dead_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let proxy_port = start_test_proxy(proxy_config());

    // 個票に中身があるようにしておく (成功 1 本、失敗 1 本)
    let host = format!("127.0.0.1:{}", origin_port);
    let ok = get_via_proxy(proxy_port, &format!("http://{}/x", host), &host);
    assert!(ok.starts_with("HTTP/1.1 200"), "{}", ok);
    let dead = format!("127.0.0.1:{}", dead_port);
    let bad = raw_get(
        proxy_port,
        &format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n", dead, dead),
    );
    assert!(bad.starts_with("HTTP/1.1 502"), "{}", bad);

    let json = endpoint_json(proxy_port, "/snapshot");
    assert!(json_is_balanced(&json), "JSON の括弧が合わない: {}", json);
    assert!(
        json.len() <= 4 * 1024 * 1024,
        "4 MiB を越えた: {} B",
        json.len()
    );

    // 頭 (いつ・どの版・どれが入っているか)
    assert!(json.contains("\"taken_at\":"), "{}", json);
    assert!(json.contains("\"version\":"), "{}", json);
    assert!(json.contains("\"uptime_secs\":"), "{}", json);
    assert!(json.contains("\"dropped\":[]"), "{}", json);
    // 中身 (17 本の URL を手で叩いていたぶん)
    for key in [
        "\"status\":{",
        "\"status_errors\":{",
        "\"status_dns\":{",
        "\"history\":{\"5\":{",
        "\"60\":{",
        "\"3600\":{",
        "\"dns\":{",
        "\"errors\":{",
        "\"connections\":{",
        "\"recent\":{",
        "\"hosts\":{",
        "\"log\":{",
    ] {
        assert!(
            json.contains(key),
            "{} が無い: {}",
            key,
            &json[..600.min(json.len())]
        );
    }
    // `parts` に名前が並ぶ (この版に何が入っていたかが JSON だけで分かる)
    for name in ["status", "history.5", "recent", "log"] {
        assert!(json.contains(&format!("\"{}\"", name)), "{}", name);
    }
    // 部品の中身が実際に入っていること (集計・履歴・個票の 3 層)
    assert!(json.contains("\"total_requests\":"), "{}", "/status の中身");
    assert!(
        json.contains("\"cause\":\"refused\""),
        "{}",
        "/errors の中身"
    );
    assert!(
        json.contains(&format!("\"target\":\"http://{}\"", host))
            || json.contains(&format!("\"target\":\"{}\"", host)),
        "{}",
        "/recent の中身"
    );
    assert!(json.contains("\"keys\":"), "{}", "/history の中身");

    // `/snapshot` は取るだけで何も保持しない (2 回取っても形は同じ)
    let again = endpoint_json(proxy_port, "/snapshot");
    assert!(json_is_balanced(&again));
    assert!(again.contains("\"dropped\":[]"), "{}", again);
}

// ---------------------------------------------------------------------------
// カーネルの RTT と再送 (`TCP_INFO`。T14.5)
// ---------------------------------------------------------------------------

/// `"key":<数>` に続く小数を取る (`null` なら `None`)。
fn json_f64(json: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{}\":", key);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, json))
        + pat.len();
    let v: String = json[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    v.parse().ok()
}

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
    let client_rtt = json_f64(rtt, "client").expect("クライアント側の RTT が null");
    let origin_rtt = json_f64(rtt, "origin").expect("オリジン側の RTT が null");
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
    let avg = json_f64(host_rtt, "avg").expect("/hosts の avg が null");
    let min = json_f64(host_rtt, "min").expect("/hosts の min が null");
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
    let c_avg = json_f64(c_rtt, "avg").expect("clients[] の avg が null");
    assert!(c_avg < 1.0, "接続元の RTT が 1 ms 未満でない: {}", c_avg);
    assert!(c_rtt.contains("\"retrans\":0"), "{}", status);

    // ---- `/metrics`: 全体の sum / count (ホスト別は出さない) ----
    // `/metrics` は Prometheus 形式だが、本文をそのまま返す口は同じもの。
    // **件数は 1 とは限らない**: `/recent` `/hosts` `/status` を引いた接続自身も
    // 閉じるときにクライアント側の RTT を 1 つ残すため
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
    let client_rtt = json_f64(rtt, "client").expect("クライアント側の RTT が null");
    assert!(client_rtt < 1.0, "{} ms: {}", client_rtt, json);
    // オリジン側はトンネルではないので個票には載らない (プールが捨てるときにホスト別へ)
    assert!(
        rtt.starts_with(&format!("{{\"client\":{},\"origin\":null}}", client_rtt)),
        "http の個票にオリジン側が載っている: {}",
        &rtt[..60.min(rtt.len())]
    );
}
