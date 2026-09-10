//! 名前解決の結果を短時間キャッシュする。
//!
//! `getaddrinfo(3)` は TTL を返さないので、[`set_ttl`] で与えた固定の TTL (`PROXY_DNS_TTL_SECS`、
//! 既定 60 秒、0 で無効) だけ保持する。解決に失敗したときは [`STALE_MAX`] 以内の古い結果を
//! 使い (オリジンの DNS 障害でトンネルが全滅しないように)、失敗そのものも [`NEGATIVE`] の間
//! 覚えて連続した再解決を抑える。IP リテラルはキャッシュしない。

use crate::sync::LockExt;
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

/// 解決の回数 (`HITS` / `MISSES`) を数えるテストは、表もカウンタも全テストで共有している
/// ので直列に回す。`dns` と `acl` と `net` のテストが同じロックを取る。
#[cfg(test)]
pub(crate) static RESOLVE_TEST_LOCK: Mutex<()> = Mutex::new(());

/// キャッシュの TTL。0 で無効 (毎回解決)。
pub fn set_ttl(ttl: Duration) {
    TTL_SECS.store(ttl.as_secs(), Ordering::Relaxed);
}

pub fn ttl() -> Duration {
    Duration::from_secs(TTL_SECS.load(Ordering::Relaxed))
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

/// 判定 (ACL) が引いた答えを、そのまま接続まで運ぶための入れ物 (T12.7)。
///
/// ローカル宛ての判定 ([`crate::acl::resolve_target`]) と、その直後の接続
/// ([`crate::net::connect_with`]) は**同じ答えを使う**。名前解決が 1 要求 1 回で
/// 済むだけでなく、`PROXY_DNS_TTL_SECS=0` (キャッシュ無効) でも判定と接続が
/// 別の答えを引かない (DNS rebinding で判定をすり抜けられない)。
pub struct Resolved<'a> {
    /// 解決したホスト名。**別のホスト宛ての接続には使わない**ための鍵で、
    /// 突き合わせは大小無視 (要求ごとに小文字の複製を作らない)
    host: &'a str,
    addrs: Vec<IpAddr>,
    /// このホストで最後に勝った族 (T12.1)。同じ鍵取りで一緒に受け取っておく
    preferred: Option<bool>,
}

impl<'a> Resolved<'a> {
    pub fn new(host: &'a str, addrs: Vec<IpAddr>, preferred: Option<bool>) -> Resolved<'a> {
        Resolved {
            host,
            addrs,
            preferred,
        }
    }

    pub fn addrs(&self) -> &[IpAddr] {
        &self.addrs
    }

    /// ホスト名の借用を手放してアドレス列だけを取り出す (別のスコープへ運ぶとき)。
    pub fn into_addrs(self) -> Vec<IpAddr> {
        self.addrs
    }

    pub fn preferred(&self) -> Option<bool> {
        self.preferred
    }

    /// `host` の答えなら、ポートを付けた候補列を返す (別のホストなら `None` = 引き直す)。
    pub fn socket_addrs_for(&self, host: &str, port: u16) -> Option<Vec<SocketAddr>> {
        self.host
            .eq_ignore_ascii_case(host)
            .then(|| with_port(&self.addrs, port))
    }
}

/// `addr_str` (`host:port`) を解決する。キャッシュがあれば OS に問い合わせない。
pub fn resolve(addr_str: &str) -> io::Result<Vec<SocketAddr>> {
    resolve_with_pref(addr_str).map(|(addrs, _)| addrs)
}

/// [`resolve`] に「最後に勝った族」の記憶を添えて返す (T12.1)。**表を引くのは 1 回だけ**
/// にするためで、接続側が別に [`preferred_family`] を呼ぶと鍵を 2 回取ることになる。
pub fn resolve_with_pref(addr_str: &str) -> io::Result<(Vec<SocketAddr>, Option<bool>)> {
    let (host, port) = crate::net::split_host_port_ref(addr_str);
    let Some(port) = port else {
        return addr_str.to_socket_addrs().map(|i| (i.collect(), None));
    };
    // IP リテラルは表も確保も要らない (熱い経路: ベンチも `127.0.0.1:port` で来る)
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok((vec![SocketAddr::new(ip, port)], None));
    }
    let (addrs, pref) = resolve_host(host, port)?;
    Ok((with_port(&addrs, port), pref))
}

/// ホスト名を解決して**アドレスだけ**を返す (T12.7)。`port` は `getaddrinfo` に渡す
/// サービス番号で、キャッシュの鍵はホスト名だけ (返す側でポートを付ける)。
///
/// IP リテラルは表を使わない (解決するものが無い)。`PROXY_DNS_TTL_SECS=0` の
/// ときも表を使わないが、**OS に投げた回数はミスとして数える**
/// (`dns.hits + dns.misses` が「解決した回数」を表すため。T12.7)。
pub fn resolve_host(host: &str, port: u16) -> io::Result<(Vec<IpAddr>, Option<bool>)> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok((vec![ip], None));
    }
    let ttl = ttl();
    if ttl.is_zero() {
        MISSES.fetch_add(1, Ordering::Relaxed);
        return system_resolve(host, port).map(|a| (a, None));
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
                return Ok((e.addrs.clone(), pref));
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
    let result = system_resolve(host, port);
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
            Ok((addrs, last_win_v6))
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
                return Ok((entry.addrs.clone(), pref));
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
    format!(
        "{{\"ttl_secs\":{},\"entries\":{},\"hits\":{},\"misses\":{},\"stale_served\":{},\"negative_hits\":{}}}",
        TTL_SECS.load(Ordering::Relaxed),
        entries,
        HITS.load(Ordering::Relaxed),
        MISSES.load(Ordering::Relaxed),
        STALE.load(Ordering::Relaxed),
        FAILURES.load(Ordering::Relaxed),
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
}
