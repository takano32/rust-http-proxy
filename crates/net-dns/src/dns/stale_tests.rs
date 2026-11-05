//! serve-stale (T21.3) の単体テスト。`dns.rs` の `tests` と同じく `RESOLVE_TEST_LOCK` を握って直列に回す
//! (表もカウンタもプロセスで 1 組)。

use super::*;

fn n(c: &AtomicU64) -> u64 {
    c.load(Ordering::Relaxed)
}

/// 各テストの始まりの状態 (TTL 60 秒・keep-warm 止め・serve-stale は `window`)。
/// keep-warm を止めるのは、2 回目の使用で warm に上がって裏の引き直しの数が動かないようにするため。
fn reset(window: Duration) {
    set_ttl(Duration::from_secs(60));
    set_warm_window(Duration::ZERO);
    set_serve_stale(window);
    clear();
}

/// 終わったら既定に戻す (ほかのテストが既定を前提にしていても困らないように)。
/// keep-warm の既定は T22.4 から止めてある ([`WARM_DEFAULT`])。
fn restore() {
    set_warm_window(WARM_DEFAULT);
    set_serve_stale(SERVE_STALE);
    clear();
}

fn wait_refreshes(want: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while n(&REFRESHES) < want {
        assert!(Instant::now() < deadline, "refreshes = {}", n(&REFRESHES));
        thread::sleep(Duration::from_millis(10));
    }
}

fn entry_addrs(host: &str) -> Vec<IpAddr> {
    TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(host))
        .map(|e| e.addrs.clone())
        .unwrap_or_default()
}

const OLD: [u8; 4] = [192, 0, 2, 1];

/// TTL を過ぎて齢が窓の中の答えは、同期で引かずにそのまま返る (ミスではない)。
/// 同じ名前に 10 本来ても、裏の引き直しは 1 回だけ頼まれる。
#[test]
fn an_expired_answer_inside_the_window_is_served_and_refreshed_once() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset(SERVE_STALE);
    let host = "localhost";
    let old = IpAddr::from(OLD);
    put_answer_for_test(host, &[old], Duration::from_secs(120));
    let (m0, e0, s0, r0) = (n(&MISSES), n(&MISS_EXPIRED), n(&STALE_HITS), n(&REFRESHES));
    let cost0 = peek_resolve_cost();

    let (addrs, _, stale) = resolve_host_marked(host, 80).unwrap();
    assert_eq!(addrs, vec![old], "古い答えをそのまま返す");
    assert!(stale, "古い答えの印が立つ");
    assert_eq!(n(&MISSES), m0, "ミスではない");
    assert_eq!(n(&MISS_EXPIRED), e0, "expired にも数えない");
    assert_eq!(n(&STALE_HITS), s0 + 1);
    assert_eq!(
        peek_resolve_cost(),
        cost0,
        "利用者は getaddrinfo を待っていない"
    );

    // 引き直しが済む前後に 10 本来ても、頼むのは 1 回だけ
    let hands: Vec<_> = (0..10)
        .map(|_| thread::spawn(|| resolve_host_marked("localhost", 80).unwrap()))
        .collect();
    for h in hands {
        h.join().unwrap();
    }
    assert_eq!(n(&MISSES), m0, "10 本とも同期では引かない");
    wait_refreshes(r0 + 1);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(n(&REFRESHES), r0 + 1, "裏の引き直しは 1 回だけ");
    assert!(!entry_addrs(host).contains(&old), "裏で新しい答えに替わる");

    // 引き直したあとは普通の当たり (印は立たない)
    let (_, _, stale) = resolve_host_marked(host, 80).unwrap();
    assert!(!stale);
    let status = status_json();
    assert!(
        // T.32.11 で後ろに欄が足されたので、末尾ではなく並びで見る
        status.contains(&format!(
            ",\"stale_hits\":{},\"stale_retries\":{},",
            n(&STALE_HITS),
            n(&STALE_RETRIES)
        )),
        "`/status` の `dns` に出る: {}",
        status
    );
    restore();
}

