//! 統計と履歴を固定サイズの状態ファイル ([`crate::rrd`]) に写し、起動時に読み戻す。
//!
//! - 履歴: 5 秒 / 1 分 / 1 時間の各リングへ、標本ができるたびに 1 レコード書く
//! - ホスト別・接続元別: [`FLUSH_INTERVAL`] ごとに上位 1000 を固定スロットへ書き直す
//! - ブロックリストの上書き: 変更のたびにそのスロットだけ書く ([`crate::blocklist`] から)
//! - **heap の基準**: 起動から [`BASELINE_AFTER_SECS`] 秒たった時点の
//!   `heap_used + mmap − cache_memory` を**1 回だけ**書く (T24.4)。`/status` の
//!   `memory.baseline` に出るので、後の雪像 1 枚で「この起動でどれだけ増えたか」が読める
//!   (0 時間の雪像が要らない = ホストの管理者に再起動されても heap を判定できる)
//!
//! 置き場所は `$HOME/.rust-http-proxy.rrd` (Pterodactyl で永続するのはそこだけ)。
//! `PROXY_STATS_PERSIST=off` で無効。大きさは **8 MiB 固定** (版 3。T14.14) で、
//! 以後は伸びない。**版 2 のファイルは起動時に 1 回だけ版 3 の形に詰め直す**ので、
//! 通算の統計は版を上げても消えない (`/status` の `state_file.converted_from`)。
//!
//! **個票 (`/recent` `/errors` `/bursts` `/log`) は隣の別のファイル**
//! `$HOME/.rust-http-proxy.recent` に落とす ([`crate::persist_recent`]。T14.9)。
//! 同じ `Store` が両方を持ち、history スレッドの 5 秒の周期で一緒に書く
//! (`.rrd` の版を上げずに個票を残せるようにするため、ファイルを分けてある)。

use crate::sync::LockExt;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::cache::Cache;
use crate::history::{Pushed, Sample};
use crate::metrics::{HostStats, Metrics};
use crate::persist_recent::{RecentFile, Restored};
use crate::rrd::ring::Ring;
use crate::rrd::{Enc, Opened, Region, Rrd};
use crate::{log_info, log_warn};

/// 統計表を書き直す間隔。
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(60);

/// 起動からこの秒数たった時点の heap を「基準」として 1 回だけ残す (T24.4)。
///
/// 60 秒にしたのは、起動の山 (状態ファイルの読み戻し・リングの確保・canary の 1 巡目・
/// 名前解決の warm) が済んだあとの値で、**その起動のあいだ動かない基準**になるから。
/// 短すぎると起動の途中の値を拾い、長すぎるとその間の増えが基準に混ざる。
pub const BASELINE_AFTER_SECS: u64 = 60;

/// 実際に使う秒数 (**試験が短い周期で確かめられるように差し替えられる**)。
static BASELINE_AFTER: AtomicU64 = AtomicU64::new(BASELINE_AFTER_SECS);

/// 基準を取るまでの秒数を差し替える (T24.4 の試験。既定は [`BASELINE_AFTER_SECS`])。
pub fn set_baseline_after_secs(secs: u64) {
    BASELINE_AFTER.store(secs, Ordering::Relaxed);
}

/// いまの「基準を取るまで」の秒数。
pub fn baseline_after_secs() -> u64 {
    BASELINE_AFTER.load(Ordering::Relaxed)
}

/// `heap_used + mmap − cache_memory` (heap の行が比べる値。T20.1 / T24.4)。
///
/// `mallinfo2(3)` が読めない環境 (musl / glibc 2.32 以下 / Linux 以外) では `None`。
/// **`/status` の `memory` と同じ引き算**であること (片方だけ直すと行が読めなくなる)。
pub fn heap_bytes(cache: &Cache) -> Option<u64> {
    let h = crate::sysinfo::malloc_info()?;
    let cache_memory = cache.mem_usage().0.saturating_add(cache.mem_reserved());
    Some(h.used.saturating_add(h.mmap).saturating_sub(cache_memory))
}

