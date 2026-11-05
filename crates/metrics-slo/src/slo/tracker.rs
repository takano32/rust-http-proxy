//! 時間ごとの集計 ([`Tracker`] と、その 1 時間 [`Hour`] / 外れた時間帯 [`Breach`] / 期間の畳み
//! [`Report`]。`slo` の一部。T.34.2 で `slo.rs` から移した)。
//!
//! リングに持つのは閾を当てる前の積み上げ ([`Slot`]) で、閾は読むとき ([`Slot::tally`]) に当てる
//! (判定は [`judge`] から何度でも作り直せる)。**触るのは履歴スレッドと `/slo` だけ**。
//! `/slo` の文字列を組むのは `report.rs` ([`Report::to_json`])。
//!
//! 行を動かしただけで中身は変えていない。外から見える名前は `slo` の `pub use` でそのまま。

use std::collections::VecDeque;

use super::judge::{RATE_AT, RATE_WINDOW_SECS, RATES, Verdict, excludes, judge};
use super::{DAY, HOUR, HOURS, MAX_DAYS, Thresholds};
use crate::history::Sample;
use crate::metrics::DestHour;

/// 1 時間ぶんの**積み上げ** (リングに持つもの。閾の当て方は [`Slot::tally`] が決める)。
///
/// 速さ (確立の 2 つ) は 5 秒の標本ごとの判定を畳み、比の 2 つは**分子と分母の合計**を持つ
/// (T.29.33。利用者の決定: 比は 1 時間の合計で判定する)。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Slot {
    t: u64,
    judged: u32,
    /// 速さの 2 つを満たした 5 秒の数
    fast: u32,
    miss_speed: [u32; 2],
    worst_speed: [f64; 2],
    num: [u64; RATES],
    den: [u64; RATES],
}

impl Slot {
    /// 閾を当てて 1 時間の集計にする。
    ///
    /// 比の 2 つはこの 1 時間の `Σ分子 ÷ Σ分母` で判定し、**どちらかを外した時間は、その時間の 5 秒を全部
    /// 「達成していない」**と数える (`met` = 0、外した比の `miss` = 判定した 5 秒の数、`worst` = その時間の比)。
    /// 満たした時間の `met` は速さの 2 つを満たした 5 秒の数。分母が 0 の比は外さない (判定なし)。
    ///
    /// `dest` はその時間の宛先の表 (T.30.4)。[`excludes`] なら、その宛先のエラーと試行を `error_rate` の
    /// 分子と分母から引いてから当てる (引いた数は `excluded`)。
    fn tally(&self, th: &Thresholds, dest: Option<&DestHour>) -> Hour {
        let limits = th.limits();
        let mut h = Hour {
            t: self.t,
            judged: self.judged,
            num: self.num,
            den: self.den,
            ..Hour::default()
        };
        if let Some(d) = dest.filter(|d| excludes(d)) {
            let e = d.top_errors.min(h.num[0]);
            let a = d.top_attempts.max(e).min(h.den[0]);
            h.num[0] -= e;
            h.den[0] -= a;
            h.excluded = [e, a];
            h.excluded_hours = 1;
        }
        h.miss[..2].copy_from_slice(&self.miss_speed);
        h.worst[..2].copy_from_slice(&self.worst_speed);
        let mut rates_ok = true;
        for i in 0..RATES {
            if h.den[i] == 0 {
                continue;
            }
            let v = h.num[i] as f64 / h.den[i] as f64;
            if v > limits[RATE_AT + i] {
                rates_ok = false;
                h.miss[RATE_AT + i] = self.judged;
                h.worst[RATE_AT + i] = v;
                h.rate_hours[i][1] = 1;
            } else {
                h.rate_hours[i][0] = 1;
            }
        }
        h.met = if rates_ok { self.fast } else { 0 };
        h
    }
}

