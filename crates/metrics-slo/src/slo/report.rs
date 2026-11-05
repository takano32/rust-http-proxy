//! `/slo` の応答を組む ([`Report::to_json`]。`slo` の一部。T.34.2 で `slo.rs` から移した)。
//!
//! ここに在るのは**文字列を組む所だけ**: 時間ごとの集計 ([`Report`] / [`Hour`] / [`Breach`]) は
//! `tracker.rs` が作り、ここはそれを `/slo` の形 (鍵の順は末尾に足すだけ) に書く。[`MAX_BODY`]
//! を越えるぶんは時間ごとの行から落として `truncated` を立てる。数の書き方 ([`num`] /
//! [`ratio_json`]) と時刻の札 ([`hour_label`]) は純粋な関数。
//!
//! 行を動かしただけで文字は変えていない (`to_json` は書く順のまま 5 つの段に割った)。

use std::fmt::Write as _;

use super::judge::{RATE_AT, RATE_WINDOW_SECS, RATES};
use super::tracker::{Breach, Hour, Report};
use super::{BASIS_JSON, HOURS, MAX_BODY, NAMES, Thresholds};
use crate::metrics::DestHour;

/// 締めくくり (`breaches` の残りと `truncated`) に残しておくぶん。
const TRAILER: usize = 2 * 1024;
/// 外れた時間帯の行数の上限 (これを越えたら `truncated`)。
pub(crate) const MAX_BREACHES: usize = 128;

impl Report {
    /// 今日 (UTC) の 1 日ぶん。
    pub fn today(&self) -> Hour {
        self.daily
            .iter()
            .find(|d| d.t == self.today_t)
            .copied()
            .unwrap_or(Hour {
                t: self.today_t,
                ..Hour::default()
            })
    }

