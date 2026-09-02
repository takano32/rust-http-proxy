//! ドメインのブロックリスト (hosts 形式 / 1 行 1 ドメイン)。
//!
//! 出所は 2 つ: `PROXY_BLOCKLIST_FILE` (ローカルのファイル) と `PROXY_BLOCKLIST_URL`
//! (StevenBlack の hosts など。`PROXY_BLOCKLIST_REFRESH_SECS` ごとに自分で取りに行き、
//! `$HOME/.sorahost-http-proxy.blocklist` に保存して再起動後も使う)。両方あれば和集合。
//! 判定は正確な一致に加えて親ドメイン (`ad.example.com` は `example.com` の登録で落ちる)。
//! `PROXY_BLOCKLIST_EXEMPT` (`*.example.com` 可) に合うホストは対象外。
//! すべて `.env` の再読込で即時反映する。

use std::collections::HashSet;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::body::{BodyReader, Framing};
use crate::config::Config;
use crate::http::{parse_origin, read_response_head};
use crate::{Upstream, log_info, log_warn, origin};

/// ファイルの mtime を見る間隔。
const CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// 取得した一覧の上限 (これ以上は切る)。
const MAX_DOWNLOAD: usize = 64 * 1024 * 1024;
/// 取得失敗時の再試行間隔。
const RETRY: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Sources {
    pub file: Option<PathBuf>,
    pub url: Option<String>,
    pub refresh: Duration,
    pub exempt: Vec<String>,
}

impl Sources {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            file: cfg.blocklist_file.clone(),
            url: cfg.blocklist_url.clone(),
            refresh: cfg.blocklist_refresh,
            exempt: cfg.blocklist_exempt.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.file.is_none() && self.url.is_none()
    }
}

#[derive(Default)]
struct State {
    sources: Sources,
    /// 出所が変わったので作り直してほしい
    dirty: bool,
    entries: usize,
    file_entries: usize,
    url_entries: usize,
    updated_at: u64,
    /// URL を最後に取得した (または保存済みを使った) 時刻
    fetched_at: u64,
    error: Option<String>,
}

static SET: RwLock<Option<Arc<HashSet<String>>>> = RwLock::new(None);
static EXEMPT: RwLock<Vec<String>> = RwLock::new(Vec::new());
static STATE: Mutex<State> = Mutex::new(State {
    sources: Sources {
        file: None,
        url: None,
        refresh: Duration::ZERO,
        exempt: Vec::new(),
    },
    dirty: false,
    entries: 0,
    file_entries: 0,
    url_entries: 0,
    updated_at: 0,
    fetched_at: 0,
    error: None,
});
static WAKE: Condvar = Condvar::new();
static BLOCKED: AtomicU64 = AtomicU64::new(0);

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 出所を設定する (起動時と `.env` の再読込時)。変わっていれば監視スレッドを起こす。
pub fn configure(sources: Sources) {
    let mut st = state();
    if st.sources == sources {
        return;
    }
    *EXEMPT.write().unwrap_or_else(|e| e.into_inner()) = sources.exempt.clone();
    st.sources = sources;
    st.dirty = true;
    WAKE.notify_all();
}

/// このホストがブロック対象か。`host` はポートなしの小文字。
pub fn is_blocked(host: &str) -> bool {
    let guard = SET.read().unwrap_or_else(|e| e.into_inner());
    let Some(set) = guard.as_ref() else {
        return false;
    };
    if set.is_empty() {
        return false;
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    let exempt = EXEMPT.read().unwrap_or_else(|e| e.into_inner());
    if exempt.iter().any(|p| crate::acl::match_pattern(p, &host)) {
        return false;
    }
    let mut cur = host.as_str();
    loop {
        if set.contains(cur) {
            BLOCKED.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        match cur.split_once('.') {
            Some((_, rest)) if rest.contains('.') => cur = rest,
            _ => return false,
        }
    }
}

/// 登録件数。
pub fn len() -> usize {
    SET.read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |s| s.len())
}

pub fn blocked_total() -> u64 {
    BLOCKED.load(Ordering::Relaxed)
}

/// hosts 形式 / 1 行 1 ドメインの文字列を解析する。
pub fn parse(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let Some(first) = tokens.next() else {
            continue;
        };
        // "0.0.0.0 host" / "127.0.0.1 host" / "::1 host" は後ろがホスト、それ以外は先頭がホスト
        let hosts: Vec<&str> = if first.parse::<std::net::IpAddr>().is_ok() {
            tokens.collect()
        } else {
            vec![first]
        };
        for h in hosts {
            let h = h.trim_end_matches('.').to_ascii_lowercase();
            if is_local_name(&h) || !looks_like_host(&h) {
                continue;
            }
            out.insert(h);
        }
    }
    out
}

