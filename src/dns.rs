//! 名前解決の結果を短時間キャッシュする。
//!
//! `getaddrinfo(3)` は TTL を返さないので、[`set_ttl`] で与えた固定の TTL (`PROXY_DNS_TTL_SECS`、
//! 既定 60 秒、0 で無効) だけ保持する。解決に失敗したときは [`STALE_MAX`] 以内の古い結果を
//! 使い (オリジンの DNS 障害でトンネルが全滅しないように)、失敗そのものも [`NEGATIVE`] の間
//! 覚えて連続した再解決を抑える。IP リテラルはキャッシュしない。

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
}

static TABLE: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

/// キャッシュの TTL。0 で無効 (毎回解決)。
pub fn set_ttl(ttl: Duration) {
    TTL_SECS.store(ttl.as_secs(), Ordering::Relaxed);
}

pub fn ttl() -> Duration {
    Duration::from_secs(TTL_SECS.load(Ordering::Relaxed))
}

/// `host:port` (IPv6 リテラルは `[..]:port`) を分ける。
fn split_host_port(addr: &str) -> Option<(&str, u16)> {
    let (host, port) = addr.rsplit_once(':')?;
    let port = port.parse().ok()?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    Some((host, port))
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
    let Some((host, port)) = split_host_port(addr_str) else {
        return addr_str.to_socket_addrs().map(|i| i.collect());
    };
    let ttl = ttl();
    if ttl.is_zero() || host.parse::<IpAddr>().is_ok() {
        return addr_str.to_socket_addrs().map(|i| i.collect());
    }
    let key = host.to_ascii_lowercase();
    let now = Instant::now();
    {
        let mut guard = TABLE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = guard.get_or_insert_with(HashMap::new).get(&key) {
            if !e.addrs.is_empty() && now.duration_since(e.resolved_at) < ttl {
                HITS.fetch_add(1, Ordering::Relaxed);
                return Ok(with_port(&e.addrs, port));
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
    let mut guard = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    let table = guard.get_or_insert_with(HashMap::new);
    match result {
        Ok(addrs) => {
            if table.len() >= MAX_ENTRIES && !table.contains_key(&key) {
                evict_oldest(table);
            }
            table.insert(
                key,
                Entry {
                    addrs: addrs.clone(),
                    resolved_at: now,
                    failed_at: None,
                },
            );
            Ok(with_port(&addrs, port))
        }
        Err(e) => {
            let entry = table.entry(key).or_insert_with(|| Entry {
                addrs: Vec::new(),
                resolved_at: now,
                failed_at: None,
            });
            entry.failed_at = Some((now, e.kind(), e.to_string()));
            if !entry.addrs.is_empty() && now.duration_since(entry.resolved_at) < STALE_MAX {
                STALE.fetch_add(1, Ordering::Relaxed);
                return Ok(with_port(&entry.addrs, port));
            }
            Err(e)
        }
    }
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
    if let Some(t) = TABLE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        t.clear();
    }
}

/// `/status` の `"dns"` 要素。
pub fn status_json() -> String {
    let entries = TABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, HashMap::len);
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
    fn splits_host_and_port() {
        assert_eq!(split_host_port("example.com:80"), Some(("example.com", 80)));
        assert_eq!(split_host_port("[::1]:8080"), Some(("::1", 8080)));
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
