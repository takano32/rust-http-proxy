use super::open::{Reported, detail_of};
use super::*;

#[test]
fn reported_keeps_the_kind_and_the_message() {
    // 呼び元は種類で EOF 系を見分け、文面は debug に出す (T.28.4)。包んでも変わらないこと
    let inner = io::Error::new(io::ErrorKind::ConnectionRefused, "refused");
    let e = io::Error::new(inner.kind(), Reported(inner));
    assert!(is_reported(&e));
    assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused);
    assert_eq!(e.to_string(), "refused");
    assert!(!is_reported(&io::Error::other("plain")));
    assert!(!is_reported(&io::Error::from(io::ErrorKind::TimedOut)));
}

/// 時計より前に払った名前解決 (入口の ACL) を作る。
fn acl_paid(ms: u64) -> u64 {
    let _ = crate::dns::take_resolve_cost();
    crate::dns::note_resolve_cost(ms * 1_000, 1);
    crate::dns::peek_resolve_cost().0
}

/// 窓の**外**で払った名前解決を引くと接続の段が 0 に潰れる (T15.0 (1))。
///
/// `PROXY_ALLOW_LOCAL=false` (既定) では入口の ACL が `open()` の時計より前に
/// 名前を引く。デプロイ先の実測では、ミスした個票 54 本のうち 40 本 (74%) が
/// `connect` 0 ms で、その 40 本のカーネル RTT の中央値は 11.4 ms だった
/// (TCP の握りが 0 ms のはずがない)。
#[test]
fn a_resolve_paid_before_the_clock_does_not_eat_the_connect_stage() {
    let dns_before = acl_paid(12);
    assert_eq!(dns_before, 12);
    let stages = StageMs {
        queue: 2,
        client_read: 15,
        ..StageMs::default()
    };
    let d = detail_of(Duration::from_millis(9), None, stages, dns_before);
    assert_eq!(
        (d.dns_ms, d.dns_misses),
        (12, 1),
        "払った費用はそのまま残す"
    );
    // 直す前は 9 − 12 → 0 (`saturating_sub`)
    assert_eq!(d.connect_ms, 9, "窓の外の名前解決は引かない");
    assert_eq!(d.stages.client_read, 3, "client_read からは除く (15 − 12)");
    assert_eq!(d.stages.queue, 2, "ほかの段は触らない");
}

/// 窓の**内側**で引いたぶんは今までどおり引く (T12.4 (2) の引き算は変えない)。
#[test]
fn a_resolve_paid_inside_the_window_is_still_subtracted() {
    let _ = crate::dns::take_resolve_cost();
    crate::dns::note_resolve_cost(6_000, 1);
    let stages = StageMs {
        client_read: 1,
        ..StageMs::default()
    };
    let d = detail_of(Duration::from_millis(20), None, stages, 0);
    assert_eq!((d.dns_ms, d.connect_ms), (6, 14), "20 − 6");
    assert_eq!(d.stages.client_read, 1, "引く相手がいないので変わらない");
}

/// 両方あるとき: 引くのは**窓の内側のぶんだけ**。
#[test]
fn only_the_part_paid_inside_the_window_is_subtracted() {
    let dns_before = acl_paid(10);
    // `open()` の中でさらに 4 ms 引いた (`connect_with_timeout` の中のミス)
    crate::dns::note_resolve_cost(4_000, 1);
    let stages = StageMs {
        client_read: 12,
        ..StageMs::default()
    };
    let d = detail_of(Duration::from_millis(30), None, stages, dns_before);
    assert_eq!((d.dns_ms, d.dns_misses), (14, 2), "費用の合計は 2 回ぶん");
    assert_eq!(d.connect_ms, 26, "30 − 4 (窓の内側の 4 ms だけ引く)");
    assert_eq!(d.stages.client_read, 2, "12 − 10");
}

/// 名前解決を 1 度もしていないとき (当たりの経路) は今までと同じ値。
#[test]
fn a_cached_name_changes_nothing() {
    let _ = crate::dns::take_resolve_cost();
    let stages = StageMs {
        queue: 1,
        client_read: 4,
        ..StageMs::default()
    };
    let d = detail_of(Duration::from_millis(8), None, stages, 0);
    assert_eq!((d.dns_ms, d.dns_misses, d.connect_ms), (0, 0, 8));
    assert_eq!((d.stages.queue, d.stages.client_read), (1, 4));
}
