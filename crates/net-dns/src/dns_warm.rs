//! keep-warm の待ち行列 (`WARM_QUEUE`) の出し入れと、裏で引き直す `dns-refresh` スレッド (T13.1・T14.1)。
//! 表 (`TABLE`) とカウンタは親の `dns` にあり、ここからそのまま触る。T21.7 で `dns.rs` から移した
//! (関数の本体は変えていない。親から呼ぶものだけ `pub(super)`)。

use super::{
    CHANGES, Entry, MAX_WARM, REFRESH_FAILURES, REFRESH_LATE, REFRESH_LATE_AFTER, REFRESH_US_MAX,
    REFRESH_US_SUM, REFRESHES, TABLE, WARM_EVICTED, WARM_QUEUE, system_resolve, ttl, warm_window,
};
use crate::log_warn;
use crate::sync::LockExt;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// 期限前に裏で引き直し始める齢 (TTL の 3/4)。warm な名前の引き直しの周期でもある。
///
/// `ttl * 3 / 4` と書くと大きな TTL で乗算が溢れるので引き算で出す。
pub(super) fn refresh_after(ttl: Duration) -> Duration {
    ttl - ttl / 4
}

/// warm の待ち行列を空にし、表の印も下ろす (`clear` と `PROXY_DNS_WARM_SECS=0`)。
pub(super) fn warm_clear() {
    let mut guard = TABLE.locked();
    let mut q = WARM_QUEUE.locked();
    if let Some(table) = guard.as_mut() {
        for (_, key) in q.iter() {
            if let Some(e) = table.get_mut(key) {
                e.warm = false;
                e.next_refresh = None;
            }
        }
    }
    q.clear();
}

/// この名前を warm にする (**表の鍵を握ったまま**呼ぶ。T14.1)。
///
/// 満杯 ([`MAX_WARM`]) なら**最後の使用がいちばん古い**名前を 1 つ外す。表から消えた名前が
/// 待ち行列に残っていれば、それを先に外す (`Option` の順で `None` が最小になる)。
pub(super) fn warm_promote(
    table: &mut HashMap<String, Entry>,
    key: &str,
    now: Instant,
    ttl: Duration,
) {
    let at = now + refresh_after(ttl);
    let mut q = WARM_QUEUE.locked();
    // 同じ名前が 2 件並ばないように、古い予定があれば先に外す (表から追い出されて
    // 作り直された名前など)
    if let Some(old) = q.iter().find(|(_, k)| k == key).cloned() {
        q.remove(&old);
    }
    if q.len() >= MAX_WARM
        && let Some(victim) = q
            .iter()
            .min_by_key(|(_, k)| table.get(k.as_str()).map(|e| e.last_used))
            .cloned()
    {
        q.remove(&victim);
        if let Some(e) = table.get_mut(&victim.1) {
            // 満杯で押し出した回数 (T16.0。要求の経路の例外。上の `WARM_EVICTED` を参照)。
            // 表から既に消えた名前の予定を外しただけのときは warm を押し出していないので数えない
            WARM_EVICTED.fetch_add(1, Ordering::Relaxed);
            e.warm = false;
            e.next_refresh = None;
        }
    }
    q.insert((at, key.to_string()));
    if let Some(e) = table.get_mut(key) {
        e.warm = true;
        e.next_refresh = Some(at);
    }
}

/// `refresher` スレッドの次の仕事。
enum Next {
    /// この名前を裏で引き直す (`refreshing` は立てたあと)。2 つ目は**予定の時刻からの
    /// 遅れ** (T15.0 (7))。予定は待ち行列の鍵そのものなので、ここでしか測れない
    Refresh(String, Duration),
    /// 次の期限までこれだけ待つ
    Wait(Duration),
    /// warm な名前が無い (要求が来るまで眠る)
    Idle,
}

