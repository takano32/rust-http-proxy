//! 日次と起動・停止の 1 枚を書くもの ([`Writer`]。`snapshots` の一部。T34.2 で `snapshots.rs`
//! から移した)。
//!
//! `/snapshot` を組む閉包と、書き先の空きを測る閉包は**引数で受ける** ([`Writer::add`] /
//! [`Writer::write_moment`]) ので、単体試験は大域に触らずディスクの予算の判定 ([`fits`]) まで
//! 動かせる。置き場所の一覧と間引きは `store.rs`。
//!
//! 行を動かしただけで中身は変えていない。

use std::fs;
use std::path::{Path, PathBuf};

use super::names::{Moment, day_name, moment_name};
use super::store::{newest_day, prune, prune_moments};
use super::{DAILY_MAX_FILE, EXT, MAX_FILE, MAX_KEPT_DAYS};
use crate::day_rollover::DayRollover;
use crate::{log_debug, log_info, log_warn};

// `Writer` は外に使い手が無いが `pub` のまま: `pub(crate)` に狭めると、試験だけが読む
// `counts` / `moments_written` (とその欄) が dead_code になる (T34.2)

/// 日次の `/snapshot` を書くもの。
///
/// 大域の状態 ([`configure`] / [`tick`]) はこれを 1 つ持っているだけで、中身は全部ここにある
/// (単体テストは大域に触らずこれを直に動かせる。T14.20 の `daily::Writer` と同じ作り)。
pub struct Writer {
    dir: Option<PathBuf>,
    /// 残す日数 (`PROXY_SNAPSHOT_DAYS`。0 なら書かない)
    days: usize,
    /// いま見ている日と最後に書いた日 (起動時にいちばん新しいファイルの日付から。T34.50)
    rollover: DayRollover,
    written: u64,
    errors: u64,
    /// ディスクの予算で書かなかった回数
    skipped: u64,
    /// 起動と停止の 1 枚を書いた数 (T28.20)
    moments: u64,
}

impl Writer {
    /// 置き場所と日数を決める。**いちばん新しいファイルの日付を読む**
    /// (同じ日を 2 度書かないため)。
    pub fn new(dir: Option<PathBuf>, days: usize) -> Writer {
        let days = days.min(MAX_KEPT_DAYS);
        let dir = dir.filter(|_| days > 0);
        let last_written = dir.as_deref().map(newest_day).unwrap_or(0);
        Writer {
            dir,
            days,
            rollover: DayRollover::new(last_written),
            written: 0,
            errors: 0,
            skipped: 0,
            moments: 0,
        }
    }

    /// 履歴の標本の時刻を 1 つ見る。**日が変わっていたら終わった日の 1 ファイルを書く**。
    ///
    /// `build` (`/snapshot` を組む) と `space` (書き先の空きとマージンを測る) は
    /// **日付の変わり目のときだけ**呼ぶ (静かな日は 5 秒ごとに割り算 1 回で戻る)。
    /// 戻り値は書いたファイル。
    pub fn add(
        &mut self,
        t: u64,
        build: impl FnOnce() -> Option<String>,
        space: impl FnOnce(&Path) -> (Option<u64>, u64),
    ) -> Option<PathBuf> {
        let dir = self.dir.clone()?;
        // 起動して最初の標本はその日を覚えるだけ (この日が終わるまでは何も書かない)
        let finished = self.rollover.cross(t)?;
        if self.rollover.written(finished) {
            // その日のファイルが既にある (同じ日に 2 回起動した / 別のプロセスが書いた)
            log_debug!(
                None,
                "daily snapshot {}: {} is already there, not writing it twice",
                dir.display(),
                day_name(finished)
            );
            return None;
        }
        let name = day_name(finished);
        let Some(body) = build() else {
            // `/snapshot` の組み方を預かっていない (本番では起動時に預かる)
            log_debug!(
                None,
                "daily snapshot {}: no snapshot builder",
                dir.display()
            );
            return None;
        };
        let path = dir.join(format!("{}{}", name, EXT));
        if !self.put(&dir, &path, &body, DAILY_MAX_FILE, space) {
            return None;
        }
        self.rollover.mark_written(finished);
        self.written += 1;
        let dropped = prune(&dir, self.days);
        log_info!(
            None,
            "daily snapshot {} ({} B, keeping {} days{})",
            path.display(),
            body.len(),
            self.days,
            if dropped > 0 {
                format!(", dropped {} older", dropped)
            } else {
                String::new()
            }
        );
        Some(path)
    }

