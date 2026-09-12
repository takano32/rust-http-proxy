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
