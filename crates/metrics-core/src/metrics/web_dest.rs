//! `/slo` の `error_rate` が外す「1 つの宛先に偏ったエラー」を数える (T.30.4。利用者の決定)。
//!
//! T.30.3 で 443・80 宛てだけにしたあとも、0.30.3 の 10-29 の最初の 6 時間は 4 時間で外れ、
//! 443・80 宛てのエラー 16 件は全部 `pravda.me:443` の確立の時間切れ (1 つのサイトが応答しない) だった。
//! `/slo` は「プロキシの調子」を見る物差しなので、宛先 1 つの事情は外す。判定は `proxy-metrics-slo` の
//! `slo` が [`DestHour`] を読んでする (エラーが 2 件以上で、いちばん多い宛先がその時間のエラーの半分以上なら、
//! その宛先の試行とエラーを分子と分母から外す)。ここは**数えるだけ**。
//!
//! **持ち方**: 宛先ごとの数はホスト別統計の行 ([`HostStats::web_hour`]) に持つ (覚える宛先の数の上限は
//! ホスト別統計と同じ [`super::MAX_HOSTS`]。溢れて `other` に畳まれた行は宛先として数えない)。表の側
//! ([`DestTable`]) に持つのは、いまの時間・その時間のエラーの合計・いちばん多い宛先の名前と件数と、
//! 確定した前の時間の 1 行だけ。
//!
//! **熱い経路に足したのはエラーのときだけ**: 成功の本数は、時間の区切りでホスト別統計の行の
//! `timed` (応答時間を測った要求の通算) と `timed_errors` を写しておき、その差で出す
//! (`/slo` の試行の成功 = 窓に入った成功 = 応答時間を測った成功)。区切りで全部の行 (最大 1,000) を
//! 1 回舐めるのは 1 時間に 1 回。
//!
//! **時間の区切り**: エラーの時刻 (要求の経路が読んだ壁時計) と、履歴スレッドの 5 秒ごとの
//! [`super::Metrics::web_error_hours`] で切り替える。`/slo` の 5 秒の標本は「標本を取った時刻の時間」に
//! 入るので、区切りの前後の 5 秒ぶんは 1 時間ずれることがある (成功の写しは区切りのあとの最初の
//! エラーか 5 秒ごとの読みのときに取るので、その間の成功は前の時間に入る)。

use super::HostStats;
use std::collections::HashMap;

const HOUR: u64 = 3600;

/// 1 時間ぶんの「いちばん多くエラーを出した 443・80 の宛先」(T.30.4。`/slo` が読む)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DestHour {
    /// その時間の 0 分 (epoch 秒)
    pub hour: u64,
    /// その時間の、名前解決に成功した 443・80 宛てのエラーの合計 (`other` に畳まれた宛先のぶんも入る)
    pub errors: u64,
    /// いちばん多くエラーを出した宛先 (ホスト別統計の鍵)。エラーが 0 件なら空
    pub destination: String,
    /// その宛先のエラーと試行 (試行 = 窓に入った成功 + 名前解決以外のエラー。`/slo` の試行と同じ数え方)
    pub top_errors: u64,
    pub top_attempts: u64,
}

/// 宛先ごとの数の表の側 (ホスト別統計と同じ鍵の内側に置く)。
#[derive(Debug, Default)]
pub(super) struct DestTable {
    /// いま数えている時間 (0 = まだ 1 回も切り替えていない)
    hour: u64,
    errors: u64,
    top: String,
    top_errors: u64,
    /// 確定した前の時間 (切り替えたときに写す)
    prev: Option<DestHour>,
}

impl DestTable {
    /// `now` の時間に切り替える (変わっていなければ比較 1 回で戻る)。
    ///
    /// 切り替えるときは、いまの時間を [`DestHour`] にして `prev` に写し、全部の行の
    /// `timed` / `timed_errors` を写す (ここからの差がこの時間の成功)。
    pub(super) fn roll(&mut self, map: &mut HashMap<String, HostStats>, now: u64) {
        let hour = now - now % HOUR;
        if hour == self.hour {
            return;
        }
        if self.hour != 0 {
            self.prev = Some(self.current(map));
        }
        for s in map.values_mut() {
            s.web_hour.base_timed = s.timed;
            s.web_hour.base_timed_errors = s.web_hour.timed_errors;
        }
        self.hour = hour;
        self.errors = 0;
        self.top.clear();
        self.top_errors = 0;
    }

    /// エラー 1 件 (ホスト別統計の行を数えたあと。**エラーの経路だけ**から呼ぶ)。
    ///
    /// `timed` は応答時間を測ったエラーか、`web` は名前解決に成功した 443・80 宛てのエラーか。
    /// `key` は行の鍵 (`other` に畳まれたときは `other`)。`roll` を先に済ませてあること。
    pub(super) fn on_error(&mut self, stats: &mut HostStats, key: &str, timed: bool, web: bool) {
        let w = &mut stats.web_hour;
        w.timed_errors += timed as u64;
        if !web {
            return;
        }
        self.errors += 1;
        if key == "other" {
            // 溢れた宛先は 1 つの宛先として数えない (合計には入れる)
            return;
        }
        if w.hour != self.hour {
            w.hour = self.hour;
            w.errors = 0;
        }
        w.errors += 1;
        if w.errors > self.top_errors {
            self.top_errors = w.errors;
            if self.top != key {
                self.top.clear();
                self.top.push_str(key);
            }
        }
    }

    /// いまの時間の途中の集計。
    pub(super) fn current(&self, map: &HashMap<String, HostStats>) -> DestHour {
        let mut out = DestHour {
            hour: self.hour,
            errors: self.errors,
            ..DestHour::default()
        };
        if self.top_errors == 0 {
            return out;
        }
        let Some(s) = map.get(&self.top) else {
            return out;
        };
        let w = &s.web_hour;
        let errors = if w.hour == self.hour { w.errors } else { 0 };
        // 窓に入る成功 (forward の `blocked://`・`loop://` は窓に入らない。CONNECT はいつも入る)
        let counted = self.top.starts_with("connect://")
            || !(self.top.starts_with("blocked://") || self.top.starts_with("loop://"));
        let ok = if counted {
            s.timed
                .saturating_sub(w.base_timed)
                .saturating_sub(w.timed_errors.saturating_sub(w.base_timed_errors))
        } else {
            0
        };
        out.destination = self.top.clone();
        out.top_errors = errors;
        out.top_attempts = ok + errors;
        out
    }

    /// 確定した前の時間 (無ければ `None`)。
    pub(super) fn prev(&self) -> Option<DestHour> {
        self.prev.clone()
    }
}
