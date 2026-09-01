//! ディスク割当が分からないときに、Wings の挙動から割当を学習する。
//!
//! Pterodactyl は割当を教えてくれないが、超過すると即座にプロセスを止める (SIGTERM)。
//! そこで次の手順で「止められない上限」を探る:
//!
//! 1. 実データの上限 (`confirmed`) は最初 512 MiB。ここまで埋まったら 10 分ごとに `cap` を
//!    25% 上げ、増えた分は **バラスト (fallocate) だけ** で埋める。実データは `confirmed` まで
//! 2. 10 分止められなければ `confirmed = cap` (Wings の走査 150 秒が数回入る)
//! 3. 止められたら停止シグナルでバラストが切り詰められるので使用量は `confirmed` 以下に戻り、
//!    再起動できる。再起動時に「上げてから 10 分以内に途切れた」と分かれば `confirmed` を
//!    割当として記憶 (`learned`) し、7 日は探らない
//!
//! 普通の再起動と区別できないので、初期段階 (512 MiB) の途切れは 1 回では学習しない。
//! 起動から 10 分以内の途切れが 2 回続いたときだけ「割当が小さい」とみなして最低値に落とす。
//! 状態はボリューム内の小さなファイルに `key=value` で残す (キャッシュディレクトリの外)。

use std::fs;
use std::path::{Path, PathBuf};

use super::config::MIB;
use crate::{log_info, log_warn};

/// 最初の上限。
pub const START: u64 = 512 * MIB;
/// 1 段上げる割合 (5/4 = 25%) と最低増分。
const STEP_NUM: u64 = 5;
const STEP_DEN: u64 = 4;
const MIN_STEP: u64 = 64 * MIB;
/// これだけ止められなければ確認済みとみなす。
const CONFIRM_SECS: u64 = 600;
/// 生存記録の間隔。
const HEARTBEAT_SECS: u64 = 30;
/// 実データがこの割合まで埋まったら次を探る (%)。
const FULL_PERCENT: u64 = 90;
/// 学習した値を見直すまでの期間。
const RELEARN_SECS: u64 = 7 * 24 * 3600;
/// 最初の 512 MiB すら止められた場合の最低値。
const FLOOR: u64 = 64 * MIB;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskProbe {
    path: PathBuf,
    /// 実データの上限 (止められないと確認済み)
    confirmed: u64,
    /// バラストを含む上限 (探索中は confirmed より大きい)
    cap: u64,
    raised_at: u64,
    started_at: u64,
    alive: u64,
    learned: Option<u64>,
    learned_at: u64,
    /// 起動から 10 分以内に途切れた回数 (連続)
    early_stops: u64,
}

impl DiskProbe {
    /// 状態ファイルを読み、前回の終わり方から割当を学習する。
    pub fn load(path: &Path, now: u64) -> Self {
        let mut p = Self {
            path: path.to_path_buf(),
            confirmed: 0,
            cap: START,
            raised_at: now,
            started_at: now,
            alive: now,
            learned: None,
            learned_at: 0,
            early_stops: 0,
        };
        let Ok(text) = fs::read_to_string(path) else {
            p.save();
            log_info!(
                None,
                "disk allocation unknown: probing it from {} MiB (grows 25% every {} min while full; if Wings stops the server once for exceeding its limit, just start it again and the limit is remembered)",
                START / MIB,
                CONFIRM_SECS / 60
            );
            return p;
        };
        let mut saved = p.clone();
        saved.raised_at = 0;
        saved.started_at = 0;
        saved.alive = 0;
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let Ok(n) = v.trim().parse::<u64>() else {
                continue;
            };
            match k.trim() {
                "confirmed" => saved.confirmed = n,
                "cap" => saved.cap = n,
                "raised_at" => saved.raised_at = n,
                "started_at" => saved.started_at = n,
                "alive" => saved.alive = n,
                "learned" => saved.learned = (n > 0).then_some(n),
                "learned_at" => saved.learned_at = n,
                "early_stops" => saved.early_stops = n,
                _ => {}
            }
        }
        let stopped_early = saved.started_at > 0
            && saved.alive >= saved.started_at
            && saved.alive.saturating_sub(saved.started_at) < CONFIRM_SECS;
        let stopped_after_raise = saved.cap > saved.confirmed
            && saved.confirmed >= START
            && saved.raised_at > 0
            && saved.alive >= saved.raised_at
            && saved.alive.saturating_sub(saved.raised_at) < CONFIRM_SECS;