/// 齢が窓の外なら今までどおり同期で引く (`expired` のミス、印は立たない)。
#[test]
fn an_answer_older_than_the_window_is_resolved_synchronously() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset(Duration::from_secs(3600));
    let host = "localhost";
    put_answer_for_test(host, &[IpAddr::from(OLD)], Duration::from_secs(7200));
    let (m0, e0, s0) = (n(&MISSES), n(&MISS_EXPIRED), n(&STALE_HITS));
    let (addrs, _, stale) = resolve_host_marked(host, 80).unwrap();
    assert!(!stale);
    assert!(!addrs.contains(&IpAddr::from(OLD)), "引き直した答え");
    assert_eq!((n(&MISSES) - m0, n(&MISS_EXPIRED) - e0), (1, 1));
    assert_eq!(n(&STALE_HITS), s0);
    restore();
}

/// `PROXY_DNS_STALE_SECS=0` なら今までどおり (期限切れは同期で引く)。
#[test]
fn zero_window_keeps_the_old_behaviour() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset(Duration::ZERO);
    let host = "localhost";
    put_answer_for_test(host, &[IpAddr::from(OLD)], Duration::from_secs(61));
    let (e0, s0) = (n(&MISS_EXPIRED), n(&STALE_HITS));
    let (_, _, stale) = resolve_host_marked(host, 80).unwrap();
    assert!(!stale);
    assert_eq!(n(&MISS_EXPIRED) - e0, 1, "期限切れのミス");
    assert_eq!(n(&STALE_HITS), s0);
    restore();
}

/// IP リテラルと `PROXY_DNS_TTL_SECS=0` には印が立たない (表を通らない)。
#[test]
fn literals_and_zero_ttl_are_never_stale() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset(SERVE_STALE);
    assert!(!resolve_with_pref_marked("127.0.0.1:80").unwrap().2);
    set_ttl(Duration::ZERO);
    assert!(!resolve_with_pref_marked("localhost:80").unwrap().2);
    assert_eq!(
        relookup_after_stale("localhost", 80, &[IpAddr::from(OLD)]),
        None,
        "TTL 0 では引き直さない (表が無い)"
    );
    restore();
}

/// 古い答えで繋げなかったあとの引き直し: 表がまだ古ければ同期で引き、番地が変わっていれば返す
/// (そのときの同期の引き直しは `expired` のミスに数える)。
#[test]
fn a_relookup_resolves_synchronously_and_reports_a_change() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset(SERVE_STALE);
    let host = "localhost";
    let old = IpAddr::from(OLD);
    put_answer_for_test(host, &[old], Duration::from_secs(120));
    let (m0, e0, t0) = (n(&MISSES), n(&MISS_EXPIRED), n(&STALE_RETRIES));
    let (addrs, _) = relookup_after_stale(host, 80, &[old]).expect("localhost は別の番地に引ける");
    assert!(!addrs.contains(&old));
    assert_eq!(n(&STALE_RETRIES) - t0, 1);
    assert_eq!((n(&MISSES) - m0, n(&MISS_EXPIRED) - e0), (1, 1));
    assert_eq!(entry_addrs(host), addrs, "表にも書き戻す");
    restore();
}

/// 裏の引き直しが先に済んでいれば、その答えを使って `getaddrinfo` は待たない。
/// 番地が同じなら `None` (繋ぎ直さない)。
#[test]
fn a_relookup_uses_a_fresh_table_answer_and_skips_an_unchanged_one() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset(SERVE_STALE);
    let moved = "t213-moved.invalid";
    let same = "t213-same.invalid";
    let new = IpAddr::from([127, 0, 0, 1]);
    put_answer_for_test(moved, &[new], Duration::ZERO);
    put_answer_for_test(same, &[new], Duration::ZERO);
    let (m0, t0) = (n(&MISSES), n(&STALE_RETRIES));
    assert_eq!(
        relookup_after_stale(moved, 80, &[IpAddr::from(OLD)]),
        Some((vec![new], None)),
        "表の新しい答え"
    );
    assert_eq!(
        relookup_after_stale(same, 80, &[new]),
        None,
        "番地が同じなら繋ぎ直さない"
    );
    assert_eq!(n(&MISSES), m0, "getaddrinfo は呼んでいない");
    assert_eq!(n(&STALE_RETRIES) - t0, 2, "どちらも引き直しには数える");
    restore();
}

