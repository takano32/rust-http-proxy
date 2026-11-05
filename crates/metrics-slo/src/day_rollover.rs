//! 「UTC の日の境目で 1 回だけ書く」判断 (T34.50)。
//!
//! 日次の要約 ([`crate::daily`]) と日次の雪像 ([`crate::snapshots`]) の `Writer` が 1 つずつ持つ。
//! 前は 2 つの `Writer::add` が同じ判断を別々に書いていた。ここが持つのは**いま見ている日**と
//! **最後に書いた日**だけで、何を書くか (要約の 1 行 / `/snapshot` の 1 枚) と、境目の標本を
//! どちらの日に足すか (要約は終わる日に足す) は呼ぶ側が決める。

/// 1 日の秒。
const SECS_PER_DAY: u64 = 86_400;

/// いま見ている日と、最後に書いた日 (どちらも epoch 秒 / 86,400)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DayRollover {
    /// いま見ている日 (最初の標本で決まる。0 = まだ 1 本も見ていない)
    day: u64,
    /// 最後に書いた日 (起動時にファイルから読む。同じ日を 2 度書かないため)
    last_written: u64,
}

impl DayRollover {
    /// 最後に書いた日を渡して作る (まだ 1 本も見ていない)。
    pub(crate) fn new(last_written: u64) -> DayRollover {
        DayRollover {
            day: 0,
            last_written,
        }
    }

    /// 標本の時刻 `t` を 1 つ見る。**日が変わっていたら終わった日**を返す。
    ///
    /// 最初の 1 本はその日を覚えるだけで `None`、同じ日も `None`。変わったときは今の日を
    /// `t` の日に進める (時計が戻って前の日に入ったときも「変わった」と見る。割る前と同じ)。
    /// その日を既に書いたかは [`DayRollover::written`] で見る。
    pub(crate) fn cross(&mut self, t: u64) -> Option<u64> {
        let day = t / SECS_PER_DAY;
        match self.day {
            0 => {
                self.day = day;
                None
            }
            cur if cur == day => None,
            cur => {
                self.day = day;
                Some(cur)
            }
        }
    }

    /// その日のぶんを既に書いたか (同じ日に 2 回起動した / 別のプロセスが書いた)。
    pub(crate) fn written(&self, day: u64) -> bool {
        day <= self.last_written
    }

    /// その日を書けたことを覚える (書けなかったときは呼ばない)。
    pub(crate) fn mark_written(&mut self, day: u64) {
        self.last_written = day;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = SECS_PER_DAY;

    #[test]
    fn the_first_sample_only_remembers_its_day() {
        let mut r = DayRollover::new(0);
        assert_eq!(r.cross(20_000 * DAY + 5), None);
        assert_eq!(r.cross(20_000 * DAY + 86_395), None, "同じ日");
    }

    #[test]
    fn the_next_day_returns_the_finished_day_once() {
        let mut r = DayRollover::new(0);
        r.cross(20_000 * DAY + 100);
        assert_eq!(r.cross(20_001 * DAY), Some(20_000), "翌日の最初の標本");
        assert_eq!(r.cross(20_001 * DAY + 5), None, "同じ境目で 2 度は返さない");
        assert!(!r.written(20_000));
        r.mark_written(20_000);
        assert!(r.written(20_000));
    }

    #[test]
    fn a_day_already_written_is_reported_as_written() {
        // 起動時に読んだ最後の行 (ファイル) が今日のもの
        let mut r = DayRollover::new(20_000);
        r.cross(20_000 * DAY + 100);
        let done = r.cross(20_001 * DAY + 100);
        assert_eq!(done, Some(20_000));
        assert!(r.written(20_000), "既に書いた日");
        assert!(!r.written(20_001));
        // 書けなかった (mark_written を呼ばない) 日は次も書いていない扱い
        assert_eq!(r.cross(20_002 * DAY), Some(20_001));
        assert!(!r.written(20_001));
    }

    #[test]
    fn skipping_two_days_returns_the_last_day_seen() {
        let mut r = DayRollover::new(0);
        r.cross(20_000 * DAY + 100);
        // 2 日ぶん止まっていた (間の日は見ていないので書かない)
        assert_eq!(r.cross(20_003 * DAY + 100), Some(20_000));
        assert_eq!(r.cross(20_003 * DAY + 200), None);
        assert_eq!(r.cross(20_004 * DAY), Some(20_003));
    }
}
