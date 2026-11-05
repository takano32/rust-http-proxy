//! 日次の要約を積んで、日が変わったら 1 行書くもの ([`Writer`])。

use std::path::{Path, PathBuf};

use crate::history::Sample;
use crate::{log_debug, log_info, log_warn};

use super::SECS_PER_DAY;
use super::SEED_RES;
use super::acc::{Acc, Cum};
use super::file::{append, last_day};
use super::versions::{clean_version, versions_of_day};
use crate::day_rollover::DayRollover;

/// 日次の要約を積んで、日が変わったら 1 行書くもの。
///
/// 大域の状態 ([`configure`] / [`tick`]) はこれを 1 つ持っているだけで、
/// 中身は全部ここにある (単体テストは大域に触らずこれを直に動かせる)。
pub struct Writer {
    path: Option<PathBuf>,
    version: String,
    /// いま見ている日と、最後にファイルへ書いた日 (起動時に最後の行から読む。T.34.50)
    rollover: DayRollover,
    acc: Option<Acc>,
    /// この起動で書いた行数と、書けなかった回数
    written: u64,
    errors: u64,
    /// 起動して最初の標本で組み直したか (T.32.18。1 回だけ)
    seeded: bool,
}

impl Writer {
    /// 書く場所と版を決める。**ファイルの最後の行の日付を読む** (同じ日を 2 度書かないため)。
    pub fn new(path: Option<PathBuf>, version: &str) -> Writer {
        let last_written = path.as_deref().map(last_day).unwrap_or(0);
        Writer {
            path,
            version: clean_version(version),
            rollover: DayRollover::new(last_written),
            acc: None,
            written: 0,
            errors: 0,
            seeded: false,
        }
    }

    /// 起動して最初の標本 `first` の前に、読み戻した `history["60"]` の**今日の UTC 0 時から**の行で
    /// 今日の積み上げを組み直す (T.32.18)。1 回だけ (2 回目からは何もしない)。
    ///
    /// 使う行は `0 時 <= t < first.t - 5` (`first` が持つ 5 秒の手前まで)。60 秒の行の `t` は窓の頭で、
    /// 起動してから最初の行が閉じるのは 1 分あと = 組み直す時点の行は全部前のプロセスのもの。
    /// `events` は `/events` の中身 (版の並びを作る)。行が 1 本も無ければ今までどおり (今の版だけ)。
    pub fn seed(
        &mut self,
        h: &crate::history::History,
        first: &Sample,
        bursts: u64,
        events: &[crate::events::Event],
    ) {
        if std::mem::replace(&mut self.seeded, true) || self.path.is_none() || self.acc.is_some() {
            return;
        }
        let day = first.t / SECS_PER_DAY;
        let day_start = day * SECS_PER_DAY;
        // 今のプロセスの累計の起点は最初の標本 (`add` の `None` の枝と同じ)
        let mut acc = Acc::start_at(day, first.t, Cum::take(first, bursts));
        let mut prev_evicted = None;
        let mut first_row = None;
        h.for_each_in(SEED_RES, day_start, first.t.saturating_sub(5), |s| {
            first_row.get_or_insert(s.t);
            acc.add_seed(s, prev_evicted);
            prev_evicted = Some(s.evicted_idle);
        });
        let Some(first_row) = first_row else {
            return;
        };
        // 0 時に動いていた版を入れるのは、組み直した行が今日の最初の `start` より前からあるとき
        let first_start = events
            .iter()
            .filter(|e| e.kind == crate::events::EventKind::Start && e.at >= day_start)
            .map(|e| e.at)
            .min();
        let with_midnight = first_start.is_none_or(|at| first_row < at);
        acc.versions = Some(versions_of_day(
            events,
            day_start,
            first.t,
            with_midnight,
            &self.version,
        ));
        log_info!(
            None,
            "daily summary: rebuilt today from {} s of restored history (versions {})",
            acc.seed_secs,
            acc.versions.as_deref().unwrap_or("")
        );
        self.acc = Some(acc);
    }

    /// 履歴の標本を 1 本足す。日が変わっていたら終わった日の 1 行を書く。
    ///
    /// `bursts` は山の写真の通算 (`BurstRing::next_seq() - 1`)。
    pub fn add(&mut self, s: &Sample, bursts: u64) {
        if self.path.is_none() {
            return;
        }
        let day = s.t / SECS_PER_DAY;
        let cum = Cum::take(s, bursts);
        let crossed = self.rollover.cross(s.t);
        let finished = match (self.acc.as_mut(), crossed) {
            (Some(acc), None) => {
                acc.add(s, cum);
                None
            }
            (Some(acc), Some(_)) => {
                // 境目の標本の窓 (5 秒) はほとんどが前の日なので、**終わる日に足す**。
                // 累計の差もここで閉じるので、次の日は同じ値から数え始める
                acc.add(s, cum);
                self.acc.replace(Acc::start_at(day, s.t, cum))
            }
            (None, _) => {
                let mut acc = Acc::start_at(day, s.t, cum);
                acc.add(s, cum);
                self.acc = Some(acc);
                None
            }
        };
        if let Some(done) = finished {
            self.finish(&done);
        }
    }

    fn finish(&mut self, done: &Acc) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if self.rollover.written(done.day) {
            // 同じ日の行が既にある (同じ日に 2 回起動した / 別のプロセスが書いた)
            log_debug!(
                None,
                "daily summary {}: day {} is already there, not writing it twice",
                path.display(),
                done.day
            );
            return;
        }
        // `/slo` の比を 1 時間の合計で判定した時間の数 (T.29.33)。集計はメモリの大域にある
        let rate_hours = crate::slo::rate_hours_of_day(done.day.saturating_mul(SECS_PER_DAY));
        let line = done.line(&self.version, rate_hours);
        match append(&path, &line) {
            Ok(()) => {
                self.rollover.mark_written(done.day);
                self.written += 1;
                log_info!(None, "daily summary {}: {}", path.display(), line);
            }
            Err(e) => {
                self.errors += 1;
                if self.errors == 1 {
                    log_warn!(
                        None,
                        "daily summary {}: write failed: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
    }

    /// 書く場所 (`PROXY_STATS_PERSIST=off` なら `None`)。
    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// 起動して最初の標本で組み直したか (T.32.18)。
    pub(crate) fn seeded(&self) -> bool {
        self.seeded
    }

    /// 書いた行数 (この起動で) と、書けなかった回数。
    pub fn counts(&self) -> (u64, u64) {
        (self.written, self.errors)
    }
}
