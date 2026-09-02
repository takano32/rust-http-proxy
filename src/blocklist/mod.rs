//! ドメインのブロックリスト (hosts 形式 / 1 行 1 ドメイン)。
//!
//! 出所は 2 つ: `PROXY_BLOCKLIST_FILE` (ローカルのファイル) と `PROXY_BLOCKLIST_URL`
//! (StevenBlack の hosts など。`PROXY_BLOCKLIST_REFRESH_SECS` ごとに自分で取りに行き、
//! `$HOME/.sorahost-http-proxy.blocklist` に保存して再起動後も使う)。両方あれば和集合。
//! 判定は正確な一致に加えて親ドメイン (`ad.example.com` は `example.com` の登録で落ちる)。
//! `PROXY_BLOCKLIST_EXEMPT` (`*.example.com` 可) に合うホストは対象外。
//! すべて `.env` の再読込で即時反映する。
//!
//! - `parse`: 一覧の解析
//! - `fetch`: URL からの取得
//! - `overrides`: 手動の上書き (状態ファイルに残る)

mod fetch;
mod overrides;
mod parse;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::clock::now_epoch;
use crate::config::Config;
use crate::sync::{LockExt, RwLockExt};
use crate::{Upstream, log_info, log_warn};

pub use fetch::fetch;
pub use overrides::{Override, clear_override, overrides, set_override, set_store};
pub use parse::parse;

/// ファイルの mtime を見る間隔。
const CHECK_INTERVAL: Duration = Duration::from_secs(60);
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
    STATE.locked()
}

/// 出所を設定する (起動時と `.env` の再読込時)。変わっていれば監視スレッドを起こす。
pub fn configure(sources: Sources) {
    let mut st = state();
    if st.sources == sources {
        return;
    }
    *EXEMPT.write_locked() = sources.exempt.clone();
    st.sources = sources;
    st.dirty = true;
    WAKE.notify_all();
}

/// 手動の上書き (一時的な許可 / 拒否)。状態ファイルの固定 256 スロットに置き、
/// いっぱいなら最も古いものを潰す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// ブロックしない (どこにも無い)
    Clear,
    /// 一覧に載っている (`&str` は一致したエントリ)
    Listed,
    Exempt,
    OverrideBlock,
    OverrideAllow,
}

impl Verdict {
    pub fn blocked(self) -> bool {
        matches!(self, Verdict::Listed | Verdict::OverrideBlock)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Clear => "clear",
            Verdict::Listed => "listed",
            Verdict::Exempt => "exempt",
            Verdict::OverrideBlock => "override:block",
            Verdict::OverrideAllow => "override:allow",
        }
    }
}

/// ホストとその親ドメインのどれかが `f` を満たすか。
pub(crate) fn walk(host: &str, mut f: impl FnMut(&str) -> bool) -> bool {
    let mut cur = host;
    loop {
        if f(cur) {
            return true;
        }
        match cur.split_once('.') {
            Some((_, rest)) if rest.contains('.') => cur = rest,
            _ => return false,
        }
    }
}

/// ホストの判定 (数えない)。`host` はポートなし。
pub fn check(host: &str) -> Verdict {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Verdict::Clear;
    }
    match overrides::lookup(&host) {
        Some(true) => return Verdict::OverrideBlock,
        Some(false) => return Verdict::OverrideAllow,
        None => {}
    }
    let guard = SET.read_locked();
    let Some(set) = guard.as_ref() else {
        return Verdict::Clear;
    };
    if set.is_empty() {
        return Verdict::Clear;
    }
    let exempt = EXEMPT.read_locked();
    if exempt.iter().any(|p| crate::acl::match_pattern(p, &host)) {
        return Verdict::Exempt;
    }
    if walk(&host, |h| set.contains(h)) {
        Verdict::Listed
    } else {
        Verdict::Clear
    }
}

/// このホストがブロック対象か。`host` はポートなしの小文字。対象なら数える。
pub fn is_blocked(host: &str) -> bool {
    let blocked = check(host).blocked();
    if blocked {
        BLOCKED.fetch_add(1, Ordering::Relaxed);
    }
    blocked
}

/// 登録件数。
pub fn len() -> usize {
    SET.read_locked().as_ref().map_or(0, |s| s.len())
}

pub fn blocked_total() -> u64 {
    BLOCKED.load(Ordering::Relaxed)
}

/// hosts 形式 / 1 行 1 ドメインの文字列を解析する。
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
    *SET.write_locked() = Some(Arc::new(set));
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
                        *SET.write_locked() = None;
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
    let q = crate::json::quote;
    format!(
        "{{\"entries\":{},\"file\":{},\"file_entries\":{},\"url\":{},\"url_entries\":{},\"refresh_secs\":{},\"updated_at\":{},\"fetched_at\":{},\"exempt\":[{}],\"blocked\":{},\"error\":{},\"overrides\":[{}]}}",
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
        overrides()
            .iter()
            .map(Override::json)
            .collect::<Vec<_>>()
            .join(","),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// グローバルな SET / OVERRIDES を触るテストを直列にする
    static TEST_LOCK: Mutex<()> = Mutex::new(());

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
    fn overrides_win_over_the_list_and_expire() {
        let _g = TEST_LOCK.lock().unwrap();
        *SET.write().unwrap() = Some(Arc::new(parse("0.0.0.0 tracker.example\n")));
        assert_eq!(check("cdn.tracker.example"), Verdict::Listed);
        set_override("tracker.example", false, Duration::from_secs(60));
        assert_eq!(check("cdn.tracker.example"), Verdict::OverrideAllow);
        assert!(!is_blocked("tracker.example"));
        let o = set_override("Fresh.Example.", true, Duration::ZERO);
        assert_eq!(o.host, "fresh.example");
        assert_eq!(o.expires, 0);
        assert_eq!(check("www.fresh.example"), Verdict::OverrideBlock);
        assert!(is_blocked("www.fresh.example"));
        assert_eq!(overrides().len(), 2);
        assert!(clear_override("fresh.example"));
        assert!(!clear_override("fresh.example"));
        // 期限切れは無視される
        overrides::expire_all_for_test();
        assert_eq!(check("cdn.tracker.example"), Verdict::Listed);
        assert!(overrides().is_empty());
        overrides::clear_all();
        *SET.write().unwrap() = None;
    }

    #[test]
    fn matches_exact_and_parent_domains_with_exemptions() {
        let _g = TEST_LOCK.lock().unwrap();
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
