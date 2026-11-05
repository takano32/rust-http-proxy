//! 表を引く本体 (`resolve_host_marked`) と、serve-stale の古い答えで繋げなかったときの引き直し
//! (`relookup_after_stale`)。T.34.54 で `dns.rs` から移した (本体は変えていない)。

use super::cost::RESOLVE_COST;
use super::entry::{Entry, Looked, MissKind};
use super::{
    CHANGES, FAILURES, HITS, MAX_ENTRIES, MISS_COLD, MISS_EXPIRED, MISSES, RESOLVE_US_SUM, STALE,
    STALE_HITS, STALE_MAX, STALE_RETRIES, TABLE, addrs_changed, evict_oldest, negative_ttl, pool,
    refresh_after, request_refresh, serve_stale, ttl, wake_refresher, warm_promote, warm_window,
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
    let mut pref = None;
    // 表から答えが出たか (**数えるのは鍵を放してから**。T12.4 (2))
    let mut found: Option<Looked> = None;
    // ミスだったときの種類 (T15.0 (7))。表に載っていない名前は `cold`
    let mut miss_kind = MissKind::Cold;
    // 期限前の先回りを頼むか (T13.1) / この参照で warm になったか (T14.1)
    let (mut ask, mut promote) = (false, false);
    {
        // 取り合いを数える (T14.3 (3))。空いていれば `locked` と同じ費用
        let mut guard = TABLE.locked_counted(&crate::sync::LOCK_CONTENDED[crate::sync::LOCK_DNS]);
        let table = guard.get_or_insert_with(HashMap::new);
        if let Some(e) = table.get_mut(&key) {
            pref = e.last_win_v6;
            // **`warm` はここで読む** (T15.0 (7)): 下の `warm_promote` が `e.warm = true` を
            // 書くので、あとで読むと「2 回目の使用でミスした」が `warm_stale` に化ける
            let was_warm = e.warm;
            // 「熱い」= **この参照の 1 つ前**の使用が TTL 以内。印はここで更新する
            let idle = now.duration_since(e.last_used);
            let hot = idle < ttl;
            // warm = 直近 W 秒に **2 回以上**使われた名前。この参照が 2 回目なら
            // 待ち行列に入れる (1 回だけの名前は入れない。引き直しても二度と来ない)。
            // **答えを持っている名前だけ**を warm にする: 引けない名前は先回りする
            // 期限を持たず (要求の経路は負のキャッシュで止まる)、1 回 約 2 秒かかるので、
            // 待ち行列に入れると `dns-refresh` の 1 本が本当に熱い名前を待たせる
            promote = !e.warm && !window.is_zero() && idle < window && !e.addrs.is_empty();
            e.prev_used = e.last_used;
            // warm の間に来た要求を数える (T15.0 (7)。**「要求の経路は増やさない」の
            // 例外その 1**: 既に握っている鍵の内側の、非原子の加算 1 つ・分岐なしで、
            // システムコールも確保も増えない)
            e.warm_requests += u64::from(was_warm);
            e.last_used = now;
            let age = now.duration_since(e.resolved_at);
            if !e.addrs.is_empty() && age < ttl {
                // 期限の 3/4 を過ぎた熱いホストは、答えは今の表から返して (ヒット)、
                // 裏で 1 回だけ引き直す。利用者は期限切れのミス (デプロイ先で 約 10 ms、
                // 混むと 100 ms 超) を待たない (T13.1)
                ask = hot && !e.refreshing && age >= refresh_after(ttl);
                if ask {
                    e.refreshing = true;
                }
                found = Some(Looked::Hit(e.addrs.clone()));
            } else {
                let negative = negative_ttl();
                if let Some((at, kind, msg)) = &e.failed_at
                    && !negative.is_zero()
                    && now.duration_since(*at) < negative
                {
                    // 覚えている失敗の内側でも、**古い答えがあるなら繋ぐ方を選ぶ**。
                    // 負のキャッシュが 60 秒になったので、ここでエラーを返すと
                    // 「古い答えで凌ぐ」窓 (`STALE_MAX`) がそのぶん潰れる (T13.1)
                    found = Some(if !e.addrs.is_empty() && age < STALE_MAX {
                        Looked::Stale(e.addrs.clone())
                    } else {
                        Looked::Negative(io::Error::new(*kind, msg.clone()))
                    });
                } else if !e.addrs.is_empty() && age < serve_stale() {
                    // serve-stale (T21.3): 期限は切れたが齢が窓の中 = 古い答えをそのまま返し、
                    // 裏で 1 回だけ引き直す (同じ名前を二重に頼まない)。**覚えている失敗が
                    // あるときは上の枝が先** (負のキャッシュの間はリゾルバを叩かない)
                    ask = !e.refreshing;
                    if ask {
                        e.refreshing = true;
                    }
                    found = Some(Looked::Served(e.addrs.clone()));
                }
            }
            if found.is_none() {
                // ここまで来た = 表では答えられない = このあとミスになる。種類の優先順位は
                // **warm_stale → negative → expired → cold** (T15.0 (7))。`refresh_one` は
                // 失敗しても `failed_at` を書かない (上のコメント) ので、warm な名前に
                // 載っている失敗は要求の経路が書いたもの = 「引き直しが間に合っていない」の
                // 証拠は `warm` の旗の方が強い
                miss_kind = if was_warm {
                    MissKind::WarmStale
                } else if e.failed_at.is_some() {
                    MissKind::Negative
                } else if !e.addrs.is_empty() {
                    MissKind::Expired
                } else {
                    // 答えを持っていない入れ物 (`remember_family` が族の記憶だけ置いた等)
                    MissKind::Cold
                };
            }
            if promote {
                warm_promote(table, &key, now, ttl);
            }
        }
    }
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
    // 締め切りつきで別のスレッドに引かせる (T.32.11。`PROXY_DNS_TIMEOUT_SECS=0` なら自分で引く)
    let result = pool::resolve(host, port);
    let us = t0.elapsed().as_micros().min(u64::MAX as u128) as u64;
    RESOLVE_US_SUM.fetch_add(us, Ordering::Relaxed);
    RESOLVE_COST.set({
        let (s, n) = RESOLVE_COST.get();
        (s + us, n + 1)
    });
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
        // 上限の本数が走っていて引きもしなかった (T.32.11)。その名前の失敗としては覚えない
        Err(e) if pool::is_busy(&e) => Err(e),
        Err(e) => {
            // 失敗の経路でも上限を守る (T.32.2)。前はここが無条件の `or_insert_with` で、
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
        None => {
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
                if let Some(e) = guard.as_mut().and_then(|t| t.get_mut(&key)) {
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
            (result.ok()?, pref)
        }
    };
    addrs_changed(old, &addrs).then_some((addrs, pref))
}
