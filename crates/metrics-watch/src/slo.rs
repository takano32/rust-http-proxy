//! SLO の達成率 (`/slo`。T14.50)。
//!
//! Phase の完了の定義は「平常時の CONNECT 確立 p50 6 ms 以下」のような**閾値**だが、
//! デプロイ先で**時間の何割がそれを満たしたか**はどこにも出ない。閾を設定で持ち
//! ([`Thresholds`]、`PROXY_SLO`)、**5 秒の標本 1 本ごと**に満たしたかを判定して
//! ([`judge`]) 時間ごと・日ごとに数えれば、T14.99 と次の Phase の判定が
//! 「満たした / 満たさない」ではなく **「99.2% の時間で満たした。外れたのは 09-11 17〜23 時」**
//! になる。
//!
//! **費用は 0**: 判定するのは履歴スレッドの周期だけ ([`observe`] を
//! [`crate::history::spawn_every`] から 1 行呼ぶ。T14.23 の [`crate::anomaly::check`] の隣)
//! で、要求ごとの経路には 1 命令も無い。5 秒に 1 回、標本 1 本から分位点を 2 回引いて
//! 割り算を 2 回するだけで、リングの走査もシステムコールも無い。
//!
//! **判定 (4 ビット)**:
//!
//! | ビット | 名前 | 標本から取る値 | 満たす |
//! |---|---|---|---|
//! | 0 | `connect_p50_ms` | `connect` の窓の p50 | 閾以下 |
//! | 1 | `connect_p95_ms` | `connect` の窓の p95 | 閾以下 |
//! | 2 | `error_rate` | `errors ÷ (確立 + 転送 + errors)` | 閾以下 |
//! | 3 | `dns_miss_per_connect` | `dns_misses ÷ 確立` | 閾以下 |
//!
//! **達成 = 4 つとも満たす** (ビットが 1 つも立たない)。**確立が 1 本も無い標本は
//! 「判定なし」**で分母に入れない (誰も使っていない夜中を「達成」と数えると、
//! 達成率が「動いていた割合」に化ける)。
//!
//! **持ち方**: 判定した 4 ビットは [`Sample`] にも `.rrd` にも書かない
//! (判定は [`Sample`] から何度でも作り直せる)。積むのは**時間ごとの集計**
//! ([`Hour`] = 判定した数・達成した数・閾ごとに外した数と最悪の値) だけで、
//! メモリ上に [`HOURS`] 時間 (31 日) ぶん = 744 × 64 B ≈ 48 KiB。**再起動で消える**
//! (T14.4〜T14.8 の共通の決まり)。
//!
//! 5 秒の標本そのものは 6 時間 (T14.32)、60 秒は 1 日、3,600 秒は 30 日しか残らないので、
//! **`/slo?days=7` を「5 秒の標本ごと」で答えられるのはこの集計を積んでいるからだけ**
//! (`res=3600` のリングを畳み直すと 1 時間に 1 回の判定になり、99.2% のような数字は出ない)。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;

use crate::history::Sample;
use crate::sync::LockExt;

/// 閾の名前 (`PROXY_SLO` の綴りと `/slo` の `thresholds` のキー。ビットの順)。
pub const NAMES: [&str; 4] = [
    "connect_p50_ms",
    "connect_p95_ms",
    "error_rate",
    "dns_miss_per_connect",
];

/// 集計を残す時間数 (31 日)。1 時間 = [`Hour`] 64 B なので約 48 KiB。
pub const HOURS: usize = 24 * 31;

/// `/slo?days=` の既定と上限 ([`HOURS`] と揃える)。
pub const DEFAULT_DAYS: u64 = 7;
pub const MAX_DAYS: u64 = 31;

/// 応答の上限 (バイト。本文 T14.50 の「64 KiB 以下」)。
pub const MAX_BODY: usize = 64 * 1024;
/// 締めくくり (`breaches` の残りと `truncated`) に残しておくぶん。
const TRAILER: usize = 2 * 1024;
/// 外れた時間帯の行数の上限 (これを越えたら `truncated`)。
pub const MAX_BREACHES: usize = 128;

const HOUR: u64 = 3600;
const DAY: u64 = 86_400;

