//! 個票のエンドポイント `/connections` の結合テスト (T13.4)。
//!
//! 初めは `tests/recent_test.rs` にあった (T.34.117 で割った)。

mod common;

use common::*;

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

/// keep-alive の HTTP 接続も `/connections` の `bytes` に運んだバイトが出ること (T24.5)。
///
/// `set_bytes` を呼ぶのは**応答を書き終えて次の要求を待つ直前**の 1 か所だけなので、
/// `bytes` は「この接続で終わった要求の和」になる (要求ごと・バイトごとには書かない。
/// T13.4 の方針はそのまま)。3 要求を送って、0 でないこと・要求ごとに増えること・
/// 増えた分がその要求のバイトに見合うことを見る。
#[test]
fn test_integration_connections_shows_the_bytes_of_a_keepalive_http_connection() {
    let (origin_port, _origin) = start_mock_origin();
    let proxy_port = start_test_proxy(park_config());
    let host = format!("127.0.0.1:{}", origin_port);
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();

    // モックの本文は `hello from mock origin` の 22 B
    const BODY: u64 = 22;
    let mut seen = 0u64;
    for (i, path) in ["/one", "/two", "/three"].into_iter().enumerate() {
        let (head, body) = one_keepalive_request(&mut stream, &host, path);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
        assert_eq!(body.len() as u64, BODY, "{}", head);
        // 応答を読み終えた直後は、プロキシがまだ `set_bytes` を書いていないことがある
        // (書くのは応答を書き終えてから)。**時間ではなく値で待つ** (T17.14 の型)
        let want = seen + BODY;
        wait_until(
            || {
                row_number(&endpoint_json(proxy_port, "/connections"), &host, "bytes")
                    .is_some_and(|b| b >= want)
            },
            "keep-alive の接続の bytes が増える",
        );
        let got = row_number(&endpoint_json(proxy_port, "/connections"), &host, "bytes")
            .unwrap_or_else(|| panic!("{} 要求目のあとに bytes が無い", i + 1));
        assert!(
            got > seen,
            "{} 要求目のあとも bytes が {} のまま",
            i + 1,
            seen
        );
        seen = got;
    }
    // 3 要求ぶんの本文は必ず数えている (ヘッダーも足されるので、下限だけ見る)
    assert!(seen >= 3 * BODY, "運んだバイトが少なすぎる: {}", seen);
    // 行は http のままで、宛先も最初の要求のもの (T14.2 (5) の性質は変えない)
    let json = endpoint_json(proxy_port, "/connections");
    assert!(
        json.contains(&format!("\"target\":\"{}\",\"kind\":\"http\"", host)),
        "{}",
        json
    );
}

/// `/connections` の行のうち、宛先が `target` のものから `key` の数値を読む。
///
/// 1 行は `{"id":…,"client":"…","target":"…",…}` の形で、`target` は接続あたり 1 回しか
/// 書かないので、そこから次の `}` までをその行として読めば足りる (入れ子の無い平らな行)。
fn row_number(json: &str, target: &str, key: &str) -> Option<u64> {
    let at = json.find(&format!("\"target\":\"{}\"", target))?;
    let row = &json[at..];
    let row = &row[..row.find('}')?];
    let key = format!("\"{}\":", key);
    let start = row.find(&key)? + key.len();
    row[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// http の宛先も CONNECT と同じ `host:port` の形で出ること (T14.48)。
///
/// ポートを書かない要求 (`http://127.0.0.1/`) の宛先には、ホスト別統計の鍵
/// (`pool_key` = `scheme://host:port`) と同じ規則でスキームの既定 (http なら 80) を補う。
/// 宛先を書くのは**要求を読んだ時点**なので、80 番に相手が居るかどうかは関係しない
/// (この機械では繋がらないので 502 になるが、見るのは個票の宛先だけ)。
#[test]
fn test_integration_an_http_target_without_a_port_gets_the_default_port() {
    use std::io::Write;

    let proxy_port = start_test_proxy(proxy_config());
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(b"GET http://127.0.0.1/t1448 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .unwrap();
    let (head, _) = read_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 "), "{}", head);
    drop(stream);

    wait_until(
        || endpoint_json(proxy_port, "/recent").contains("\"kind\":\"http\""),
        "閉じた http 接続が /recent に出る",
    );
    let json = endpoint_json(proxy_port, "/recent");
    assert!(
        json.contains("\"target\":\"127.0.0.1:80\",\"kind\":\"http\""),
        "http の宛先にポートが付いていない: {}",
        json
    );
    // `/explain?host=` はポート付きの宛先でも、ポートを書かずに引ける (T14.36)
    let explain = endpoint_json(proxy_port, "/explain?host=127.0.0.1");
    assert!(explain.contains("\"known\":true"), "{}", explain);
    assert!(
        explain.contains("\"target\":\"127.0.0.1:80\""),
        "個票がホスト名で引けていない: {}",
        explain
    );
    // ポートを書いても同じ 1 件が引ける
    let with_port = endpoint_json(proxy_port, "/explain?host=127.0.0.1:80");
    assert!(
        with_port.contains("\"target\":\"127.0.0.1:80\""),
        "ポート付きで引けていない: {}",
        with_port
    );
}
