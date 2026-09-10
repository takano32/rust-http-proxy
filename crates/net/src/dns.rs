//! 名前解決の結果を短時間キャッシュする。
//!
//! `getaddrinfo(3)` は TTL を返さないので、[`set_ttl`] で与えた固定の TTL (`PROXY_DNS_TTL_SECS`、
//! 既定 60 秒、0 で無効) だけ保持する。解決に失敗したときは [`STALE_MAX`] 以内の古い結果を
//! 使い (オリジンの DNS 障害でトンネルが全滅しないように)、失敗そのものも [`NEGATIVE`] の間
//! 覚えて連続した再解決を抑える。IP リテラルはキャッシュしない。

use crate::sync::LockExt;
use std::cell::Cell;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 解決に失敗したとき、この時間以内の古い結果なら使う。
pub const STALE_MAX: Duration = Duration::from_secs(3600);
/// 失敗を覚えておく時間。
pub const NEGATIVE: Duration = Duration::from_secs(5);
/// 保持するホスト数の上限 (超えたら最も古いものを捨てる)。
const MAX_ENTRIES: usize = 4096;

static TTL_SECS: AtomicU64 = AtomicU64::new(60);
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static STALE: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU64 = AtomicU64::new(0);
/// ミスのときに `getaddrinfo` に費やした時間の合計 (us)。**ミスの経路でしか書かない**
/// ので、当たりの経路 (熱い方) には原子操作が 1 つも増えない (T12.4 (2))。
static RESOLVE_US_SUM: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// このスレッドが直近に払った名前解決の費用 (us の合計と回数)。**ホスト別の内訳に
    /// 使う**もので、原子操作を増やさないための thread-local。読む側が 0 に戻す。
    ///
    /// `const` で初期化しているので destructor が登録されず、スレッドの終了中に触っても
    /// `AccessError` にならない (`Drop` を持つ thread-local と違って `with` で足りる)。
    static RESOLVE_COST: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
    /// このスレッドで直近に確立した接続の族 (`Some(true)` = IPv6)。
    /// 書くのは [`note_family`] (接続が確立した 1 か所だけ)、読む側が `None` に戻す。
    static LAST_FAMILY: Cell<Option<bool>> = const { Cell::new(None) };
}

/// 直近の名前解決の費用を読み、0 に戻す (ms の合計と回数)。
///
/// **測る前にも 1 回呼んで捨てること**。ここは要求をまたいで貯まる箱なので、
/// 前の要求が残していったぶんを次の要求のホストに付けないようにする。
pub fn take_resolve_cost() -> (u64, u64) {
    let (us, n) = RESOLVE_COST.replace((0, 0));
    // 1 ms 未満のミス (手元の loopback) は 0 ms として数える。デプロイ先の
    // ミスは Docker の内蔵 DNS 越しで ms の単位なので、この丸めで足りる
    ((us + 500) / 1000, n)
}

/// 直近に確立した接続の族を読み、`None` に戻す。
pub fn take_family() -> Option<bool> {
    LAST_FAMILY.replace(None)
}

/// 接続が確立した族を控える (`crate::net` の確立点だけが呼ぶ)。thread-local への
/// 書き込み 1 回で、原子操作もシステムコールも増えない。
pub fn note_family(v6: bool) {
    LAST_FAMILY.set(Some(v6));
}

/// `getaddrinfo` に費やした時間の合計 (us) と回数 (`/metrics` と `/status`)。
pub fn resolve_cost_total() -> (u64, u64) {
    (
        RESOLVE_US_SUM.load(Ordering::Relaxed),
        MISSES.load(Ordering::Relaxed),
    )
}

struct Entry {
    addrs: Vec<IpAddr>,
    resolved_at: Instant,
    /// 直近の失敗 (負のキャッシュ)
    failed_at: Option<(Instant, io::ErrorKind, String)>,
    /// このホストで最後に接続できた族 (`Some(true)` = IPv6)。RFC 8305 §8 の
    /// 「過去の結果で優先する族を変える」ための記憶で、**TTL で引き直しても残す**
    /// (アドレスは変わっても、そのホストへどちらの族で届くかは変わりにくい)。
    last_win_v6: Option<bool>,
}

static TABLE: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

/// キャッシュの TTL。0 で無効 (毎回解決)。
pub fn set_ttl(ttl: Duration) {
    TTL_SECS.store(ttl.as_secs(), Ordering::Relaxed);
}