/// 4 つの閾 (`PROXY_SLO`)。**満たす = 値がこれ以下**。
///
/// 既定は本文 (T14.50) のとおり。`crates/config` の `Slo` が同じ既定を持っている
/// (層が下なのであちらからこの型は見えない。食い違ったら `tests/slo_test.rs` が落ちる)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// CONNECT 確立の p50 (ms)
    pub connect_p50_ms: f64,
    /// CONNECT 確立の p95 (ms)
    pub connect_p95_ms: f64,
    /// エラー ÷ 試み (確立 + 転送 + エラー)
    pub error_rate: f64,
    /// 名前解決のミス ÷ 確立
    pub dns_miss_per_connect: f64,
}

/// 既定の閾 (`PROXY_SLO=connect_p50_ms=10,connect_p95_ms=100,error_rate=0.005,dns_miss_per_connect=0.2`)。
pub const DEFAULT: Thresholds = Thresholds {
    connect_p50_ms: 10.0,
    connect_p95_ms: 100.0,
    error_rate: 0.005,
    dns_miss_per_connect: 0.2,
};

impl Default for Thresholds {
    fn default() -> Self {
        DEFAULT
    }
}

impl Thresholds {
    /// [`NAMES`] と同じ順の配列 (判定はこれを回すだけ)。
    pub fn limits(&self) -> [f64; 4] {
        [
            self.connect_p50_ms,
            self.connect_p95_ms,
            self.error_rate,
            self.dns_miss_per_connect,
        ]
    }

    /// `{"connect_p50_ms":10,...}`。
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(128);
        out.push('{');
        for (i, (name, v)) in NAMES.iter().zip(self.limits()).enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{}\":{}", name, num(v));
        }
        out.push('}');
        out
    }
}

/// 標本 1 本の判定。**`bits` が本文の「4 ビット」** (立っている = その閾を外した)。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Verdict {
    /// 判定できたか (**確立が 1 本以上あった**)。偽なら分母に入れない
    pub judged: bool,
    /// 外した閾のビット (`1 << i`、`i` は [`NAMES`] の位置)
    pub bits: u8,
    /// その標本の 4 つの値 (外れた時間帯の「その値」に使う)
    pub values: [f64; 4],
}

impl Verdict {
    /// 4 つとも満たしたか (判定できていないときは偽)。
    pub fn met(&self) -> bool {
        self.judged && self.bits == 0
    }
}

/// 標本 1 本を 4 つの閾に当てる (**時計もリングも触らない**ので試験しやすい)。
pub fn judge(th: &Thresholds, s: &Sample) -> Verdict {
    let connects = s.connect.count;
    if connects == 0 {
        // 確立が 1 本も無い標本は「判定なし」。分位点が 0 になるだけでなく、
        // 「誰も使っていない時間」を達成として数えると達成率の意味が変わる
        return Verdict::default();
    }
    let attempts = connects + s.forward.count + s.errors;
    let values = [
        s.connect.quantile_ms(0.5),
        s.connect.quantile_ms(0.95),
        if attempts == 0 {
            0.0
        } else {
            s.errors as f64 / attempts as f64
        },
        s.dns_misses as f64 / connects as f64,
    ];
    let limits = th.limits();
    let mut bits = 0u8;
    for (i, (v, lim)) in values.iter().zip(limits).enumerate() {
        if *v > lim {
            bits |= 1 << i;
        }
    }
    Verdict {
        judged: true,
        bits,
        values,
    }
}

/// 1 時間ぶんの集計 (**標本ごとの 4 ビットを畳んだもの**)。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Hour {
    /// その時間の 0 分 0 秒 (epoch 秒)
    pub t: u64,
    /// 判定できた標本の数 (確立が 1 本以上あったもの)
    pub judged: u32,
    /// そのうち 4 つとも満たした数
    pub met: u32,
    /// 閾ごとに外した標本の数 ([`NAMES`] の順)
    pub miss: [u32; 4],
    /// 閾ごとの最悪の値 (外した標本の中で。外していなければ 0)
    pub worst: [f64; 4],
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

    fn add(&mut self, o: &Hour) {
        self.judged += o.judged;
        self.met += o.met;
        for i in 0..4 {
            self.miss[i] += o.miss[i];
            self.worst[i] = self.worst[i].max(o.worst[i]);
        }
    }
}