pub struct Store {
    rrd: Rrd,
    pub path: PathBuf,
    rings: Mutex<[Ring; 3]>,
    /// 書込エラーの回数 (連発しないよう最初だけ警告する)
    write_errors: AtomicU64,
    flushes: AtomicU64,
    /// 開いたときに分かったこと (版と、版 2 から変換したか。T14.14)
    opened: Opened,
    /// 個票のファイル (`$HOME/.rust-http-proxy.recent`。T14.9)。開けなければ `None`
    recent: Option<RecentFile>,
    /// **この起動で取った** heap の基準 (`at` = 取った時刻 (epoch 秒), `bytes`)。T24.4。
    /// 前の起動が状態ファイルに残した値は読み戻さない (別の起動の値なので使えない)
    baseline: Mutex<Option<(u64, u64)>>,
    /// ホスト別・接続元別の表の、スロットごとに最後に書いた中身 (`[hosts, clients]`。T.32.22)
    tables: Mutex<[Slots; 2]>,
    /// 表のレコードを書いた・消した回数 (試験と `/status` の説明用。T.32.22)
    table_writes: AtomicU64,
}

/// 表の 1 領域の「どのスロットに誰を、どんな中身で書いたか」(T.32.22)。
///
/// 60 秒ごとの書き直しは上位 1,000 行を 1 行ずつ `pwrite` していた (1 分に 1,001 回、
/// 1 回の flush で 3.75 ms)。**名前ごとにスロットを固定し、中身の hash が前回と同じ行は
/// 書かない**。並びの順位が入れ替わっても、名前のスロットは動かさない (読み戻しは
/// 並びを見ないので、どのスロットに居てもよい)。上位から外れた名前のスロットは、
/// 新しく入った名前が使い、余ったぶんだけ消す。
#[derive(Default)]
struct Slots {
    /// スロット → (名前, 中身の hash)。空なら `None`
    slots: Vec<Option<(String, u64)>>,
    /// 名前 → スロット
    by_name: std::collections::HashMap<String, usize>,
}

impl Slots {
    /// 読み戻した中身から作る (起動直後の 1 回目の書き直しも、変わった行だけにする)。
    fn from_loaded(count: usize, recs: &[(usize, Vec<u8>)]) -> Slots {
        let mut t = Slots {
            slots: vec![None; count],
            by_name: std::collections::HashMap::new(),
        };
        for (idx, payload) in recs {
            if *idx >= count {
                continue;
            }
            let Some((name, _)) = HostStats::decode(payload) else {
                continue;
            };
            // 同じ名前が 2 つあれば (壊れた書き直しの名残)、後のほうは空き扱い
            if t.by_name.contains_key(&name) {
                continue;
            }
            t.by_name.insert(name.clone(), *idx);
            t.slots[*idx] = Some((name, payload_hash(payload)));
        }
        t
    }
}