    /// `/slo` の応答 (**[`MAX_BODY`] 以下**。越えるぶんは時間ごとの行から落として `truncated`)。
    ///
    /// 段は 5 つ (T.34.2 で割った。書く順と文字は変えていない): 頭と期間の合計 → 今日と日ごと →
    /// 外れた時間帯 → 時間ごと → 締めくくり (直近 1 時間)。
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(8 * 1024);
        self.push_head(&mut out);
        self.push_days(&mut out);
        let mut cut = self.push_breaches(&mut out);
        cut |= self.push_hourly(&mut out);
        self.push_tail(&mut out, cut);
        out
    }

    /// 応答の形の版・期間・閾・名前と、期間ぜんぶの集計 (`today` の鍵まで)。
    fn push_head(&self, out: &mut String) {
        // 応答の形の版は**いちばん先頭の鍵** (T14.49)。`thresholds` は入れ子なので持たない
        out.push_str(crate::metrics::SCHEMA_HEAD);
        let _ = write!(
            out,
            "\"now\":{},\"days\":{},\"from\":{},\"to\":{},\"sample_secs\":{},\
             \"thresholds\":{},\"basis\":{},\"names\":[",
            self.now,
            self.days,
            self.from,
            self.to,
            crate::history::INTERVAL.as_secs(),
            self.thresholds.to_json(),
            BASIS_JSON,
        );
        for (i, n) in NAMES.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{}\"", n);
        }
        let _ = write!(
            out,
            "],\"judged\":{},\"met\":{},\"ratio\":{},\"misses\":{},\
             \"first_t\":{},\"last_t\":{},\"hours\":{},\"kept_hours\":{},\"today\":",
            self.total.judged,
            self.total.met,
            ratio_json(self.total.ratio()),
            miss_json(&self.total.miss),
            self.first_t,
            self.last_t,
            self.hourly.len(),
            HOURS,
        );
    }

    /// 今日の 1 日ぶんと、日ごとの行 (`days` 日ぶんなので最大 31 行)。
    fn push_days(&self, out: &mut String) {
        push_day(out, &self.today());
        out.push_str(",\"daily\":[");
        for (i, d) in self.daily.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            push_day(out, d);
        }
    }

    /// 外れた時間帯 (連続する外れは 1 行。どの閾を外したか・その値つき)。返すのは打ち切ったか。
    fn push_breaches(&self, out: &mut String) -> bool {
        out.push_str("],\"breaches\":[");
        let mut cut = self.breaches.len() > MAX_BREACHES;
        for (i, b) in self.breaches.iter().take(MAX_BREACHES).enumerate() {
            // 判定はコンマより前 (T.34.149。後だと打ち切ったとき `[{…},]` が残る。`hourly` と同じ順)
            if out.len() > MAX_BODY - TRAILER {
                cut = true;
                break;
            }
            if i > 0 {
                out.push(',');
            }
            push_breach(out, b, &self.thresholds, &self.excluded);
        }
        cut
    }

    /// 時間ごとの行。返すのは打ち切ったか。
    fn push_hourly(&self, out: &mut String) -> bool {
        // 時間ごとは**キーを 1 回だけ**出して配列の配列にする (`/history` と同じ作法。T12.4 (3))。
        // 末尾の `rate_sums` は比の 2 つの `[分子, 分母]` (T.29.33。`error_rate` は宛先を外したあと)、
        // そのうしろの `excluded` は外した宛先 (T.30.4。外していない時間は `null`)
        out.push_str(
            "],\"hourly_keys\":[\"t\",\"judged\",\"met\",\"misses\",\"rate_sums\",\"excluded\"],\
             \"hourly\":[",
        );
        let mut cut = false;
        let mut ex = self.excluded.iter().peekable();
        for (i, h) in self.hourly.iter().enumerate() {
            if out.len() > MAX_BODY - TRAILER {
                cut = true;
                break;
            }
            if i > 0 {
                out.push(',');
            }
            while ex.next_if(|d| d.hour < h.t).is_some() {}
            let dest = ex.next_if(|d| d.hour == h.t);
            let _ = write!(
                out,
                "[{},{},{},{},{},{}]",
                h.t,
                h.judged,
                h.met,
                miss_json(&h.miss),
                sums_json(h),
                dest.map_or_else(|| "null".to_string(), |d| excluded_json(d, h)),
            );
        }
        cut
    }

    /// 締めくくり: 打ち切ったか、比を合計する窓、直近 1 時間の合計。
    fn push_tail(&self, out: &mut String, cut: bool) {
        let _ = write!(out, "],\"truncated\":{},", cut);
        // **末尾に足した鍵** (T.29.33): 比を合計する窓と、直近 1 時間の合計
        let _ = write!(
            out,
            "\"rate_window_secs\":{},\"last_hour\":{{\"from\":{},\"to\":{},\"samples\":{},\"rates\":",
            RATE_WINDOW_SECS, self.last_hour.from, self.last_hour.to, self.last_hour.samples,
        );
        let lh = &self.last_hour;
        push_rates(
            out,
            &lh.num,
            &lh.den,
            Tail::Met(&self.thresholds),
            &format!(
                "\"errors\":{},\"attempts\":{},\"destinations\":{}",
                lh.excluded[0],
                lh.excluded[1],
                crate::json::list(&lh.destinations)
            ),
        );
        out.push_str("}}");
    }
}

/// `hourly` の末尾の 1 つ (T.30.4): `{"destination":"connect://pravda.example:443","errors":4,"attempts":4,"hour_errors":5}`
/// (`errors` / `attempts` は実際に引いた数、`hour_errors` はその時間の 443・80 宛てのエラーの合計)。
fn excluded_json(d: &DestHour, h: &Hour) -> String {
    format!(
        "{{\"destination\":{},\"errors\":{},\"attempts\":{},\"hour_errors\":{}}}",
        crate::json::quote(&d.destination),
        h.excluded[0],
        h.excluded[1],
        d.errors,
    )
}