/// 時間ごとの集計のリング (**触るのは履歴スレッドと `/slo` だけ**)。
#[derive(Debug, Default)]
pub struct Tracker {
    hours: VecDeque<Hour>,
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
        let v = judge(th, s);
        if !v.judged {
            return v;
        }
        let key = s.t - s.t % HOUR;
        if self.hours.back().is_none_or(|h| h.t < key) {
            if self.hours.len() >= HOURS {
                self.hours.pop_front();
            }
            self.hours.push_back(Hour {
                t: key,
                ..Hour::default()
            });
        }
        let Some(slot) = self.hours.iter_mut().rev().find(|h| h.t == key) else {
            return v;
        };
        slot.judged += 1;
        if v.bits == 0 {
            slot.met += 1;
        }
        for i in 0..4 {
            if v.bits & (1 << i) != 0 {
                slot.miss[i] += 1;
                slot.worst[i] = slot.worst[i].max(v.values[i]);
            }
        }
        v
    }

    /// いま持っている時間ごとの集計 (古い順)。
    pub fn hours(&self) -> impl Iterator<Item = &Hour> {
        self.hours.iter()
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
            ..Report::default()
        };
        for h in self.hours.iter().filter(|h| h.t >= from) {
            out.total.add(h);
            if out.first_t == 0 {
                out.first_t = h.t;
            }
            out.last_t = h.t;
            let day = h.t - h.t % DAY;
            match out.daily.last_mut() {
                Some(d) if d.t == day => d.add(h),
                _ => out.daily.push(Hour { t: day, ..*h }),
            }
            // 外れた時間は**連続していれば 1 行に畳む** (17 時から 23 時まで = 1 件)
            if h.breached() {
                match out.breaches.last_mut() {
                    Some(b) if b.to == h.t => {
                        b.to = h.t + HOUR;
                        b.hours += 1;
                        b.tally.add(h);
                    }
                    _ => out.breaches.push(Breach {
                        from: h.t,
                        to: h.t + HOUR,
                        hours: 1,
                        tally: *h,
                    }),
                }
            }
            out.hourly.push(*h);
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
}

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
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(8 * 1024);
        // 応答の形の版は**いちばん先頭の鍵** (T14.49)。`thresholds` は入れ子なので持たない
        out.push_str(crate::metrics::SCHEMA_HEAD);
        let _ = write!(
            out,
            "\"now\":{},\"days\":{},\"from\":{},\"to\":{},\"sample_secs\":{},\
             \"thresholds\":{},\"names\":[",
            self.now,
            self.days,
            self.from,
            self.to,
            crate::history::INTERVAL.as_secs(),
            self.thresholds.to_json(),
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
        push_day(&mut out, &self.today());
        // 日ごと (`days` 日ぶんなので最大 31 行)
        out.push_str(",\"daily\":[");
        for (i, d) in self.daily.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            push_day(&mut out, d);
        }
        // 外れた時間帯 (連続する外れは 1 行。どの閾を外したか・その値つき)
        out.push_str("],\"breaches\":[");
        let mut cut = self.breaches.len() > MAX_BREACHES;
        for (i, b) in self.breaches.iter().take(MAX_BREACHES).enumerate() {
            if i > 0 {
                out.push(',');
            }
            if out.len() > MAX_BODY - TRAILER {
                cut = true;
                break;
            }
            push_breach(&mut out, b, &self.thresholds);
        }
        // 時間ごとは**キーを 1 回だけ**出して配列の配列にする (`/history` と同じ作法。T12.4 (3))
        out.push_str("],\"hourly_keys\":[\"t\",\"judged\",\"met\",\"misses\"],\"hourly\":[");
        for (i, h) in self.hourly.iter().enumerate() {
            if out.len() > MAX_BODY - TRAILER {
                cut = true;
                break;
            }
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "[{},{},{},{}]",
                h.t,
                h.judged,
                h.met,
                miss_json(&h.miss)
            );
        }
        let _ = write!(out, "],\"truncated\":{}}}", cut);
        out
    }
}

