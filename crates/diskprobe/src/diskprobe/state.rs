//! 状態ファイルの中身 (`Saved`) と、前回の終わり方から割当を学ぶ判断 (`learn`)。T34.63 で
//! `DiskProbe::load` (153 行) から出した。どちらもファイルも時計も触らない (時刻は引数で受ける)。

use super::{CONFIRM_SECS, FAST_STEP, FLOOR, Mode, RELEARN_SECS, START};
use crate::config::MIB;
use crate::{log_info, log_warn};

/// 状態ファイルの 11 行 (`key=value`)。並びは [`Saved::to_text`] の順。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Saved {
    pub(super) mode: Mode,
    pub(super) confirmed: u64,
    pub(super) cap: u64,
    pub(super) raised_at: u64,
    pub(super) started_at: u64,
    pub(super) alive: u64,
    pub(super) learned: Option<u64>,
    pub(super) learned_at: u64,
    pub(super) early_stops: u64,
    pub(super) found: Option<u64>,
    pub(super) found_at: u64,
}

impl Saved {
    /// 状態ファイルが無いときの初めの姿 (`now` に起動した)。
    pub(super) fn fresh(now: u64) -> Saved {
        Saved {
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
        }
    }

    /// 状態ファイルを読む。書いていない鍵と読めない値は、時刻と `confirmed` が 0、ほかは
    /// [`Saved::fresh`] の値 (前と同じ)。`mode` は `slow` のときだけ slow。
    pub(super) fn parse(text: &str) -> Saved {
        let mut saved = Saved {
            confirmed: 0,
            raised_at: 0,
            started_at: 0,
            alive: 0,
            ..Saved::fresh(0)
        };
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
        saved
    }

    /// 状態ファイルに書く文字列 (11 行。`DiskProbe::save` が書く形)。
    pub(super) fn to_text(self) -> String {
        format!(
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
        )
    }
}

/// [`learn`] が決めたことのうち、ログに 1 行出すもの (文面は [`Note::log`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Note {
    /// 状態ファイルが無い (初めての起動)
    Fresh,
    /// 前に学んだ値がまだ新しい
    LearnedEarlier(u64),
    /// fast で伸ばしている間に止められた (起動から何秒で)
    SwitchToSlow(u64),
    /// fast のまま探り直す
    FastAgain,
    /// slow で上げた直後に止められた = 確認済みが割当 (確認済み、上げてから止められるまでの秒、上げた値)
    LearnedAfterRaise(u64, u64, u64),
    /// slow で 2 回続けて起動直後に止められた = 最低値
    Tiny,
    /// slow で 1 回目の起動直後の途切れ
    SlowAgain,
    /// slow の探索を続ける (その上限)
    Resume(u64),
}

impl Note {
    /// 1 行出す (`DiskProbe::load` にあった文面のまま)。
    pub(super) fn log(self) {
        match self {
            Note::Fresh => log_info!(
                None,
                "disk allocation unknown: probing it by reserving {} MiB at a time until the filesystem refuses (data limited to {} MiB until confirmed after {} min)",
                FAST_STEP / MIB,
                START / MIB,
                CONFIRM_SECS / 60
            ),
            Note::LearnedEarlier(l) => log_info!(
                None,
                "disk allocation learned earlier: {} MiB (re-probed after {} days)",
                l / MIB,
                RELEARN_SECS / 86_400
            ),
            Note::SwitchToSlow(after) => log_warn!(
                None,
                "the server was stopped {} s after start while reserving disk quickly: the allocation is enforced by Wings, switching to the gradual probe from {} MiB",
                after,
                START / MIB
            ),
            Note::FastAgain => log_info!(
                None,
                "disk allocation unknown: probing it by reserving {} MiB at a time until the filesystem refuses",
                FAST_STEP / MIB
            ),
            Note::LearnedAfterRaise(confirmed, after, cap) => log_warn!(
                None,
                "disk allocation learned: {} MiB (the server was stopped {} s after raising the cache to {} MiB)",
                confirmed / MIB,
                after,
                cap / MIB
            ),
            Note::Tiny => log_warn!(
                None,
                "disk allocation seems smaller than {} MiB (stopped within {} min of start twice): using {} MiB; set SERVER_DISK in $HOME/.env if this is wrong",
                START / MIB,
                CONFIRM_SECS / 60,
                FLOOR / MIB
            ),
            Note::SlowAgain => log_info!(
                None,
                "disk allocation unknown: probing gradually again from {} MiB (previous run ended early)",
                START / MIB
            ),
            Note::Resume(cap) => log_info!(
                None,
                "disk allocation unknown: resuming the gradual probe at {} MiB",
                cap / MIB
            ),
        }
    }
}

/// 前回の状態 (`saved`。ファイルが無ければ `None`) から、`now` に起動したこの回の状態を決める。
///
/// 返す状態の時刻 (`raised_at`・`started_at`・`alive`) はどれも `now`、fast の候補 (`found`) は
/// 持ち越さない。判断の順は `DiskProbe::load` にあった頃と同じ。
pub(super) fn learn(saved: Option<&Saved>, now: u64) -> (Saved, Note) {
    let mut p = Saved::fresh(now);
    let Some(saved) = saved else {
        return (p, Note::Fresh);
    };
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
        return (p, Note::LearnedEarlier(l));
    }
    if saved.mode == Mode::Fast {
        if stopped_early {
            // fast で伸ばしている間に止められた = Wings がディレクトリサイズで止めるホスト。
            // 以後は緩やかな探索に切り替える
            p.mode = Mode::Slow;
            p.confirmed = 0;
            p.cap = START;
            return (
                p,
                Note::SwitchToSlow(saved.alive.saturating_sub(saved.started_at)),
            );
        }
        return (p, Note::FastAgain);
    }
    learn_slow(saved, now, stopped_early, p)
}

/// [`learn`] の slow の枝 (前回が slow で、学んだ値が無いか古い)。
fn learn_slow(saved: &Saved, now: u64, stopped_early: bool, mut p: Saved) -> (Saved, Note) {
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
        let note = Note::LearnedAfterRaise(
            saved.confirmed,
            saved.alive.saturating_sub(saved.raised_at),
            saved.cap,
        );
        return (p, note);
    }
    if stopped_early && saved.confirmed < START {
        p.early_stops = saved.early_stops + 1;
        if p.early_stops >= 2 {
            p.learned = Some(FLOOR);
            p.learned_at = now;
            p.confirmed = FLOOR;
            p.cap = FLOOR;
            return (p, Note::Tiny);
        }
        p.confirmed = 0;
        p.cap = START;
        return (p, Note::SlowAgain);
    }
    p.confirmed = saved.cap.max(saved.confirmed).max(FLOOR);
    p.cap = p.confirmed;
    (p, Note::Resume(p.cap))
}

#[cfg(test)]
mod tests;