fn push_day(out: &mut String, d: &Hour) {
    let (y, mo, dd, ..) = crate::log::civil_from_epoch(d.t);
    let _ = write!(
        out,
        "{{\"date\":\"{:04}-{:02}-{:02}\",\"t\":{},\"judged\":{},\"met\":{},\"ratio\":{},\"misses\":{},\"rates\":",
        y,
        mo,
        dd,
        d.t,
        d.judged,
        d.met,
        ratio_json(d.ratio()),
        miss_json(&d.miss),
    );
    // 比の 2 つの合計と、満たした / 外した時間の数 (T.29.33。閾は `/slo` の `thresholds`)。
    // `error_rate` には外した宛先の時間の数とエラー・試行も (T.30.4)
    push_rates(
        out,
        &d.num,
        &d.den,
        Tail::Hours(&d.rate_hours),
        &format!(
            "\"hours\":{},\"errors\":{},\"attempts\":{}",
            d.excluded_hours, d.excluded[0], d.excluded[1]
        ),
    );
    out.push('}');
}

/// 比の 1 つの値のあとに何を付けるか ([`push_rates`])。
enum Tail<'a> {
    /// 閾を当てた `met` (直近 1 時間)
    Met(&'a Thresholds),
    /// 満たした / 外した時間の数 (日ごと)
    Hours(&'a [[u32; 2]; RATES]),
}

/// 比の 2 つを `{"error_rate":{"num":..,"den":..,"value":..,…},...}` で書く (T.29.33)。
///
/// `value` (と `met`) は分母が 0 なら `null`。`error_rate` の末尾には `"excluded":{excluded}` を付ける
/// (T.30.4。外した宛先のぶん。`num` / `den` は外したあと)。
fn push_rates(
    out: &mut String,
    num: &[u64; RATES],
    den: &[u64; RATES],
    tail: Tail<'_>,
    excluded: &str,
) {
    out.push('{');
    for i in 0..RATES {
        if i > 0 {
            out.push(',');
        }
        let v = (den[i] > 0).then(|| num[i] as f64 / den[i] as f64);
        let _ = write!(
            out,
            "\"{}\":{{\"num\":{},\"den\":{},\"value\":{}",
            NAMES[RATE_AT + i],
            num[i],
            den[i],
            ratio_json(v),
        );
        match tail {
            Tail::Hours(h) => {
                let _ = write!(
                    out,
                    ",\"hours_met\":{},\"hours_missed\":{}",
                    h[i][0], h[i][1]
                );
            }
            Tail::Met(th) => {
                let met = v.map(|v| (v <= th.limits()[RATE_AT + i]).to_string());
                let _ = write!(out, ",\"met\":{}", met.as_deref().unwrap_or("null"));
            }
        }
        if i == 0 {
            let _ = write!(out, ",\"excluded\":{{{}}}", excluded);
        }
        out.push('}');
    }
    out.push('}');
}

fn sums_json(h: &Hour) -> String {
    format!("[[{},{}],[{},{}]]", h.num[0], h.den[0], h.num[1], h.den[1])
}

fn push_breach(out: &mut String, b: &Breach, th: &Thresholds, excluded: &[DestHour]) {
    let _ = write!(
        out,
        "{{\"from\":{},\"to\":{},\"from_hour\":\"{}\",\"to_hour\":\"{}\",\"hours\":{},\
         \"judged\":{},\"met\":{},\"ratio\":{},\"breached\":[",
        b.from,
        b.to,
        hour_label(b.from),
        hour_label(b.to),
        b.hours,
        b.tally.judged,
        b.tally.met,
        ratio_json(b.tally.ratio()),
    );
    let limits = th.limits();
    let mut first = true;
    for (i, name) in NAMES.iter().enumerate() {
        if b.tally.miss[i] == 0 {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        let _ = write!(
            out,
            "{{\"name\":\"{}\",\"samples\":{},\"worst\":{},\"threshold\":{}",
            name,
            b.tally.miss[i],
            num(b.tally.worst[i]),
            num(limits[i]),
        );
        // 比は外した時間の数と、その時間帯の合計の分子・分母も添える (T.29.33)
        if i >= RATE_AT {
            let r = i - RATE_AT;
            let _ = write!(
                out,
                ",\"hours\":{},\"num\":{},\"den\":{}",
                b.tally.rate_hours[r][1], b.tally.num[r], b.tally.den[r]
            );
        }
        out.push('}');
    }
    // その時間帯に `error_rate` から外した宛先 (T.30.4。外したあとも外れた時間。名前は重ねない)
    let mut names: Vec<&str> = Vec::new();
    for d in excluded
        .iter()
        .filter(|d| d.hour >= b.from && d.hour < b.to)
    {
        if !names.contains(&d.destination.as_str()) {
            names.push(&d.destination);
        }
    }
    let _ = write!(
        out,
        "],\"excluded\":{{\"hours\":{},\"errors\":{},\"attempts\":{},\"destinations\":{}}}}}",
        b.tally.excluded_hours,
        b.tally.excluded[0],
        b.tally.excluded[1],
        crate::json::list(names),
    );
}

/// `2026-09-11T17Z` (UTC。人が読むための札で、機械は `from` / `to` を読む)。
fn hour_label(t: u64) -> String {
    let (y, mo, d, h, ..) = crate::log::civil_from_epoch(t);
    format!("{:04}-{:02}-{:02}T{:02}Z", y, mo, d, h)
}

fn ratio_json(r: Option<f64>) -> String {
    match r {
        Some(v) => format!("{:.5}", v),
        None => "null".to_string(),
    }
}

fn miss_json(miss: &[u32; 4]) -> String {
    format!("[{},{},{},{}]", miss[0], miss[1], miss[2], miss[3])
}

/// 閾と値を JSON の数として書く (整数なら整数、そうでなければ意味のある桁だけ)。
pub(super) fn num(v: f64) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let s = format!("{:.6}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    if s.is_empty() { "0".to_string() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 閾と値は「整数なら整数、そうでなければ意味のある桁だけ」(末尾の 0 と `.` を落とす。
    /// T.34.2 で直に押さえる)。
    #[test]
    fn numbers_are_written_without_trailing_zeros() {
        assert_eq!(num(5.0), "5");
        assert_eq!(num(50.0), "50");
        assert_eq!(num(0.005), "0.005");
        assert_eq!(num(0.2), "0.2");
        assert_eq!(num(46.875), "46.875");
        assert_eq!(num(6.0 / 1006.0), "0.005964", "6 桁で丸める");
        assert_eq!(num(0.0), "0");
        assert_eq!(num(f64::NAN), "0", "数でなければ 0");
        assert_eq!(num(f64::INFINITY), "0");
        assert_eq!(
            num(1e15),
            "1000000000000000",
            "1e15 からは小数の書き方を通る"
        );
    }

    /// 達成率は小数 5 桁、判定が無ければ `null`。時刻の札は UTC の時まで。
    #[test]
    fn ratios_and_hour_labels_have_a_fixed_shape() {
        assert_eq!(ratio_json(Some(714.0 / 720.0)), "0.99167");
        assert_eq!(ratio_json(Some(1.0)), "1.00000");
        assert_eq!(ratio_json(None), "null");
        assert_eq!(miss_json(&[1, 2, 3, 4]), "[1,2,3,4]");
        // 2026-09-11 17:00 UTC
        assert_eq!(hour_label(1_789_084_800 + 17 * 3600), "2026-09-11T17Z");
        assert_eq!(hour_label(0), "1970-01-01T00Z");
        let h = Hour {
            num: [3, 7],
            den: [723, 400],
            ..Hour::default()
        };
        assert_eq!(sums_json(&h), "[[3,723],[7,400]]");
    }
}
