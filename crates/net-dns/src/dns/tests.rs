//! `dns` の単体テスト (表・keep-warm・負のキャッシュ・ミスの種類・費用の箱)。T21.7 で `dns.rs` の
//! `mod tests` をそのまま移した。表もカウンタもプロセスで 1 組なので、`RESOLVE_TEST_LOCK` を握って直列に回す。

use super::*;

fn cached(host: &str) -> Option<(usize, bool)> {
    TABLE
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|t| t.get(host))
        .map(|e| (e.addrs.len(), e.failed_at.is_some()))
}

/// JSON の `"<鍵>":` の後ろの数字を読む (`tests/common/mod.rs` の `status_number` と
/// 同じ形)。**末尾の `}` で突き合わせない**ため: 鍵の後ろに欄が足されても落ちず、
/// `contains` と違って `1` が `12` に前方一致することもない。
fn json_number(json: &str, key: &str) -> u64 {
    let pat = format!("\"{}\":", key);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("no {} in {}", key, json))
        + pat.len();
    json[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{} is not a number in {}", key, json))
}

/// (hits, misses, negative_hits, refreshes)。
fn tally() -> (u64, u64, u64, u64) {
    let [hits, misses, _, negative, refreshes] = counters();
    (hits, misses, negative, refreshes)
}

/// 表の 1 件の時計を巻き戻す (秒を待たずに「t=50 秒」の状態を作る)。
fn age_entry(host: &str, resolved_ago: Duration, used_ago: Duration) {
    let now = Instant::now();
    let mut guard = TABLE.locked();
    let e = guard
        .as_mut()
        .and_then(|t| t.get_mut(host))
        .expect("表に載っている");
    e.resolved_at = now - resolved_ago;
    e.last_used = now - used_ago;
    e.prev_used = e.last_used;
}

/// warm な名前か (T14.1)。
fn is_warm(host: &str) -> bool {
    TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(host))
        .is_some_and(|e| e.warm)
}

/// 次に裏で引き直すまでの ms (warm でなければ `None`)。秒で見ると切り捨てで
/// 1 秒ずれるので ms で測る。
fn next_in_ms(host: &str) -> Option<u128> {
    let now = Instant::now();
    TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(host))
        .and_then(|e| e.next_refresh)
        .map(|at| at.saturating_duration_since(now).as_millis())
}

/// 次の引き直しがおよそ `want` 秒後に予定されていること。
fn assert_next_in(host: &str, want: u64, what: &str) {
    let ms = next_in_ms(host).unwrap_or_else(|| panic!("{}: 予定が無い", what));
    let want = u128::from(want) * 1000;
    assert!(
        ms + 500 >= want && ms <= want,
        "{}: 次の引き直しは {} ms 後 (期待 {} ms)",
        what,
        ms,
        want
    );
}

/// 「次に引き直す時刻」を今にして `dns-refresh` を起こす (秒を待たずに周期を回す)。
fn warm_due_now(host: &str) {
    warm_due_ago(host, Duration::ZERO);
}

/// [`warm_due_now`] の、予定を `ago` だけ過去に置く版 (T15.0 (7))。
/// **予定に遅れて取り出される引き直し**を秒を待たずに作れる。
fn warm_due_ago(host: &str, ago: Duration) {
    let due = Instant::now() - ago;
    {
        let mut guard = TABLE.locked();
        let table = guard.get_or_insert_with(HashMap::new);
        let mut q = WARM_QUEUE.locked();
        let old: Vec<_> = q.iter().filter(|(_, k)| k == host).cloned().collect();
        for o in old {
            q.remove(&o);
        }
        q.insert((due, host.to_string()));
        if let Some(e) = table.get_mut(host) {
            e.next_refresh = Some(due);
        }
    }
    wake_refresher();
}

/// warm が外れるまで待つ (最大 5 秒)。
fn wait_unwarm(host: &str) {
    for _ in 0..500 {
        if !is_warm(host) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("{} is still warm", host);
}

/// 表に答えを直接置く (`getaddrinfo` を呼ばずに「当たる名前」を作る)。
fn put(host: &str, last_used_ago: Duration) {
    put_addrs(host, &[IpAddr::from([127, 0, 0, 1])], last_used_ago);
}

/// [`put`] の、覚えさせる答えを選べる版 (T14.37 の「前の答え」を作る)。
fn put_addrs(host: &str, addrs: &[IpAddr], last_used_ago: Duration) {
    let now = Instant::now();
    let mut guard = TABLE.locked();
    let table = guard.get_or_insert_with(HashMap::new);
    let mut e = Entry::empty(now, None);
    e.addrs = addrs.to_vec();
    e.last_used = now - last_used_ago;
    e.prev_used = e.last_used;
    table.insert(host.to_string(), e);
}

/// この名前の答えが変わった回数 (T14.37)。
fn changes_of(host: &str) -> u64 {
    TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(host))
        .map_or(0, |e| e.changes)
}

/// この名前が warm の間に引かれた回数 (T15.0 (7))。
fn warm_requests_of(host: &str) -> u64 {
    TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(host))
        .map_or(0, |e| e.warm_requests)
}