/// 待ち行列の先頭を見て、期限が来ていれば 1 件処理する (T14.1)。
///
/// 使われなくなった名前 (最後の使用から窓を過ぎた) はここで warm を外す = 引き直しが止まる。
/// **鍵は `TABLE` → `WARM_QUEUE` の順**。引き直しそのものは鍵を放してから呼び出し側がやる。
fn warm_next(now: Instant) -> Next {
    let ttl = ttl();
    let window = warm_window();
    let mut guard = TABLE.locked();
    let table = guard.get_or_insert_with(HashMap::new);
    let mut q = WARM_QUEUE.locked();
    loop {
        let Some(first) = q.first().cloned() else {
            return Next::Idle;
        };
        if first.0 > now {
            return Next::Wait(first.0 - now);
        }
        q.remove(&first);
        let (due, key) = first;
        let Some(e) = table.get_mut(&key) else {
            // 表から消えた名前 (`MAX_ENTRIES` で追い出された) は黙って落とす
            continue;
        };
        if !e.warm || ttl.is_zero() || window.is_zero() || now.duration_since(e.last_used) >= window
        {
            // 窓の外に出た = 誰も使っていない。ここで止める (引き直し続けない)
            e.warm = false;
            e.next_refresh = None;
            continue;
        }
        let at = now + refresh_after(ttl);
        e.next_refresh = Some(at);
        q.insert((at, key.clone()));
        if e.refreshing {
            // 要求の経路が既に引き直しを頼んでいる。二重には引かない
            continue;
        }
        e.refreshing = true;
        // 予定 (`due`) からどれだけ遅れて取り出せたか。前の引き直しが詰まっていれば
        // ここに出る (T15.0 (7))
        return Next::Refresh(key, now.saturating_duration_since(due));
    }
}

/// `dns-refresh` スレッドへの用件。
enum Msg {
    /// この名前を 1 回だけ引き直す (期限前の先回り。T13.1)
    Refresh(String),
    /// warm な名前が増えた / 期限が早まった。待ち時間を計算し直す (T14.1)
    Wake,
}

/// 裏で引き直す名前の受け口。**最初の引き直しのときに遅延起動する** (引き直しが 1 回も
/// 起きないプロセスではスレッドを作らない)。要求ごとにスレッドを起こさないための 1 本で、
/// プロファイル (`--lite`) に依らず同じ。スレッドが作れなかったら `None` (以後は先回りせず、
/// 今までどおり期限切れの同期のミスで引き直す)。
static REFRESHER: OnceLock<Option<Sender<Msg>>> = OnceLock::new();

/// `dns-refresh` スレッドへの送り口 (無ければ遅延起動する)。
fn refresher() -> Option<&'static Sender<Msg>> {
    REFRESHER
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Msg>();
            match thread::Builder::new()
                .name("dns-refresh".into())
                .spawn(move || refresher_loop(rx))
            {
                Ok(_) => Some(tx),
                Err(e) => {
                    log_warn!(None, "could not start the dns-refresh thread: {}", e);
                    None
                }
            }
        })
        .as_ref()
}

/// 裏の引き直しをする 1 本のスレッド。**warm な名前の待ち行列の期限まで眠り**、
/// その間に来た用件 (先回りの依頼・起こし) を捌く。送り口は `OnceLock` の中で
/// 生き続けるので、この繰り返しは終わらない。
fn refresher_loop(rx: mpsc::Receiver<Msg>) {
    loop {
        let next = warm_next(Instant::now());
        let got = match next {
            Next::Refresh(key, late) => {
                // 予定より大きく遅れて始まった = 前の引き直し (1 回 約 2 秒のことがある)
                // が詰まっていた。**引き直しの前に数える** (T15.0 (7))
                if late >= REFRESH_LATE_AFTER {
                    REFRESH_LATE.fetch_add(1, Ordering::Relaxed);
                }
                refresh_one(&key);
                // 期限の来た名前が他にもあるかもしれないので、すぐ次を見る
                continue;
            }
            Next::Wait(d) => match rx.recv_timeout(d) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            Next::Idle => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return,
            },
        };
        if let Some(Msg::Refresh(host)) = got {
            refresh_one(&host);
        }
    }
}

/// 裏の引き直しを頼む。**表の鍵を放してから呼ぶこと** (送り先が詰まっても表を止めない)。
pub(super) fn request_refresh(key: String) {
    match refresher() {
        // 送れなかった (スレッドが無い) ときは旗を戻す。戻さないとこの名前は
        // 二度と先回りされない
        Some(tx) => {
            if let Err(back) = tx.send(Msg::Refresh(key))
                && let Msg::Refresh(k) = back.0
            {
                clear_refreshing(&k);
            }
        }
        None => clear_refreshing(&key),
    }
}