/// 1 時間ぶん (と、それを日・期間で足したもの) の集計。
///
/// T.29.33 から `met` は「速さの 2 つを 5 秒ごとに満たし、**その時間の比の 2 つも 1 時間の合計で満たした**」
/// 5 秒の数。`miss` の 2・3 番目 (比) は、その比を外した時間の判定した 5 秒の数。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Hour {
    /// その時間の 0 分 0 秒 (epoch 秒)
    pub t: u64,
    /// 判定できた標本の数 (確立かエラーが 1 本以上あったもの)
    pub judged: u32,
    /// そのうち達成した数 (上の読み方)
    pub met: u32,
    /// 閾ごとに外した標本の数 ([`NAMES`] の順)
    pub miss: [u32; 4],
    /// 閾ごとの最悪の値 (速さは外した 5 秒の中で、比は外した時間の 1 時間の値。外していなければ 0)
    pub worst: [f64; 4],
    /// 比の 2 つの分子と分母の合計 (T.29.33。`error_rate` = 名前解決に成功した試みのエラー ÷ 試み
    /// (T.30.3 から 443・80 宛てだけ)、
    /// `dns_miss_per_connect` = 失敗した試みを除いたミス ÷ 確立)
    pub num: [u64; RATES],
    pub den: [u64; RATES],
    /// 比ごとの `[満たした時間, 外した時間]` (分母 0 の時間はどちらにも数えない。T.29.33)
    pub rate_hours: [[u32; 2]; RATES],
    /// `error_rate` から外した宛先の `[エラー, 試行]` と、外した時間の数 (T.30.4。`num` / `den` は外したあと)
    pub excluded: [u64; 2],
    pub excluded_hours: u32,
}

impl Hour {
    /// 達成率 (判定した標本が無ければ `None`)。
    pub fn ratio(&self) -> Option<f64> {
        (self.judged > 0).then(|| self.met as f64 / self.judged as f64)
    }

    /// この時間は外したか (**判定した標本があって、1 本でも達成していない**)。
    pub fn breached(&self) -> bool {
        self.judged > 0 && self.met < self.judged
    }

    /// 比 `i` (0 = `error_rate`、1 = `dns_miss_per_connect`) の合計の値 (分母 0 なら `None`)。
    pub fn rate(&self, i: usize) -> Option<f64> {
        (self.den[i] > 0).then(|| self.num[i] as f64 / self.den[i] as f64)
    }

    fn add(&mut self, o: &Hour) {
        self.judged += o.judged;
        self.met += o.met;
        for i in 0..4 {
            self.miss[i] += o.miss[i];
            self.worst[i] = self.worst[i].max(o.worst[i]);
        }
        for i in 0..RATES {
            self.num[i] += o.num[i];
            self.den[i] += o.den[i];
            self.rate_hours[i][0] += o.rate_hours[i][0];
            self.rate_hours[i][1] += o.rate_hours[i][1];
            self.excluded[i] += o.excluded[i];
        }
        self.excluded_hours += o.excluded_hours;
    }
}

/// 直近 1 時間 (いまから [`RATE_WINDOW_SECS`] 秒) の比の合計 (`/slo` の `last_hour`。T.29.33)。
///
/// T.30.4 から `error_rate` の `num` / `den` は、外した宛先のぶんを引いたあと (`excluded`)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LastHour {
    /// 窓の始まり (いま − 1 時間) と終わり (いま)
    pub from: u64,
    pub to: u64,
    /// 窓に入った 5 秒の標本の数
    pub samples: u32,
    pub num: [u64; RATES],
    pub den: [u64; RATES],
    /// `error_rate` から外した `[エラー, 試行]` と、外した宛先 (いまの時間・前の時間の順。T.30.4)
    pub excluded: [u64; 2],
    pub destinations: Vec<String>,
}

/// 時間ごとの集計のリング (**触るのは履歴スレッドと `/slo` だけ**)。
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    hours: VecDeque<Slot>,
    /// 直近 1 時間の 5 秒ごとの比の分子と分母 (`last_hour` 用。720 本 × 40 B ≈ 28 KiB。T.29.33)
    recent: VecDeque<(u64, [u64; RATES], [u64; RATES])>,
    /// 最後に [`Tracker::observe`] が受けた閾 ([`Tracker::hours`] が当てる)
    th: Option<Thresholds>,
    /// 時間ごとの「いちばん多くエラーを出した 443・80 の宛先」(T.30.4。時刻の順、エラーのあった時間だけ。
    /// 多くて [`HOURS`] 行)
    dests: VecDeque<DestHour>,
}

impl Tracker {
    pub fn new() -> Tracker {
        Tracker::default()
    }

