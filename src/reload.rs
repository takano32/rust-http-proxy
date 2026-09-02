//! `$HOME/.env` の変更を検知して設定を読み直す。
//!
//! 検知は `$HOME` ディレクトリの inotify (エディタや Wings のファイルマネージャは一時ファイル
//! を rename して置き換えるので、ファイルではなくディレクトリを見る)。inotify が使えない
//! ファイルシステムに備えて、[`POLL_INTERVAL`] ごとの mtime / サイズ確認も常に行う。
//!
//! 即時反映できるのは接続単位で参照する値だけ: ACL (`PROXY_ALLOW_HOSTS` / `PROXY_DENY_HOSTS`)、
//! `PROXY_TIMEOUT_SECS`、`PROXY_KEEPALIVE_SECS`、`PROXY_LOG_LEVEL`、`PROXY_DNS_TTL_SECS`、
//! `PROXY_PAC_DIRECT`、`PROXY_BLOCKLIST_*`。それ以外 (ポート、bind、
//! TLS、オリジンプール、キャッシュ予算) は起動時に固定されるので、変更を検知したら
//! `/status` と dashboard に「再起動が必要」と出す。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::sysinfo::inotify::Watch;
use crate::{envfile, log, log_info, log_warn};

/// inotify が無い / 取りこぼした場合に備えた mtime 確認の間隔。
pub const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// 保存直後の連続イベントをまとめるための待ち時間。
const SETTLE: Duration = Duration::from_millis(200);

/// 接続ごとに参照する現在の設定。
pub struct Live {
    current: RwLock<Arc<Config>>,
    /// 起動時の設定 (再起動が必要な項目の比較元)
    boot: Arc<Config>,
    reloads: AtomicU64,
    last_reload: AtomicU64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// 検知方法 (`inotify` / `poll` / `none`)
    watch: &'static str,
    /// 変更されたが起動時に固定される項目
    restart_required: Vec<String>,
    /// 直前の再読込で反映したキー
    applied: Vec<String>,
    /// 直前の再読込で設定が解釈できなかったときのメッセージ
    error: Option<String>,
}

static GLOBAL: OnceLock<Arc<Live>> = OnceLock::new();

impl Live {
    pub fn new(config: Config) -> Arc<Live> {
        let boot = Arc::new(config);
        let live = Arc::new(Live {
            current: RwLock::new(Arc::clone(&boot)),
            boot,
            reloads: AtomicU64::new(0),
            last_reload: AtomicU64::new(0),
            state: Mutex::new(State {
                watch: "none",
                ..State::default()
            }),
        });
        let _ = GLOBAL.set(Arc::clone(&live));
        live
    }

    /// 現在の設定のスナップショット。
    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&self.current.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// `.env` を読み直して反映する。変更の有無にかかわらず呼んでよい。
    pub fn reload(&self) {
        let changed = envfile::reload();
        if changed.is_empty() {
            return;
        }
        self.reloads.fetch_add(1, Ordering::Relaxed);
        self.last_reload.store(now_epoch(), Ordering::Relaxed);
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = match Config::from_env() {
            Ok(c) => c,
            Err(e) => {
                log_warn!(None, "settings reload: {} (keeping previous settings)", e);
                st.error = Some(e);
                st.applied.clear();
                return;
            }
        };
        st.error = None;
        let old = self.config();
        let mut next = (*old).clone();
        let mut applied = Vec::new();
        if fresh.acl != old.acl {
            next.acl = fresh.acl.clone();
            applied.push("PROXY_ALLOW_HOSTS/PROXY_DENY_HOSTS");
        }
        if fresh.timeout != old.timeout {
            next.timeout = fresh.timeout;
            applied.push("PROXY_TIMEOUT_SECS");
        }
        if fresh.keepalive != old.keepalive {
            next.keepalive = fresh.keepalive;
            applied.push("PROXY_KEEPALIVE_SECS");
        }
        if fresh.dns_ttl != old.dns_ttl {
            next.dns_ttl = fresh.dns_ttl;
            crate::dns::set_ttl(fresh.dns_ttl);
            crate::dns::clear();
            applied.push("PROXY_DNS_TTL_SECS");
        }
        if fresh.pac_direct != old.pac_direct {
            next.pac_direct = fresh.pac_direct.clone();
            applied.push("PROXY_PAC_DIRECT");
        }
        let bl = crate::blocklist::Sources::from_config(&fresh);
        if bl != crate::blocklist::Sources::from_config(&old) {
            next.blocklist_file = fresh.blocklist_file.clone();
            next.blocklist_url = fresh.blocklist_url.clone();
            next.blocklist_refresh = fresh.blocklist_refresh;
            next.blocklist_exempt = fresh.blocklist_exempt.clone();
            crate::blocklist::configure(bl);
            applied.push("PROXY_BLOCKLIST_*");
        }
        let level = envfile::var("PROXY_LOG_LEVEL")
            .and_then(|v| log::Level::parse(&v))
            .unwrap_or(log::Level::Info);
        if level != log::current_level() {
            log::set_level(level);
            applied.push("PROXY_LOG_LEVEL");
        }
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);

