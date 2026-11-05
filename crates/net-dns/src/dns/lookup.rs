//! 表を引く本体 (`resolve_host_marked`) と、serve-stale の古い答えで繋げなかったときの引き直し
//! (`relookup_after_stale`)。T34.54 で `dns.rs` から移した (本体は変えていない)。T34.55 で、鍵の内側の
//! 判断を `Entry::look` に、ミスの書き戻しを `store_answer` に、同期の引き直しを `relookup_sync` に出した。

use super::cost::RESOLVE_COST;
use super::entry::{Entry, Looked, Lookup, MissKind};
use super::{
    CHANGES, FAILURES, HITS, MAX_ENTRIES, MISS_COLD, MISS_EXPIRED, MISSES, RESOLVE_US_SUM, STALE,
    STALE_HITS, STALE_MAX, STALE_RETRIES, TABLE, addrs_changed, evict_oldest, negative_ttl, pool,
    request_refresh, serve_stale, ttl, wake_refresher, warm_promote, warm_window,
};
use crate::sync::LockExt;
use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// [`resolve_host`] に「serve-stale で返した期限切れの答えか」の印 (3 つ目) を添えて返す (T21.3)。
/// 印が立つのは serve-stale の回だけで、IP リテラル・TTL 0・負のキャッシュ・引き直しの失敗で
/// 凌いだ古い答え (`stale_served`) では立たない。
pub fn resolve_host_marked(host: &str, port: u16) -> io::Result<(Vec<IpAddr>, Option<bool>, bool)> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok((vec![ip], None, false));
    }
    let ttl = ttl();
    if ttl.is_zero() {
        MISSES.fetch_add(1, Ordering::Relaxed);
        // 表を触らないので「直前の姿」も無い = いつも `cold` (T15.0 (7))
        MISS_COLD.fetch_add(1, Ordering::Relaxed);
        return pool::resolve(host, port).map(|a| (a, None, false));
    }
    let key = host.to_ascii_lowercase();
    let now = Instant::now();
    let window = warm_window();
    // 鍵の内側で決めたこと (表に載っていない名前は `Lookup::ABSENT` = `cold` のミス)
    let look = {
        // 取り合いを数える (T14.3 (3))。空いていれば `locked` と同じ費用
        let mut guard = TABLE.locked_counted(&crate::sync::LOCK_CONTENDED[crate::sync::LOCK_DNS]);
        let table = guard.get_or_insert_with(HashMap::new);
        match table.get_mut(&key) {
            Some(e) => {
                // 負のキャッシュと serve-stale の窓は、要る枝でだけ読む (当たりの経路の原子の読みを増やさない)
                let look = e.look(now, ttl, window, negative_ttl, serve_stale);
                if look.promote {
                    warm_promote(table, &key, now, ttl);
                }
                look
            }
            None => Lookup::ABSENT,
        }
    };
    let Lookup {
        pref,
        found,
        miss_kind,
        ask,
        promote,
    } = look;
    if promote {
        wake_refresher();
    }
    if ask {
        request_refresh(key.clone());
    }
    match found {
        Some(Looked::Hit(addrs)) => {
            HITS.fetch_add(1, Ordering::Relaxed);
            return Ok((addrs, pref, false));
        }
        Some(Looked::Stale(addrs)) => {
            STALE.fetch_add(1, Ordering::Relaxed);
            return Ok((addrs, pref, false));
        }
        Some(Looked::Served(addrs)) => {
            // ミスではない (`misses` にも `misses_by_kind` にも入れない。T21.3)
            STALE_HITS.fetch_add(1, Ordering::Relaxed);
            return Ok((addrs, pref, true));
        }
        Some(Looked::Negative(e)) => {
            FAILURES.fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
        None => {}
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    // 種類別も同じ場所で 1 つ (T15.0 (7)。**4 つの和は `MISSES` と一致する**)
    miss_kind.counter().fetch_add(1, Ordering::Relaxed);
    // ミスのときだけ `Instant` を 2 回読む (当たりの経路は 1 命令も増えない。T12.4 (2))
    let t0 = Instant::now();
    // 締め切りつきで別のスレッドに引かせる (T32.11。`PROXY_DNS_TIMEOUT_SECS=0` なら自分で引く)
    let result = pool::resolve(host, port);
    let us = t0.elapsed().as_micros().min(u64::MAX as u128) as u64;
    RESOLVE_US_SUM.fetch_add(us, Ordering::Relaxed);
    RESOLVE_COST.set({
        let (s, n) = RESOLVE_COST.get();
        (s + us, n + 1)
    });
    store_answer(key, now, pref, miss_kind, result)
}

/// ミスで引いた答え (または失敗) を表に書き戻して、呼び出し側へ返す値を作る (T34.55 で
/// `resolve_host_marked` から出した。本体は変えていない)。`pref` は鍵の内側で読んだ族の記憶、
/// `miss_kind` はこのミスの種類 (行の `misses_by_kind` に足す)。
fn store_answer(
    key: String,
    now: Instant,
    pref: Option<bool>,
    miss_kind: MissKind,
    result: io::Result<Vec<IpAddr>>,
) -> io::Result<(Vec<IpAddr>, Option<bool>, bool)> {
    let mut guard = TABLE.locked();
    let table = guard.get_or_insert_with(HashMap::new);
    match result {
        Ok(addrs) => {
            if table.len() >= MAX_ENTRIES && !table.contains_key(&key) {
                evict_oldest(table);
            }
            // 引き直しでも族の記憶は引き継ぐ。裏の引き直しが走っている最中なら
            // その旗も残す (同じ名前を 2 本引きに行かせない)
            let slot = table.entry(key).or_insert_with(|| Entry::empty(now, pref));
            // 期限切れの引き直しで答えが変わったか (T14.37。裏の引き直しと同じ物差し)。
            // ここは既にミスの経路 (`getaddrinfo` を待ったあと) なので、鍵の内側の
            // 比較 1 回と原子 1 つは測れない
            if addrs_changed(&slot.addrs, &addrs) {
                slot.changes += 1;
                CHANGES.fetch_add(1, Ordering::Relaxed);
            }
            slot.addrs = addrs.clone();
            slot.resolved_at = now;
            slot.last_used = now;
            slot.misses += 1;
            slot.misses_by_kind[miss_kind.index()] += 1;
            slot.failed_at = None;
            if slot.last_win_v6.is_none() {
                slot.last_win_v6 = pref;
            }
            Ok((addrs, slot.last_win_v6, false))
        }
        // 上限の本数が走っていて引きもしなかった (T32.11)。その名前の失敗としては覚えない
        Err(e) if pool::is_busy(&e) => Err(e),
        Err(e) => {
            // 失敗の経路でも上限を守る (T32.2)。前はここが無条件の `or_insert_with` で、
            // 相異なる引けない名前の数だけ表が伸びた (tokyo の 48 時間で 7,511 行)
            if table.len() >= MAX_ENTRIES && !table.contains_key(&key) {
                evict_oldest(table);
            }
            let entry = table.entry(key).or_insert_with(|| Entry::empty(now, None));
            entry.misses += 1;
            entry.misses_by_kind[miss_kind.index()] += 1;
            entry.failed_at = Some((now, e.kind(), e.to_string()));
            if !entry.addrs.is_empty() && now.duration_since(entry.resolved_at) < STALE_MAX {
                STALE.fetch_add(1, Ordering::Relaxed);
                let pref = entry.last_win_v6;
                return Ok((entry.addrs.clone(), pref, false));
            }
            Err(e)
        }
    }
}

/// serve-stale で返した古い答えで繋げなかったときに、**1 回だけ**引き直す (T21.3)。
///
/// 返すのは**番地の集合が `old` から変わったときだけ** `Some((新しい答え, 族の記憶))` で、
/// 呼び出し側はそのときだけ繋ぎ直す (同じ番地へ繋ぎ直しても同じ失敗をするだけなので)。
/// 引けなかったときも `None` (呼び出し側は元の接続のエラーを返す)。
///
/// 裏の引き直し (古い答えを返したときに頼んだもの) が既に済んでいれば、表の新しい答えを使い
/// `getaddrinfo` は待たない。まだなら同期で引いて表に書き戻し、その回は**ミス (`expired`) に数える**
/// (利用者が待ったので。`stale_retries` はどちらの場合も 1 つ増える)。
/// 要求の経路で呼ばれるのは「古い答えで `refused` / `unreachable` / `timeout`」のときだけ。
pub fn relookup_after_stale(
    host: &str,
    port: u16,
    old: &[IpAddr],
) -> Option<(Vec<IpAddr>, Option<bool>)> {
    let ttl = ttl();
    if ttl.is_zero() || host.parse::<IpAddr>().is_ok() {
        return None;
    }
    STALE_RETRIES.fetch_add(1, Ordering::Relaxed);
    let key = host.to_ascii_lowercase();
    // 裏の引き直しが先に済んでいれば、その答えを使う
    let fresh = TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(&key))
        .filter(|e| {
            !e.addrs.is_empty() && Instant::now().saturating_duration_since(e.resolved_at) < ttl
        })
        .map(|e| (e.addrs.clone(), e.last_win_v6));
    let (addrs, pref) = match fresh {
        Some(f) => f,
        None => relookup_sync(host, port, &key)?,
    };
    addrs_changed(old, &addrs).then_some((addrs, pref))
}

