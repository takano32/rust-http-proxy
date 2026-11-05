//! 個票の絞りと並べ替え (`?n=` `?since=` `?client=` `?sort=`。T34.86 で tests.rs から分けた)。

use super::*;

/// 原因の分からないエラーはリングに書かない。
#[test]
fn errors_without_a_cause_are_not_recorded() {
    let m = Metrics::new();
    m.record_error(
        false,
        "example.com:80",
        "127.0.0.1",
        502,
        &Detail::default(),
    );
    assert!(m.errors.is_empty());
}

/// `/events` の形と、`?n=` `?since=` の絞り (T14.11)。
///
/// リングは静的に 1 本なので、ここでは 1 つのテストにまとめて順に見る。
#[test]
fn the_events_endpoint_filters_by_n_and_since() {
    use crate::events::{EventKind, MAX_EVENTS};
    let m = crate::metrics::Metrics::new();
    with_ep(&m, |ep| {
        crate::events::clear();
        crate::events::push(EventKind::Start, "version 0.0.0 on port 8080");
        crate::events::push(EventKind::Reload, "PROXY_TIMEOUT_SECS 30 \u{2192} 10");
        let body = events(ep, None).2;
        assert!(body.starts_with("{\"schema\":1,\"events\":["), "{}", body);
        assert!(body.contains("\"kind\":\"reload\""), "{}", body);
        assert!(
            body.contains("PROXY_TIMEOUT_SECS 30 \u{2192} 10"),
            "前後の値が出る: {}",
            body
        );
        assert!(body.contains("\"count\":2"), "{}", body);
        assert!(body.contains("\"recorded\":2"), "{}", body);
        assert!(body.contains("\"capacity\":512"), "{}", body);
        assert!(body.contains("\"truncated\":false"), "{}", body);
        // 12 種の名前が全部出る (README の一覧と合っているか)
        for kind in [
            "start",
            "reload",
            "blocklist",
            "ipv6",
            "pressure",
            "ballast",
            "state_file",
            "evict",
            "emfile",
            "shutdown",
            "anomaly",
            "new_client",
        ] {
            assert!(body.contains(&format!("\"{}\"", kind)), "{} が無い", kind);
        }
        // 新しい順
        let first = body.find("reload").unwrap();
        let second = body.find("\"kind\":\"start\"").unwrap();
        assert!(first < second, "新しい順でない: {}", body);
        // `?n=1` で 1 件
        let one = events(ep, Some("n=1")).2;
        assert!(one.contains("\"count\":1"), "{}", one);
        assert!(!one.contains("\"kind\":\"start\""), "{}", one);
        // `?since=` は「その時刻以降」。先の時刻なら 0 件
        let none = events(ep, Some("since=9999999999")).2;
        assert!(none.starts_with("{\"schema\":1,\"events\":[]"), "{}", none);
        assert!(none.contains("\"since\":9999999999"), "{}", none);
        assert!(none.contains("\"recorded\":2"), "通算は残る: {}", none);
        // 満杯 (512 件) の最悪でも 256 KiB に収まる
        crate::events::clear();
        for i in 0..MAX_EVENTS + 10 {
            crate::events::push(
                EventKind::Blocklist,
                &format!("{}{}", "\u{2192}".repeat(50), i),
            );
        }
        let body = events(ep, Some("n=512")).2;
        assert!(body.contains("\"count\":512"), "{}", body);
        assert!(body.contains("\"truncated\":false"), "{}", body);
        assert!(body.len() <= MAX_BODY, "{} B", body.len());
        println!(
            "events 512 件の応答: {} B (上限 {} B)",
            body.len(),
            MAX_BODY
        );
        crate::events::clear();
    });
}

/// `?sort=` の並びと `?since=` / `?client=` の絞り (T14.4)。
#[test]
fn recent_sorts_and_filters() {
    let m = Metrics::new();
    let now = Instant::now();
    for (i, (client, connect_ms, bytes)) in [
        ("10.0.0.1", 5u64, 100u64),
        ("10.0.0.2", 90, 10),
        ("10.0.0.1", 1, 5000),
    ]
    .into_iter()
    .enumerate()
    {
        let slot = m.conns.register(i as u64, client, now).expect("登録できる");
        slot.begin_tunnel("example.net:443");
        slot.finish(
            crate::recent::CloseReason::ClientEof,
            crate::recent::ConnTally {
                up: 0,
                down: bytes,
                status: 0,
                stage_ms: [0, connect_ms, 0, 0, 0, 0],
                ..Default::default()
            },
            0,
        );
        m.record_closed(i as u64);
    }
    with_ep(&m, |ep| {
        // 既定は閉じた新しい順
        let body = recent(ep, None).2;
        assert!(body.contains("\"count\":3"), "{}", body);
        assert!(body.contains("\"sort\":\"time\""), "{}", body);
        let ids = |b: &str| {
            b.match_indices("\"id\":")
                .map(|(i, _)| b[i + 5..].split(',').next().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&body), ["2", "1", "0"]);
        // 確立の遅い順 / 転送の多い順
        assert_eq!(ids(&recent(ep, Some("sort=slow")).2), ["1", "0", "2"]);
        assert_eq!(ids(&recent(ep, Some("sort=bytes")).2), ["2", "0", "1"]);
        // 接続元で絞る
        let mine = recent(ep, Some("client=10.0.0.1")).2;
        assert_eq!(ids(&mine), ["2", "0"]);
        assert!(mine.contains("\"client\":\"10.0.0.1\""), "{}", mine);
        // 未来の時刻で絞れば 0 件 (`recorded` は減らない)
        let none = recent(ep, Some("since=9999999999")).2;
        assert!(none.contains("\"recent\":[]"), "{}", none);
        assert!(none.contains("\"recorded\":3"), "{}", none);
        // `?n=` で件数を絞る
        assert_eq!(ids(&recent(ep, Some("n=1")).2).len(), 1);
    });
}
