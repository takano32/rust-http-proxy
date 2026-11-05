//! `Entry::look` の単体試験 (T.34.55)。表 (`TABLE`) にも時計にも触らないので、鍵は握らない。

use super::*;
use std::cell::Cell;
use std::net::Ipv4Addr;

const TTL: Duration = Duration::from_secs(60);
const WINDOW: Duration = Duration::from_secs(3600);
const NEG: Duration = Duration::from_secs(60);
const SERVE: Duration = Duration::from_secs(21_600);

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// `at` に答え (127.0.0.1) を得た行。
fn answered(at: Instant) -> Entry {
    let mut e = Entry::empty(at, None);
    e.addrs = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    e
}

fn look(e: &mut Entry, now: Instant) -> Lookup {
    e.look(now, TTL, WINDOW, || NEG, || SERVE)
}

fn is_hit(l: &Lookup) -> bool {
    matches!(l.found, Some(Looked::Hit(_)))
}

#[test]
fn a_hot_name_past_three_quarters_of_the_ttl_asks_for_one_refresh() {
    let t0 = Instant::now();
    let mut e = answered(t0);
    // 3/4 TTL (45 秒) の手前: 当たりで、頼まない
    let l = look(&mut e, t0 + secs(44));
    assert!(is_hit(&l) && !l.ask && !e.refreshing);
    // 3/4 を過ぎた (1 つ前の使用は 2 秒前 = 熱い): 当たりのまま裏に 1 回頼む
    let l = look(&mut e, t0 + secs(46));
    assert!(is_hit(&l) && l.ask && e.refreshing);
    // 旗が立っている間は二重に頼まない
    let l = look(&mut e, t0 + secs(47));
    assert!(is_hit(&l) && !l.ask);
}

#[test]
fn a_name_refreshed_in_the_background_but_unused_for_a_ttl_does_not_ask() {
    let t0 = Instant::now();
    // 裏の引き直しは `resolved_at` だけを進める。1 つ前の使用が TTL より前なら熱くない
    let mut e = answered(t0);
    e.last_used = t0 - secs(100);
    let l = look(&mut e, t0 + secs(50));
    assert!(is_hit(&l) && !l.ask && !e.refreshing);
    // その直後の 2 回目は熱い (1 つ前の使用が 1 秒前) ので頼む
    let l = look(&mut e, t0 + secs(51));
    assert!(is_hit(&l) && l.ask);
}

#[test]
fn a_remembered_failure_prefers_an_old_answer_as_stale() {
    let t0 = Instant::now();
    let mut e = answered(t0);
    let now = t0 + secs(120);
    e.failed_at = Some((now - secs(10), io::ErrorKind::NotFound, "nope".into()));
    let l = look(&mut e, now);
    assert!(
        matches!(l.found, Some(Looked::Stale(_))),
        "古い答えがあれば繋ぐ方"
    );
    assert!(!l.ask);
    // 答えが無ければ覚えている失敗をそのまま返す
    let mut e = Entry::empty(t0, None);
    e.failed_at = Some((now - secs(10), io::ErrorKind::NotFound, "nope".into()));
    match look(&mut e, now).found {
        Some(Looked::Negative(err)) => {
            assert_eq!(err.kind(), io::ErrorKind::NotFound);
            assert_eq!(err.to_string(), "nope");
        }
        _ => panic!("負のキャッシュの当たり"),
    }
}

#[test]
fn an_expired_answer_inside_the_serve_stale_window_is_served_and_asks_once() {
    let t0 = Instant::now();
    let mut e = answered(t0);
    let l = look(&mut e, t0 + secs(600));
    assert!(matches!(l.found, Some(Looked::Served(_))));
    assert!(l.ask && e.refreshing);
    let l = look(&mut e, t0 + secs(601));
    assert!(
        matches!(l.found, Some(Looked::Served(_))) && !l.ask,
        "二重に頼まない"
    );
    // 窓を 0 にすれば同期で引く (= ミス)
    let mut e = answered(t0);
    let l = e.look(t0 + secs(600), TTL, WINDOW, || NEG, || Duration::ZERO);
    assert!(l.found.is_none() && l.miss_kind == MissKind::Expired);
}

