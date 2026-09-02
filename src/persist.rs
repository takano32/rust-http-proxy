//! 統計と履歴を固定サイズの状態ファイル ([`crate::rrd`]) に写し、起動時に読み戻す。
//!
//! - 履歴: 5 秒 / 1 分 / 1 時間の各リングへ、標本ができるたびに 1 レコード書く
//! - ホスト別・接続元別: [`FLUSH_INTERVAL`] ごとに上位 1000 を固定スロットへ書き直す
//! - ブロックリストの上書き: 変更のたびにそのスロットだけ書く ([`crate::blocklist`] から)
//!
//! 置き場所は `$HOME/.sorahost-http-proxy.rrd` (Pterodactyl で永続するのはそこだけ)。
//! `PROXY_STATS_PERSIST=off` で無効。大きさは約 1 MiB で、以後は伸びない。

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::history::{Pushed, Sample};
use crate::metrics::{HostStats, Metrics};
use crate::rrd::ring::Ring;
use crate::rrd::{Region, Rrd};
use crate::{log_info, log_warn};

/// 統計表を書き直す間隔。
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(60);

pub struct Store {
    rrd: Rrd,
    pub path: PathBuf,
    rings: Mutex<[Ring; 3]>,
    /// 書込エラーの回数 (連発しないよう最初だけ警告する)
    write_errors: AtomicU64,
    flushes: AtomicU64,
}

impl Store {
    /// 既定の場所 (`$HOME/.sorahost-http-proxy.rrd`)。
    pub fn default_path() -> Option<PathBuf> {
        crate::envfile::env_path().map(|p| p.with_file_name(".sorahost-http-proxy.rrd"))
    }

    /// 開き (無ければ作り)、履歴のリングを読み戻す。
    pub fn open(path: PathBuf) -> io::Result<(Arc<Store>, Loaded)> {
        let (rrd, created) = Rrd::open(&path)?;
        let l = rrd.layout;
        let (fine, fine_recs) = Ring::load(&rrd, l.history_fine)?;
        let (minute, minute_recs) = Ring::load(&rrd, l.history_minute)?;
        let (hour, hour_recs) = Ring::load(&rrd, l.history_hour)?;
        let decode = |recs: Vec<Vec<u8>>| -> Vec<Sample> {
            recs.iter().filter_map(|p| Sample::decode(p)).collect()
        };
        let stats = |region: Region| -> io::Result<Vec<(String, HostStats)>> {
            Ok(rrd
                .read_all(region)?
                .iter()
                .filter_map(|(_, p)| HostStats::decode(p))
                .collect())
        };
        let loaded = Loaded {
            created,
            history: [decode(fine_recs), decode(minute_recs), decode(hour_recs)],
            hosts: stats(l.hosts)?,
            clients: stats(l.clients)?,
            size: l.total,
        };
        let store = Arc::new(Store {
            rrd,
            path,
            rings: Mutex::new([fine, minute, hour]),
            write_errors: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
        });
        Ok((store, loaded))
    }