/// 覚えている失敗があるときは serve-stale より負のキャッシュの枝が先 (リゾルバを叩かない)。
#[test]
fn a_remembered_failure_is_not_turned_into_a_background_refresh() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset(SERVE_STALE);
    set_negative_ttl(NEGATIVE);
    let host = "t213-failed.invalid";
    put_answer_for_test(host, &[IpAddr::from(OLD)], Duration::from_secs(120));
    if let Some(e) = TABLE.locked().as_mut().and_then(|t| t.get_mut(host)) {
        e.failed_at = Some((Instant::now(), io::ErrorKind::NotFound, "boom".into()));
    }
    let (s0, st0) = (n(&STALE_HITS), n(&STALE));
    let (_, _, stale) = resolve_host_marked(host, 80).unwrap();
    assert!(
        !stale,
        "引き直しが失敗して凌いだ古い答え (stale_served) の方"
    );
    assert_eq!((n(&STALE_HITS), n(&STALE) - st0), (s0, 1));
    restore();
}

/// T22.4: keep-warm の既定は止めてある。窓の中で 2 回目に使われた名前も warm に上げず、
/// 期限切れの答えは serve-stale が受ける (ミスではなく古い答え + 裏の引き直し 1 回)。
/// `PROXY_DNS_WARM_SECS=3600` (T21.3 までの既定) なら同じ使い方で warm に上がる。
#[test]
fn the_default_does_not_keep_names_warm_and_serve_stale_takes_the_expired_use() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    assert!(WARM_DEFAULT.is_zero(), "既定は keep-warm を止める");
    assert_eq!(
        WARM,
        Duration::from_secs(3600),
        "戻すときの値は T21.3 までの既定"
    );
    let host = "localhost";
    let old = IpAddr::from(OLD);

    // 既定: 120 秒前に引いた名前の 2 回目の使用 (窓 3,600 秒なら warm に上がる間隔)
    set_ttl(Duration::from_secs(60));
    set_serve_stale(SERVE_STALE);
    set_warm_window(WARM_DEFAULT);
    clear();
    put_answer_for_test(host, &[old], Duration::from_secs(120));
    let (m0, s0, r0) = (n(&MISSES), n(&STALE_HITS), n(&REFRESHES));
    let (addrs, _, stale) = resolve_host_marked(host, 80).unwrap();
    assert_eq!((addrs, stale), (vec![old], true), "古い答えをそのまま返す");
    assert_eq!(n(&MISSES), m0, "ミスではない");
    assert_eq!(n(&STALE_HITS), s0 + 1);
    assert_eq!(warm_count(), 0, "既定では warm に上げない");
    wait_refreshes(r0 + 1);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        n(&REFRESHES),
        r0 + 1,
        "裏の引き直しは serve-stale の 1 回だけ"
    );
    assert_eq!(warm_count(), 0);

    // `PROXY_DNS_WARM_SECS=3600` に戻すと、同じ使い方で warm に上がる
    clear();
    set_warm_window(WARM);
    put_answer_for_test(host, &[old], Duration::from_secs(120));
    resolve_host_marked(host, 80).unwrap();
    assert_eq!(warm_count(), 1, "3,600 秒の窓なら 2 回目で warm");
    restore();
}

/// 頼んだあとで `clear()` された名前に同じ名前の新しい項目ができても、前の頼みでは引き直さない
/// (T.28.39)。以前は待ち行列に残った前のテストの頼みが新しい項目を引き直して数え、
/// `an_expired_answer_inside_the_window_is_served_and_refreshed_once` がたまに 1 回多く数えていた。
#[test]
fn a_refresh_requested_before_clear_does_not_touch_the_new_entry() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset(SERVE_STALE);
    let old = IpAddr::from(OLD);
    // `clear()` のあとに置いた新しい項目 (旗は倒れている)
    put_answer_for_test("localhost", &[old], Duration::from_secs(120));
    let r0 = n(&REFRESHES);
    // 前の項目のために頼んであった 1 回が、いま届いた
    warm::refresh_one("localhost", true);
    assert_eq!(n(&REFRESHES), r0, "旗の倒れた項目は引き直さない");
    assert_eq!(entry_addrs("localhost"), vec![old], "答えも替えない");
    // keep-warm の待ち行列から来る分 (旗を使わない) は今までどおり引き直す
    warm::refresh_one("localhost", false);
    assert_eq!(n(&REFRESHES), r0 + 1);
    restore();
}
