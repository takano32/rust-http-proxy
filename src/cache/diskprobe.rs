//! ディスク割当が分からないときに、割当を探る。
//!
//! Pterodactyl は割当を教えてくれないが、超過すると即座にプロセスを止める (SIGTERM)。
//! 2 つのモードで「止められない上限」を探る:
//!
//! **fast (既定)**: 毎プローブ 512 MiB ずつバラスト (fallocate) を伸ばす。実データの上限は 512 MiB。
//! fallocate が失敗する、またはファイルシステムの空きの都合でバラストが追いつけなくなったら、
//! そのときの保持量を割当の候補にする。候補のまま 10 分止められなければ確定 (`learned`) し、
//! 実データもそこまで使う。ファイルシステム側で割当が効いているホストなら数分で終わる。
//!
//! **slow**: fast の途中で止められた (起動から 10 分以内に途切れた) ホスト向け。Wings が
//! ディレクトリサイズで止めるタイプなので、実データが埋まったら 10 分ごとに 25% ずつ上げ、
//! 増えた分はバラストだけで埋める。止められたら再起動時に「上げてから 10 分以内に途切れた」と
//! 分かるので、確認済みの値を割当として記憶する。
//!
//! 止められても停止シグナルでバラストは切り詰められるので、使用量は実データの上限以下に戻り
//! そのまま再起動できる。状態はボリューム内の小さなファイルに `key=value` で残す。

use std::fs;
use std::path::{Path, PathBuf};

use super::config::MIB;
use crate::{log_info, log_warn};

/// 最初の上限 (実データの上限でもある)。
pub const START: u64 = 512 * MIB;
/// fast モードで 1 プローブごとに伸ばす量。
pub const FAST_STEP: u64 = 512 * MIB;
/// slow モードで 1 段上げる割合 (5/4 = 25%) と最低増分。
const STEP_NUM: u64 = 5;
const STEP_DEN: u64 = 4;
const MIN_STEP: u64 = 64 * MIB;
/// これだけ止められなければ確認済みとみなす。
const CONFIRM_SECS: u64 = 600;
/// 生存記録の間隔。
const HEARTBEAT_SECS: u64 = 30;
/// slow モードで、実データがこの割合まで埋まったら次を探る (%)。
const FULL_PERCENT: u64 = 90;
/// 学習した値を見直すまでの期間。
const RELEARN_SECS: u64 = 7 * 24 * 3600;
/// 最初の 512 MiB すら止められた場合の最低値。
const FLOOR: u64 = 64 * MIB;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Fast,
    Slow,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Fast => "fast",
            Mode::Slow => "slow",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskProbe {
    path: PathBuf,
    mode: Mode,
    /// 実データの上限 (止められないと確認済み)
    confirmed: u64,
    /// バラストを含む上限 (探索中は confirmed より大きい)
    cap: u64,
    raised_at: u64,
    started_at: u64,
    alive: u64,
    learned: Option<u64>,
    learned_at: u64,
    /// slow モードで、起動から 10 分以内に途切れた回数 (連続)
    early_stops: u64,
    /// fast モードで見つけた割当の候補と、その時刻
    found: Option<u64>,
    found_at: u64,
}

