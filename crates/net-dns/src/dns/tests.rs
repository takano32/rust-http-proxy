//! `dns` の単体テスト (表・負のキャッシュ・ミスの種類・費用の箱)。T21.7 で `dns.rs` の
//! `mod tests` をそのまま移した。表もカウンタもプロセスで 1 組なので、`RESOLVE_TEST_LOCK` を握って直列に回す。
//! keep-warm と裏の引き直しの試験は T34.62 で `warm_tests.rs` へ移した。補助関数はそちらからも
//! 使うので `pub(super)`。

use super::*;

pub(super) fn cached(host: &str) -> Option<(usize, bool)> {
    TABLE
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|t| t.get(host))
        .map(|e| (e.addrs.len(), e.failed_at.is_some()))
}

/// (hits, misses, negative_hits, refreshes)。
pub(super) fn tally() -> (u64, u64, u64, u64) {
    let [hits, misses, _, negative, refreshes] = counters();
    (hits, misses, negative, refreshes)
}

/// 表の 1 件の時計を巻き戻す (秒を待たずに「t=50 秒」の状態を作る)。
pub(super) fn age_entry(host: &str, resolved_ago: Duration, used_ago: Duration) {
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
pub(super) fn is_warm(host: &str) -> bool {
    TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(host))
        .is_some_and(|e| e.warm)
}

/// 次に裏で引き直すまでの ms (warm でなければ `None`)。秒で見ると切り捨てで
/// 1 秒ずれるので ms で測る。
pub(super) fn next_in_ms(host: &str) -> Option<u128> {
    let now = Instant::now();
    TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(host))
        .and_then(|e| e.next_refresh)
        .map(|at| at.saturating_duration_since(now).as_millis())
}

/// [`warm_due_now`] の、予定を `ago` だけ過去に置く版 (T15.0)。
/// **予定に遅れて取り出される引き直し**を秒を待たずに作れる。
pub(super) fn warm_due_ago(host: &str, ago: Duration) {
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

/// [`put`] の、覚えさせる答えを選べる版 (T14.37 の「前の答え」を作る)。
pub(super) fn put_addrs(host: &str, addrs: &[IpAddr], last_used_ago: Duration) {
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
pub(super) fn changes_of(host: &str) -> u64 {
    TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(host))
        .map_or(0, |e| e.changes)
}

/// この名前が warm の間に引かれた回数 (T15.0)。
pub(super) fn warm_requests_of(host: &str) -> u64 {
    TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(host))
        .map_or(0, |e| e.warm_requests)
}

/// この名前のミスの内訳 (T15.0)。
pub(super) fn misses_by_kind_of(host: &str) -> [u64; 4] {
    TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(host))
        .map_or([0; 4], |e| e.misses_by_kind)
}

/// ミスの種類別の合計 (cold / expired / warm_stale / negative。T15.0)。
pub(super) fn miss_kinds() -> [u64; 4] {
    [
        MISS_COLD.load(Ordering::Relaxed),
        MISS_EXPIRED.load(Ordering::Relaxed),
        MISS_WARM_STALE.load(Ordering::Relaxed),
        MISS_NEGATIVE.load(Ordering::Relaxed),
    ]
}

/// 2 つの [`miss_kinds`] の差 (この 1 手で何が増えたか)。
pub(super) fn kinds_delta(before: [u64; 4], after: [u64; 4]) -> [u64; 4] {
    std::array::from_fn(|i| after[i] - before[i])
}

/// 「前に引けなかった」状態を直接作る (`getaddrinfo` の失敗を待たずに
/// 負のキャッシュを置く。T15.0)。
pub(super) fn put_failed(host: &str) {
    let now = Instant::now();
    let mut guard = TABLE.locked();
    let table = guard.get_or_insert_with(HashMap::new);
    let mut e = Entry::empty(now, None);
    e.failed_at = Some((now, io::ErrorKind::NotFound, "test".to_string()));
    table.insert(host.to_string(), e);
}

/// 覚えている失敗の時計を巻き戻す。
pub(super) fn age_failure(host: &str, ago: Duration) {
    let now = Instant::now();
    let mut guard = TABLE.locked();
    let f = guard
        .as_mut()
        .and_then(|t| t.get_mut(host))
        .and_then(|e| e.failed_at.as_mut())
        .expect("失敗を覚えている");
    f.0 = now - ago;
}

/// [`wait_refreshes`] の、待つ長さを選べる版。引けない名前の `getaddrinfo` は
/// 1 回 約 2 秒かかることがあるので、失敗を待つ側は長めに取る。
pub(super) fn wait_refreshes_within(want: u64, limit: Duration) {
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
    // 同時に引く本数の上限で断られた回は、引きもしないので失敗として覚えない (本体の決まりどおり)。
    // 前の試験が締め切りで諦めた問い合わせがまだ枠を持っていると起きるので、枠が空くまで少し待って
    // 引き直す (T37.10。名前解決の遅い CI の runner で 1 回だけ覚えていなかった)
    let mut e1 = resolve(&format!("{host}:80")).unwrap_err();
    for _ in 0..200 {
        if !pool::is_busy(&e1) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        e1 = resolve(&format!("{host}:80")).unwrap_err();
    }
    assert_eq!(cached(host), Some((0, true)), "{e1}");
    let e2 = resolve(&format!("{host}:80")).unwrap_err();
    assert_eq!(e1.kind(), e2.kind());
    assert!(status_json().contains("\"negative_hits\":"));
}

/// T13.1: 解決してから使われていない名前は、今までどおり期限切れでミスになる
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

/// T13.1: 失敗した名前は `PROXY_DNS_NEGATIVE_SECS` (既定 60 秒) の間 OS に
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

/// T13.1: `PROXY_DNS_NEGATIVE_SECS=0` なら失敗を覚えない。
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
    // T15.0 で `changes` の後ろに 2 欄 (`warm_requests` / `misses_by_kind`) が
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

/// T15.0: ミス 1 件ごとに「そのとき何だったか」を 4 種で残す。
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
    // 行の末尾に欄を足す次の担当がここで落ちないため (T15.0)
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

/// `peek` は読むだけ、`take` は読んで 0 に戻す (T15.0)。
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

/// T32.2: **引けない名前**でも表の上限 ([`MAX_ENTRIES`] = 4,096) を守る。前は失敗の経路が
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