pub fn ttl() -> Duration {
    Duration::from_secs(TTL_SECS.load(Ordering::Relaxed))
}

/// `host:port` (IPv6 リテラルは `[..]:port`) を分ける。ポートが無ければ `None`。
fn split_host_port(addr: &str) -> Option<(String, u16)> {
    let (host, port) = crate::net::split_host_port(addr);
    Some((host, port?))
}

fn system_resolve(host: &str, port: u16) -> io::Result<Vec<IpAddr>> {
    let addrs: Vec<IpAddr> = (host, port).to_socket_addrs()?.map(|a| a.ip()).collect();
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve host",
        ));
    }
    Ok(addrs)
}

/// `addr_str` (`host:port`) を解決する。キャッシュがあれば OS に問い合わせない。
pub fn resolve(addr_str: &str) -> io::Result<Vec<SocketAddr>> {
    resolve_with_pref(addr_str).map(|(addrs, _)| addrs)
}

/// [`resolve`] に「最後に勝った族」の記憶を添えて返す (T12.1)。**表を引くのは 1 回だけ**
/// にするためで、接続側が別に [`preferred_family`] を呼ぶと鍵を 2 回取ることになる。
pub fn resolve_with_pref(addr_str: &str) -> io::Result<(Vec<SocketAddr>, Option<bool>)> {
    let Some((host, port)) = split_host_port(addr_str) else {
        return addr_str.to_socket_addrs().map(|i| (i.collect(), None));
    };
    let host = host.as_str();
    let ttl = ttl();
    if ttl.is_zero() || host.parse::<IpAddr>().is_ok() {
        return addr_str.to_socket_addrs().map(|i| (i.collect(), None));
    }
    let key = host.to_ascii_lowercase();
    let now = Instant::now();
    let mut pref = None;
    {
        let mut guard = TABLE.locked();
        if let Some(e) = guard.get_or_insert_with(HashMap::new).get(&key) {
            pref = e.last_win_v6;
            if !e.addrs.is_empty() && now.duration_since(e.resolved_at) < ttl {
                HITS.fetch_add(1, Ordering::Relaxed);
                return Ok((with_port(&e.addrs, port), pref));
            }
            if let Some((at, kind, msg)) = &e.failed_at
                && now.duration_since(*at) < NEGATIVE
            {
                FAILURES.fetch_add(1, Ordering::Relaxed);
                return Err(io::Error::new(*kind, msg.clone()));
            }
        }
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    // ミスのときだけ `Instant` を 2 回読む (当たりの経路は 1 命令も増えない。T12.4 (2))
    let t0 = Instant::now();
    let result = system_resolve(host, port);
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
            // 引き直しでも族の記憶は引き継ぐ
            let last_win_v6 = table.get(&key).and_then(|e| e.last_win_v6).or(pref);
            table.insert(
                key,
                Entry {
                    addrs: addrs.clone(),
                    resolved_at: now,
                    failed_at: None,
                    last_win_v6,
                },
            );
            Ok((with_port(&addrs, port), last_win_v6))
        }
        Err(e) => {
            let entry = table.entry(key).or_insert_with(|| Entry {
                addrs: Vec::new(),
                resolved_at: now,
                failed_at: None,
                last_win_v6: None,
            });
            entry.failed_at = Some((now, e.kind(), e.to_string()));
            if !entry.addrs.is_empty() && now.duration_since(entry.resolved_at) < STALE_MAX {
                STALE.fetch_add(1, Ordering::Relaxed);
                let pref = entry.last_win_v6;
                return Ok((with_port(&entry.addrs, port), pref));
            }
            Err(e)
        }
    }
}

/// このホストで最後に接続できた族 (`Some(true)` = IPv6)。覚えていなければ `None`。
pub fn preferred_family(host: &str) -> Option<bool> {
    if host.is_empty() || host.parse::<IpAddr>().is_ok() {
        return None;
    }
    let key = host.to_ascii_lowercase();
    TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(&key))
        .and_then(|e| e.last_win_v6)
}