#[test]
fn the_miss_kind_is_warm_stale_then_negative_then_expired_then_cold() {
    let t0 = Instant::now();
    let now = t0 + secs(30_000);
    // 負のキャッシュの期限を過ぎた失敗 (= 引き直す) を載せた行で比べる
    let failed = |e: &mut Entry| {
        e.failed_at = Some((now - secs(120), io::ErrorKind::NotFound, "x".into()));
    };
    let mut e = answered(t0);
    e.warm = true;
    failed(&mut e);
    assert_eq!(look(&mut e, now).miss_kind, MissKind::WarmStale);
    let mut e = answered(t0);
    failed(&mut e);
    assert_eq!(look(&mut e, now).miss_kind, MissKind::Negative);
    let mut e = answered(t0);
    assert_eq!(look(&mut e, now).miss_kind, MissKind::Expired);
    let mut e = Entry::empty(t0, Some(true));
    let l = look(&mut e, now);
    assert_eq!(l.miss_kind, MissKind::Cold);
    assert_eq!(l.pref, Some(true), "族の記憶は答えが無くても返す");
    // 当たりなら種類は決めない (既定の cold のまま)
    let mut e = answered(t0);
    e.warm = true;
    let l = look(&mut e, t0 + secs(1));
    assert!(is_hit(&l) && l.miss_kind == MissKind::Cold);
}

#[test]
fn the_second_use_inside_the_window_promotes_an_answered_name() {
    let t0 = Instant::now();
    let mut e = answered(t0);
    let l = look(&mut e, t0 + secs(100));
    assert!(l.promote, "窓の中で 2 回目");
    assert_eq!(e.prev_used, t0);
    assert_eq!(e.last_used, t0 + secs(100));
    // 既に warm・窓 0・窓の外・答えなし は上げない
    let mut e = answered(t0);
    e.warm = true;
    assert!(!look(&mut e, t0 + secs(100)).promote);
    let mut e = answered(t0);
    assert!(
        !e.look(t0 + secs(1), TTL, Duration::ZERO, || NEG, || SERVE)
            .promote
    );
    let mut e = answered(t0);
    assert!(!look(&mut e, t0 + WINDOW + secs(1)).promote);
    let mut e = Entry::empty(t0, None);
    assert!(!look(&mut e, t0 + secs(1)).promote);
}

#[test]
fn warm_requests_count_only_uses_made_while_warm() {
    let t0 = Instant::now();
    let mut e = answered(t0);
    look(&mut e, t0 + secs(1));
    assert_eq!(e.warm_requests, 0);
    e.warm = true;
    look(&mut e, t0 + secs(2));
    look(&mut e, t0 + secs(3));
    assert_eq!(e.warm_requests, 2);
}

#[test]
fn a_hit_reads_neither_the_negative_ttl_nor_the_serve_stale_window() {
    let t0 = Instant::now();
    let (neg, serve) = (Cell::new(0), Cell::new(0));
    let mut e = answered(t0);
    let read = |c: &Cell<u32>, d: Duration| {
        c.set(c.get() + 1);
        d
    };
    let l = e.look(
        t0 + secs(1),
        TTL,
        WINDOW,
        || read(&neg, NEG),
        || read(&serve, SERVE),
    );
    assert!(is_hit(&l));
    assert_eq!((neg.get(), serve.get()), (0, 0), "当たりの経路では読まない");
    let l = e.look(
        t0 + secs(600),
        TTL,
        WINDOW,
        || read(&neg, NEG),
        || read(&serve, SERVE),
    );
    assert!(matches!(l.found, Some(Looked::Served(_))));
    assert_eq!((neg.get(), serve.get()), (1, 1));
}