fn is_local_name(h: &str) -> bool {
    matches!(
        h,
        "localhost"
            | "localhost.localdomain"
            | "local"
            | "broadcasthost"
            | "ip6-localhost"
            | "ip6-loopback"
            | "ip6-localnet"
            | "ip6-mcastprefix"
            | "ip6-allnodes"
            | "ip6-allrouters"
            | "ip6-allhosts"
            | "0.0.0.0"
    )
}

fn looks_like_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.contains('.')
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 取得した一覧を置いておくファイル。
fn cache_path() -> Option<PathBuf> {
    crate::envfile::env_path().map(|p| p.with_file_name(".sorahost-http-proxy.blocklist"))
}

fn mtime(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// `url` を GET して本文を返す (リダイレクトは 5 回まで追う)。
pub fn fetch(url: &str, upstream: &Upstream, timeout: Duration) -> io::Result<Vec<u8>> {
    let mut url = url.to_string();
    for _ in 0..6 {
        let o = parse_origin(&url, None)?;
        let server_addr = o.server_addr();
        let stream = origin::connect(
            o.scheme,
            &server_addr,
            &o.host(),
            timeout,
            upstream.tls.as_ref(),
        )?;
        let mut reader = BufReader::new(stream);
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: sorahost-http-proxy\r\nAccept: text/plain, */*\r\nConnection: close\r\n\r\n",
            o.path, o.host_port
        );
        reader.get_mut().write_all(req.as_bytes())?;
        reader.get_mut().flush()?;
        let (_, status, headers) = read_response_head(&mut reader)?;
        if (300..400).contains(&status)
            && let Some((_, loc)) = headers.iter().find(|(k, _)| k == "location")
        {
            url = if loc.contains("://") {
                loc.clone()
            } else {
                format!("{}://{}{}", o.scheme, server_addr, loc)
            };
            continue;
        }
        if status != 200 {
            return Err(io::Error::other(format!("HTTP {} from {}", status, url)));
        }
        let framing = Framing::of_response(status, false, &headers);
        let mut body = Vec::new();
        BodyReader::new(&mut reader, framing)
            .take(MAX_DOWNLOAD as u64)
            .read_to_end(&mut body)?;
        return Ok(body);
    }
    Err(io::Error::other("too many redirects"))
}

/// 一覧を作り直す。`fetch_url` が true なら URL を取りに行き、そうでなければ保存済みを使う。
fn rebuild(upstream: &Upstream, timeout: Duration, fetch_url: bool) {
    let sources = state().sources.clone();
    let mut set = HashSet::new();
    let mut file_entries = 0;
    let mut url_entries = 0;
    let mut error = None;

    if let Some(path) = &sources.file {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let parsed = parse(&text);
                file_entries = parsed.len();
                set.extend(parsed);
            }
            Err(e) => error = Some(format!("{}: {}", path.display(), e)),
        }
    }

    let mut fetched_now = false;
    if let Some(url) = &sources.url {
        let cache = cache_path();
        let mut text = None;
        if fetch_url {
            let started = Instant::now();
            match fetch(url, upstream, timeout) {
                Ok(body) => {
                    let body = String::from_utf8_lossy(&body).into_owned();
                    if let Some(p) = &cache
                        && let Err(e) = std::fs::write(p, &body)
                    {
                        log_warn!(None, "blocklist: could not save {}: {}", p.display(), e);
                    }
                    log_info!(
                        None,
                        "blocklist: fetched {} ({} bytes in {:.1}s)",
                        url,
                        body.len(),
                        started.elapsed().as_secs_f64()
                    );
                    fetched_now = true;
                    text = Some(body);
                }
                Err(e) => {
                    log_warn!(None, "blocklist: fetch {} failed: {}", url, e);
                    error = Some(format!("{}: {}", url, e));
                }
            }
        }
        if text.is_none()
            && let Some(p) = &cache
            && let Ok(saved) = std::fs::read_to_string(p)
        {
            text = Some(saved);
        }
        if let Some(t) = text {
            let parsed = parse(&t);
            url_entries = parsed.len();
            set.extend(parsed);
        }
    }

    let entries = set.len();
    *SET.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(set));
    let mut st = state();
    st.entries = entries;
    st.file_entries = file_entries;
    st.url_entries = url_entries;
    st.updated_at = now_epoch();
    st.error = error;
    if fetched_now {
        st.fetched_at = st.updated_at;
    } else if fetch_url && sources.url.is_some() {
        // 失敗: RETRY 後にもう一度
        st.fetched_at = now_epoch().saturating_sub(sources.refresh.as_secs()) + RETRY.as_secs();
    }
    log_info!(
        None,
        "blocklist: {} domains (file {}, url {}){}",
        entries,
        file_entries,
        url_entries,
        st.error
            .as_ref()
            .map(|e| format!("; last error: {}", e))
            .unwrap_or_default()
    );
}

/// 監視スレッド: 出所の変更・ファイルの更新・URL の期限で一覧を作り直す。
pub fn spawn(upstream: Arc<Upstream>, timeout: Duration) -> Option<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("blocklist".into())
        .spawn(move || {
            let mut seen_mtime: Option<u64> = None;
            loop {
                let (sources, dirty) = {
                    let mut st = state();
                    let dirty = st.dirty;
                    st.dirty = false;
                    (st.sources.clone(), dirty)
                };
                if sources.is_empty() {
                    if dirty {
                        *SET.write().unwrap_or_else(|e| e.into_inner()) = None;
                        let mut st = state();
                        st.entries = 0;
                        st.file_entries = 0;
                        st.url_entries = 0;
                        st.error = None;
                    }
                } else {
                    let file_mtime = sources.file.as_deref().and_then(mtime);
                    let file_changed = file_mtime != seen_mtime;
                    seen_mtime = file_mtime;
                    let (fetched_at, refresh) = (state().fetched_at, sources.refresh.as_secs());
                    let saved_mtime = cache_path().as_deref().and_then(mtime).unwrap_or(0);
                    let now = now_epoch();
                    // 起動直後は保存済みが新しければそれを使い、期限が来たら取りに行く
                    let base = fetched_at.max(saved_mtime);
                    let url_due = sources.url.is_some() && now.saturating_sub(base) >= refresh;
                    if dirty || file_changed || url_due {
                        rebuild(&upstream, timeout, url_due);
                        if !url_due && sources.url.is_some() && state().fetched_at == 0 {
                            state().fetched_at = saved_mtime;
                        }
                    }
                }
                let guard = state();
                let _ = WAKE.wait_timeout(guard, CHECK_INTERVAL);
            }
        })
        .ok()
}

/// `/status` の `"blocklist"` 要素。
pub fn status_json() -> String {
    let st = state();
    let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    format!(
        "{{\"entries\":{},\"file\":{},\"file_entries\":{},\"url\":{},\"url_entries\":{},\"refresh_secs\":{},\"updated_at\":{},\"fetched_at\":{},\"exempt\":[{}],\"blocked\":{},\"error\":{}}}",
        st.entries,
        st.sources
            .file
            .as_ref()
            .map(|p| q(&p.display().to_string()))
            .unwrap_or_else(|| "null".into()),
        st.file_entries,
        st.sources
            .url
            .as_ref()
            .map(|u| q(u))
            .unwrap_or_else(|| "null".into()),
        st.url_entries,
        st.sources.refresh.as_secs(),
        st.updated_at,
        st.fetched_at,
        st.sources
            .exempt
            .iter()
            .map(|e| q(e))
            .collect::<Vec<_>>()
            .join(","),
        BLOCKED.load(Ordering::Relaxed),
        st.error
            .as_ref()
            .map(|e| q(e))
            .unwrap_or_else(|| "null".into()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosts_and_plain_formats() {
        let set = parse(
            "# comment\n127.0.0.1 localhost\n0.0.0.0 ads.example.com # trailing\n0.0.0.0 a.tracker.net b.tracker.net\n::1 ip6-localhost\nplain.example.org\nbad host name\n",
        );
        assert!(set.contains("ads.example.com"));
        assert!(set.contains("a.tracker.net") && set.contains("b.tracker.net"));
        assert!(set.contains("plain.example.org"));
        assert!(!set.contains("localhost") && !set.contains("ip6-localhost"));
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn matches_exact_and_parent_domains_with_exemptions() {
        *SET.write().unwrap() = Some(Arc::new(parse(
            "0.0.0.0 doubleclick.net\nexact.example.com\n",
        )));
        *EXEMPT.write().unwrap() = vec!["*.safe.example.com".into()];
        assert!(is_blocked("doubleclick.net"));
        assert!(is_blocked("ad.doubleclick.net"));
        assert!(is_blocked("Exact.Example.com."));
        assert!(
            is_blocked("sub.exact.example.com"),
            "child of an entry is blocked too"
        );
        assert!(
            !is_blocked("example.com"),
            "parent of an entry is not blocked"
        );
        assert!(!is_blocked("net"));
        *SET.write().unwrap() = Some(Arc::new(parse("0.0.0.0 safe.example.com\n")));
        assert!(!is_blocked("x.safe.example.com"), "exempt pattern wins");
        assert!(!is_blocked("safe.example.com"));
        *SET.write().unwrap() = None;
        assert!(!is_blocked("doubleclick.net"));
    }
}
