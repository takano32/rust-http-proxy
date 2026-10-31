//! `/slo` の `error_rate` が外す「1 つの宛先に偏ったエラー」を数える (T.30.4。利用者の決定)。
//!
//! T.30.3 で 443・80 宛てだけにしたあとも、0.30.3 の 10-29 の最初の 6 時間は 4 時間で外れ、
//! 443・80 宛てのエラー 16 件は全部 `pravda.me:443` の確立の時間切れ (1 つのサイトが応答しない) だった。
//! `/slo` は「プロキシの調子」を見る物差しなので、宛先 1 つの事情は外す。判定は `proxy-metrics-slo` の
//! `slo` が [`DestHour`] を読んでする (エラーが 2 件以上で、いちばん多い宛先がその時間のエラーの半分以上なら、
//! その宛先の試行とエラーを分子と分母から外す)。ここは**数えるだけ**。
//!
//! **持ち方 (T.32.10 で変えた)**: その時間の宛先ごとのエラーの数は、ホスト別統計から切り離した小さな表
//! ([`DestTable`] の `counts`、[`MAX_DESTS`] 行まで) に持つ。前はホスト別統計の行に持っていたので、表が
//! 1,000 行で満杯になって `other` に畳まれた宛先 (tokyo の 0.30.4 では `pravda.me:443` も 10-30 20Z の 2 つも)
//! は宛先として数えられず、48 時間で 1 時間も外せなかった。小さな表はその時間のエラーの宛先だけを持ち、
//! 時間の区切りで空にする。溢れた宛先は合計にだけ入る。表の側に持つのはほかに、いまの時間・その時間の
//! エラーの合計・いちばん多い宛先の名前と件数と、確定した前の時間の 1 行。
//!
//! いちばん多い宛先の**成功**の本数だけはホスト別統計の行 ([`HostStats::web_hour`]) から読む (下)。行が
//! 無ければ (その時間のうちに満杯の表から捨てられた) 成功は 0 と見る (外す試行が少なめになるだけ)。
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
/// 1 時間に覚える「エラーを出した 443・80 の宛先」の数の上限 (T.32.10)。
///
/// tokyo の 0.30.4 の 48 時間で、443・80 のエラーのあった時間の宛先は多くても数個。外すのは
/// 「その時間のエラーの半分以上」を出した 1 つだけなので、64 を越えるほどばらけた時間に
/// 外す宛先は無い (溢れた宛先を数えなくても判定は変わらない)。1 行は名前 + 8 バイト。
pub(super) const MAX_DESTS: usize = 64;

/// 1 時間ぶんの「いちばん多くエラーを出した 443・80 の宛先」(T.30.4。`/slo` が読む)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DestHour {
    /// その時間の 0 分 (epoch 秒)
    pub hour: u64,
    /// その時間の、名前解決に成功した 443・80 宛てのエラーの合計 (小さな表から溢れた宛先のぶんも入る)
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
    /// その時間の宛先ごとのエラー (ホスト別統計の鍵, 件数)。[`MAX_DESTS`] 行まで (T.32.10)
    counts: Vec<(String, u64)>,
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
        self.counts.clear();
        self.top.clear();
        self.top_errors = 0;
    }

    /// エラー 1 件 (ホスト別統計の行を数えたあと。**エラーの経路だけ**から呼ぶ)。
    ///
    /// `timed` は応答時間を測ったエラーか、`web` は名前解決に成功した 443・80 宛てのエラーか。
    /// `stats` はそのエラーを数えた行 (`other` に畳まれたときは `other` の行)、`key` は**宛先の鍵**
    /// (畳まれても本当の宛先。T.32.10)。`roll` を先に済ませてあること。
    pub(super) fn on_error(&mut self, stats: &mut HostStats, key: &str, timed: bool, web: bool) {
        stats.web_hour.timed_errors += timed as u64;
        if !web {
            return;
        }
        self.errors += 1;
        // 宛先は小さな表で数える (64 行を舐めるのはエラーの経路だけ)
        let n = match self.counts.iter().position(|(k, _)| k == key) {
            Some(i) => {
                self.counts[i].1 += 1;
                self.counts[i].1
            }
            None if self.counts.len() < MAX_DESTS => {
                self.counts.push((key.to_string(), 1));
                1
            }
            // 溢れた宛先は 1 つの宛先として数えない (合計には入れる)
            None => return,
        };
        if n > self.top_errors {
            self.top_errors = n;
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
        let errors = self.top_errors;
        // 窓に入る成功 (forward の `blocked://`・`loop://` は窓に入らない。CONNECT はいつも入る)。
        // 成功の本数はホスト別統計の行から (行が無ければ 0)
        let counted = self.top.starts_with("connect://")
            || !(self.top.starts_with("blocked://") || self.top.starts_with("loop://"));
        let ok = match map.get(&self.top) {
            Some(s) if counted => {
                let w = &s.web_hour;
                s.timed
                    .saturating_sub(w.base_timed)
                    .saturating_sub(w.timed_errors.saturating_sub(w.base_timed_errors))
            }
            _ => 0,
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