/// この名前のミスの内訳 (T15.0 (7))。
fn misses_by_kind_of(host: &str) -> [u64; 4] {
    TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(host))
        .map_or([0; 4], |e| e.misses_by_kind)
}

/// ミスの種類別の合計 (cold / expired / warm_stale / negative。T15.0 (7))。
fn miss_kinds() -> [u64; 4] {
    [
        MISS_COLD.load(Ordering::Relaxed),
        MISS_EXPIRED.load(Ordering::Relaxed),
        MISS_WARM_STALE.load(Ordering::Relaxed),
        MISS_NEGATIVE.load(Ordering::Relaxed),
    ]
}

/// 2 つの [`miss_kinds`] の差 (この 1 手で何が増えたか)。
fn kinds_delta(before: [u64; 4], after: [u64; 4]) -> [u64; 4] {
    std::array::from_fn(|i| after[i] - before[i])
}

/// 「前に引けなかった」状態を直接作る (`getaddrinfo` の失敗を待たずに
/// 負のキャッシュを置く。T15.0 (7))。
fn put_failed(host: &str) {
    let now = Instant::now();
    let mut guard = TABLE.locked();
    let table = guard.get_or_insert_with(HashMap::new);
    let mut e = Entry::empty(now, None);
    e.failed_at = Some((now, io::ErrorKind::NotFound, "test".to_string()));
    table.insert(host.to_string(), e);
}

/// 覚えている失敗の時計を巻き戻す。
fn age_failure(host: &str, ago: Duration) {
    let now = Instant::now();
    let mut guard = TABLE.locked();
    let f = guard
        .as_mut()
        .and_then(|t| t.get_mut(host))
        .and_then(|e| e.failed_at.as_mut())
        .expect("失敗を覚えている");
    f.0 = now - ago;
}

/// 裏の引き直しが `want` 回になるまで待つ (最大 5 秒)。
fn wait_refreshes(want: u64) {
    wait_refreshes_within(want, Duration::from_secs(5));
}

/// [`wait_refreshes`] の、待つ長さを選べる版。引けない名前の `getaddrinfo` は
/// 1 回 約 2 秒かかることがあるので、失敗を待つ側は長めに取る。
fn wait_refreshes_within(want: u64, limit: Duration) {
    let deadline = Instant::now() + limit;
    loop {
        if REFRESHES.load(Ordering::Relaxed) >= want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "refreshes = {} (expected {})",
            REFRESHES.load(Ordering::Relaxed),
            want
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn second_lookup_is_served_from_cache() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let a = resolve("LocalHost:1234").unwrap();
    assert!(a.iter().all(|s| s.port() == 1234));
    let (n, failed) = cached("localhost").expect("cached under the lowercase name");
    assert_eq!(n, a.len());
    assert!(!failed);
    let b = resolve("localhost:4321").unwrap();
    assert_eq!(b.len(), a.len());
    assert!(b.iter().all(|s| s.port() == 4321));
}

#[test]
fn ip_literals_bypass_the_cache() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let a = resolve("127.0.0.1:9").unwrap();
    assert_eq!(a, vec!["127.0.0.1:9".parse().unwrap()]);
    assert!(cached("127.0.0.1").is_none());
    let v6 = resolve("[::1]:9").unwrap();
    assert_eq!(v6, vec!["[::1]:9".parse().unwrap()]);
}

/// 勝った族の記憶は TTL で引き直しても残る (T12.1)。
#[test]
fn the_winning_family_survives_a_relookup() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 期限切れをミスとして見るテストなので serve-stale は止める (T21.3)
    set_serve_stale(Duration::ZERO);
    let host = "localhost";
    remember_family("LocalHost", true);
    assert_eq!(preferred_family(host), Some(true));
    assert_eq!(preferred_family("127.0.0.1"), None, "IP リテラルは覚えない");
    // TTL 切れにして引き直させる
    {
        let mut guard = TABLE.locked();
        let table = guard.get_or_insert_with(HashMap::new);
        if let Some(e) = table.get_mut(host) {
            e.resolved_at = Instant::now() - Duration::from_secs(24 * 3600);
        }
    }
    let (addrs, pref) = resolve_with_pref("localhost:1234").unwrap();
    assert!(!addrs.is_empty());
    assert_eq!(pref, Some(true), "引き直しても記憶は残る");
    remember_family(host, false);
    assert_eq!(preferred_family(host), Some(false));
}

#[test]
fn failure_is_remembered_briefly() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let host = "no-such-host.invalid";
    let e1 = resolve(&format!("{host}:80")).unwrap_err();
    assert_eq!(cached(host), Some((0, true)));
    let e2 = resolve(&format!("{host}:80")).unwrap_err();
    assert_eq!(e1.kind(), e2.kind());
    assert!(status_json().contains("\"negative_hits\":"));
}