    /// 起動と停止の 1 枚 (T28.20) を `t` (撮った時刻) の名前で書く。戻り値は書いたファイル。
    ///
    /// 書いたら同じ種類の古いもの ([`prune_moments`]) を消す。書き先が無い設定
    /// (`PROXY_SNAPSHOT_DAYS=0` / `PROXY_STATS_PERSIST=off`) では `build` を呼ばない。
    pub fn write_moment(
        &mut self,
        t: u64,
        m: Moment,
        build: impl FnOnce() -> Option<String>,
        space: impl FnOnce(&Path) -> (Option<u64>, u64),
    ) -> Option<PathBuf> {
        let dir = self.dir.clone()?;
        let Some(body) = build() else {
            log_debug!(None, "{} snapshot: no snapshot builder", m.name());
            return None;
        };
        let path = dir.join(format!("{}{}", moment_name(t, m), EXT));
        if !self.put(&dir, &path, &body, MAX_FILE, space) {
            return None;
        }
        self.moments += 1;
        let dropped = prune_moments(&dir, self.days, m, t);
        log_info!(
            None,
            "{} snapshot {} ({} B, keeping {} days{})",
            m.name(),
            path.display(),
            body.len(),
            self.days,
            if dropped > 0 {
                format!(", dropped {} older", dropped)
            } else {
                String::new()
            }
        );
        Some(path)
    }

    /// 大きさとディスクの予算を確かめてから 1 ファイル書く (日次と起動・停止で共通)。
    fn put(
        &mut self,
        dir: &Path,
        path: &Path,
        body: &str,
        limit: u64,
        space: impl FnOnce(&Path) -> (Option<u64>, u64),
    ) -> bool {
        let need = body.len() as u64;
        if need > limit {
            // `/snapshot` 側が同じ上限で切っているので本来ここへは来ない (念のための歯止め)
            self.note_skip(path, &format!("{} B is over the {} B limit", need, limit));
            return false;
        }
        let (free, keep_free) = space(dir);
        if !fits(free, keep_free, need) {
            self.note_skip(
                path,
                &format!(
                    "only {} free and {} must stay free (needs {} B)",
                    free.map(mib).unwrap_or_else(|| "?".into()),
                    mib(keep_free),
                    need
                ),
            );
            return false;
        }
        match write_file(path, body) {
            Ok(()) => true,
            Err(e) => {
                self.errors += 1;
                if self.errors == 1 {
                    log_warn!(None, "snapshot {}: write failed: {}", path.display(), e);
                    // 出来事の時系列に 1 件 (**最初の 1 回だけ**。T14.11 と同じ口)
                    crate::events::note_state_file(path, "write", &e);
                }
                false
            }
        }
    }

    /// 書かずに終わったことを `/events` に 1 件残す (`state_file` の種類。T14.11)。
    fn note_skip(&mut self, path: &Path, why: &str) {
        self.skipped += 1;
        log_warn!(
            None,
            "daily snapshot {}: not written: {}",
            path.display(),
            why
        );
        crate::events::push(
            crate::events::EventKind::StateFile,
            &format!("{}: not written: {}", path.display(), why),
        );
    }

    /// 書いた数・書けなかった数・予算で見送った数。
    pub fn counts(&self) -> (u64, u64, u64) {
        (self.written, self.errors, self.skipped)
    }

    /// 起動と停止の 1 枚を書いた数 (T28.20)。
    pub fn moments_written(&self) -> u64 {
        self.moments
    }

    /// 書き先 (`PROXY_SNAPSHOT_DAYS=0` と `PROXY_STATS_PERSIST=off` では `None`)。
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// 残す日数 (`PROXY_SNAPSHOT_DAYS`。[`MAX_KEPT_DAYS`] で頭打ち。`/snapshots` の `days`)。
    pub fn days(&self) -> usize {
        self.days
    }
}

/// 書いてよいか (ディスクの予算。T12.5 の `keep_free` を割り込まない)。
///
/// 空きが測れない環境 (`statvfs` の無い OS) では**今までどおり書く**
/// (キャッシュの予算が `FALLBACK_DISK` に倒すのと同じ方針)。
pub(super) fn fits(free: Option<u64>, keep_free: u64, need: u64) -> bool {
    match free {
        None => true,
        Some(free) => free >= need.saturating_add(keep_free),
    }
}

fn mib(b: u64) -> String {
    format!("{} MiB", b / (1024 * 1024))
}

/// 一時ファイルに書いてから置き換える (読む側に途中の JSON を見せない)。
fn write_file(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, body.as_bytes())?;
    fs::rename(&tmp, path)
}