/// [`relookup_after_stale`] で表にまだ新しい答えが無いときに、同期で引いて表に書き戻す
/// (T34.55 で出した。本体は変えていない)。引けなければ `None`。
fn relookup_sync(host: &str, port: u16, key: &str) -> Option<(Vec<IpAddr>, Option<bool>)> {
    // 同期で引く。数え方は `resolve_host` のミスの経路と同じ (4 種の和 = `misses`)
    MISSES.fetch_add(1, Ordering::Relaxed);
    MISS_EXPIRED.fetch_add(1, Ordering::Relaxed);
    let t0 = Instant::now();
    let result = pool::resolve(host, port);
    let now = Instant::now();
    let us = now
        .saturating_duration_since(t0)
        .as_micros()
        .min(u64::MAX as u128) as u64;
    RESOLVE_US_SUM.fetch_add(us, Ordering::Relaxed);
    RESOLVE_COST.set({
        let (s, n) = RESOLVE_COST.get();
        (s + us, n + 1)
    });
    let mut changed = false;
    let mut pref = None;
    {
        let mut guard = TABLE.locked();
        if let Some(e) = guard.as_mut().and_then(|t| t.get_mut(key)) {
            e.misses += 1;
            e.misses_by_kind[MissKind::Expired.index()] += 1;
            pref = e.last_win_v6;
            if let Ok(addrs) = &result {
                changed = addrs_changed(&e.addrs, addrs);
                e.changes += u64::from(changed);
                e.addrs = addrs.clone();
                e.resolved_at = now;
                e.failed_at = None;
            }
        }
    }
    if changed {
        CHANGES.fetch_add(1, Ordering::Relaxed);
    }
    Some((result.ok()?, pref))
}
