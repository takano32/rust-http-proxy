//! `?offset=` の頁送り (`next_offset`。T34.86 で tests.rs から分けた)。

use super::*;

/// `next_offset` は「続きがあれば次の `offset`、無ければ `null`」(T15.0 (11))。
#[test]
fn the_next_offset_is_null_at_the_end() {
    assert_eq!(next_offset(0, 500, 2000), "500");
    assert_eq!(next_offset(1500, 500, 2000), "null");
    assert_eq!(next_offset(0, 0, 0), "null");
    // バイト数で切れた頁でも続きの位置は出す (切れた所から次を引ける)
    assert_eq!(next_offset(0, 615, 2000), "615");
    // 並びの外を指した頁は空で終わり
    assert_eq!(next_offset(2000, 0, 2000), "null");
}

/// `/recent?offset=` の 4 頁で **2,000 本が重複なく揃う** (T15.0 (11))。
///
/// 1 枚では [`MAX_BODY`] に 615 本しか入らない (1 件 424 B × 2,000 = 828 KiB) ので、
/// 雪像を 1 枚で読むには頁が要る。ここで縛るのは「4 頁の `id` の集合が
/// 0..2,000 と完全に一致する」= **落ちない・重ならない**こと。
#[test]
fn the_recent_endpoint_pages_through_the_whole_ring_with_offset() {
    use crate::recent::{CloseReason, ConnTally, MAX_RECENT};

    let m = Metrics::new();
    let now = Instant::now();
    for i in 0..(MAX_RECENT as u64) {
        let slot = m
            .conns
            .register(i, "198.51.100.7", now)
            .expect("登録できる");
        slot.begin_tunnel("host.example.net:443");
        slot.finish(CloseReason::ClientEof, ConnTally::default(), 1);
        m.record_closed(i);
    }
    assert_eq!(m.closed.len(), MAX_RECENT);

    with_ep(&m, |ep| {
        let page = 500;
        let mut seen: Vec<u64> = Vec::with_capacity(MAX_RECENT);
        let mut offset = 0usize;
        for p in 0..4 {
            let body = recent(ep, Some(&format!("n={}&offset={}", page, offset))).2;
            assert!(body.len() <= MAX_BODY, "{} 頁目で {} B", p + 1, body.len());
            assert!(
                body.contains("\"truncated\":false"),
                "{} 頁目がバイト数で切れた ({} B)",
                p + 1,
                body.len()
            );
            assert!(
                body.contains(&format!("\"offset\":{}", offset)),
                "{}",
                p + 1
            );
            let ids: Vec<u64> = body
                .match_indices("\"id\":")
                .map(|(at, pat)| {
                    body[at + pat.len()..]
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<String>()
                        .parse()
                        .expect("id は数")
                })
                .collect();
            assert_eq!(ids.len(), page, "{} 頁目の件数", p + 1);
            seen.extend(ids);
            // 最後の頁だけ `next_offset` が `null`
            let want_next = if p == 3 {
                "\"next_offset\":null".to_string()
            } else {
                format!("\"next_offset\":{}", offset + page)
            };
            assert!(body.contains(&want_next), "{} 頁目: {}", p + 1, want_next);
            offset += page;
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), MAX_RECENT, "重複か欠けがある");
        assert_eq!(seen[0], 0);
        assert_eq!(seen[MAX_RECENT - 1], MAX_RECENT as u64 - 1);

        // 並びの外を指したら空の頁 (エラーにはしない)
        let past = recent(ep, Some("n=200&offset=2000")).2;
        assert!(past.contains("\"recent\":[]"), "{}", past);
        assert!(past.contains("\"next_offset\":null"), "{}", past);
        assert!(past.contains("\"matched\":2000"), "{}", past);
    });
}

/// `/hosts?offset=` の 2 頁で **1,000 ホストが重複なく揃う** (T15.0 (11))。
///
/// 1 枚では 639 件で切れていた (1,000 件 × 408 B = 398 KiB)。2 枚の雪像の差分から
/// 8 ホストが黙って落ちたのがこの欄を足した理由なので、**落ちないこと**を縛る。
#[test]
fn the_hosts_endpoint_pages_through_every_host_with_offset() {
    use crate::metrics::{HostOutcome, MAX_HOSTS};
    use std::time::Duration;

    let m = Metrics::new();
    for i in 0..MAX_HOSTS {
        m.record_host_timed(
            &format!("connect://host{:04}.example.net:443", i),
            HostOutcome::Bypass,
            1_234,
            Duration::from_millis(12),
        );
    }
    with_ep(&m, |ep| {
        let page = 500;
        let mut seen: Vec<String> = Vec::with_capacity(MAX_HOSTS);
        for (p, offset) in [0usize, page].into_iter().enumerate() {
            let body = hosts(
                ep,
                Some(&format!("sort=requests&limit={}&offset={}", page, offset)),
            )
            .2;
            assert!(body.len() <= MAX_BODY, "{} 頁目で {} B", p + 1, body.len());
            assert!(
                body.contains("\"truncated\":false"),
                "{} 頁目がバイト数で切れた ({} B)",
                p + 1,
                body.len()
            );
            assert!(body.contains("\"count\":1000"), "{}", body.len());
            assert!(
                body.contains(&format!("\"offset\":{}", offset)),
                "{}",
                p + 1
            );
            for (at, pat) in body.match_indices("{\"host\":\"") {
                let rest = &body[at + pat.len()..];
                let end = rest.find('"').expect("閉じ引用符");
                seen.push(rest[..end].to_string());
            }
            let want_next = if p == 0 {
                format!("\"next_offset\":{}", page)
            } else {
                "\"next_offset\":null".to_string()
            };
            assert!(body.contains(&want_next), "{} 頁目: {}", p + 1, want_next);
        }
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), MAX_HOSTS, "重複か欠けがある");
    });
}
