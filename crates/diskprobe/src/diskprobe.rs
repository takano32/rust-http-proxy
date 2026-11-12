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
//! ディレクトリサイズで止めるタイプなので、10 分止められないごとに 512 MiB ずつ上げ、
//! 増えた分はバラストだけで埋める (実データは確認済みの上限まで)。止められたら再起動時に
//! 「上げてから 10 分以内に途切れた」と分かるので、確認済みの値を割当として記憶する。
//!
//! 止められても停止シグナルでバラストは切り詰められるので、使用量は実データの上限以下に戻り
//! そのまま再起動できる。状態はボリューム内の小さなファイルに `key=value` で残す。

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::MIB;
use crate::{log_info, log_warn};

// 状態ファイルの中身と、前回の終わり方から学ぶ判断 (T34.63)
mod state;
use state::{Note, Saved, learn};

/// 最初の上限 (実データの上限でもある)。
pub const START: u64 = 512 * MIB;
/// fast モードで 1 プローブごとに伸ばす量。
pub(crate) const FAST_STEP: u64 = 512 * MIB;
/// これだけ止められなければ確認済みとみなす。
const CONFIRM_SECS: u64 = 600;
/// 生存記録の間隔。
const HEARTBEAT_SECS: u64 = 30;
/// fast モードで、バラストがこの回数連続で伸びなければ限界とみなす。
const STALL_TICKS: u32 = 2;
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
    /// fast モードの伸び検知用 (前回の保持量と、伸びなかった連続回数)
    prev_owned: u64,
    stalled: u32,
}

impl DiskProbe {
    /// 状態ファイルを読み、前回の終わり方から割当を学習する。
    ///
    /// 段取り (T34.63 で割った): 読む ([`Saved::parse`]) → 判断 ([`learn`]) → ログ → 保存。
    /// 状態ファイルが無いときだけは前と同じく、保存してからログを出す (書けなかったときの WARN が先)。
    pub fn load(path: &Path, now: u64) -> Self {
        let saved = fs::read_to_string(path).ok().map(|t| Saved::parse(&t));
        let (state, note) = learn(saved.as_ref(), now);
        let p = Self::from_saved(path, state);
        if note == Note::Fresh {
            p.save();
            note.log();
        } else {
            note.log();
            p.save();
        }
        p
    }

    /// 状態ファイルの 11 項目から組む (fast の伸び検知は 0 から)。
    fn from_saved(path: &Path, s: Saved) -> Self {
        Self {
            path: path.to_path_buf(),
            mode: s.mode,
            confirmed: s.confirmed,
            cap: s.cap,
            raised_at: s.raised_at,
            started_at: s.started_at,
            alive: s.alive,
            learned: s.learned,
            learned_at: s.learned_at,
            early_stops: s.early_stops,
            found: s.found,
            found_at: s.found_at,
            prev_owned: 0,
            stalled: 0,
        }
    }