    fn note(&self, what: &str, r: io::Result<()>) {
        if let Err(e) = r {
            let n = self.write_errors.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                log_warn!(
                    None,
                    "state file {}: {} failed: {}",
                    self.path.display(),
                    what,
                    e
                );
            }
        }
    }

    /// 履歴の標本を各リングへ書く。
    pub fn write_samples(&self, pushed: &Pushed) {
        let mut rings = self.rings.lock().unwrap_or_else(|p| p.into_inner());
        for (i, s) in [pushed.fine, pushed.minute, pushed.hour].iter().enumerate() {
            if let Some(s) = s {
                let r = rings[i].push(&self.rrd, &s.encode());
                self.note("history write", r);
            }
        }
    }

    /// ホスト別・接続元別の表を書き直す。
    pub fn flush_stats(&self, metrics: &Metrics) {
        let l = self.rrd.layout;
        self.write_table(l.hosts, &metrics.hosts_sorted());
        self.write_table(l.clients, &metrics.clients_sorted());
        self.flushes.fetch_add(1, Ordering::Relaxed);
    }

    fn write_table(&self, region: Region, rows: &[(String, HostStats)]) {
        for (i, (name, s)) in rows.iter().take(region.count).enumerate() {
            self.note("stats write", self.rrd.write(region, i, &s.encode(name)));
        }
        for i in rows.len().min(region.count)..region.count {
            // 前回より減った分は消す (最初の 1 本が空なら以降も空なので打ち切る)
            if self.rrd.read_all_is_empty_at(region, i) {
                break;
            }
            self.note("stats clear", self.rrd.clear(region, i));
        }
    }

    /// ブロックリストの上書き領域。
    pub fn overrides_region(&self) -> Region {
        self.rrd.layout.overrides
    }

    pub fn write_override(&self, idx: usize, payload: &[u8]) {
        self.note(
            "override write",
            self.rrd.write(self.rrd.layout.overrides, idx, payload),
        );
    }

    pub fn clear_override(&self, idx: usize) {
        self.note(
            "override clear",
            self.rrd.clear(self.rrd.layout.overrides, idx),
        );
    }

    pub fn read_overrides(&self) -> Vec<(usize, Vec<u8>)> {
        self.rrd
            .read_all(self.rrd.layout.overrides)
            .unwrap_or_default()
    }

    /// `/status` の `"state_file"` 要素。
    pub fn status_json(&self) -> String {
        format!(
            "{{\"path\":\"{}\",\"bytes\":{},\"flushes\":{},\"write_errors\":{}}}",
            self.path.display().to_string().replace('"', "\\\""),
            self.rrd.layout.total,
            self.flushes.load(Ordering::Relaxed),
            self.write_errors.load(Ordering::Relaxed)
        )
    }
}

static GLOBAL: std::sync::OnceLock<Arc<Store>> = std::sync::OnceLock::new();

/// `/status` 用。永続化していなければ `null`。
pub fn status_json() -> String {
    GLOBAL
        .get()
        .map(|s| s.status_json())
        .unwrap_or_else(|| "null".to_string())
}

/// 起動時に読み戻した内容。
pub struct Loaded {
    pub created: bool,
    pub history: [Vec<Sample>; 3],
    pub hosts: Vec<(String, HostStats)>,
    pub clients: Vec<(String, HostStats)>,
    pub size: u64,
}

/// 状態ファイルを開いて `metrics` に読み戻し、定期的な書き出しスレッドを起動する。
/// 失敗したら警告して `None` (永続化なしで動く)。
pub fn start(path: PathBuf, metrics: &Arc<Metrics>) -> Option<(Arc<Store>, JoinHandle<()>)> {
    let (store, loaded) = match Store::open(path.clone()) {
        Ok(v) => v,
        Err(e) => {
            log_warn!(
                None,
                "state file {}: {} (statistics will not survive restarts)",
                path.display(),
                e
            );
            return None;
        }
    };
    let [fine, minute, hour] = loaded.history;
    let counts = (fine.len(), minute.len(), hour.len());
    metrics.history.restore(0, fine);
    metrics.history.restore(1, minute);
    metrics.history.restore(2, hour);
    let (nh, nc) = (loaded.hosts.len(), loaded.clients.len());
    metrics.restore(loaded.hosts, loaded.clients);
    log_info!(
        None,
        "state file {} ({} KiB, {}): history {}/{}/{} samples, {} hosts, {} clients restored",
        store.path.display(),
        loaded.size / 1024,
        if loaded.created { "created" } else { "opened" },
        counts.0,
        counts.1,
        counts.2,
        nh,
        nc
    );
    let _ = GLOBAL.set(Arc::clone(&store));
    let st = Arc::clone(&store);
    let m = Arc::clone(metrics);
    let handle = thread::Builder::new()
        .name("persist".into())
        .spawn(move || {
            loop {
                thread::sleep(FLUSH_INTERVAL);
                st.flush_stats(&m);
            }
        })
        .ok()?;
    Some((store, handle))
}