impl DiskProbe {
    /// 状態ファイルを読み、前回の終わり方から割当を学習する。
    pub fn load(path: &Path, now: u64) -> Self {
        let mut p = Self {
            path: path.to_path_buf(),
            mode: Mode::Fast,
            confirmed: START,
            cap: START,
            raised_at: now,
            started_at: now,
            alive: now,
            learned: None,
            learned_at: 0,
            early_stops: 0,
            found: None,
            found_at: 0,
        };
        let Ok(text) = fs::read_to_string(path) else {
            p.save();
            log_info!(
                None,
                "disk allocation unknown: probing it by reserving {} MiB at a time until the filesystem refuses (data limited to {} MiB until confirmed after {} min)",
                FAST_STEP / MIB,
                START / MIB,
                CONFIRM_SECS / 60
            );
            return p;
        };
        let mut saved = p.clone();
        saved.raised_at = 0;
        saved.started_at = 0;
        saved.alive = 0;
        saved.confirmed = 0;
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            if k.trim() == "mode" {
                saved.mode = if v == "slow" { Mode::Slow } else { Mode::Fast };
                continue;
            }
            let Ok(n) = v.parse::<u64>() else {
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
                "found" => saved.found = (n > 0).then_some(n),
                "found_at" => saved.found_at = n,
                _ => {}
            }
        }
        let stopped_early = saved.started_at > 0
            && saved.alive >= saved.started_at
            && saved.alive.saturating_sub(saved.started_at) < CONFIRM_SECS;
        p.mode = saved.mode;

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
        } else if saved.mode == Mode::Fast {
            if stopped_early {
                // fast で伸ばしている間に止められた = Wings がディレクトリサイズで止めるホスト。
                // 以後は緩やかな探索に切り替える
                p.mode = Mode::Slow;
                p.confirmed = 0;
                p.cap = START;
                log_warn!(
                    None,
                    "the server was stopped {} s after start while reserving disk quickly: the allocation is enforced by Wings, switching to the gradual probe from {} MiB",
                    saved.alive.saturating_sub(saved.started_at),
                    START / MIB
                );
            } else {
                log_info!(
                    None,
                    "disk allocation unknown: probing it by reserving {} MiB at a time until the filesystem refuses",
                    FAST_STEP / MIB
                );
            }
        } else {
            p.confirmed = saved.confirmed;
            p.cap = saved.cap.max(FLOOR);
            let stopped_after_raise = saved.cap > saved.confirmed
                && saved.confirmed >= START
                && saved.raised_at > 0
                && saved.alive >= saved.raised_at
                && saved.alive.saturating_sub(saved.raised_at) < CONFIRM_SECS;
            if stopped_after_raise {
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
                    p.confirmed = 0;
                    p.cap = START;
                    log_info!(
                        None,
                        "disk allocation unknown: probing gradually again from {} MiB (previous run ended early)",
                        START / MIB
                    );
                }
            } else {
                p.confirmed = saved.cap.max(saved.confirmed).max(FLOOR);
                p.cap = p.confirmed;
                log_info!(
                    None,
                    "disk allocation unknown: resuming the gradual probe at {} MiB",
                    p.cap / MIB
                );
            }
        }
        p.save();
        p
    }

    pub fn mode(&self) -> Mode {
        self.mode
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

    /// 探索中 (まだ確定していない) か。
    pub fn probing(&self) -> bool {
        self.learned.is_none() && (self.cap > self.confirmed || self.found.is_some())
    }

    /// 毎プローブ呼ぶ。`entries` は実データ、`owned` は実データ + バラスト、`fill_failed` は
    /// 直前のバラスト伸長が (ENOSPC 等で) 失敗したか。戻り値は (実データの上限, 全体の上限)。
    pub fn tick(&mut self, now: u64, entries: u64, owned: u64, fill_failed: bool) -> (u64, u64) {
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
            changed |= match self.mode {
                Mode::Fast => self.tick_fast(now, owned, fill_failed),
                Mode::Slow => self.tick_slow(now, entries),
            };
        }
        if changed {
            self.save();
        }
        (self.confirmed, self.cap)
    }

    fn tick_fast(&mut self, now: u64, owned: u64, fill_failed: bool) -> bool {
        if let Some(found) = self.found {
            if now.saturating_sub(self.found_at) >= CONFIRM_SECS {
                self.learned = Some(found);
                self.learned_at = now;
                self.confirmed = found;
                self.cap = found;
                log_info!(
                    None,
                    "disk allocation confirmed: {} MiB (held for {} min without being stopped)",
                    found / MIB,
                    CONFIRM_SECS / 60
                );
                return true;
            }
            return false;
        }
        let followed = owned.saturating_add(FAST_STEP / 2) >= self.cap;
        if fill_failed || (self.cap > START && !followed) {
            let found = owned.max(START);
            self.found = Some(found);
            self.found_at = now;
            self.cap = found;
            log_info!(
                None,
                "disk allocation candidate: {} MiB ({}); confirming it for {} min before using it for data",
                found / MIB,
                if fill_failed {
                    "the filesystem refused to reserve more"
                } else {
                    "the reservation stopped growing"
                },
                CONFIRM_SECS / 60
            );
            return true;
        }
        if followed {
            self.cap = self.cap.saturating_add(FAST_STEP);
            self.raised_at = now;
            return true;
        }
        false
    }

    fn tick_slow(&mut self, now: u64, entries: u64) -> bool {
        let mut changed = false;
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
        changed
    }

    fn save(&self) {
        let text = format!(
            "mode={}\nconfirmed={}\ncap={}\nraised_at={}\nstarted_at={}\nalive={}\nlearned={}\nlearned_at={}\nearly_stops={}\nfound={}\nfound_at={}\n",
            self.mode.as_str(),
            self.confirmed,
            self.cap,
            self.raised_at,
            self.started_at,
            self.alive,
            self.learned.unwrap_or(0),
            self.learned_at,
            self.early_stops,
            self.found.unwrap_or(0),
            self.found_at
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
    fn fast_mode_ramps_by_512_mib_until_the_reservation_stops_following() {
        let path = fresh_path("shp-probe-fast.state");
        let mut p = DiskProbe::load(&path, 1_000);
        assert_eq!(p.mode(), Mode::Fast);
        assert_eq!((p.confirmed(), p.cap()), (START, START));
        // バラストが追いついたら 512 MiB ずつ上げる
        assert_eq!(p.tick(1_001, 0, START, false), (START, START + FAST_STEP));
        assert_eq!(
            p.tick(1_002, 0, START + FAST_STEP, false),
            (START, START + 2 * FAST_STEP)
        );
        // 追いつけなくなったら (FS の空きの都合) そこが候補: cap は保持量に戻る
        let held = START + FAST_STEP + 100 * MIB;
        assert_eq!(p.tick(1_003, 0, held, false), (START, held));
        assert!(p.probing());
        // 10 分待って確定 → 実データもそこまで
        assert_eq!(p.tick(1_100, 0, held, false), (START, held));
        assert_eq!(p.tick(1_003 + CONFIRM_SECS, 0, held, false), (held, held));
        assert_eq!(p.learned(), Some(held));
        assert!(!p.probing());
        // 再起動しても学習済み
        let again = DiskProbe::load(&path, 5_000);
        assert_eq!((again.confirmed(), again.cap()), (held, held));
    }

    #[test]
    fn fast_mode_stops_when_fallocate_fails() {
        let path = fresh_path("shp-probe-fast-fail.state");
        let mut p = DiskProbe::load(&path, 1_000);
        p.tick(1_001, 0, START, false);
        let (_, cap) = p.tick(1_002, 0, START + 200 * MIB, true);
        assert_eq!(cap, START + 200 * MIB);
        assert!(p.probing());
    }

    #[test]
    fn being_stopped_during_fast_mode_switches_to_slow() {
        let path = fresh_path("shp-probe-fast-killed.state");
        let mut p = DiskProbe::load(&path, 10_000);
        p.tick(10_001, 0, START, false);
        p.tick(10_000 + HEARTBEAT_SECS, 0, START + FAST_STEP, false); // 生存記録
        // ここで Wings に止められて再起動
        let again = DiskProbe::load(&path, 10_100);
        assert_eq!(again.mode(), Mode::Slow);
        assert_eq!((again.confirmed(), again.cap()), (0, START));
        assert_eq!(again.learned(), None);
        // 以後の再起動でも slow のまま
        let third = DiskProbe::load(&path, 10_100 + CONFIRM_SECS + 10);
        assert_eq!(third.mode(), Mode::Slow);
    }

    #[test]
    fn slow_mode_raises_when_full_and_learns_when_stopped_after_a_raise() {
        let path = fresh_path("shp-probe-slow.state");
        let mut p = DiskProbe::load(&path, 10_000);
        p.tick(10_000 + HEARTBEAT_SECS, 0, START, false);
        let mut p = DiskProbe::load(&path, 10_100); // 早期途切れ → slow
        assert_eq!(p.mode(), Mode::Slow);
        // 最初の 512 MiB は 10 分後に確認済み
        assert_eq!(p.tick(10_100 + CONFIRM_SECS, 0, 0, false), (START, START));
        // 90% 埋まったら 25% 上げる
        let (entries, cap) = p.tick(20_000, START * 95 / 100, START, false);
        assert_eq!((entries, cap), (START, START * 5 / 4));
        p.tick(20_000 + HEARTBEAT_SECS, START, START, false);
        // 上げた直後に止められた → 確認済みを学習
        let again = DiskProbe::load(&path, 21_000);
        assert_eq!(again.learned(), Some(START));
        assert_eq!((again.confirmed(), again.cap()), (START, START));
        assert_eq!(again.mode(), Mode::Slow);
    }

    #[test]
    fn slow_mode_two_early_stops_mean_a_tiny_allocation() {
        let path = fresh_path("shp-probe-slow-tiny.state");
        let mut p = DiskProbe::load(&path, 5_000);
        p.tick(5_000 + HEARTBEAT_SECS, 0, START, false);
        let mut again = DiskProbe::load(&path, 6_000); // → slow (early stop #0 in slow terms)
        assert_eq!(again.mode(), Mode::Slow);
        again.tick(6_000 + HEARTBEAT_SECS, 0, 0, false);
        let mut third = DiskProbe::load(&path, 7_000); // slow で 1 回目の早期途切れ
        assert_eq!(third.learned(), None);
        third.tick(7_000 + HEARTBEAT_SECS, 0, 0, false);
        let fourth = DiskProbe::load(&path, 8_000); // 2 回目 → 最低値
        assert_eq!(fourth.learned(), Some(FLOOR));
        assert_eq!(fourth.cap(), FLOOR);
    }
}