        if let Some(l) = saved.learned
            && now.saturating_sub(saved.learned_at) < RELEARN_SECS
        {
            p.learned = Some(l);
            p.learned_at = saved.learned_at;
            p.confirmed = l;
            p.cap = l;
            log_info!(
                None,
                "disk allocation learned earlier: {} MiB (re-probed after {} days)",
                l / MIB,
                RELEARN_SECS / 86_400
            );
        } else if stopped_after_raise {
            // 上げた直後に途切れた = 止められた。確認済みの値を割当として記憶する
            p.learned = Some(saved.confirmed);
            p.learned_at = now;
            p.confirmed = saved.confirmed;
            p.cap = saved.confirmed;
            log_warn!(
                None,
                "disk allocation learned: {} MiB (the server was stopped {} s after raising the cache to {} MiB)",
                saved.confirmed / MIB,
                saved.alive.saturating_sub(saved.raised_at),
                saved.cap / MIB
            );
        } else if stopped_early && saved.confirmed < START {
            // 初期段階で起動直後に途切れた。1 回なら普通の再起動かもしれないので数えるだけ
            p.early_stops = saved.early_stops + 1;
            if p.early_stops >= 2 {
                p.learned = Some(FLOOR);
                p.learned_at = now;
                p.confirmed = FLOOR;
                p.cap = FLOOR;
                log_warn!(
                    None,
                    "disk allocation seems smaller than {} MiB (stopped within {} min of start twice): using {} MiB; set SERVER_DISK in $HOME/.env if this is wrong",
                    START / MIB,
                    CONFIRM_SECS / 60,
                    FLOOR / MIB
                );
            } else {
                log_info!(
                    None,
                    "disk allocation unknown: probing again from {} MiB (previous run ended early)",
                    START / MIB
                );
            }
        } else {
            // 普通に止まっていた: 前回の上限は確認済み扱いで再開
            p.confirmed = saved.cap.max(saved.confirmed).max(FLOOR);
            p.cap = p.confirmed;
            log_info!(
                None,
                "disk allocation unknown: resuming the probe at {} MiB",
                p.cap / MIB
            );
        }
        p.save();
        p
    }

    /// 実データの上限。
    pub fn confirmed(&self) -> u64 {
        self.confirmed
    }

    /// バラストを含む上限。
    pub fn cap(&self) -> u64 {
        self.cap
    }

    pub fn learned(&self) -> Option<u64> {
        self.learned
    }

    /// 探索中 (cap を上げてから確認待ち) か。
    pub fn probing(&self) -> bool {
        self.cap > self.confirmed
    }

    /// 毎プローブ呼ぶ。`entries` は実データのバイト数。戻り値は (実データの上限, 全体の上限)。
    pub fn tick(&mut self, now: u64, entries: u64) -> (u64, u64) {
        let mut changed = false;
        if now.saturating_sub(self.alive) >= HEARTBEAT_SECS {
            self.alive = now;
            changed = true;
        }
        if self.early_stops > 0 && now.saturating_sub(self.started_at) >= CONFIRM_SECS {
            self.early_stops = 0;
            changed = true;
        }
        if self.learned.is_none() {
            if self.cap > self.confirmed && now.saturating_sub(self.raised_at) >= CONFIRM_SECS {
                self.confirmed = self.cap;
                changed = true;
                log_info!(
                    None,
                    "disk allocation probe: {} MiB survived {} min, now confirmed",
                    self.cap / MIB,
                    CONFIRM_SECS / 60
                );
            }
            let full = self.confirmed > 0
                && entries.saturating_mul(100) >= self.confirmed.saturating_mul(FULL_PERCENT);
            if self.cap == self.confirmed && full {
                let next = (self.cap / STEP_DEN)
                    .saturating_mul(STEP_NUM)
                    .max(self.cap.saturating_add(MIN_STEP));
                self.cap = next;
                self.raised_at = now;
                self.alive = now;
                changed = true;
                log_info!(
                    None,
                    "disk allocation probe: cache is full at {} MiB, raising to {} MiB with reservation only",
                    self.confirmed / MIB,
                    next / MIB
                );
            }
        }
        if changed {
            self.save();
        }
        (self.confirmed, self.cap)
    }

    fn save(&self) {
        let text = format!(
            "confirmed={}\ncap={}\nraised_at={}\nstarted_at={}\nalive={}\nlearned={}\nlearned_at={}\nearly_stops={}\n",
            self.confirmed,
            self.cap,
            self.raised_at,
            self.started_at,
            self.alive,
            self.learned.unwrap_or(0),
            self.learned_at,
            self.early_stops
        );
        if let Err(e) = fs::write(&self.path, text) {
            log_warn!(
                None,
                "cannot write disk probe state {}: {}",
                self.path.display(),
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_path(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(name);
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn starts_small_raises_when_full_and_confirms_after_ten_minutes() {
        let path = fresh_path("shp-probe-basic.state");
        let mut p = DiskProbe::load(&path, 1_000);
        assert_eq!(p.confirmed(), 0);
        assert_eq!(p.cap(), START);
        // 最初の 512 MiB も確認待ち → 10 分後に確認済み
        assert_eq!(p.tick(1_100, 0), (0, START));
        assert_eq!(p.tick(1_000 + CONFIRM_SECS, 0), (START, START));
        // 埋まるまでは上げない
        assert_eq!(p.tick(2_000, START / 2), (START, START));
        // 90% 埋まったら 25% 上げる (バラスト分)
        let (entries, cap) = p.tick(2_100, START * 95 / 100);
        assert_eq!(entries, START);
        assert_eq!(cap, START * 5 / 4);
        assert!(p.probing());
        // 確認前は上げない
        assert_eq!(p.tick(2_200, START), (START, START * 5 / 4));
        // 10 分生き延びたら確認済み
        assert_eq!(
            p.tick(2_100 + CONFIRM_SECS, START),
            (START * 5 / 4, START * 5 / 4)
        );
        assert!(path.exists());
    }

    #[test]
    fn learns_the_limit_when_stopped_right_after_raising() {
        let path = fresh_path("shp-probe-learn.state");
        let mut p = DiskProbe::load(&path, 10_000);
        p.tick(10_000 + CONFIRM_SECS, 0);
        p.tick(11_000, START); // 上げる → cap = 640 MiB
        assert!(p.probing());
        p.tick(11_000 + HEARTBEAT_SECS, START); // 生存記録
        // ここで Wings に止められたとして再起動
        let again = DiskProbe::load(&path, 12_000);
        assert_eq!(again.learned(), Some(START));
        assert_eq!(again.confirmed(), START);
        assert_eq!(again.cap(), START);
        assert!(!again.probing());
        // 学習済みなら埋まっていても上げない
        let mut again = again;
        assert_eq!(again.tick(13_000, START), (START, START));
        // 7 日経てば見直す
        let later = DiskProbe::load(&path, 12_000 + RELEARN_SECS + 1);
        assert_eq!(later.learned(), None);
    }

    #[test]
    fn a_normal_restart_keeps_probing_from_the_last_cap() {
        let path = fresh_path("shp-probe-restart.state");
        let mut p = DiskProbe::load(&path, 100_000);
        p.tick(100_000 + CONFIRM_SECS, 0);
        p.tick(101_000, START); // 上げる
        // 確認済みになってから (10 分以上生存の記録) 普通に止まる
        p.tick(101_000 + CONFIRM_SECS + 5, START);
        let again = DiskProbe::load(&path, 200_000);
        assert_eq!(again.learned(), None);
        assert_eq!(again.confirmed(), START * 5 / 4);
        assert_eq!(again.cap(), START * 5 / 4);
    }

    #[test]
    fn a_single_early_restart_does_not_shrink_the_cache() {
        let path = fresh_path("shp-probe-early.state");
        let mut p = DiskProbe::load(&path, 5_000);
        p.tick(5_000 + HEARTBEAT_SECS, 0); // 起動 30 秒で止まった (普通の再起動)
        let again = DiskProbe::load(&path, 6_000);
        assert_eq!(again.learned(), None);
        assert_eq!(again.cap(), START);
        // 10 分生き延びればカウントは消える
        let mut again = again;
        again.tick(6_000 + CONFIRM_SECS, 0);
        again.tick(6_000 + CONFIRM_SECS + HEARTBEAT_SECS, 0);
        let third = DiskProbe::load(&path, 20_000);
        assert_eq!(third.learned(), None);
        assert_eq!(third.cap(), START);
    }

    #[test]
    fn two_early_stops_in_a_row_mean_a_tiny_allocation() {
        let path = fresh_path("shp-probe-tiny.state");
        let mut p = DiskProbe::load(&path, 5_000);
        p.tick(5_000 + HEARTBEAT_SECS, 0);
        let mut again = DiskProbe::load(&path, 6_000);
        again.tick(6_000 + HEARTBEAT_SECS, 0);
        let third = DiskProbe::load(&path, 7_000);
        assert_eq!(third.learned(), Some(FLOOR));
        assert_eq!(third.cap(), FLOOR);
    }
}