    /// 状態ファイルに書く 11 項目。
    fn saved(&self) -> Saved {
        Saved {
            mode: self.mode,
            confirmed: self.confirmed,
            cap: self.cap,
            raised_at: self.raised_at,
            started_at: self.started_at,
            alive: self.alive,
            learned: self.learned,
            learned_at: self.learned_at,
            early_stops: self.early_stops,
            found: self.found,
            found_at: self.found_at,
        }
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

    #[cfg(test)]
    pub(crate) fn learned(&self) -> Option<u64> {
        self.learned
    }

    /// 探索中 (まだ確定していない) か。
    pub fn probing(&self) -> bool {
        self.learned.is_none() && (self.cap > self.confirmed || self.found.is_some())
    }

    /// 毎プローブ呼ぶ。`owned` は実データ + バラスト、`fill_failed` は
    /// 直前のバラスト伸長が (ENOSPC 等で) 失敗したか。戻り値は (実データの上限, 全体の上限)。
    ///
    /// 実データの量 (`entries`) は、どの段でも使っていなかったので引数から外した (T36.17。
    /// T34.63 で `tick_slow` の引数を外したときは、公開の形を呼ぶ側に合わせて残していた)。
    pub fn tick(&mut self, now: u64, owned: u64, fill_failed: bool) -> (u64, u64) {
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
                Mode::Slow => self.tick_slow(now),
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
        if !followed && owned <= self.prev_owned {
            self.stalled += 1;
        } else {
            self.stalled = 0;
        }
        self.prev_owned = owned;
        if fill_failed || (self.cap > START && !followed && self.stalled >= STALL_TICKS) {
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

    fn tick_slow(&mut self, now: u64) -> bool {
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
        if self.cap == self.confirmed && self.confirmed >= START {
            let next = self.cap.saturating_add(FAST_STEP);
            self.cap = next;
            self.raised_at = now;
            self.alive = now;
            changed = true;
            log_info!(
                None,
                "disk allocation probe: {} MiB is confirmed, raising to {} MiB with reservation only",
                self.confirmed / MIB,
                next / MIB
            );
        }
        changed
    }

    fn save(&self) {
        let text = self.saved().to_text();
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
        assert_eq!(p.tick(1_001, START, false), (START, START + FAST_STEP));
        assert_eq!(
            p.tick(1_002, START + FAST_STEP, false),
            (START, START + 2 * FAST_STEP)
        );
        // 伸びが 2 回連続で止まったら (FS の空きの都合) そこが候補: cap は保持量に戻る
        let held = START + FAST_STEP + 100 * MIB;
        assert_eq!(p.tick(1_003, held, false), (START, START + 2 * FAST_STEP));
        assert_eq!(p.tick(1_004, held, false), (START, START + 2 * FAST_STEP));
        assert_eq!(p.tick(1_005, held, false), (START, held));
        assert!(p.probing());
        // 10 分待って確定 → 実データもそこまで
        assert_eq!(p.tick(1_100, held, false), (START, held));
        assert_eq!(p.tick(1_005 + CONFIRM_SECS, held, false), (held, held));
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
        p.tick(1_001, START, false);
        let (_, cap) = p.tick(1_002, START + 200 * MIB, true);
        assert_eq!(cap, START + 200 * MIB);
        assert!(p.probing());
    }

    #[test]
    fn being_stopped_during_fast_mode_switches_to_slow() {
        let path = fresh_path("shp-probe-fast-killed.state");
        let mut p = DiskProbe::load(&path, 10_000);
        p.tick(10_001, START, false);
        p.tick(10_000 + HEARTBEAT_SECS, START + FAST_STEP, false); // 生存記録
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
    fn slow_mode_raises_every_confirmation_and_learns_when_stopped_after_a_raise() {
        let path = fresh_path("shp-probe-slow.state");
        let mut p = DiskProbe::load(&path, 10_000);
        p.tick(10_000 + HEARTBEAT_SECS, START, false);
        let mut p = DiskProbe::load(&path, 10_100); // 早期途切れ → slow
        assert_eq!(p.mode(), Mode::Slow);
        // 最初の 512 MiB は 10 分後に確認済みになり、すぐ次の 512 MiB を (バラストで) 探る
        assert_eq!(
            p.tick(10_100 + CONFIRM_SECS, 0, false),
            (START, START + FAST_STEP)
        );
        // 10 分止められなければ確認済みになり、また 512 MiB 上げる
        let (entries, cap) = p.tick(10_100 + 2 * CONFIRM_SECS + 1, START + FAST_STEP, false);
        assert_eq!((entries, cap), (START + FAST_STEP, START + 2 * FAST_STEP));
        let raised = 10_100 + 2 * CONFIRM_SECS + 1;
        p.tick(raised + HEARTBEAT_SECS, START + FAST_STEP, false); // 生存記録
        // 上げた直後に止められた → 確認済みを学習
        let again = DiskProbe::load(&path, raised + 120);
        assert_eq!(again.learned(), Some(START + FAST_STEP));
        assert_eq!(
            (again.confirmed(), again.cap()),
            (START + FAST_STEP, START + FAST_STEP)
        );
        assert_eq!(again.mode(), Mode::Slow);
    }

    #[test]
    fn slow_mode_two_early_stops_mean_a_tiny_allocation() {
        let path = fresh_path("shp-probe-slow-tiny.state");
        let mut p = DiskProbe::load(&path, 5_000);
        p.tick(5_000 + HEARTBEAT_SECS, START, false);
        let mut again = DiskProbe::load(&path, 6_000); // → slow (early stop #0 in slow terms)
        assert_eq!(again.mode(), Mode::Slow);
        again.tick(6_000 + HEARTBEAT_SECS, 0, false);
        let mut third = DiskProbe::load(&path, 7_000); // slow で 1 回目の早期途切れ
        assert_eq!(third.learned(), None);
        third.tick(7_000 + HEARTBEAT_SECS, 0, false);
        let fourth = DiskProbe::load(&path, 8_000); // 2 回目 → 最低値
        assert_eq!(fourth.learned(), Some(FLOOR));
        assert_eq!(fourth.cap(), FLOOR);
    }
}
