//! `$HOME/.env` の変更を検知して設定を読み直す。
//!
//! 検知は `$HOME` ディレクトリの inotify (エディタや Wings のファイルマネージャは一時ファイル
//! を rename して置き換えるので、ファイルではなくディレクトリを見る)。inotify が使えない
//! ファイルシステムに備えて、[`POLL_INTERVAL`] ごとの mtime / サイズ確認も常に行う。
//!
//! 即時反映できるのは接続単位で参照する値だけ: ACL (`PROXY_ALLOW_HOSTS` / `PROXY_DENY_HOSTS`)、
//! `PROXY_TIMEOUT_SECS`、`PROXY_KEEPALIVE_SECS`、`PROXY_LOG_LEVEL`、`PROXY_DNS_TTL_SECS`、
//! `PROXY_DNS_NEGATIVE_SECS`、`PROXY_DNS_WARM_SECS`、`PROXY_DNS_STALE_SECS`、`PROXY_DNS_TIMEOUT_SECS`、`PROXY_CANARY`、`PROXY_CANARY_SECS`、
//! `PROXY_CANARY_IPV6`、`PROXY_PAC_DIRECT`、`PROXY_BLOCKLIST_*`、`PROXY_CONNECT_PORTS`、`PROXY_ALLOW_LOCAL`、
//! `PROXY_ENDPOINTS_READONLY`、`PROXY_ALLOW_CLIENTS`、`PROXY_TRACE_CLIENT`、`PROXY_TCP_KEEPALIVE`、
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
use crate::{envfile, log_info, log_warn};

// 再読込で当てる項目と、再起動が要る項目の判断 (T.34.58)
mod merge;

/// inotify が無い / 取りこぼした場合に備えた mtime 確認の間隔。
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(30);
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
        // 記録の一括 off とハッシュ化 (T14.41)。旗はプロセス全体に 1 つなので、
        // 起動時のここと下の再読込の 2 か所だけで当てる (`--check` は通らない)
        crate::records::set(config.records);
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
    ///
    /// 段取りは T.34.58 で割った: 読む → 当てる ([`merge::merge_live`]) → 起動時に固定される
    /// 項目を比べる ([`merge::restart_required`]) → ログと出来事と `/status` の状態に残す
    /// ([`record_reload`])。順と文面は割る前と同じ。
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
            Err(e) => return reject_reload(&mut st, e),
        };
        st.error = None;
        let old = self.config();
        let (next, applied) = merge::merge_live(&old, &fresh);
        *self.current.write_locked() = Arc::new(next);
        let restart = merge::restart_required(&self.boot, &fresh);
        record_reload(&mut st, &changed, applied, restart, &old, &fresh);
    }

    fn set_watch(&self, kind: &'static str) {
        self.state.locked().watch = kind;
    }

    /// `/status` の `"settings"` 要素。
    pub fn json(&self) -> String {
        let problems = self.config().problems.clone();
        let st = self.state.locked();
        format!(
            "{{\"path\":{},\"watch\":\"{}\",\"reloads\":{},\"last_reload\":{},\"applied\":{},\"restart_required\":{},\"error\":{},\"invalid\":{},\"unknown\":{}}}",
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
            // 読めなかった値と知らない名前 (T.28.6。足しただけ。正しい `.env` では `[]`)
            problems.invalid_json(),
            problems.unknown_json(),
        )
    }
}

/// 新しい `.env` が解釈できなかった: 前の設定のまま、ログと出来事と `/status` の状態に残す。
fn reject_reload(st: &mut State, e: String) {
    log_warn!(None, "settings reload: {} (keeping previous settings)", e);
    // 出来事の時系列にも 1 件 (読めなかったことが分かるように。T14.11)
    crate::events::push(
        crate::events::EventKind::Reload,
        &format!("rejected: {} (keeping the previous settings)", e),
    );
    st.error = Some(e);
    st.applied.clear();
}

/// 再読込の結果を、ログ・出来事の時系列 (`/events`)・`/status` の状態に残す (文面は割る前と同じ)。
fn record_reload(
    st: &mut State,
    changed: &[String],
    applied: Vec<&str>,
    restart: Vec<&str>,
    old: &Config,
    fresh: &Config,
) {
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
            changed_values(changed, old, fresh),
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
        .spawn(move || watch_loop(live, path, watch, tick))
        .ok()?;
    Some(handle)
}

/// `env-reload` スレッドの本体 (T.34.58 で [`spawn`] から出した。本体は変えていない)。
fn watch_loop(live: Arc<Live>, path: PathBuf, watch: Option<Watch>, tick: impl Fn()) {
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
}

/// `reload` の判断の単体試験 (T.34.58)。
#[cfg(test)]
mod tests;