    /// 標本 1 本を判定して、その時間の集計に足す。
    ///
    /// 標本は時刻の順に来る (履歴スレッド) が、順が戻った標本 (状態ファイルの読み戻しや
    /// 時計の巻き戻し) は**まだ残っている時間なら足し、落ちていれば捨てる**。
    pub fn observe(&mut self, th: &Thresholds, s: &Sample) -> Verdict {
        self.th = Some(*th);
        let v = judge(th, s);
        let any = v.den.iter().any(|&d| d > 0);
        if !v.judged && !any {
            return v;
        }
        if any {
            // 直近 1 時間の窓 (時刻の順に来る前提。戻った標本も入れて、古いものは時刻で落とす)
            self.recent.push_back((s.t, v.num, v.den));
            let newest = self.recent.iter().map(|r| r.0).max().unwrap_or(s.t);
            while self
                .recent
                .front()
                .is_some_and(|r| r.0 + RATE_WINDOW_SECS <= newest)
            {
                self.recent.pop_front();
            }
        }
        let key = s.t - s.t % HOUR;
        if self.hours.back().is_none_or(|h| h.t < key) {
            if self.hours.len() >= HOURS {
                self.hours.pop_front();
            }
            self.hours.push_back(Slot {
                t: key,
                ..Slot::default()
            });
        }
        let Some(slot) = self.hours.iter_mut().rev().find(|h| h.t == key) else {
            return v;
        };
        for i in 0..RATES {
            slot.num[i] += v.num[i];
            slot.den[i] += v.den[i];
        }
        if !v.judged {
            return v;
        }
        slot.judged += 1;
        if v.fast() {
            slot.fast += 1;
        }
        for i in 0..2 {
            if v.bits & (1 << i) != 0 {
                slot.miss_speed[i] += 1;
                slot.worst_speed[i] = slot.worst_speed[i].max(v.values[i]);
            }
        }
        v
    }

    /// その時間の宛先の表を受ける (T.30.4。履歴スレッドが 5 秒ごとに、確定した前の時間と
    /// いまの時間の途中の集計を渡す)。同じ時間の行は差し替える。エラーの無い時間は持たない。
    pub fn observe_destinations(&mut self, prev: Option<&DestHour>, cur: &DestHour) {
        for d in prev.into_iter().chain(std::iter::once(cur)) {
            if d.errors == 0 || d.hour == 0 {
                continue;
            }
            match self.dests.binary_search_by_key(&d.hour, |x| x.hour) {
                Ok(i) => {
                    if self.dests[i] != *d {
                        self.dests[i] = d.clone();
                    }
                }
                Err(i) => {
                    self.dests.insert(i, d.clone());
                    while self.dests.len() > HOURS {
                        self.dests.pop_front();
                    }
                }
            }
        }
    }

    /// その時間の宛先の表 (無ければ `None`)。
    fn dest_of(&self, t: u64) -> Option<&DestHour> {
        self.dests
            .binary_search_by_key(&t, |x| x.hour)
            .ok()
            .map(|i| &self.dests[i])
    }

    /// 時間 1 つに閾を当てる (その時間の宛先の表も渡す)。
    fn tally(&self, s: &Slot, th: &Thresholds) -> Hour {
        s.tally(th, self.dest_of(s.t))
    }

    /// その時間に `error_rate` から外した宛先 (外していなければ `None`。T.30.4)。
    pub fn excluded_destination(&self, t: u64) -> Option<&DestHour> {
        self.dest_of(t).filter(|d| excludes(d))
    }