/// T13.1 (a): 直近 TTL 内に使われた名前は期限の 3/4 で裏で引き直され、
/// 期限を過ぎたはずの t=61 秒でも**ヒット**する (ミスにならない)。
#[test]
fn a_hot_name_is_refreshed_before_it_expires() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_ttl(Duration::from_secs(60));
    clear();
    let host = "localhost";
    // t=0: 表に無いので同期で引く (ミス)
    let (_, m0, _, _) = tally();
    resolve_host(host, 80).unwrap();
    assert_eq!(tally().1 - m0, 1, "t=0 はミス");

    // t=50: 期限 (60 秒) の 3/4 を過ぎ、直近 TTL 内に使われている
    age_entry(host, Duration::from_secs(50), Duration::from_secs(50));
    let (h1, m1, _, r1) = tally();
    let (addrs, _) = resolve_host(host, 80).unwrap();
    assert!(!addrs.is_empty());
    let (h2, m2, _, _) = tally();
    assert_eq!((h2 - h1, m2 - m1), (1, 0), "t=50 は表から返す (ヒット)");
    wait_refreshes(r1 + 1);

    // t=61: 裏で引き直したので期限内 (引き直しから 11 秒後の姿にする)
    age_entry(host, Duration::from_secs(11), Duration::ZERO);
    let (h3, m3, _, _) = tally();
    resolve_host(host, 80).unwrap();
    let (h4, m4, _, _) = tally();
    assert_eq!((h4 - h3, m4 - m3), (1, 0), "t=61 でもヒット");
}

/// T13.1 (b): 解決してから使われていない名前は、今までどおり期限切れでミスになる
/// (熱くないので裏では引き直さない = 誰も見ない名前に `getaddrinfo` を払わない)。
#[test]
fn an_idle_name_still_expires() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 期限切れをミスとして見るテストなので serve-stale は止める (T21.3)
    set_serve_stale(Duration::ZERO);
    set_ttl(Duration::from_secs(60));
    clear();
    let host = "localhost";
    resolve_host(host, 80).unwrap();
    // t=61: 解決してから一度も使われていない
    age_entry(host, Duration::from_secs(61), Duration::from_secs(61));
    let (h0, m0, _, r0) = tally();
    resolve_host(host, 80).unwrap();
    let (h1, m1, _, r1) = tally();
    assert_eq!((h1 - h0, m1 - m0), (0, 1), "使われていない名前はミス");
    assert_eq!(r1, r0, "裏の引き直しは走らない");
}

/// T13.1 (c): 同じ名前に同時に 10 本来ても、裏の引き直しは 1 本だけ
/// (要求ごとにスレッドも `getaddrinfo` も増やさない)。
#[test]
fn ten_callers_trigger_one_refresh() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_ttl(Duration::from_secs(60));
    clear();
    let host = "localhost";
    resolve_host(host, 80).unwrap();
    age_entry(host, Duration::from_secs(50), Duration::from_secs(1));
    let (_, m0, _, r0) = tally();
    let hands: Vec<_> = (0..10)
        .map(|_| thread::spawn(|| resolve_host("localhost", 80).unwrap()))
        .collect();
    for h in hands {
        h.join().unwrap();
    }
    assert_eq!(tally().1, m0, "10 本とも表から返る (ミスは増えない)");
    wait_refreshes(r0 + 1);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(tally().3, r0 + 1, "引き直したのは 1 本だけ");
}

/// T13.1 (d): 失敗した名前は `PROXY_DNS_NEGATIVE_SECS` (既定 60 秒) の間 OS に
/// 問い合わせず、過ぎたら問い合わせる。
#[test]
fn a_failure_is_remembered_for_the_negative_ttl() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_ttl(Duration::from_secs(60));
    set_negative_ttl(NEGATIVE);
    clear();
    let host = "no-such-host-t131.invalid";
    let e1 = resolve_host(host, 80).unwrap_err();
    assert_eq!(cached(host), Some((0, true)));

    // 10 秒後: 覚えた失敗をそのまま返す (OS には行かない)
    age_failure(host, Duration::from_secs(10));
    let (_, m0, n0, _) = tally();
    let e2 = resolve_host(host, 80).unwrap_err();
    assert_eq!(e1.kind(), e2.kind(), "同じエラーを返す");
    let (_, m1, n1, _) = tally();
    assert_eq!(m1, m0, "10 秒後は OS に問い合わせない");
    assert_eq!(n1 - n0, 1, "負のキャッシュの当たりが 1 増える");

    // 61 秒後: 覚えた失敗は切れているので問い合わせる
    age_failure(host, Duration::from_secs(61));
    let (_, m2, _, _) = tally();
    resolve_host(host, 80).unwrap_err();
    assert_eq!(tally().1 - m2, 1, "61 秒後は OS に問い合わせる");
}

/// T13.1 (e): `PROXY_DNS_NEGATIVE_SECS=0` なら失敗を覚えない。
#[test]
fn zero_negative_ttl_does_not_remember_failures() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_ttl(Duration::from_secs(60));
    clear();
    set_negative_ttl(Duration::ZERO);
    let host = "no-such-host-t131-zero.invalid";
    let (_, m0, n0, _) = tally();
    resolve_host(host, 80).unwrap_err();
    resolve_host(host, 80).unwrap_err();
    let (_, m1, n1, _) = tally();
    set_negative_ttl(NEGATIVE);
    assert_eq!(m1 - m0, 2, "0 なら毎回 OS に問い合わせる");
    assert_eq!(n1, n0, "負のキャッシュには当たらない");
}