        let boot = &self.boot;
        let mut restart = Vec::new();
        if fresh.port != boot.port {
            restart.push("SERVER_PORT");
        }
        if fresh.bind_addrs != boot.bind_addrs || fresh.ipv6 != boot.ipv6 {
            restart.push("PROXY_BIND/PROXY_IPV6");
        }
        if fresh.tls_enabled != boot.tls_enabled
            || fresh.tls_verify != boot.tls_verify
            || fresh.tls_ca_file != boot.tls_ca_file
        {
            restart.push("PROXY_TLS*");
        }
        if fresh.pool_per_host != boot.pool_per_host {
            restart.push("PROXY_ORIGIN_POOL");
        }
        if fresh.stats_persist != boot.stats_persist {
            restart.push("PROXY_STATS_PERSIST");
        }
        if fresh.cache != boot.cache {
            restart.push("cache settings (SERVER_MEMORY/SERVER_DISK/PROXY_CACHE_*)");
        }
        log_info!(
            None,
            "settings reloaded: changed [{}], applied [{}]{}",
            changed.join(", "),
            applied.join(", "),
            if restart.is_empty() {
                String::new()
            } else {
                format!(", restart required for [{}]", restart.join(", "))
            }
        );
        st.applied = applied.into_iter().map(String::from).collect();
        st.restart_required = restart.into_iter().map(String::from).collect();
    }

    fn set_watch(&self, kind: &'static str) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).watch = kind;
    }

    /// `/status` の `"settings"` 要素。
    pub fn json(&self) -> String {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let list = |v: &[String]| {
            v.iter()
                .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            "{{\"path\":{},\"watch\":\"{}\",\"reloads\":{},\"last_reload\":{},\"applied\":[{}],\"restart_required\":[{}],\"error\":{}}}",
            envfile::loaded_path()
                .or_else(envfile::env_path)
                .map(|p| format!("\"{}\"", p.display().to_string().replace('"', "\\\"")))
                .unwrap_or_else(|| "null".to_string()),
            st.watch,
            self.reloads.load(Ordering::Relaxed),
            self.last_reload.load(Ordering::Relaxed),
            list(&st.applied),
            list(&st.restart_required),
            st.error
                .as_ref()
                .map(|e| format!("\"{}\"", e.replace('"', "\\\"")))
                .unwrap_or_else(|| "null".to_string()),
        )
    }
}

/// プロセス全体の設定 (main が [`Live::new`] を呼んでいなければ `None`)。
pub fn global() -> Option<&'static Arc<Live>> {
    GLOBAL.get()
}

/// `/status` 用。`Live` が無いテストでは `null`。
pub fn status_json() -> String {
    global()
        .map(|l| l.json())
        .unwrap_or_else(|| "null".to_string())
}

/// ファイルの (mtime, size)。無ければ `None`。
fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let md = std::fs::metadata(path).ok()?;
    Some((md.modified().ok()?, md.len()))
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 監視スレッドを起動する。`HOME` が無ければ何もしない。
pub fn spawn(live: Arc<Live>) -> Option<thread::JoinHandle<()>> {
    let path: PathBuf = envfile::env_path()?;
    let dir = path.parent()?.to_path_buf();
    let watch = match Watch::open(&dir, ".env") {
        Ok(w) => {
            live.set_watch("inotify");
            Some(w)
        }
        Err(e) => {
            log_warn!(
                None,
                "inotify unavailable for {} ({}); polling {} every {}s",
                dir.display(),
                e,
                path.display(),
                POLL_INTERVAL.as_secs()
            );
            live.set_watch("poll");
            None
        }
    };
    log_info!(
        None,
        "watching {} for changes ({})",
        path.display(),
        if watch.is_some() { "inotify" } else { "poll" }
    );
    let handle = thread::Builder::new()
        .name("env-reload".into())
        .spawn(move || {
            let mut seen = stamp(&path);
            loop {
                let event = match &watch {
                    Some(w) => match w.wait(POLL_INTERVAL) {
                        Ok(hit) => hit,
                        Err(e) => {
                            log_warn!(None, "inotify read failed: {}; polling only", e);
                            live.set_watch("poll");
                            thread::sleep(POLL_INTERVAL);
                            false
                        }
                    },
                    None => {
                        thread::sleep(POLL_INTERVAL);
                        false
                    }
                };
                if event {
                    thread::sleep(SETTLE);
                }
                let now = stamp(&path);
                if event || now != seen {
                    seen = now;
                    live.reload();
                }
            }
        })
        .ok()?;
    Some(handle)
}