    /// いま持っている時間ごとの集計 (古い順。最後に受けた閾を当てる)。**読むのは試験だけ**
    /// (本番は [`Tracker::report`] / [`Tracker::rate_hours_of_day`] が閾を渡して畳む)。
    #[cfg(test)]
    pub fn hours(&self) -> impl Iterator<Item = Hour> + '_ {
        let th = self.th.unwrap_or_default();
        self.hours.iter().map(move |s| self.tally(s, &th))
    }

    /// 直近 1 時間 (`now − 1 時間 < t ≤ now`) の比の合計。
    pub fn last_hour(&self, now: u64) -> LastHour {
        let from = now.saturating_sub(RATE_WINDOW_SECS);
        let mut out = LastHour {
            from,
            to: now,
            ..LastHour::default()
        };
        for (_, num, den) in self.recent.iter().filter(|r| r.0 > from && r.0 <= now) {
            out.samples += 1;
            for i in 0..RATES {
                out.num[i] += num[i];
                out.den[i] += den[i];
            }
        }
        // 外した宛先 (T.30.4): いまの時間 (窓にまるごと入る) はそのまま、前の時間は窓に掛かる秒の割合で按分
        let cur = now - now % HOUR;
        let share = HOUR - now % HOUR;
        for (t, part) in [(cur, HOUR), (cur.saturating_sub(HOUR), share)] {
            if t == 0 {
                continue;
            }
            let Some(d) = self.excluded_destination(t) else {
                continue;
            };
            let e = (d.top_errors * part / HOUR).min(out.num[0]);
            if e == 0 {
                // 窓に掛かるぶんにエラーが無い (前の時間の早いうちのエラー) なら試行も引かない
                continue;
            }
            let a = (d.top_attempts.max(d.top_errors) * part / HOUR)
                .max(e)
                .min(out.den[0]);
            out.num[0] -= e;
            out.den[0] -= a;
            out.excluded[0] += e;
            out.excluded[1] += a;
            out.destinations.push(d.destination.clone());
        }
        out
    }

    /// その日 (UTC の 0 時 `day_t` から 24 時間) の、比ごとの `[満たした時間, 外した時間]` (日次の 1 行。T.29.33)。
    pub fn rate_hours_of_day(&self, th: &Thresholds, day_t: u64) -> [[u32; 2]; RATES] {
        let mut out = [[0u32; 2]; RATES];
        for h in self
            .hours
            .iter()
            .filter(|h| h.t >= day_t && h.t < day_t + DAY)
            .map(|s| self.tally(s, th))
        {
            for (o, r) in out.iter_mut().zip(h.rate_hours) {
                o[0] += r[0];
                o[1] += r[1];
            }
        }
        out
    }

    /// `days` 日ぶん (**今日を含む。「今日」は UTC**) を畳む。
    pub fn report(&self, th: &Thresholds, now: u64, days: u64) -> Report {
        let days = days.clamp(1, MAX_DAYS);
        let today = now - now % DAY;
        let from = today.saturating_sub((days - 1) * DAY);
        let mut out = Report {
            now,
            days,
            from,
            to: now,
            thresholds: *th,
            today_t: today,
            last_hour: self.last_hour(now),
            ..Report::default()
        };
        for h in self
            .hours
            .iter()
            .filter(|h| h.t >= from)
            .map(|s| self.tally(s, th))
        {
            out.total.add(&h);
            if out.first_t == 0 {
                out.first_t = h.t;
            }
            out.last_t = h.t;
            let day = h.t - h.t % DAY;
            match out.daily.last_mut() {
                Some(d) if d.t == day => d.add(&h),
                _ => out.daily.push(Hour { t: day, ..h }),
            }
            // 外した宛先の名前 (T.30.4。`hourly` の末尾と `breaches` の `excluded` が読む)
            if h.excluded_hours > 0
                && let Some(d) = self.dest_of(h.t)
            {
                out.excluded.push(d.clone());
            }
            // 外れた時間は**連続していれば 1 行に畳む** (17 時から 23 時まで = 1 件)
            if h.breached() {
                match out.breaches.last_mut() {
                    Some(b) if b.to == h.t => {
                        b.to = h.t + HOUR;
                        b.hours += 1;
                        b.tally.add(&h);
                    }
                    _ => out.breaches.push(Breach {
                        from: h.t,
                        to: h.t + HOUR,
                        hours: 1,
                        tally: h,
                    }),
                }
            }
            out.hourly.push(h);
        }
        out
    }
}

/// 外れた時間帯 1 行 (**連続する外れを畳んだもの**)。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Breach {
    /// 始まり (最初の時間の 0 分。epoch 秒)
    pub from: u64,
    /// 終わり (最後の時間の**次**の 0 分。epoch 秒)
    pub to: u64,
    /// 何時間続いたか
    pub hours: u32,
    /// その間の集計 (`t` は最初の時間)
    pub tally: Hour,
}