/// T13.1: 失敗を覚えている間でも、`STALE_MAX` 以内の古い答えがあれば繋ぐ方を選ぶ
/// (負のキャッシュが 60 秒になったので、ここでエラーを返すと窓が 60 秒潰れる)。
#[test]
fn a_remembered_failure_does_not_hide_a_stale_answer() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_ttl(Duration::from_secs(60));
    set_negative_ttl(NEGATIVE);
    clear();
    // 表に直接置く (**他のテストと同じ名前を汚さない**。失敗を覚えた印は古い答えを
    // 配っても消えないので、`localhost` でやると後続のテストがそれを見る)
    let host = "t131-stale.invalid";
    let addrs = vec![IpAddr::from([127, 0, 0, 1])];
    let now = Instant::now();
    {
        let mut guard = TABLE.locked();
        let table = guard.get_or_insert_with(HashMap::new);
        // 期限切れ (61 秒前) の答え + 直近の失敗
        let mut e = Entry::empty(now - Duration::from_secs(61), None);
        e.addrs = addrs.clone();
        e.failed_at = Some((now, io::ErrorKind::NotFound, "boom".into()));
        table.insert(host.to_string(), e);
    }
    let (_, m0, _, _) = tally();
    let (again, _) = resolve_host(host, 80).expect("古い答えで繋ぐ");
    assert_eq!(again, addrs, "古い答えをそのまま返す");
    assert_eq!(tally().1, m0, "OS には問い合わせない");
    clear();
}

/// T14.1 (a): TTL 4 秒・W 60 秒で t=0 と t=10 に使った名前は warm になり、
/// 使われないまま 3 秒 (3/4 TTL) ごとに裏で引き直されるので **t=40 でもヒット**する。
///
/// 3 秒ずつ実時間を待たずに、`warm_due_now` で「次に引き直す時刻」を今にして回す。
#[test]
fn a_warm_name_is_kept_fresh_while_it_is_not_used() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 期限切れをミスとして見るテストなので serve-stale は止める (T21.3)
    set_serve_stale(Duration::ZERO);
    set_ttl(Duration::from_secs(4));
    set_warm_window(Duration::from_secs(60));
    clear();
    let host = "localhost";
    // t=0: 表に無いので同期で引く (ミス)。1 回目なので warm にはしない
    resolve_host(host, 80).unwrap();
    assert!(!is_warm(host), "1 回目の使用では warm にしない");

    // t=10: TTL 4 秒は切れているのでミスだが、直近 60 秒に **2 回目**なので warm になる
    age_entry(host, Duration::from_secs(10), Duration::from_secs(10));
    let (_, m0, _, r0) = tally();
    resolve_host(host, 80).unwrap();
    assert_eq!(tally().1 - m0, 1, "t=10 はまだミス (先回りはこれから)");
    assert!(is_warm(host), "2 回目の使用で warm");
    assert_next_in(host, 3, "warm になった直後");

    // t=13, 16, ... 37: 使われていないが裏で引き直し続ける
    for i in 1..=9 {
        warm_due_now(host);
        wait_refreshes(r0 + i);
        assert!(is_warm(host), "{} 回目の引き直しでも warm のまま", i);
        assert_next_in(host, 3, "引き直しの直後");
    }

    // t=40: 最後の引き直しから 3 秒しか経っていない (最後の使用からは 30 秒)
    age_entry(host, Duration::from_secs(3), Duration::from_secs(30));
    let (h0, m0, _, _) = tally();
    resolve_host(host, 80).unwrap();
    let (h1, m1, _, _) = tally();
    assert_eq!((h1 - h0, m1 - m0), (1, 0), "t=40 でもヒット");
    set_ttl(Duration::from_secs(60));
    clear();
}

/// T14.1 (b): 1 回しか使われていない名前は warm にならないので、裏の引き直しも
/// 走らず t=10 で**ミス**になる (誰も来ない名前に `getaddrinfo` を払わない)。
#[test]
fn a_name_used_once_never_becomes_warm() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 期限切れをミスとして見るテストなので serve-stale は止める (T21.3)
    set_serve_stale(Duration::ZERO);
    set_ttl(Duration::from_secs(4));
    set_warm_window(Duration::from_secs(60));
    clear();
    let host = "localhost";
    resolve_host(host, 80).unwrap();
    assert!(!is_warm(host));
    assert_eq!(warm_count(), 0, "待ち行列は空のまま");

    // t=10: 1 回しか使われていないので誰も引き直していない = 期限切れのミス
    age_entry(host, Duration::from_secs(10), Duration::from_secs(10));
    let (h0, m0, _, r0) = tally();
    resolve_host(host, 80).unwrap();
    let (h1, m1, _, r1) = tally();
    assert_eq!((h1 - h0, m1 - m0), (0, 1), "t=10 はミス");
    assert_eq!(r1, r0, "裏の引き直しは 1 回も走っていない");
    set_ttl(Duration::from_secs(60));
    clear();
}

