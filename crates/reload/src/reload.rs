//! `$HOME/.env` の変更を検知して設定を読み直す。
//!
//! 検知は `$HOME` ディレクトリの inotify (エディタや Wings のファイルマネージャは一時ファイル
//! を rename して置き換えるので、ファイルではなくディレクトリを見る)。inotify が使えない
//! ファイルシステムに備えて、[`POLL_INTERVAL`] ごとの mtime / サイズ確認も常に行う。
//!
//! 即時反映できるのは接続単位で参照する値だけ: ACL (`PROXY_ALLOW_HOSTS` / `PROXY_DENY_HOSTS`)、
//! `PROXY_TIMEOUT_SECS`、`PROXY_KEEPALIVE_SECS`、`PROXY_LOG_LEVEL`、`PROXY_DNS_TTL_SECS`、
//! `PROXY_DNS_NEGATIVE_SECS`、`PROXY_DNS_WARM_SECS`、`PROXY_CANARY`、`PROXY_CANARY_SECS`、
//! `PROXY_CANARY_IPV6`、`PROXY_PAC_DIRECT`、`PROXY_BLOCKLIST_*`、`PROXY_CONNECT_PORTS`、`PROXY_ALLOW_LOCAL`、
//! `PROXY_ENDPOINTS_READONLY`、`PROXY_ALLOW_CLIENTS`、
//! `PROXY_TUNNEL_IDLE_SECS`、`PROXY_MAX_CONNS`、`PROXY_MAX_THREADS`。それ以外 (ポート、bind、
//! TLS、オリジンプール、キャッシュ予算) は起動時に固定されるので、変更を検知したら
//! `/status` と dashboard に「再起動が必要」と出す。