/// `/slo` の中身。
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub now: u64,
    pub days: u64,
    /// 期間の始まり (`days − 1` 日前の UTC 0 時) と終わり (いま)
    pub from: u64,
    pub to: u64,
    pub thresholds: Thresholds,
    /// 期間ぜんぶの集計 (`t` は使わない)
    pub total: Hour,
    /// 実際に集計があった最初と最後の時間 (無ければ 0)
    pub first_t: u64,
    pub last_t: u64,
    /// 今日 (UTC) の 0 時
    pub today_t: u64,
    /// 日ごと (古い順。`t` はその日の 0 時)
    pub daily: Vec<Hour>,
    /// 時間ごと (古い順)
    pub hourly: Vec<Hour>,
    /// 外れた時間帯 (古い順)
    pub breaches: Vec<Breach>,
    /// 直近 1 時間の比の合計 (T.29.33)
    pub last_hour: LastHour,
    /// `error_rate` から宛先を外した時間 (古い順。T.30.4)
    pub excluded: Vec<DestHour>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 時間の積み上げに閾を当てる: 比を満たせば `met` は速さを満たした 5 秒の数、比を外せばその時間の
    /// 5 秒を全部「達成していない」と数え、分母 0 の比は判定しない (T.29.33。T.34.2 で直に押さえる)。
    #[test]
    fn tally_turns_a_slot_into_an_hour_with_the_rates_judged_on_the_sum() {
        let th = super::super::DEFAULT;
        let slot = Slot {
            t: 3600,
            judged: 720,
            fast: 700,
            miss_speed: [20, 5],
            worst_speed: [7.5, 60.0],
            // error_rate 4 / 1,004 = 0.0040 (満たす)、dns_miss 10 / 1,000 = 0.01 (満たす)
            num: [4, 10],
            den: [1004, 1000],
        };
        let h = slot.tally(&th, None);
        assert_eq!((h.t, h.judged, h.met), (3600, 720, 700));
        assert_eq!(h.miss, [20, 5, 0, 0]);
        assert_eq!(h.worst, [7.5, 60.0, 0.0, 0.0]);
        assert_eq!(h.rate_hours, [[1, 0], [1, 0]]);
        assert_eq!((h.excluded, h.excluded_hours), ([0, 0], 0));
        // エラー率を外す時間 (6 / 1,006 = 0.0060 > 0.005): その時間の 5 秒は全部達成していない
        let bad = Slot {
            num: [6, 10],
            den: [1006, 1000],
            ..slot
        };
        let h = bad.tally(&th, None);
        assert_eq!(h.met, 0);
        assert_eq!(h.miss, [20, 5, 720, 0]);
        assert!((h.worst[2] - 6.0 / 1006.0).abs() < 1e-12);
        assert_eq!(h.rate_hours, [[0, 1], [1, 0]]);
        // 分母 0 の比はどちらにも数えない
        let quiet = Slot {
            num: [0, 0],
            den: [0, 0],
            ..slot
        };
        assert_eq!(quiet.tally(&th, None).rate_hours, [[0, 0], [0, 0]]);
        // 偏った宛先を外すと分子と分母から引かれ、外した数が残る (T.30.4)
        let dest = DestHour {
            hour: 3600,
            errors: 6,
            destination: "connect://x:443".to_string(),
            top_errors: 5,
            top_attempts: 5,
        };
        let h = bad.tally(&th, Some(&dest));
        assert_eq!((h.num[0], h.den[0]), (1, 1001));
        assert_eq!((h.excluded, h.excluded_hours), ([5, 5], 1));
        assert_eq!(h.met, 700, "外したら 1 / 1,001 で満たす");
    }

    /// 日・期間へ足すときは数は和、最悪の値は最大 (T.34.2 で直に押さえる)。
    #[test]
    fn hours_add_up_and_keep_the_worst_value() {
        let mut a = Hour {
            t: 0,
            judged: 10,
            met: 8,
            miss: [1, 2, 0, 0],
            worst: [3.0, 9.0, 0.0, 0.0],
            num: [1, 2],
            den: [100, 10],
            rate_hours: [[1, 0], [1, 0]],
            excluded: [0, 0],
            excluded_hours: 0,
        };
        let b = Hour {
            t: 3600,
            judged: 5,
            met: 0,
            miss: [0, 0, 5, 0],
            worst: [0.0, 12.0, 0.5, 0.0],
            num: [3, 0],
            den: [6, 0],
            rate_hours: [[0, 1], [0, 0]],
            excluded: [2, 2],
            excluded_hours: 1,
        };
        a.add(&b);
        assert_eq!((a.t, a.judged, a.met), (0, 15, 8), "`t` は動かない");
        assert_eq!(a.miss, [1, 2, 5, 0]);
        assert_eq!(a.worst, [3.0, 12.0, 0.5, 0.0]);
        assert_eq!((a.num, a.den), ([4, 2], [106, 10]));
        assert_eq!(a.rate_hours, [[1, 1], [1, 0]]);
        assert_eq!((a.excluded, a.excluded_hours), ([2, 2], 1));
    }
}