/// T14.1 (c): 最後の使用から W 秒過ぎたら warm を外して引き直しを止める。
#[test]
fn a_warm_name_stops_when_nobody_uses_it() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_ttl(Duration::from_secs(4));
    set_warm_window(Duration::from_secs(60));
    clear();
    let host = "localhost";
    resolve_host(host, 80).unwrap();
    age_entry(host, Duration::from_secs(1), Duration::from_secs(1));
    resolve_host(host, 80).unwrap();
    assert!(is_warm(host), "2 回目の使用で warm");

    // 最後の使用から 61 秒 (W = 60 秒の外)
    age_entry(host, Duration::from_secs(1), Duration::from_secs(61));
    let r0 = tally().3;
    warm_due_now(host);
    wait_unwarm(host);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(tally().3, r0, "窓の外に出た名前は引き直さない");
    assert_eq!(warm_count(), 0, "待ち行列からも外れる");
    assert_eq!(next_in_ms(host), None);
    set_ttl(Duration::from_secs(60));
    clear();
}

/// T14.1 (d): 同時に warm でいられるのは [`MAX_WARM`] 件まで。33 個目を入れると
/// **最後の使用がいちばん古い** 1 件が外れる。
#[test]
fn the_warm_set_is_capped_and_drops_the_oldest() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_ttl(Duration::from_secs(60));
    set_warm_window(WARM);
    clear();
    let names: Vec<String> = (0..=MAX_WARM).map(|i| format!("t141-warm-{i}")).collect();
    // 答えを表に直接置いてから引く = 当たり + 2 回目の使用 で warm になる
    // (`getaddrinfo` は 1 回も呼ばない)
    for n in &names {
        put(n, Duration::from_secs(1));
    }
    // 押し出しの通算 (T16.0)。**満杯のときだけ**増える (ほかのテストも同じ原子を
    // 触りうるが、warm にするテストは全部 `RESOLVE_TEST_LOCK` を握っている)
    let evicted0 = WARM_EVICTED.load(Ordering::Relaxed);
    for (i, n) in names.iter().enumerate() {
        resolve_host(n, 80).unwrap();
        assert!(is_warm(n), "{} は warm", n);
        let want = (i + 1).min(MAX_WARM);
        assert_eq!(warm_count(), want, "{} 件目", i + 1);
        let evicted = WARM_EVICTED.load(Ordering::Relaxed) - evicted0;
        assert_eq!(
            evicted,
            u64::from(i >= MAX_WARM),
            "{} 件目: 枠が満杯になるまでは押し出さない",
            i + 1
        );
    }
    // T21.3 で後ろに `stale_hits` / `stale_retries` が付いたので、末尾ではなく次の `,` で突き合わせる
    assert!(
        status_json().contains(&format!(
            ",\"warm_evicted\":{},",
            WARM_EVICTED.load(Ordering::Relaxed)
        )),
        "`/status` の `dns` に出る"
    );
    assert!(!is_warm(&names[0]), "33 件目で最古 ({}) が外れる", names[0]);
    assert_eq!(next_in_ms(&names[0]), None);
    assert!(is_warm(&names[MAX_WARM]), "いちばん新しいものは warm");
    clear();
}

/// T17.3: 枠が満杯でも、外す相手が**表から既に消えている** (`evict_oldest` で表だけから
/// 落ち、待ち行列に予定が残った) なら warm を押し出していないので `warm_evicted` は増えない。
/// T17.0a の判定 `warm_evicted == 0` が誤って崩れないことを見張る。
#[test]
fn warm_evicted_does_not_count_a_victim_already_gone_from_the_table() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_ttl(Duration::from_secs(60));
    set_warm_window(WARM);
    clear();
    let names: Vec<String> = (0..=MAX_WARM).map(|i| format!("t173-warm-{i}")).collect();
    for n in &names {
        put(n, Duration::from_secs(1));
    }
    // 32 件で枠を埋める (ここまでは押し出しが無い)
    for n in &names[..MAX_WARM] {
        resolve_host(n, 80).unwrap();
        assert!(is_warm(n), "{} は warm", n);
    }
    assert_eq!(warm_count(), MAX_WARM);
    // 作り物: 最初の名前を**表からだけ**消す (待ち行列には予定が残る)
    let gone = &names[0];
    TABLE.locked().as_mut().unwrap().remove(gone);
    assert_eq!(warm_count(), MAX_WARM, "待ち行列には残っている");

    let evicted0 = WARM_EVICTED.load(Ordering::Relaxed);
    // 33 件目: 満杯の枝に入り、外す相手は表に無い `gone` (`None` が最小) になる
    let newest = &names[MAX_WARM];
    resolve_host(newest, 80).unwrap();
    assert!(is_warm(newest), "33 件目は warm");
    assert_eq!(
        WARM_EVICTED.load(Ordering::Relaxed) - evicted0,
        0,
        "表から消えた名前の予定を外しただけなら数えない"
    );
    assert_eq!(warm_count(), MAX_WARM, "消えた名前の予定が外れて 32 件");
    for n in &names[1..] {
        assert!(is_warm(n), "{} は warm のまま (誰も押し出されない)", n);
    }
    clear();
}