/// このホストで勝った族を覚える。**答えが変わるときだけ**呼ぶこと (定常状態では鍵を取らない)。
/// IP リテラルは覚えない (族は見れば分かるし、表に載せる意味が無い)。
pub fn remember_family(host: &str, v6: bool) {
    // TTL 0 (キャッシュ無効) のときは読む側が表を見ないので、覚えても引かれない
    if host.is_empty() || ttl().is_zero() || host.parse::<IpAddr>().is_ok() {
        return;
    }
    let key = host.to_ascii_lowercase();
    let mut guard = TABLE.locked();
    let table = guard.get_or_insert_with(HashMap::new);
    if let Some(e) = table.get_mut(&key) {
        e.last_win_v6 = Some(v6);
        return;
    }
    if table.len() >= MAX_ENTRIES {
        evict_oldest(table);
    }
    // まだ引いていないホスト (TTL 0 や解決を経ない経路) でも記憶だけは置ける。
    // `addrs` が空なので当たりにはならず、次の解決で埋まる
    table.insert(
        key,
        Entry {
            addrs: Vec::new(),
            resolved_at: Instant::now(),
            failed_at: None,
            last_win_v6: Some(v6),
        },
    );
}

fn with_port(addrs: &[IpAddr], port: u16) -> Vec<SocketAddr> {
    addrs.iter().map(|&ip| SocketAddr::new(ip, port)).collect()
}

fn evict_oldest(table: &mut HashMap<String, Entry>) {
    if let Some(k) = table
        .iter()
        .min_by_key(|(_, e)| e.resolved_at)
        .map(|(k, _)| k.clone())
    {
        table.remove(&k);
    }
}

/// 覚えている結果を全部捨てる (`.env` の TTL 変更やテスト用)。
pub fn clear() {
    if let Some(t) = TABLE.locked().as_mut() {
        t.clear();
    }
}

/// `/status` の `"dns"` 要素。
pub fn status_json() -> String {
    let entries = TABLE.locked().as_ref().map_or(0, HashMap::len);
    let (us, misses) = resolve_cost_total();
    format!(
        "{{\"ttl_secs\":{},\"entries\":{},\"hits\":{},\"misses\":{},\"stale_served\":{},\"negative_hits\":{},\"miss_ms_sum\":{:.1},\"miss_avg_ms\":{:.2}}}",
        TTL_SECS.load(Ordering::Relaxed),
        entries,
        HITS.load(Ordering::Relaxed),
        MISSES.load(Ordering::Relaxed),
        STALE.load(Ordering::Relaxed),
        FAILURES.load(Ordering::Relaxed),
        us as f64 / 1000.0,
        if misses == 0 {
            0.0
        } else {
            us as f64 / 1000.0 / misses as f64
        },
    )
}

/// Prometheus 用のカウンタ (hits, misses, stale, negative)。
pub fn counters() -> [u64; 4] {
    [
        HITS.load(Ordering::Relaxed),
        MISSES.load(Ordering::Relaxed),
        STALE.load(Ordering::Relaxed),
        FAILURES.load(Ordering::Relaxed),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached(host: &str) -> Option<(usize, bool)> {
        TABLE
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|t| t.get(host))
            .map(|e| (e.addrs.len(), e.failed_at.is_some()))
    }

    #[test]
    fn splits_host_and_port() {
        assert_eq!(
            split_host_port("example.com:80"),
            Some(("example.com".into(), 80))
        );
        assert_eq!(split_host_port("[::1]:8080"), Some(("::1".into(), 8080)));
        assert_eq!(split_host_port("nope"), None);
    }

    #[test]
    fn second_lookup_is_served_from_cache() {
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
        let a = resolve("127.0.0.1:9").unwrap();
        assert_eq!(a, vec!["127.0.0.1:9".parse().unwrap()]);
        assert!(cached("127.0.0.1").is_none());
        let v6 = resolve("[::1]:9").unwrap();
        assert_eq!(v6, vec!["[::1]:9".parse().unwrap()]);
    }

    /// 勝った族の記憶は TTL で引き直しても残る (T12.1)。
    #[test]
    fn the_winning_family_survives_a_relookup() {
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
        let host = "no-such-host.invalid";
        let e1 = resolve(&format!("{host}:80")).unwrap_err();
        assert_eq!(cached(host), Some((0, true)));
        let e2 = resolve(&format!("{host}:80")).unwrap_err();
        assert_eq!(e1.kind(), e2.kind());
        assert!(status_json().contains("\"negative_hits\":"));
    }
}