/// warm な名前が増えたことを `dns-refresh` スレッドに伝える (待ち時間の計算し直し)。
/// **表の鍵を放してから呼ぶこと**。
pub(super) fn wake_refresher() {
    if let Some(tx) = refresher() {
        let _ = tx.send(Msg::Wake);
    }
}

/// 裏で 1 回引き直して表に書き戻す (`dns-refresh` スレッドの中だけ)。
///
/// **ミスとしては数えない**: 利用者はこの時間を待っていないので、`misses` と
/// `miss_avg_ms` (`sorahost_dns_seconds_*`) に混ぜると「ミス 1 回の値段」が読めなくなる。
fn refresh_one(key: &str) {
    // 引き直しにかかった時間 (T15.0 (7))。下の `now` と引き算するので時計は 1 回増えるだけ
    let t0 = Instant::now();
    // `getaddrinfo` にポート (サービス) は渡らない。std は service を NULL で引いて、
    // 返ってきたアドレスにあとからポートを詰めるので、ここは 0 でよい
    let result = system_resolve(key, 0);
    let now = Instant::now();
    // `result` は下の `if let Ok(addrs)` で move されるので、先に控える (T15.0 (7))
    let failed = result.is_err();
    let us = now
        .saturating_duration_since(t0)
        .as_micros()
        .min(u64::MAX as u128) as u64;
    // 答えが変わったか。**数える (原子操作) のは鍵を放してから** (T12.4 (2))
    let mut changed = false;
    {
        let mut guard = TABLE.locked();
        let Some(entry) = guard.as_mut().and_then(|t| t.get_mut(key)) else {
            // 途中で `clear()` された。引き直した答えを蘇らせない
            return;
        };
        entry.refreshing = false;
        entry.refreshes += 1;
        if let Ok(addrs) = result {
            // 答えが変わっていれば差し替える。族の記憶 (T12.1) は引き継ぐ。
            // **変わったことも数える** (T14.37): ここは元の答えを手元に持っている
            // 唯一の場所で、比較の費用は既に取っている鍵の内側の数本の比較だけ
            changed = addrs_changed(&entry.addrs, &addrs);
            entry.changes += u64::from(changed);
            entry.addrs = addrs;
            entry.resolved_at = now;
            entry.failed_at = None;
        }
        // 失敗したら古い答えをそのまま残す (`resolved_at` も動かさないので、期限が来たら
        // 今までどおり同期で引き直す)。**失敗も覚えない** — 裏の失敗で利用者の経路に
        // エラーを配らないため
    }
    REFRESHES.fetch_add(1, Ordering::Relaxed);
    REFRESH_US_SUM.fetch_add(us, Ordering::Relaxed);
    REFRESH_US_MAX.fetch_max(us, Ordering::Relaxed);
    if failed {
        // 失敗しても `resolved_at` は動かさない (上のコメント) ので、この名前は次の
        // 期限でミスになる。**warm なら `misses_by_kind.warm_stale`** に出る
        REFRESH_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
    if changed {
        CHANGES.fetch_add(1, Ordering::Relaxed);
    }
}

/// 引き直しで**答えの集合**が変わったか (T14.37)。
///
/// **順序は見ない**: `getaddrinfo` は同じ答えを毎回違う順で返すことがあり (CDN の
/// ラウンドロビン、`rotate` を有効にした resolv.conf)、並びの入れ替わりを「変化」と
/// 数えると `changes` が「本当に別のサーバーに振られた回数」として読めなくなる。
///
/// **前の答えが空のとき (= 最初の解決、族の記憶だけ置いた入れ物) は変化と数えない。**
/// 1 つの名前のアドレスは多くても十数本で、`getaddrinfo` は同じアドレスを 2 度返さない
/// ので、本数と両向きの包含で足りる (並べ替えも確保もしない)。
pub(super) fn addrs_changed(old: &[IpAddr], new: &[IpAddr]) -> bool {
    if old.is_empty() || old.len() != new.len() {
        return !old.is_empty();
    }
    !new.iter().all(|a| old.contains(a)) || !old.iter().all(|a| new.contains(a))
}

/// 引き直しの旗を下ろす (頼めなかったとき)。
fn clear_refreshing(key: &str) {
    if let Some(e) = TABLE.locked().as_mut().and_then(|t| t.get_mut(key)) {
        e.refreshing = false;
    }
}