/// T14.1 (e): `PROXY_DNS_WARM_SECS=0` なら keep-warm は働かず、T13.1 の動き
/// (直近 TTL 内に使われた名前だけ、当たりのついでに 1 回引き直す) に戻る。
#[test]
fn zero_warm_window_falls_back_to_t131() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_ttl(Duration::from_secs(60));
    set_warm_window(Duration::ZERO);
    clear();
    let host = "localhost";
    resolve_host(host, 80).unwrap();
    // t=50: 期限の 3/4 を過ぎた「熱い」名前 → 当たりを返して裏で 1 回引き直す
    age_entry(host, Duration::from_secs(50), Duration::from_secs(50));
    let (h0, m0, _, r0) = tally();
    resolve_host(host, 80).unwrap();
    let (h1, m1, _, _) = tally();
    assert_eq!((h1 - h0, m1 - m0), (1, 0), "T13.1 どおり当たり");
    wait_refreshes(r0 + 1);
    assert!(!is_warm(host), "0 なら warm にしない");
    assert_eq!(warm_count(), 0, "待ち行列は空のまま");

    // t=61: 引き直しから 11 秒なのでまだ期限内 (T13.1 の (a) と同じ)
    age_entry(host, Duration::from_secs(11), Duration::from_secs(70));
    let (h2, m2, _, _) = tally();
    resolve_host(host, 80).unwrap();
    let (h3, m3, _, _) = tally();
    assert_eq!((h3 - h2, m3 - m2), (1, 0), "t=61 でもヒット");
    set_warm_window(WARM);
    clear();
}

/// T14.37: 引き直しで**答えの集合**が変わったときだけ `changes` が増える
/// (順序が違うだけなら増えない)。`/dns` の行と `/status` の合計にも出る。
#[test]
fn a_relookup_counts_only_a_real_change_of_the_answer() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 期限切れをミスとして見るテストなので serve-stale は止める (T21.3)
    set_serve_stale(Duration::ZERO);
    // (1) 突き合わせそのもの。`getaddrinfo` は同じ答えを違う順で返すことがあるので、
    // 並びの入れ替わりは「変わった」ではない
    let a: IpAddr = [192, 0, 2, 1].into();
    let b: IpAddr = [192, 0, 2, 2].into();
    assert!(!addrs_changed(&[a, b], &[b, a]), "順序が違うだけ");
    assert!(!addrs_changed(&[a], &[a]));
    assert!(!addrs_changed(&[], &[a]), "最初の答えは変化ではない");
    assert!(addrs_changed(&[a], &[b]), "別のアドレス");
    assert!(addrs_changed(&[a], &[a, b]), "1 本増えた");
    assert!(addrs_changed(&[a, b], &[a]), "1 本減った");

    // (2) 表を通した引き直し。期限切れ (齢 7,200 秒) で、最後の使用も 7,200 秒前なので
    // keep-warm の窓 (既定 3,600 秒) の外 = 裏の引き直しは走らない
    set_ttl(Duration::from_secs(60));
    clear();
    let host = "localhost";
    let total0 = CHANGES.load(Ordering::Relaxed);
    let expire = |ago: Duration| age_entry(host, ago, ago);
    put_addrs(host, &[a], Duration::from_secs(7200));
    expire(Duration::from_secs(7200));
    resolve_host(host, 80).expect("localhost は引ける");
    assert!(!is_warm(host), "窓の外なので warm にしない");
    assert_eq!(changes_of(host), 1, "答えが差し替わったら +1");
    assert_eq!(CHANGES.load(Ordering::Relaxed) - total0, 1, "合計も +1");

    // 同じ答えが返る引き直しでは増えない
    expire(Duration::from_secs(7200));
    resolve_host(host, 80).expect("localhost は引ける");
    assert_eq!(changes_of(host), 1, "同じ答えなら増えない");
    assert_eq!(
        CHANGES.load(Ordering::Relaxed) - total0,
        1,
        "合計も増えない"
    );

    // (3) `/dns` の行と `/status` の合計に出る
    let row = table(DnsSort::Host)
        .into_iter()
        .find(|r| r.host == host)
        .expect("表に載っている");
    assert_eq!(row.changes, 1);
    let json = row.to_json();
    // T15.0 (7) で `changes` の後ろに 2 欄 (`warm_requests` / `misses_by_kind`) が
    // 付いたので、末尾の `}` ではなく次の鍵で突き合わせる
    assert!(
        json.contains("\"refreshes\":0,\"changes\":1,\"warm_requests\":"),
        "{}",
        json
    );
    let status = status_json();
    assert!(
        status.contains(&format!("\"changes\":{},", total0 + 1)),
        "{}",
        status
    );
    clear();
}