use crate::sync::{LockExt, RwLockExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::clock::now_epoch;

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
        Arc::clone(&self.current.read_locked())
    }

    /// `.env` を読み直して反映する。変更の有無にかかわらず呼んでよい。
    pub fn reload(&self) {
        let changed = envfile::reload();
        if changed.is_empty() {
            return;
        }
        self.reloads.fetch_add(1, Ordering::Relaxed);
        self.last_reload.store(now_epoch(), Ordering::Relaxed);
        let mut st = self.state.locked();
        let fresh = match Config::from_env() {
            Ok(c) => c,
            Err(e) => {
                log_warn!(None, "settings reload: {} (keeping previous settings)", e);
                // 出来事の時系列にも 1 件 (読めなかったことが分かるように。T14.11)
                crate::events::push(
                    crate::events::EventKind::Reload,
                    &format!("rejected: {} (keeping the previous settings)", e),
                );
                st.error = Some(e);
                st.applied.clear();
                return;
            }
        };
        st.error = None;
        let old = self.config();
        let mut next = (*old).clone();
        let mut applied = Vec::new();
        // 当てた値は**出どころも差し替える** (`/config` の `source` が追随する。T14.15)。
        // 起動時に固定される項目 (下の `restart` の側) は写さない: `.env` に書いても
        // 効いていないので、起動時の出どころのままにしておく
        if fresh.acl != old.acl {
            next.acl = fresh.acl.clone();
            next.sources.adopt(&fresh.sources, "PROXY_ALLOW_HOSTS");
            next.sources.adopt(&fresh.sources, "PROXY_DENY_HOSTS");
            applied.push("PROXY_ALLOW_HOSTS/PROXY_DENY_HOSTS");
        }
        if fresh.timeout != old.timeout {
            next.timeout = fresh.timeout;
            next.sources.adopt(&fresh.sources, "PROXY_TIMEOUT_SECS");
            applied.push("PROXY_TIMEOUT_SECS");
        }
        if fresh.keepalive != old.keepalive {
            next.keepalive = fresh.keepalive;
            next.sources.adopt(&fresh.sources, "PROXY_KEEPALIVE_SECS");
            applied.push("PROXY_KEEPALIVE_SECS");
        }
        if fresh.connect_ports != old.connect_ports {
            next.connect_ports = fresh.connect_ports.clone();
            next.sources.adopt(&fresh.sources, "PROXY_CONNECT_PORTS");
            applied.push("PROXY_CONNECT_PORTS");
        }
        if fresh.allow_local != old.allow_local {
            next.allow_local = fresh.allow_local;
            next.sources.adopt(&fresh.sources, "PROXY_ALLOW_LOCAL");
            applied.push("PROXY_ALLOW_LOCAL");
        }
        if fresh.endpoints_readonly != old.endpoints_readonly {
            next.endpoints_readonly = fresh.endpoints_readonly;
            applied.push("PROXY_ENDPOINTS_READONLY");
        }
        // 接続元の ACL は `serve` が accept ごとに引き直すので、次に来る接続から効く
        if fresh.allow_clients != old.allow_clients {
            next.allow_clients = fresh.allow_clients.clone();
            applied.push("PROXY_ALLOW_CLIENTS");
        }
        // 接続元ごとの上限も `serve` が accept ごとに引き直す (次に来る接続から効く)
        if fresh.max_conns_per_client != old.max_conns_per_client {
            next.max_conns_per_client = fresh.max_conns_per_client;
            applied.push("PROXY_MAX_CONNS_PER_CLIENT");
        }
        if fresh.tunnel_idle != old.tunnel_idle {
            next.tunnel_idle = fresh.tunnel_idle;
            next.sources.adopt(&fresh.sources, "PROXY_TUNNEL_IDLE_SECS");
            applied.push("PROXY_TUNNEL_IDLE_SECS");
        }
        if fresh.max_conns != old.max_conns {
            next.max_conns = fresh.max_conns;
            next.sources.adopt(&fresh.sources, "PROXY_MAX_CONNS");
            applied.push("PROXY_MAX_CONNS");
        }
        // `auto` は `PROXY_MAX_CONNS` から決まるので、そちらが変わるとこの値も変わる
        // (当てるのは `serve` が接続ごとに `Workers::set_limit` で。T11.6)
        if fresh.max_threads != old.max_threads {
            next.max_threads = fresh.max_threads;
            next.sources.adopt(&fresh.sources, "PROXY_MAX_THREADS");
            applied.push("PROXY_MAX_THREADS");
        }
        if fresh.dns_ttl != old.dns_ttl {
            next.dns_ttl = fresh.dns_ttl;
            next.sources.adopt(&fresh.sources, "PROXY_DNS_TTL_SECS");
            crate::dns::set_ttl(fresh.dns_ttl);
            crate::dns::clear();
            applied.push("PROXY_DNS_TTL_SECS");
        }
        // 負のキャッシュは長さを変えるだけ (表は捨てない。覚えている失敗は次の参照で
        // 新しい長さと比べられるので、0 にすればその場で効かなくなる)
        if fresh.dns_negative != old.dns_negative {
            next.dns_negative = fresh.dns_negative;
            next.sources
                .adopt(&fresh.sources, "PROXY_DNS_NEGATIVE_SECS");
            crate::dns::set_negative_ttl(fresh.dns_negative);
            applied.push("PROXY_DNS_NEGATIVE_SECS");
        }
        // keep-warm の窓も長さを変えるだけ (表は捨てない)。0 にしたときだけ
        // 待ち行列をその場で空にする (`set_warm_window` の中。T14.1)
        if fresh.dns_warm != old.dns_warm {
            next.dns_warm = fresh.dns_warm;
            next.sources.adopt(&fresh.sources, "PROXY_DNS_WARM_SECS");
            crate::dns::set_warm_window(fresh.dns_warm);
            applied.push("PROXY_DNS_WARM_SECS");
        }
        // canary の宛先と周期 (T14.10) と IPv6 側 (T14.37)。`canary` スレッドは
        // 次の周期から新しい宛先を使う
        if fresh.canary != old.canary
            || fresh.canary_secs != old.canary_secs
            || fresh.canary_ipv6 != old.canary_ipv6
        {
            next.canary = fresh.canary.clone();
            next.canary_secs = fresh.canary_secs;
            next.canary_ipv6 = fresh.canary_ipv6;
            next.sources.adopt(&fresh.sources, "PROXY_CANARY_IPV6");
            crate::canary::configure(&fresh.canary, fresh.canary_secs, fresh.canary_ipv6);
            applied.push("PROXY_CANARY");
        }
        if fresh.pac_direct != old.pac_direct {
            next.pac_direct = fresh.pac_direct.clone();
            next.sources.adopt(&fresh.sources, "PROXY_PAC_DIRECT");
            applied.push("PROXY_PAC_DIRECT");
        }
        let bl = crate::blocklist::Sources::from_config(&fresh);
        if bl != crate::blocklist::Sources::from_config(&old) {
            next.blocklist_file = fresh.blocklist_file.clone();
            next.blocklist_url = fresh.blocklist_url.clone();
            next.blocklist_refresh = fresh.blocklist_refresh;
            next.blocklist_exempt = fresh.blocklist_exempt.clone();
            for key in [
                "PROXY_BLOCKLIST_FILE",
                "PROXY_BLOCKLIST_URL",
                "PROXY_BLOCKLIST_REFRESH_SECS",
                "PROXY_BLOCKLIST_EXEMPT",
            ] {
                next.sources.adopt(&fresh.sources, key);
            }
            crate::blocklist::configure(bl);
            applied.push("PROXY_BLOCKLIST_*");
        }
        let level = envfile::var("PROXY_LOG_LEVEL")
            .and_then(|v| log::Level::parse(&v))
            .unwrap_or(log::Level::Info);
        if level != log::current_level() {
            log::set_level(level);
            next.sources.adopt(&fresh.sources, "PROXY_LOG_LEVEL");
            applied.push("PROXY_LOG_LEVEL");
        }
        *self.current.write_locked() = Arc::new(next);

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
        // 出来事の時系列に「変わった名前と前後の値」を 1 件 (T14.11)。`/status` の
        // `settings` は最後の 1 回しか残さないので、いつ何を変えたかはここでだけ読める
        crate::events::push(
            crate::events::EventKind::Reload,
            &format!(
                "{}{}",
                changed_values(&changed, &old, &fresh),
                if restart.is_empty() {
                    String::new()
                } else {
                    format!("; restart required for {}", restart.join(", "))
                }
            ),
        );
        st.applied = applied.into_iter().map(String::from).collect();
        st.restart_required = restart.into_iter().map(String::from).collect();
    }

    fn set_watch(&self, kind: &'static str) {
        self.state.locked().watch = kind;
    }

    /// `/status` の `"settings"` 要素。
    pub fn json(&self) -> String {
        let st = self.state.locked();
        format!(
            "{{\"path\":{},\"watch\":\"{}\",\"reloads\":{},\"last_reload\":{},\"applied\":{},\"restart_required\":{},\"error\":{}}}",
            crate::json::quote_opt(
                envfile::loaded_path()
                    .or_else(envfile::env_path)
                    .map(|p| p.display().to_string())
                    .as_deref()
            ),
            st.watch,
            self.reloads.load(Ordering::Relaxed),
            self.last_reload.load(Ordering::Relaxed),
            crate::json::list(&st.applied),
            crate::json::list(&st.restart_required),
            crate::json::quote_opt(st.error.as_deref()),
        )
    }
}

