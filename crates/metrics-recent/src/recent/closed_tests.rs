//! 閉じた接続の分布の単体試験 (T14.6。T.34.2 で `recent/mod.rs` の `burst_tests` から移した)。

use super::*;
use crate::metrics::ErrCause;
use crate::recent::{CloseReason, SIDES, STAGES};

fn closed(reason: CloseReason, secs: u64, up: u64, down: u64) -> RecentEntry {
    RecentEntry {
        id: 1,
        at: 1_700_000_000,
        client: "198.51.100.7".to_string(),
        target: "example.net:443".to_string(),
        connect: true,
        secs,
        requests: 0,
        up,
        down,
        reason,
        status: 0,
        parked_secs: 3,
        parks: 2,
        stage_ms: [0; STAGES],
        stall_ms: [0; SIDES],
        rtt_us: [0; SIDES],
        retrans: [0; SIDES],
        sni: None,
        syn_retrans: 0,
        spins: 0,
        half_closed: None,
    }
}

/// 閉じた理由・寿命・バイト・預けの分布が 1 本ずつ積み上がること。
#[test]
fn the_distribution_counts_reasons_lifetimes_and_bytes() {
    let mut c = ClosedCounts::default();
    assert!(c.is_empty());
    // 寿命 12 秒 (10 < 12 <= 15 = `PROXY_KEEPALIVE_SECS` の段)、上り 2 KiB、下り 3 MiB
    c.observe(&closed(CloseReason::KeepaliveTimeout, 12, 2048, 3 << 20));
    c.observe(&closed(CloseReason::Error(ErrCause::Reset), 0, 0, 0));
    c.observe(&closed(
        CloseReason::Error(ErrCause::Dns),
        100_000,
        u64::MAX,
        1,
    ));
    assert_eq!(c.closed, 3);
    assert_eq!(c.reasons[CloseReason::KeepaliveTimeout.index()], 1);
    assert_eq!(c.reasons[7], 2, "`error:*` は 1 つにまとめる");
    assert_eq!(c.life[4], 1, "12 秒は 15 秒の段");
    assert_eq!(c.life[0], 1, "0 秒は 1 秒の段");
    assert_eq!(c.life[LIFE_BUCKETS - 1], 1, "100,000 秒は上限なしの段");
    assert_eq!(c.up[1], 1, "2 KiB は 4 KiB の段");
    assert_eq!(c.up[0], 1, "0 B は 1 KiB の段");
    assert_eq!(c.up[BYTE_BUCKETS - 1], 1, "20 桁は上限なしの段");
    assert_eq!(c.down[6], 1, "3 MiB は 4 MiB の段");
    assert_eq!(c.life_secs_sum, 12 + 100_000);
    assert_eq!(c.parked_secs_sum, 9);
    assert_eq!(c.parks, 6);
    assert_eq!(c.down_bytes, (3 << 20) + 1);
    // 畳むと足し合わせ
    let mut agg = ClosedCounts::default();
    agg.merge(&c);
    agg.merge(&c);
    assert_eq!(agg.closed, 6);
    assert_eq!(agg.reasons[7], 4);
    assert_eq!(agg.life[4], 2);
    assert_eq!(agg.parks, 12);

    let mut out = String::new();
    c.push_row(&mut out, 1_700_000_005);
    assert!(
        out.starts_with("[1700000005,3,[0,0,0,1,0,0,0,2],"),
        "{}",
        out
    );
    assert!(
        out.ends_with(",100012,9,6,18446744073709551615,3145729]"),
        "{}",
        out
    );
    // 列の数が `CLOSED_KEYS` と合っていること (入れ子の配列は 1 列)
    let mut depth = 0;
    let cols = 1 + out
        .chars()
        .filter(|ch| {
            match ch {
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            }
            *ch == ',' && depth == 1
        })
        .count();
    assert_eq!(cols, CLOSED_KEYS.len(), "{}", out);
}

/// 8 つの理由が分布の 8 列に 1 対 1 で並ぶこと (名前も同じ綴り)。
#[test]
fn every_reason_has_its_own_column() {
    let all = [
        CloseReason::ClientEof,
        CloseReason::ServerEof,
        CloseReason::IdleTimeout,
        CloseReason::KeepaliveTimeout,
        CloseReason::Evicted,
        CloseReason::Limit,
        CloseReason::Shutdown,
        CloseReason::Error(ErrCause::Refused),
    ];
    for (i, reason) in all.into_iter().enumerate() {
        assert_eq!(reason.index(), i, "{}", reason.text());
        // `error:*` 以外は `/recent` の綴りと同じ名前
        if i < 7 {
            assert_eq!(CLOSE_REASON_NAMES[i], reason.text());
        }
    }
    assert_eq!(CLOSE_REASON_NAMES[7], "error");
    // 区間は 12 段ずつで、単調に増える
    assert_eq!(LIFE_BOUNDS_SECS.len(), 12);
    assert_eq!(BYTE_BOUNDS.len(), 12);
    assert!(LIFE_BOUNDS_SECS.windows(2).all(|w| w[0] < w[1]));
    assert!(BYTE_BOUNDS.windows(2).all(|w| w[0] < w[1]));
    // 設定の値そのものが境目にある (T14.6 の目的)
    assert!(LIFE_BOUNDS_SECS.contains(&15), "PROXY_KEEPALIVE_SECS");
    assert!(LIFE_BOUNDS_SECS.contains(&300), "PROXY_TUNNEL_IDLE_SECS");
    assert_eq!(BYTE_BOUNDS[0], 1024);
    assert_eq!(BYTE_BOUNDS[10], 1 << 30);
}

/// 区間は「`v <= 境目` の最初の段」、どれにも収まらなければ最後の「上限なし」(T.34.2 で直に押さえる)。
#[test]
fn bucket_of_puts_a_value_on_its_first_fitting_bound() {
    assert_eq!(bucket_of(&LIFE_BOUNDS_SECS, 0), 0);
    assert_eq!(bucket_of(&LIFE_BOUNDS_SECS, 1), 0, "境目そのものは下の段");
    assert_eq!(bucket_of(&LIFE_BOUNDS_SECS, 2), 1);
    assert_eq!(
        bucket_of(&LIFE_BOUNDS_SECS, 15),
        4,
        "PROXY_KEEPALIVE_SECS の段"
    );
    assert_eq!(bucket_of(&LIFE_BOUNDS_SECS, 16), 5);
    assert_eq!(bucket_of(&LIFE_BOUNDS_SECS, 21_600), 11);
    assert_eq!(
        bucket_of(&LIFE_BOUNDS_SECS, 21_601),
        LIFE_BUCKETS - 1,
        "上限なし"
    );
    assert_eq!(bucket_of(&BYTE_BOUNDS, 1024), 0);
    assert_eq!(bucket_of(&BYTE_BOUNDS, 1025), 1);
    assert_eq!(bucket_of(&BYTE_BOUNDS, u64::MAX), BYTE_BUCKETS - 1);
    assert_eq!(bucket_of(&[], 7), 0, "境目が無ければ 1 段だけ");
}