/// T15.0 (7): ミス 1 件ごとに「そのとき何だったか」を 4 種で残す。
/// **4 種の和は `dns.misses` と一致する** (受け入れ基準)。
#[test]
fn misses_are_counted_by_kind() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 期限切れをミスとして見るテストなので serve-stale は止める (T21.3)
    set_serve_stale(Duration::ZERO);
    set_ttl(Duration::from_secs(60));
    set_warm_window(WARM);
    clear();
    let host = "localhost";
    let (m0, k0) = (tally().1, miss_kinds());

    // (a) cold: 表に無い名前の 1 回目
    resolve_host(host, 80).expect("localhost は引ける");
    assert_eq!(kinds_delta(k0, miss_kinds()), [1, 0, 0, 0], "1 回目は cold");
    assert!(!is_warm(host), "1 回では warm にしない");

    // (b) expired: 答えは持っているが TTL を過ぎた (窓の中なのでこの参照で warm になる)
    let k1 = miss_kinds();
    age_entry(host, Duration::from_secs(61), Duration::from_secs(61));
    resolve_host(host, 80).unwrap();
    assert_eq!(
        kinds_delta(k1, miss_kinds()),
        [0, 1, 0, 0],
        "期限切れは expired"
    );
    assert!(is_warm(host), "2 回目の使用で warm");
    assert_eq!(
        warm_requests_of(host),
        0,
        "warm になった参照そのものは warm の要求ではない"
    );

    // (c) warm_stale: warm なのに期限が切れていた = 裏の引き直しが間に合っていない
    let k2 = miss_kinds();
    age_entry(host, Duration::from_secs(61), Duration::from_secs(1));
    resolve_host(host, 80).unwrap();
    assert_eq!(
        kinds_delta(k2, miss_kinds()),
        [0, 0, 1, 0],
        "warm のミスは warm_stale"
    );
    assert_eq!(warm_requests_of(host), 1, "warm の間に来た要求だけ数える");
    assert_eq!(
        misses_by_kind_of(host),
        [1, 1, 1, 0],
        "名前ごとの行にも 1 件ずつ残る"
    );

    // (d) negative: 覚えている失敗の期限が切れたので引き直した
    let k3 = miss_kinds();
    clear();
    put_failed(host);
    age_failure(host, Duration::from_secs(61));
    resolve_host(host, 80).unwrap();
    assert_eq!(
        kinds_delta(k3, miss_kinds()),
        [0, 0, 0, 1],
        "失敗のあとの引き直しは negative"
    );

    // (e) `PROXY_DNS_TTL_SECS=0` は表を触らない = 「直前の姿」が無いので cold
    let k4 = miss_kinds();
    set_ttl(Duration::ZERO);
    resolve_host(host, 80).unwrap();
    set_ttl(Duration::from_secs(60));
    assert_eq!(kinds_delta(k4, miss_kinds()), [1, 0, 0, 0], "TTL 0 は cold");

    // 和は `dns.misses` と一致する
    let k5 = miss_kinds();
    assert_eq!(
        tally().1 - m0,
        kinds_delta(k0, k5).iter().sum::<u64>(),
        "4 種の和 = misses"
    );

    // `/dns` の行と `/status` に出る
    let status = status_json();
    let row = table(DnsSort::Host)
        .into_iter()
        .find(|r| r.host == host)
        .expect("表に載っている");
    assert_eq!(
        row.misses_by_kind,
        [0, 0, 0, 1],
        "(d) の 1 件だけ残っている"
    );
    let json = row.to_json();
    // **末尾の `}` は突き合わせない** (`misses_by_kind` の入れ子の閉じまで)。
    // 行の末尾に欄を足す次の担当がここで落ちないため (T15.0 (7))
    assert!(
        json.contains(
            "\"warm_requests\":0,\"misses_by_kind\":{\"cold\":0,\"expired\":0,\"warm_stale\":0,\"negative\":1}"
        ),
        "{}",
        json
    );
    assert!(
        status.contains(&format!("\"misses_by_kind\":{}", misses_by_kind_json(&k5))),
        "{}",
        status
    );
    clear();
}

/// T15.0 (7): 予定から [`REFRESH_LATE_AFTER`] 以上遅れて始まった引き直しを数える
/// (引き直しは 1 本のスレッドが順にやるので、詰まるとここに出る)。
#[test]
fn a_late_refresh_is_counted() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_ttl(Duration::from_secs(60));
    set_warm_window(WARM);
    clear();
    let host = "localhost";
    // 答えを表に直接置いてから引く = 当たり + 2 回目の使用で warm (`getaddrinfo` は呼ばない)
    put(host, Duration::from_secs(1));
    resolve_host(host, 80).unwrap();
    assert!(is_warm(host), "2 回目の使用で warm");

    let late0 = REFRESH_LATE.load(Ordering::Relaxed);
    let r0 = tally().3;
    warm_due_ago(host, Duration::from_secs(6));
    wait_refreshes(r0 + 1);
    assert_eq!(
        REFRESH_LATE.load(Ordering::Relaxed),
        late0 + 1,
        "6 秒遅れて取り出された引き直しは 1 件"
    );

    // 予定どおりに取り出せた引き直しは数えない
    let r1 = tally().3;
    warm_due_now(host);
    wait_refreshes(r1 + 1);
    assert_eq!(
        REFRESH_LATE.load(Ordering::Relaxed),
        late0 + 1,
        "遅れていない引き直しは数えない"
    );
    let status = status_json();
    assert_eq!(
        json_number(&status, "refresh_late"),
        late0 + 1,
        "{}",
        status
    );
    clear();
}