/// レコードの中身の hash。読み戻した中身はスロットの大きさまでゼロで埋まっているので、
/// **末尾のゼロを除いて**比べる (書く前の `encode` の結果と同じ値になるように)。
///
/// 1 分に 1,000 行 × 約 600 B を通すので、SipHash (約 340 ns/行) ではなく 8 バイトずつ
/// 混ぜる軽いもの (約 30 ns/行)。比べるのは同じスロットの前回の中身とだけで、外れると
/// 「変わったのに書かない」になるが、64 ビットで偶然に揃う見込みは無視できる
fn payload_hash(payload: &[u8]) -> u64 {
    let end = payload.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ end as u64;
    let mut mix = |w: u64| {
        h = (h ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        h ^= h >> 29;
    };
    let mut words = payload[..end].chunks_exact(8);
    for c in &mut words {
        mix(u64::from_le_bytes(c.try_into().unwrap_or([0; 8])));
    }
    let rest = words.remainder();
    if !rest.is_empty() {
        let mut w = [0u8; 8];
        w[..rest.len()].copy_from_slice(rest);
        mix(u64::from_le_bytes(w));
    }
    h
}

/// 書き直しの 1 行: (いまのスロット (無ければ `None`), 名前, 中身, 中身の hash)。
type Changed = (Option<usize>, String, Vec<u8>, u64);

impl Store {
    /// 既定の場所 (`$HOME/.rust-http-proxy.rrd`)。
    pub fn default_path() -> Option<PathBuf> {
        crate::envfile::env_path().map(|p| p.with_file_name(".rust-http-proxy.rrd"))
    }

    /// 個票のファイル (`/status` と結合テストが場所を知るため)。
    pub fn recent_path(&self) -> PathBuf {
        crate::persist_recent::path_next_to(&self.path)
    }

    /// 個票を永続化しているか (`/recent` などの `"persisted"`)。
    pub fn recent_enabled(&self) -> bool {
        self.recent.is_some()
    }

    /// まだ書いていない個票をファイルへ追記する (**history スレッドの 5 秒の周期と、
    /// 停止シグナルの最後の 1 回**。T14.9)。戻り値は書いたレコード数。
    pub fn write_recent(&self, metrics: &Metrics) -> usize {
        // 記録の一括 off (T14.41)。メモリのリングは既に空なので書くものは無いが、
        // `on` → `off` に切り替えた瞬間にまだ書いていない件が残りうるのでここでも止める
        // (`off` の間は**ファイルに 1 バイトも足さない**)
        if !crate::records::recording() {
            return 0;
        }
        self.recent
            .as_ref()
            .map(|r| r.write_new(metrics).total())
            .unwrap_or(0)
    }

    /// 開き (無ければ作り)、履歴のリングと個票を読み戻す。
    pub fn open(path: PathBuf) -> io::Result<(Arc<Store>, Loaded)> {
        let (rrd, opened) = Rrd::open(&path)?;
        let l = rrd.layout;
        let (fine, fine_recs) = Ring::load(&rrd, l.history_fine)?;
        let (minute, minute_recs) = Ring::load(&rrd, l.history_minute)?;
        let (hour, hour_recs) = Ring::load(&rrd, l.history_hour)?;
        let decode = |recs: Vec<Vec<u8>>| -> Vec<Sample> {
            recs.iter().filter_map(|p| Sample::decode(p)).collect()
        };
        // 表は 1 回だけ読み、中身と「どのスロットに誰が居るか」(T.32.22) の両方に使う
        let host_recs = rrd.read_all(l.hosts)?;
        let client_recs = rrd.read_all(l.clients)?;
        let stats = |recs: &[(usize, Vec<u8>)]| -> Vec<(String, HostStats)> {
            recs.iter()
                .filter_map(|(_, p)| HostStats::decode(p))
                .collect()
        };
        let tables = [
            Slots::from_loaded(l.hosts.count, &host_recs),
            Slots::from_loaded(l.clients.count, &client_recs),
        ];
        // 個票は隣の別のファイル (T14.9)。開けなくても統計はそのまま動く
        let (recent, recent_loaded) = match crate::persist_recent::open_beside(&path) {
            Some((f, r)) => (Some(f), Some(r)),
            None => (None, None),
        };
        let loaded = Loaded {
            opened,
            history: [decode(fine_recs), decode(minute_recs), decode(hour_recs)],
            hosts: stats(&host_recs),
            clients: stats(&client_recs),
            size: l.total,
            recent: recent_loaded,
        };
        // 基準は**この起動のぶんだけ** (T24.4)。状態ファイルに前の起動が残した値は
        // 読み戻さない (別の起動の値なので、増えの基準にはできない)
        let store = Arc::new(Store {
            rrd,
            path,
            rings: Mutex::new([fine, minute, hour]),
            write_errors: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            opened,
            recent,
            baseline: Mutex::new(None),
            tables: Mutex::new(tables),
            table_writes: AtomicU64::new(0),
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
                // 出来事の時系列にも最初の 1 回だけ (T14.11)
                crate::events::note_state_file(&self.path, what, &e);
            }
        }
    }

    /// 履歴の標本を各リングへ書く。
    pub fn write_samples(&self, pushed: &Pushed) {
        let mut rings = self.rings.locked();
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
        let mut tables = self.tables.locked();
        let [hosts, clients] = &mut *tables;
        // ホスト別は統計の鍵の内側で、写さず並べずに上位を見る (T.32.22)
        self.write_table(l.hosts, hosts, |f| {
            metrics.visit_top_hosts(l.hosts.count, f)
        });
        // 接続元別は記録の形から直す必要があるので、今までどおり並べた写しから
        let rows = metrics.clients_sorted();
        self.write_table(l.clients, clients, |f| {
            for (name, s) in rows.iter().take(l.clients.count) {
                f(name, s);
            }
        });
        drop(tables);
        self.flushes.fetch_add(1, Ordering::Relaxed);
    }

    /// 表のレコードを書いた・消した回数の累計 (T.32.22)。
    pub fn table_writes(&self) -> u64 {
        self.table_writes.load(Ordering::Relaxed)
    }

    /// 上位 `region.count` 行を書く。**中身が前回と同じ行は書かない** ([`Slots`]。T.32.22)。
    ///
    /// `visit` は上位の行を順不同で 1 回ずつ渡す。渡された中では中身を組んで hash を比べる
    /// だけで、書く (`pwrite`) のは `visit` を抜けたあと (統計の鍵を握ったまま書かない)。
    fn write_table(
        &self,
        region: Region,
        t: &mut Slots,
        visit: impl FnOnce(&mut dyn FnMut(&str, &HostStats)),
    ) {
        if t.slots.len() != region.count {
            *t = Slots {
                slots: vec![None; region.count],
                by_name: std::collections::HashMap::new(),
            };
        }
        let mut seen = vec![false; region.count];
        let mut changed: Vec<Changed> = Vec::new();
        let mut visited = 0usize;
        visit(&mut |name: &str, s: &HostStats| {
            if visited >= region.count {
                return;
            }
            visited += 1;
            let payload = s.encode(name);
            let h = payload_hash(&payload);
            match t.by_name.get(name) {
                Some(&i) => {
                    seen[i] = true;
                    if t.slots[i].as_ref().is_none_or(|(_, old)| *old != h) {
                        changed.push((Some(i), name.to_owned(), payload, h));
                    }
                }
                None => changed.push((None, name.to_owned(), payload, h)),
            }
        });
        // 上位から外れた名前のスロットを空ける (新しく入った名前が上書きするか、最後に消す)。
        // 空きは若い番号から使う (どのスロットでも読み戻せるが、ファイルの前に寄せておく)
        let mut free: Vec<usize> = Vec::new();
        for (i, slot) in t.slots.iter().enumerate().rev() {
            match slot {
                None => free.push(i),
                Some((name, _)) if !seen[i] => {
                    t.by_name.remove(name);
                    free.push(i);
                }
                Some(_) => {}
            }
        }
        let mut writes = 0u64;
        for (at, name, payload, h) in changed {
            let Some(idx) = at.or_else(|| free.pop()) else {
                // 上位 `count` 行に絞ってあるので空きは必ず足りる
                continue;
            };
            writes += 1;
            let r = self.rrd.write(region, idx, &payload);
            // 書けなかったスロットは中身が分からないので、次の回に書き直す (hash 0)
            let h = if r.is_ok() { h } else { 0 };
            self.note("stats write", r);
            t.by_name.insert(name.clone(), idx);
            t.slots[idx] = Some((name, h));
        }
        // 外れた名前のスロットで、誰も上書きしなかったものを消す
        for i in free {
            if t.slots[i]
                .as_ref()
                .is_some_and(|(n, _)| t.by_name.get(n) != Some(&i))
            {
                writes += 1;
                let r = self.rrd.clear(region, i);
                if r.is_ok() {
                    t.slots[i] = None;
                }
                self.note("stats clear", r);
            }
        }
        self.table_writes.fetch_add(writes, Ordering::Relaxed);
    }

    /// 起動から [`baseline_after_secs`] 秒たったら、heap の基準を**1 回だけ**書く (T24.4)。
    ///
    /// `uptime` は起動からの経過 (呼ぶ側 = 5 秒の記録スレッドが持っている)。戻り値は
    /// 「今回書いたか」。**この起動で 1 回だけ**で、2 回目からは何もしない。前の起動が
    /// 状態ファイルに残した値は読み戻さないので、起動し直すと (管理者の再起動でも) 60 秒後に
    /// 取り直した値がその起動の基準になる。`mallinfo2(3)` が読めない環境 (musl / glibc 2.32 以下 /
    /// Linux 以外) では書かない = `/status` は `baseline: null` のまま。
    pub fn write_baseline_if_due(&self, cache: &Cache, uptime: Duration) -> bool {
        if uptime.as_secs() < baseline_after_secs() || self.baseline.locked().is_some() {
            return false;
        }
        let Some(bytes) = heap_bytes(cache) else {
            return false;
        };
        let at = crate::cache::now_epoch();
        let mut enc = Enc::new();
        enc.u64(at).u64(bytes);
        self.note(
            "baseline write",
            self.rrd.write(self.rrd.layout.baseline, 0, &enc.0),
        );
        *self.baseline.locked() = Some((at, bytes));
        log_info!(
            None,
            "heap baseline at {} s after start: {} bytes",
            uptime.as_secs(),
            bytes
        );
        true
    }

    /// この起動の heap の基準 (`at`, `bytes`)。まだ取っていなければ `None` (T24.4)。
    pub fn baseline(&self) -> Option<(u64, u64)> {
        *self.baseline.locked()
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

    /// `/status` の `"state_file"` 要素。`"version"` は保存形式の版、
    /// `"converted_from"` は**この起動で詰め直した元の版** (していなければ `null`。
    /// T14.14)。個票のファイルは `"recent"` に入れ子で出す (T14.9)。
    pub fn status_json(&self) -> String {
        format!(
            "{{\"path\":{},\"bytes\":{},\"version\":{},\"converted_from\":{},\"flushes\":{},\"write_errors\":{},\"recent\":{}}}",
            crate::json::quote(&self.path.display().to_string()),
            self.rrd.layout.total,
            crate::rrd::VERSION,
            self.opened
                .converted_from
                .map(|v| v.to_string())
                .unwrap_or_else(|| "null".to_string()),
            self.flushes.load(Ordering::Relaxed),
            self.write_errors.load(Ordering::Relaxed),
            self.recent
                .as_ref()
                .map(|r| r.status_json())
                .unwrap_or_else(|| "null".to_string())
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

/// 書込エラーの累計 (`/healthz` の検査。永続化していなければ `None` = 検査しない。T14.12)。
pub fn write_errors() -> Option<u64> {
    GLOBAL.get().map(|s| s.write_errors.load(Ordering::Relaxed))
}

/// この起動の heap の基準 (`at` = 取った epoch 秒、`bytes` = そのときの heap)。T24.4。
///
/// **この起動の基準だけ**を返す。まだ 60 秒たっていない (起動したばかり)・`mallinfo2` が読めない・
/// 永続化していない (`PROXY_STATS_PERSIST=off` / `--lite`)、のどれかなら `None`。
/// `/status` の `memory.baseline` (下の [`baseline_json`]) と `/metrics` の
/// `sorahost_heap_baseline_*` (T25.3) が**同じ値**を読むための口。
pub fn baseline() -> Option<(u64, u64)> {
    GLOBAL.get().and_then(|s| s.baseline())
}

/// `/status` の `memory.baseline` (`{"at":…,"bytes":…}`。T24.4)。
///
/// **この起動の基準だけ**を出す。まだ 60 秒たっていない (起動したばかり)・`mallinfo2` が読めない・
/// 永続化していない (`PROXY_STATS_PERSIST=off` / `--lite`)、のどれかなら `null`。
pub fn baseline_json() -> String {
    baseline()
        .map(|(at, bytes)| format!("{{\"at\":{},\"bytes\":{}}}", at, bytes))
        .unwrap_or_else(|| "null".to_string())
}

/// 起動時に読み戻した内容。
pub struct Loaded {
    /// 作り直したか・版 2 から詰め直したか (T14.14)
    pub opened: Opened,
    pub history: [Vec<Sample>; 3],
    pub hosts: Vec<(String, HostStats)>,
    pub clients: Vec<(String, HostStats)>,
    pub size: u64,
    /// 隣の個票のファイルから読み戻したもの (T14.9。開けなければ `None`)
    pub recent: Option<Restored>,
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
    let recent_loaded = loaded.recent;
    let [fine, minute, hour] = loaded.history;
    let counts = (fine.len(), minute.len(), hour.len());
    metrics.history.restore(0, fine);
    metrics.history.restore(1, minute);
    metrics.history.restore(2, hour);
    let (nh, nc) = (loaded.hosts.len(), loaded.clients.len());
    metrics.restore(loaded.hosts, loaded.clients);
    // 版 2 から詰め直したときは、その旨と所要 (起動時 1 回だけ) を残す (T14.14)
    let how = match loaded.opened.converted_from {
        Some(v) => format!(
            "converted from version {} in {} ms",
            v, loaded.opened.convert_ms
        ),
        None if loaded.opened.created => "created".to_string(),
        None => "opened".to_string(),
    };
    log_info!(
        None,
        "state file {} ({} KiB, {}): history {}/{}/{} samples, {} hosts, {} clients restored",
        store.path.display(),
        loaded.size / 1024,
        how,
        counts.0,
        counts.1,
        counts.2,
        nh,
        nc
    );
    // 個票を読み戻す (`/recent?since=` が再起動前に届くようにする。T14.9)
    if let (Some(file), Some(r)) = (store.recent.as_ref(), recent_loaded) {
        let created = r.created;
        let n = r.install(metrics);
        crate::persist_recent::log_restored(file, created, n);
        metrics
            .recent_persisted
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let _ = GLOBAL.set(Arc::clone(&store));
    // 書込エラーの数は `/status` の `kernel.state_file` にも出る。下の層 (`kernel`) からは
    // ここを呼べないので、開いたこの 1 回だけ読み口を預ける (T14.55)
    crate::kernel::set_state_file_errors(write_errors);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 試験用の状態ファイル (プロセスごとに別の名前)。
    fn tmp(name: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("shp-baseline-{}-{}.rrd", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(crate::persist_recent::path_next_to(&p));
        p
    }

    fn cache() -> Cache {
        Cache::new(crate::cache::CacheConfig::disabled())
    }

    /// `n` 件のホストを持つ統計 (要求数は `i + 1`、`bump` に入った番号だけ 1,000,000 足す)。
    fn hosts(n: usize, bump: &[usize]) -> Metrics {
        let m = Metrics::new();
        let rows = (0..n)
            .map(|i| {
                let s = HostStats {
                    requests: i as u64 + 1 + if bump.contains(&i) { 1_000_000 } else { 0 },
                    last_seen: 1_700_000_000 + i as u64,
                    ..HostStats::default()
                };
                (format!("host{i}.example"), s)
            })
            .collect();
        m.restore(rows, Vec::new());
        m
    }

    /// 60 秒ごとの表の書き直しは**変わった行だけ** (T.32.22)。1,200 ホストのうち 10 件が動いた回は
    /// 10 回 + 上位 1,000 の出入りのぶんだけ書き、再起動で読み戻す中身は今と同じ。
    #[test]
    fn the_stats_table_rewrites_only_the_rows_that_changed() {
        let path = tmp("tables");
        let (store, _) = Store::open(path.clone()).unwrap();
        let first = hosts(1200, &[]);
        store.flush_stats(&first);
        assert_eq!(store.table_writes(), 1000, "初回は上位 1,000 行を書く");
        // 何も変わらなければ 1 行も書かない
        store.flush_stats(&first);
        assert_eq!(store.table_writes(), 1000);
        // 上位 1,000 の中の 5 件と、外の 5 件 (199 番以下 = 上位から外れていた) が動いた:
        // 中の 5 件を書き直し、外の 5 件が入ったぶん 5 件が押し出される (5 スロットに上書き)
        let bumped = [1000usize, 1100, 1150, 1199, 1050, 0, 1, 2, 3, 4];
        let second = hosts(1200, &bumped);
        store.flush_stats(&second);
        let writes = store.table_writes() - 1000;
        assert!(writes <= 50, "1 分の pwrite が 50 回を越えた: {writes}");
        assert_eq!(
            writes, 10,
            "動いた 10 件だけ (押し出された 5 件のスロットは上書きで済む)"
        );
        drop(store);
        // 読み戻すと、いまの上位 1,000 がそのまま戻る (並びはスロットの順で、名前で比べる)
        let (store, loaded) = Store::open(path.clone()).unwrap();
        let mut got = loaded.hosts.clone();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        let mut want: Vec<_> = second.hosts_sorted().into_iter().take(1000).collect();
        want.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(got, want);
        // 読み戻した直後の書き直しも、変わっていなければ 1 行も書かない
        store.flush_stats(&second);
        assert_eq!(store.table_writes(), 0);
        // 減ったら余りを消す
        store.flush_stats(&hosts(10, &[]));
        drop(store);
        let (_, loaded) = Store::open(path.clone()).unwrap();
        assert_eq!(loaded.hosts.len(), 10);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(crate::persist_recent::path_next_to(&path));
    }

    /// 起動から 60 秒の基準が **1 回だけ**書かれ、次の起動では取り直されること (T24.4)。
    ///
    /// 状態ファイルの版は上げていないので、記録は「余白に 1 本」置かれるだけ。
    #[test]
    fn the_baseline_is_written_once_and_taken_again_after_a_restart() {
        let path = tmp("once");
        let cache = cache();
        if heap_bytes(&cache).is_none() {
            // `mallinfo2` が読めない環境 (musl / glibc 2.32 以下 / Linux 以外) では書かない
            let (store, _) = Store::open(path.clone()).unwrap();
            assert!(!store.write_baseline_if_due(&cache, Duration::from_secs(3600)));
            assert_eq!(store.baseline(), None);
            let _ = std::fs::remove_file(&path);
            return;
        }
        let (store, _) = Store::open(path.clone()).unwrap();
        assert_eq!(store.baseline(), None, "まだ取っていない");
        assert!(
            !store.write_baseline_if_due(&cache, Duration::from_secs(BASELINE_AFTER_SECS - 1)),
            "60 秒の手前では取らない"
        );
        assert_eq!(store.baseline(), None);
        assert!(
            store.write_baseline_if_due(&cache, Duration::from_secs(BASELINE_AFTER_SECS)),
            "60 秒で 1 回だけ取る"
        );
        let (at, bytes) = store.baseline().expect("この起動の基準");
        assert!(bytes > 0, "基準のバイト数が 0");
        assert!(at > 0);
        // 2 回目以降は書かない (値も動かない)
        assert!(!store.write_baseline_if_due(&cache, Duration::from_secs(3_600)));
        assert_eq!(store.baseline(), Some((at, bytes)), "1 回だけ");
        drop(store);

        // 次の起動: 状態ファイルには前の起動の値が残っているが、**この起動の基準は出さない**
        // (別の起動の値なので、増えの基準にはできない)。60 秒たったら取り直す
        let (store, _) = Store::open(path.clone()).unwrap();
        assert_eq!(store.baseline(), None, "まだ取っていない");
        let recs = store.rrd.read_all(store.rrd.layout.baseline).unwrap();
        assert_eq!(
            recs.len(),
            1,
            "記録そのものは状態ファイルに残っている (版は 3 のまま)"
        );
        let mut d = crate::rrd::Dec(&recs[0].1);
        assert_eq!((d.u64(), d.u64()), (at, bytes), "前の起動の値が残っている");
        std::thread::sleep(Duration::from_millis(1_100));
        assert!(store.write_baseline_if_due(&cache, Duration::from_secs(BASELINE_AFTER_SECS)));
        let (again, _) = store.baseline().expect("取り直した基準");
        assert!(again > at, "取り直した時刻は前より後 ({} > {})", again, at);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(crate::persist_recent::path_next_to(&path));
    }
}