fn push_day(out: &mut String, d: &Hour) {
    let (y, mo, dd, ..) = crate::log::civil_from_epoch(d.t);
    let _ = write!(
        out,
        "{{\"date\":\"{:04}-{:02}-{:02}\",\"t\":{},\"judged\":{},\"met\":{},\"ratio\":{},\"misses\":{}}}",
        y,
        mo,
        dd,
        d.t,
        d.judged,
        d.met,
        ratio_json(d.ratio()),
        miss_json(&d.miss),
    );
}

fn push_breach(out: &mut String, b: &Breach, th: &Thresholds) {
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
            "{{\"name\":\"{}\",\"samples\":{},\"worst\":{},\"threshold\":{}}}",
            name,
            b.tally.miss[i],
            num(b.tally.worst[i]),
            num(limits[i]),
        );
    }
    out.push_str("]}");
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
fn num(v: f64) -> String {
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

/// いま効いている閾 ([`configure`] が入れる。`crates/run/src/lib.rs` の起動時に 1 回)。
static THRESHOLDS: Mutex<Thresholds> = Mutex::new(DEFAULT);
/// 時間ごとの集計。**触るのは履歴スレッド (5 秒に 1 回) と `/slo` だけ**。
static TRACKER: Mutex<Option<Tracker>> = Mutex::new(None);

/// 閾を教える (`crates/run/src/lib.rs` の起動時に 1 回。`PROXY_SLO`)。
pub fn configure(th: Thresholds) {
    *THRESHOLDS.locked() = th;
}

/// いま効いている閾。
pub fn thresholds() -> Thresholds {
    *THRESHOLDS.locked()
}

/// 標本 1 本を判定して時間ごとの集計に足す (**履歴スレッドから 1 行**)。
pub fn observe(sample: &Sample) -> Verdict {
    let th = thresholds();
    let mut guard = TRACKER.locked();
    guard.get_or_insert_with(Tracker::new).observe(&th, sample)
}

/// `/slo?days=` の中身を組む (この口に来たときだけ畳む)。
pub fn report(now: u64, days: u64) -> Report {
    let th = thresholds();
    let guard = TRACKER.locked();
    match guard.as_ref() {
        Some(t) => t.report(&th, now, days),
        None => Tracker::new().report(&th, now, days),
    }
}

/// 覚えている集計を捨てる (テスト用)。
pub fn reset() {
    *TRACKER.locked() = None;
    *THRESHOLDS.locked() = DEFAULT;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 5 秒の標本 1 本。`connects` 本の確立を `ms` で置く。
    fn sample(t: u64, connects: u64, ms: u64) -> Sample {
        let mut s = Sample {
            t,
            ..Sample::default()
        };
        for _ in 0..connects {
            s.connect.observe(ms);
        }
        s
    }

    /// T14.0 の 2026-09-11 のバーストを模した 1 日 (UTC)。
    ///
    /// 5 秒の標本を 24 時間 (720 本 × 24 = 17,280 本) 並べ、**17〜23 時の 7 時間だけ**
    /// p50 / p95 とエラーが閾を越えるようにする (平常時は 1 標本 2 本を 3 ms、
    /// バーストは 7 本のうち 4 本を 30 ms・3 本を 300 ms にしてエラーを 1 件)。
    fn burst_day(t0: u64) -> Vec<Sample> {
        (0..24 * 720u64)
            .map(|i| {
                let t = t0 + i * 5;
                let hour = (t - t0) / HOUR;
                if (17..24).contains(&hour) {
                    // 4 本が 30 ms ([25,50] の区間) と 3 本が 300 ms ([250,500] の区間) で
                    // p50 = 25 + 25 × 3.5/4 = 46.875 (> 10)、p95 = 300 (> 100)。
                    // エラー 1 件 ÷ 試み 8 = 0.125 (> 0.005)
                    let mut s = sample(t, 0, 0);
                    for _ in 0..4 {
                        s.connect.observe(30);
                    }
                    for _ in 0..3 {
                        s.connect.observe(300);
                    }
                    s.errors = 1;
                    s
                } else {
                    // 2 本を 3 ms → p50 = p95 = 3 ms、エラー 0、名前解決のミス 0
                    sample(t, 2, 3)
                }
            })
            .collect()
    }

    #[test]
    fn a_sample_without_a_connect_is_not_judged() {
        let th = DEFAULT;
        let empty = Sample {
            t: 0,
            errors: 9,
            ..Sample::default()
        };
        let v = judge(&th, &empty);
        assert!(!v.judged, "{:?}", v);
        assert!(!v.met());
        // 分母にも入らない
        let mut tr = Tracker::new();
        tr.observe(&th, &empty);
        assert_eq!(tr.hours().count(), 0);
    }

    #[test]
    fn each_of_the_four_thresholds_sets_its_own_bit() {
        let th = DEFAULT;
        // 確立 1 本 3 ms、エラーも名前解決も無い = 4 つとも満たす
        let ok = sample(0, 1, 3);
        assert_eq!(judge(&th, &ok).bits, 0);
        assert!(judge(&th, &ok).met());
        // p50 と p95 (1 本しか無いので同じ値が両方に効く)
        let slow = sample(0, 1, 300);
        assert_eq!(judge(&th, &slow).bits, 0b0011, "{:?}", judge(&th, &slow));
        // エラー率だけ: 確立 100 本 + エラー 1 件 = 1/101 = 0.0099 > 0.005
        let mut err = sample(0, 100, 3);
        err.errors = 1;
        let v = judge(&th, &err);
        assert_eq!(v.bits, 0b0100, "{:?}", v);
        assert!((v.values[2] - 1.0 / 101.0).abs() < 1e-12, "{:?}", v);
        // 名前解決だけ: 確立 10 本にミス 3 回 = 0.3 > 0.2
        let mut dns = sample(0, 10, 3);
        dns.dns_misses = 3;
        let v = judge(&th, &dns);
        assert_eq!(v.bits, 0b1000, "{:?}", v);
        assert!((v.values[3] - 0.3).abs() < 1e-12, "{:?}", v);
        // 閾ちょうどは満たす (満たす = 閾以下)
        let mut edge = sample(0, 10, 3);
        edge.dns_misses = 2;
        assert_eq!(judge(&th, &edge).bits, 0);
    }

    /// **受け入れ基準**: 既知の標本列から達成率が手計算と一致し、外れた時間帯が 17〜23 時。
    #[test]
    fn the_burst_day_matches_the_hand_calculation() {
        let th = DEFAULT;
        // 2026-09-11 00:00:00 UTC
        let t0 = 1_789_084_800;
        assert_eq!(t0 % DAY, 0);
        let mut tr = Tracker::new();
        for s in burst_day(t0) {
            tr.observe(&th, &s);
        }
        let rep = tr.report(&th, t0 + DAY - 5, 1);
        // 手計算: 24 時間 × 720 標本 = 17,280 本すべて判定でき、
        // 外れるのは 17〜23 時の 7 時間 × 720 = 5,040 本。12,240 / 17,280 = 0.708333…
        assert_eq!(rep.total.judged, 17_280);
        assert_eq!(rep.total.met, 12_240);
        let ratio = rep.total.ratio().unwrap();
        assert!((ratio - 12_240.0 / 17_280.0).abs() < 1e-12, "{}", ratio);
        assert_eq!(rep.hourly.len(), 24);
        // 外れた時間帯は 1 行 (17 時から 24 時まで連続)
        assert_eq!(rep.breaches.len(), 1, "{:?}", rep.breaches);
        let b = rep.breaches[0];
        assert_eq!(b.from, t0 + 17 * HOUR);
        assert_eq!(b.to, t0 + 24 * HOUR);
        assert_eq!(b.hours, 7);
        assert_eq!(b.tally.judged, 5_040);
        assert_eq!(b.tally.met, 0);
        // 外したのは p50 / p95 / エラー率の 3 つで、名前解決は外していない
        assert_eq!(b.tally.miss, [5_040, 5_040, 5_040, 0], "{:?}", b);
        assert_eq!(b.tally.worst[0], 46.875, "{:?}", b.tally.worst);
        assert_eq!(b.tally.worst[1], 300.0, "{:?}", b.tally.worst);
        assert!(
            (b.tally.worst[2] - 0.125).abs() < 1e-12,
            "{:?}",
            b.tally.worst
        );
        assert_eq!(b.tally.worst[3], 0.0);
        // 日ごとは 1 行 (その日ぜんぶ)
        assert_eq!(rep.daily.len(), 1);
        assert_eq!(rep.daily[0].t, t0);
        assert_eq!(rep.daily[0].judged, 17_280);
        // 平常の時間は 100%、バーストの時間は 0%
        assert_eq!(rep.hourly[16].ratio(), Some(1.0));
        assert_eq!(rep.hourly[17].ratio(), Some(0.0));
        assert_eq!(rep.hourly[23].ratio(), Some(0.0));
    }

    /// 「99.2% の時間で満たした」の形 (1 時間の中で一部だけ外す)。
    #[test]
    fn a_partly_bad_hour_gives_a_fractional_ratio() {
        let th = DEFAULT;
        let t0 = 1_789_084_800;
        let mut tr = Tracker::new();
        // 1 時間 720 標本のうち 6 本だけ遅い = 714 / 720 = 0.991666…
        for i in 0..720u64 {
            let ms = if i < 6 { 300 } else { 3 };
            tr.observe(&th, &sample(t0 + i * 5, 1, ms));
        }
        let rep = tr.report(&th, t0 + HOUR, 1);
        assert_eq!((rep.total.judged, rep.total.met), (720, 714));
        assert_eq!(rep.breaches.len(), 1);
        assert_eq!(rep.breaches[0].hours, 1);
        let json = rep.to_json();
        assert!(json.contains("\"ratio\":0.99167"), "{}", json);
    }

    /// 外れた時間が離れていれば別の行、続いていれば 1 行。
    #[test]
    fn breaches_merge_only_while_they_are_next_to_each_other() {
        let th = DEFAULT;
        let t0 = 1_789_084_800;
        let mut tr = Tracker::new();
        // 0 時と 1 時が外れ、2 時は満たし、3 時がまた外れ → 2 行
        for (hour, ms) in [(0u64, 300u64), (1, 300), (2, 3), (3, 300)] {
            tr.observe(&th, &sample(t0 + hour * HOUR, 1, ms));
        }
        let rep = tr.report(&th, t0 + 4 * HOUR, 1);
        assert_eq!(rep.breaches.len(), 2, "{:?}", rep.breaches);
        assert_eq!(rep.breaches[0].hours, 2);
        assert_eq!(rep.breaches[0].from, t0);
        assert_eq!(rep.breaches[0].to, t0 + 2 * HOUR);
        assert_eq!(rep.breaches[1].hours, 1);
        assert_eq!(rep.breaches[1].from, t0 + 3 * HOUR);
        // 記録の無い時間は行にならない (穴は畳まない)
        let mut gap = Tracker::new();
        gap.observe(&th, &sample(t0, 1, 300));
        gap.observe(&th, &sample(t0 + 5 * HOUR, 1, 300));
        assert_eq!(gap.report(&th, t0 + 6 * HOUR, 1).breaches.len(), 2);
    }

    #[test]
    fn the_window_is_utc_days_and_today_is_the_last_one() {
        let th = DEFAULT;
        let t0 = 1_789_084_800; // 2026-09-11 00:00 UTC
        let mut tr = Tracker::new();
        for d in 0..9u64 {
            tr.observe(&th, &sample(t0 + d * DAY + 12 * HOUR, 1, 3));
        }
        let now = t0 + 8 * DAY + 20 * HOUR;
        let rep = tr.report(&th, now, 7);
        // 今日を含めて 7 日 = 2 日目から 8 日目
        assert_eq!(rep.from, t0 + 2 * DAY);
        assert_eq!(rep.today_t, t0 + 8 * DAY);
        assert_eq!(rep.daily.len(), 7);
        assert_eq!(rep.total.judged, 7);
        assert_eq!(rep.today().judged, 1);
        // `days` は 1〜31 に丸める
        assert_eq!(tr.report(&th, now, 0).days, 1);
        assert_eq!(tr.report(&th, now, 9_999).days, MAX_DAYS);
    }

    #[test]
    fn the_ring_keeps_31_days_of_hours() {
        let th = DEFAULT;
        let mut tr = Tracker::new();
        for i in 0..(HOURS as u64 + 100) {
            tr.observe(&th, &sample(i * HOUR, 1, 3));
        }
        assert_eq!(tr.hours().count(), HOURS);
        assert_eq!(tr.hours().next().unwrap().t, 100 * HOUR);
    }

    /// 応答は 64 KiB 以下 (**最悪の形**: 31 日ぶん、1 時間おきに外れる)。
    #[test]
    fn the_body_stays_under_64_kib() {
        let th = DEFAULT;
        let mut tr = Tracker::new();
        for i in 0..HOURS as u64 {
            // 1 時間おきに外す = 外れた時間帯が畳めずに最多になる
            let ms = if i % 2 == 0 { 300 } else { 3 };
            for k in 0..720u64 {
                tr.observe(&th, &sample(i * HOUR + k * 5, 1, ms));
            }
        }
        let rep = tr.report(&th, HOURS as u64 * HOUR, MAX_DAYS);
        let json = rep.to_json();
        println!(
            "最悪 (31 日・1 時間おきに外れ): {} B / 外れた時間帯 {} 件",
            json.len(),
            rep.breaches.len()
        );
        assert!(json.len() <= MAX_BODY, "{} B", json.len());
        assert!(json.contains("\"truncated\":true"), "{}", &json[..200]);
        assert!(json.ends_with('}'));
        // 空でも読める形 (判定 0 本のときは `ratio` が null)
        let empty = Tracker::new().report(&th, 0, 7).to_json();
        assert!(empty.contains("\"ratio\":null"), "{}", empty);
        assert!(empty.contains("\"truncated\":false"), "{}", empty);
        assert!(empty.len() < 1024, "{} B", empty.len());
    }

    #[test]
    fn the_json_carries_the_thresholds_and_the_bad_hours() {
        let th = DEFAULT;
        let t0 = 1_789_084_800;
        let mut tr = Tracker::new();
        for s in burst_day(t0) {
            tr.observe(&th, &s);
        }
        let json = tr.report(&th, t0 + DAY - 5, 1).to_json();
        println!("{}", json);
        assert!(json.len() <= MAX_BODY, "{} B", json.len());
        assert!(
            json.contains(
                "\"thresholds\":{\"connect_p50_ms\":10,\"connect_p95_ms\":100,\
                 \"error_rate\":0.005,\"dns_miss_per_connect\":0.2}"
            ),
            "{}",
            json
        );
        assert!(json.contains("\"ratio\":0.70833"), "{}", json);
        assert!(
            json.contains("\"from_hour\":\"2026-09-11T17Z\",\"to_hour\":\"2026-09-12T00Z\""),
            "{}",
            json
        );
        assert!(
            json.contains(
                "{\"name\":\"connect_p50_ms\",\"samples\":5040,\"worst\":46.875,\"threshold\":10}"
            ),
            "{}",
            json
        );
        assert!(json.contains("\"date\":\"2026-09-11\""), "{}", json);
        assert!(
            !json.contains("dns_miss_per_connect\",\"samples\""),
            "{}",
            json
        );
    }

    #[test]
    fn out_of_order_samples_land_in_their_own_hour() {
        let th = DEFAULT;
        let t0 = 1_789_084_800;
        let mut tr = Tracker::new();
        tr.observe(&th, &sample(t0 + HOUR, 1, 3));
        // 1 つ前の時間に戻った標本 (読み戻し) は、その時間がまだ無ければ捨てる
        tr.observe(&th, &sample(t0, 1, 300));
        assert_eq!(tr.hours().count(), 1);
        // 同じ時間に戻った標本は足す
        tr.observe(&th, &sample(t0 + HOUR + 5, 1, 300));
        let h: Vec<Hour> = tr.hours().copied().collect();
        assert_eq!((h[0].judged, h[0].met), (2, 1));
    }

    #[test]
    fn the_global_tracker_is_configurable_and_resettable() {
        reset();
        assert_eq!(thresholds(), DEFAULT);
        configure(Thresholds {
            connect_p50_ms: 1.0,
            ..DEFAULT
        });
        assert_eq!(thresholds().connect_p50_ms, 1.0);
        let v = observe(&sample(1_789_084_800, 1, 3));
        assert_eq!(v.bits, 0b0001, "{:?}", v);
        assert_eq!(report(1_789_084_800, 1).total.judged, 1);
        reset();
        assert_eq!(thresholds(), DEFAULT);
        assert_eq!(report(1_789_084_800, 1).total.judged, 0);
    }
}
