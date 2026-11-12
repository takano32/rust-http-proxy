//! keep-warm (T14.1) と、期限前に裏で引き直す `dns-refresh` スレッド (T13.1) の単体試験。T34.62 で
//! `tests.rs` から移した (名前も本体もそのまま)。`tests.rs` と同じく `RESOLVE_TEST_LOCK` を握って直列に回す
//! (表もカウンタもプロセスで 1 組)。ここでしか使わない補助関数も一緒に移した。

use super::tests::{
    age_entry, cached, is_warm, next_in_ms, put_addrs, tally, wait_refreshes_within, warm_due_ago,
};
use super::*;

/// JSON の `"<鍵>":` の後ろの数字を読む (`tests/common/client.rs` の `status_number` と
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

/// 裏の引き直しが `want` 回になるまで待つ (最大 5 秒)。
fn wait_refreshes(want: u64) {
    wait_refreshes_within(want, Duration::from_secs(5));
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