/// T15.0 (7): 裏の引き直しの**失敗**と所要時間を数える。失敗しても
/// `resolved_at` は動かないので、この名前は次の期限でミスになる。
#[test]
fn a_failed_refresh_is_counted_with_its_time() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_ttl(Duration::from_secs(60));
    set_warm_window(WARM);
    clear();
    // 引けない名前に「前の答え」を置いて warm にする。要求の経路は当たりで帰るので、
    // `getaddrinfo` の失敗を待つのは `dns-refresh` スレッドだけ
    let host = "t150-no-such-host.invalid";
    put(host, Duration::from_secs(1));
    resolve_host(host, 80).unwrap();
    assert!(is_warm(host), "2 回目の使用で warm");

    let f0 = REFRESH_FAILURES.load(Ordering::Relaxed);
    let s0 = REFRESH_US_SUM.load(Ordering::Relaxed);
    let r0 = tally().3;
    warm_due_now(host);
    // 引けない名前は 1 回 約 2 秒かかることがある
    wait_refreshes_within(r0 + 1, Duration::from_secs(20));
    assert_eq!(
        REFRESH_FAILURES.load(Ordering::Relaxed),
        f0 + 1,
        "引けなかった引き直しは 1 件"
    );
    assert!(
        REFRESH_US_SUM.load(Ordering::Relaxed) > s0,
        "かかった時間が足されている"
    );
    assert!(
        REFRESH_US_MAX.load(Ordering::Relaxed) > 0,
        "最大も入っている"
    );
    // 失敗しても古い答えは残る (T13.1 の決まり)
    assert_eq!(cached(host).map(|(n, _)| n), Some(1), "古い答えは捨てない");
    let status = status_json();
    assert!(
        status.contains(&format!("\"refresh_failures\":{},", f0 + 1)),
        "{}",
        status
    );
    clear();
}

/// `peek` は読むだけ、`take` は読んで 0 に戻す (T15.0 (1))。
///
/// 入口の ACL (`crates/server/src/conn/serve_one.rs`) が時計より前に払ったぶんを、トンネルと forward が
/// **消さずに**控えるための口なので、2 回読んでも同じ値が出ることが要点。
#[test]
fn peeking_the_resolve_cost_does_not_take_it() {
    // このテストのスレッドの箱を空にしてから始める (thread-local)
    let _ = take_resolve_cost();
    assert_eq!(peek_resolve_cost(), (0, 0));

    note_resolve_cost(11_400, 1);
    assert_eq!(
        peek_resolve_cost(),
        (11, 1),
        "0.5 ms で丸める (take と同じ)"
    );
    assert_eq!(peek_resolve_cost(), (11, 1), "読むだけなので減らない");

    note_resolve_cost(600, 1);
    assert_eq!(peek_resolve_cost(), (12, 2), "足される");
    assert_eq!(take_resolve_cost(), (12, 2), "peek と同じ値が取れる");
    assert_eq!(peek_resolve_cost(), (0, 0), "take のあとは空");
    assert_eq!(take_resolve_cost(), (0, 0));
}

/// T.32.2: **引けない名前**でも表の上限 ([`MAX_ENTRIES`] = 4,096) を守る。前は失敗の経路が
/// 無条件に `entry().or_insert_with` で行を足していたので、相異なる引けない名前の数だけ
/// 表が伸びた (tokyo の 48 時間で 7,511 行、うち古い側の 4,146 行が答えの無い失敗の行)。
#[test]
fn failed_names_do_not_grow_the_table_past_the_cap() {
    let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_ttl(Duration::from_secs(60));
    set_negative_ttl(NEGATIVE);
    clear();
    fn nxdomain(_: &str, _: u16) -> io::Result<Vec<IpAddr>> {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve host",
        ))
    }
    fn answer(_: &str, _: u16) -> io::Result<Vec<IpAddr>> {
        Ok(vec![IpAddr::from([192, 0, 2, 7])])
    }
    // 答えのある名前を先に 100 個置く (失敗の行より先に捨てられてはいけない)
    set_resolver_for_test(Some(answer));
    for i in 0..100 {
        resolve(&format!("t322-ok-{i}.example:443")).unwrap();
    }
    set_resolver_for_test(Some(nxdomain));
    for i in 0..10_000 {
        assert!(resolve(&format!("t322-nx-{i}.invalid:443")).is_err());
    }
    set_resolver_for_test(None);
    let n = entries();
    let kept_ok = (0..100)
        .filter(|i| cached(&format!("t322-ok-{i}.example")).is_some_and(|(a, _)| a == 1))
        .count();
    // 直近の失敗は負のキャッシュとして残っている (上限の内側で古い失敗から捨てる)
    let last = cached("t322-nx-9999.invalid");
    clear();
    assert!(
        n <= MAX_ENTRIES,
        "引けない名前 10,000 個のあとに表が {n} 行"
    );
    assert_eq!(kept_ok, 100, "答えのある行は失敗の行より後に捨てる");
    assert_eq!(last, Some((0, true)), "直近の失敗は覚えている");
}
