//! 閉じた接続の個票 `/recent` の結合テスト (T14.4)。
//!
//! 初めは個票のエンドポイント (T13.4) を全部ここで見ていた。`/errors` は `recent_errors_test.rs`、
//! `/connections` は `recent_connections_test.rs`、`/dns` `/log` `/hosts` とカーネルの RTT は
//! `recent_tables_test.rs`、`/snapshot` は `recent_snapshot_test.rs` に割った (T34.117)。
//!
//! 集計 (`/status`) と時系列 (`/history`) では読めない「**誰が・いつ・なぜ**」を
//! 出す口なので、見るのは「実際に起きたことがその形で出てくるか」だけ。

mod common;

use common::*;

// ---------------------------------------------------------------------------
// `/recent` — 閉じた接続の個票 (T14.4)

// ---------------------------------------------------------------------------

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

/// **T29.18**: `/recent?rtt_client=lt100|ge100` で接続元のカーネルの RTT (100 ms) で個票を 2 つに割る。
/// 手元の lo の RTT は数十 us なので、閉じたトンネルは `lt100` にだけ出て `ge100` には出ない。
#[test]
#[cfg(target_os = "linux")]
fn test_integration_recent_splits_by_the_client_rtt() {
    use std::io::{Read, Write};

    let origin_port = start_echo_origin();
    let proxy_port = start_test_proxy(park_config());
    let mut tunnel = open_tunnel(proxy_port, origin_port);
    tunnel.write_all(b"near").unwrap();
    let mut back = [0u8; 4];
    tunnel.read_exact(&mut back).unwrap();
    tunnel.shutdown(std::net::Shutdown::Write).unwrap();
    let mut rest = Vec::new();
    let _ = tunnel.read_to_end(&mut rest);
    drop(tunnel);
    wait_until(
        || endpoint_json(proxy_port, "/recent").contains("\"kind\":\"connect\""),
        "閉じたトンネルが /recent に出る",
    );
    let all = endpoint_json(proxy_port, "/recent");
    assert!(all.contains("\"rtt_client\":\"\"}"), "{}", all);
    let near = endpoint_json(proxy_port, "/recent?rtt_client=lt100");
    assert!(near.contains("\"kind\":\"connect\""), "{}", near);
    assert!(near.contains("\"rtt_client\":\"lt100\"}"), "{}", near);
    let far = endpoint_json(proxy_port, "/recent?rtt_client=ge100");
    assert!(far.contains("\"recent\":[]"), "{}", far);
    assert!(far.contains("\"matched\":0,"), "{}", far);
    assert!(far.contains("\"rtt_client\":\"ge100\"}"), "{}", far);
    // 知らない値は絞らない
    let other = endpoint_json(proxy_port, "/recent?rtt_client=nope");
    assert!(other.contains("\"kind\":\"connect\""), "{}", other);
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
    // 宛先は接続の最初の要求のもの (T14.2)、バイトは上り (要求) / 下り (応答) 別
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

/// `/recent?offset=` と `/hosts?offset=` が**口を通して**効くこと (T15.0)。
///
/// 中の切り方は単体 (`crates/endpoints` の `..._pages_through_...`) で縛ってあるので、
/// ここで見るのは「問い合わせが handler まで届き、`offset` / `next_offset` が応答に出て、
/// 頁を継ぐと落ちも重なりもしない」ことだけ。1 件ずつ頁を切って読む。
#[test]
#[cfg(target_os = "linux")]
fn test_integration_recent_and_hosts_can_be_paged_with_offset() {
    use std::io::{Read, Write};

    let origin_port = start_echo_origin();
    let proxy_port = start_test_proxy(park_config());

    // 何も閉じていないうちは 1 頁目で終わり
    let empty = endpoint_json(proxy_port, "/recent");
    assert!(empty.contains("\"offset\":0"), "{}", empty);
    assert!(empty.contains("\"next_offset\":null"), "{}", empty);

    // トンネルを 3 本、順に開いて閉じる
    for i in 0..3u8 {
        let mut tunnel = open_tunnel(proxy_port, origin_port);
        tunnel.write_all(&[b'a' + i]).unwrap();
        let mut back = [0u8; 1];
        tunnel.read_exact(&mut back).unwrap();
        tunnel.shutdown(std::net::Shutdown::Write).unwrap();
        let mut rest = Vec::new();
        let _ = tunnel.read_to_end(&mut rest);
        drop(tunnel);
        let want = i as u64 + 1;
        wait_until(
            || status_number(&endpoint_json(proxy_port, "/recent"), "matched") >= want,
            "閉じたトンネルが /recent に出る",
        );
    }

    // 1 件ずつ 3 頁。並びは閉じた新しい順なので、`id` は降順で重複しない
    let mut ids = Vec::new();
    for offset in 0..3usize {
        let json = endpoint_json(proxy_port, &format!("/recent?n=1&offset={}", offset));
        assert!(json.contains(&format!("\"offset\":{}", offset)), "{}", json);
        assert!(json.contains("\"matched\":3"), "{}", json);
        let want_next = match offset {
            2 => "\"next_offset\":null".to_string(),
            _ => format!("\"next_offset\":{}", offset + 1),
        };
        assert!(json.contains(&want_next), "offset={}: {}", offset, json);
        let page = recent_ids(&json);
        assert_eq!(page.len(), 1, "offset={}: {}", offset, json);
        ids.push(page[0]);
    }
    let sorted = {
        let mut v = ids.clone();
        v.sort_unstable();
        v.dedup();
        v
    };
    assert_eq!(sorted.len(), 3, "頁が重なっている: {:?}", ids);
    // 並びは新しい順 = `id` の降順 (同じ接続で順に開いたので `id` も昇順に振られる)
    let mut desc = ids.clone();
    desc.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(ids, desc, "新しい順になっていない: {:?}", ids);

    // 並びの外を指したら空の頁
    let past = endpoint_json(proxy_port, "/recent?n=1&offset=3");
    assert!(past.contains("\"recent\":[]"), "{}", past);
    assert!(past.contains("\"next_offset\":null"), "{}", past);

    // `/hosts` も同じ綴り (このプロキシが見たホストは 1 つだけ)
    let hosts = endpoint_json(proxy_port, "/hosts?limit=1&offset=0");
    assert!(hosts.contains("\"offset\":0"), "{}", hosts);
    assert!(hosts.contains("\"next_offset\":null"), "{}", hosts);
    let past_hosts = endpoint_json(proxy_port, "/hosts?limit=1&offset=1");
    assert!(past_hosts.contains("\"hosts\":[]"), "{}", past_hosts);
    assert!(
        past_hosts.contains("\"next_offset\":null"),
        "{}",
        past_hosts
    );
}