/// `/events` の `reload` に書く「変わった名前と前後の値」(T14.11)。
///
/// [`envfile::reload`] が返すのは**変わったキーの名前だけ**なので、値は
/// `/config` と `--check` が並べるのと同じ一覧 ([`Config::settings`]) から前後を引く。
/// 一覧に無いキー (このプロキシが読まない名前) と、書き換えても効く値が変わらなかった
/// キーは名前だけを出す。**再起動が要る項目も前後は出す** (`.env` は変わっているので)。
fn changed_values(changed: &[String], old: &Config, fresh: &Config) -> String {
    let (before, after) = (old.settings(), fresh.settings());
    let value = |all: &[crate::config::Setting], key: &str| {
        all.iter().find(|s| s.key == key).map(|s| {
            // JSON の値をそのまま出すと文字列に引用符が付くので、単純なものは外す
            // (`--check` の印字と同じ扱い)
            match s.value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
                Some(inner) if !inner.contains('\\') => inner.to_string(),
                _ => s.value.clone(),
            }
        })
    };
    changed
        .iter()
        .map(|key| match (value(&before, key), value(&after, key)) {
            (Some(b), Some(a)) if b != a => format!("{} {} \u{2192} {}", key, b, a),
            _ => key.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
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

/// 監視スレッドを起動する。`HOME` が無ければ何もしない。
/// `.env` の監視スレッドを起こす。`tick` は待ち受けが一巡するたび ([`POLL_INTERVAL`] ごと、
/// または `.env` の変化時) に呼ばれる。接続プールの掃除など、専用スレッドを立てるほどでない
/// 定期処理をここに載せる。
pub fn spawn(live: Arc<Live>, tick: impl Fn() + Send + 'static) -> Option<thread::JoinHandle<()>> {
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
            // この環境で何が読めるか (T14.15) は**起動時 1 回 + 1 時間ごと**。専用のスレッドは
            // 立てず、この一巡 (最長 `POLL_INTERVAL`) のついでに測り直す (要求の経路では触らない)。
            // 起動時の 1 回もここで測る: 名前解決の測定だけ最大 2 秒かかるので、
            // 待ち受けを始めるのを遅らせないため
            crate::sysinfo::capabilities::refresh();
            let mut caps_at = std::time::Instant::now();
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
                if caps_at.elapsed() >= crate::sysinfo::capabilities::REFRESH {
                    caps_at = std::time::Instant::now();
                    crate::sysinfo::capabilities::refresh();
                }
                tick();
            }
        })
        .ok()?;
    Some(handle)
}
