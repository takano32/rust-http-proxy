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
//! `proxy_metrics::history::spawn_every` から 1 行呼ぶ。T14.23 の `proxy_metrics_watch::anomaly::check` の隣)
//! で、要求ごとの経路には 1 命令も無い。5 秒に 1 回、標本 1 本から分位点を 2 回引いて
//! 割り算を 2 回するだけで、リングの走査もシステムコールも無い。
//!
//! **判定 (4 ビット)**:
//!
//! | ビット | 名前 | 標本から取る値 | 満たす |
//! |---|---|---|---|
//! | 0 | `connect_p50_ms` | `conn_excess` (確立 − カーネルの RTT。名前解決を除く) の窓の p50 | 閾以下 |
//! | 1 | `connect_p95_ms` | `conn_excess` の窓の p95 | 閾以下 |
//! | 2 | `error_rate` | `(errors − dns) ÷ (確立 + 転送 + errors − dns)` を **443・80 宛てだけ**で (T30.3) | 閾以下 |
//! | 3 | `dns_miss_per_connect` | `(dns_misses − dns_fail_misses) ÷ 確立` | 閾以下 |
//!
//! **達成 = 4 つとも満たす**。**確立も (名前が引けたうえでの)
//! エラーも 1 本も無い標本は「判定なし」**で分母に入れない (誰も使っていない夜中を
//! 「達成」と数えると、達成率が「動いていた割合」に化ける)。
//!
//! **T29.33 から比の 2 つ (`error_rate`・`dns_miss_per_connect`) は 1 時間の合計で判定する**
//! (利用者の決定)。5 秒の標本の 96% は確立 1〜4 本で、「新しい名前が 1 つ」「エラーが 1 件」で外れた
//! (0.28.48 の 27 時間で外れ 887 中 `dns_miss` 461 = 52%、外れた 32 標本は全部分母 1〜6 本) =
//! 比の閾として働いていなかった。速さ (確立の 2 つ) は今までどおり 5 秒の標本ごとに判定し、
//! 比は**その時間 (UTC の 1 時間の区切り) の分子と分母の合計**で判定する。達成した 5 秒 =
//! 速さの 2 つを満たし、かつ**その時間の**比の 2 つを満たした 5 秒 (比を外した時間は、その時間の
//! 5 秒を全部「達成していない」と数える)。`/slo` には時間・日ごとの合計 (`rates`・`rate_sums`)、
//! 比ごとに満たした / 外した時間の数、**直近 1 時間** (いまから 3,600 秒) の合計 (`last_hour`) も出す。
//! いまの時間 (途中の 1 時間) はそこまでの合計で判定する。
//!
//! **T28.45 で 3 つの読み方を変えた** (利用者の決定 9。T28.16 段 1 の模型。綴りと
//! `PROXY_SLO` の鍵はそのまま、`/slo` の `basis` に読み方が出る):
//! - 確立の 2 つは `connect` (確立そのもの) ではなく**接続ごとの「確立 − RTT」**。
//!   確立の p50 は 1 RTT = 距離で決まる (tokyo 18 ms、nagoya 8 ms) ので、固定の閾では
//!   遠いデプロイ先ほど外れた。窓が空の標本 (T28.45 より前のレコード・RTT が読めない
//!   Linux 以外) では 2 つとも 0 = 外さない
//! - `error_rate` は**名前解決に成功した試みあたり** (E1。メール宛の総当たりの NXDOMAIN は
//!   客が無い名前を頼んだもの)。名前解決の失敗だけの標本は「判定なし」
//! - `dns_miss_per_connect` は**名前解決に失敗した試みのミスを除く**
//!
//! **T30.3 から `error_rate` は宛先のポートが 443・80 の試みだけを数える** (利用者の決定。`basis` の
//! `error_rate` は `resolved_web_attempts_hourly_sum`)。T29.33 で 1 時間の合計にしたあとも、10-28 の tokyo は
//! 15 時間とも `error_rate` で外れて達成 0% (839 / 17,622 = 0.048) で、エラーの大半はメール宛
//! (`:993`・`:995`・`:143`) の確立の時間切れ (3 秒で切る) と拒否・経路なし = NAT の 1 つの接続元からの
//! メールサーバーの総当たり (使われ方) だった。5 秒の標本の `other_port_attempts` / `other_port_errors`
//! (443・80 以外の試みとそのエラー) を今までの分子と分母から引く。**T30.3 より前の標本はどちらも 0** なので
//! 旧の数え方 (全部のポート) で読める。443・80 以外のエラーは今までどおり `/errors`・`/history` の
//! `errors_by_cause` に出る。確立の 2 つと `dns_miss_per_connect` は全部のポートのまま
//!
//! **T30.4 から `error_rate` は 1 つの宛先に偏ったエラーを外す** (利用者の決定。`basis` の `error_rate` は
//! `resolved_web_attempts_hourly_sum_excluding_top_destination`)。T30.3 のあとも 0.30.3 の 10-29 の最初の 6 時間は
//! 4 時間で外れ (16 / 2,018 = 0.79%)、443・80 宛てのエラー 16 件は全部 `pravda.me:443` の確立の時間切れ
//! (1 つのサイトが応答しない) だった。443・80 宛ての試行は 1 時間 300〜400 本なので 2 件で閾を越える。
//! **その時間のエラーが 2 件以上で、いちばん多い宛先 (ホスト別統計の鍵) がその時間のエラーの半分以上なら、
//! その宛先の試行とエラーを 1 時間の分子と分母から外す** (外すのは 1 つだけ。[`excludes`])。宛先ごとの数は
//! 5 秒の標本には無いので、`Metrics` の宛先の表 ([`DestHour`]) を履歴スレッドが 5 秒ごとに
//! [`observe_destinations`] で渡す (確定した前の時間と、いまの時間の途中の集計)。いまの時間は途中の集計で
//! 外し、`last_hour` (いまから 3,600 秒) は、いまの時間のぶんはそのまま、前の時間のぶんは窓に掛かる秒の
//! 割合で按分して外す。どちらもメモリだけなので、**起動し直すと揃って 0 から** (起動した時間は起動してからの
//! 分で判定する)
//!
//! **T29.12 で確立の窓から名前解決を外した** (`basis` の `connect` は `connect_minus_rtt`)。
//! T28.45 の `excess` は `dns + connect − RTT` で、10-26 の tokyo で 50 ms を越えた成功 36 本の
//! excess の合計 19,789 ms のうち 15,159 ms (77%) が名前解決 (34 本がメール宛) だった =
//! 確立の外れの大半がリゾルバの遅さ。5 秒の標本の `conn_excess` (`connect − RTT`) で判定し、
//! 名前解決の遅れは異常検知の `dns_slow` に任せる (`/slo` に閾を増やさない)。`/recent` から
//! 作り直した 5 秒の窓で再生すると、10-26〜27 の 1 時間 (`history["5"]` と同じ 698 本) で確立の
//! 外れが p50 10・p95 12 → 3・4、`/recent` の 3.4 時間で 38・41 → 11・8。T29.12 より前の標本は
//! 窓が空なので確立の 2 つを外さない
//!
//! 10-25 の tokyo の雪像の `history["5"]` (1 時間) を新しい判定で再生すると 0.061 → 0.937
//! (標本に窓が無いので `/recent` の個票から窓を作り直した。T28.45 の結果)。
//!
//! **持ち方**: 判定した 4 ビットは [`Sample`] にも `.rrd` にも書かない
//! (判定は [`Sample`] から何度でも作り直せる)。積むのは**時間ごとの集計**
//! ([`Hour`] = 判定した数・達成した数・閾ごとに外した数と最悪の値) だけで、
//! メモリ上に [`HOURS`] 時間 (31 日) ぶん = 744 × 72 B ≈ 52 KiB (T29.33 で比の分子・分母を足した。
//! 直近 1 時間の 5 秒ごとの分子・分母 720 本 ≈ 28 KiB も持つ。T30.4 の宛先の表はエラーのあった時間だけ
//! 1 行 = 宛先の名前 + 32 B)。**再起動で消える**
//! (T14.4〜T14.8 の共通の決まり)。
//!
//! 5 秒の標本そのものは 6 時間 (T14.32)、60 秒は 1 日、3,600 秒は 30 日しか残らないので、
//! **`/slo?days=7` を「5 秒の標本ごと」で答えられるのはこの集計を積んでいるからだけ**
//! (`res=3600` のリングを畳み直すと 1 時間に 1 回の判定になり、99.2% のような数字は出ない)。

//!
//! **T34.2 で 4 ファイルに割った**: 標本 1 本の判定 (`judge`) は `slo/judge.rs`、時間ごとの集計と
//! リング (`Tracker` / `Hour` / `Report` の中身) は `slo/tracker.rs`、`/slo` の応答を組む
//! `Report::to_json` と数の書き方は `slo/report.rs`。ここには閾 ([`Thresholds`]) と大域の口
//! ([`configure`] / [`observe`] / [`report`] …) だけを置く。行を動かしただけで中身は変えていない。
//! `pub use` で出し直すので、外からの `slo::…` の経路は変わらない。

use std::fmt::Write as _;
use std::sync::Mutex;

use crate::history::Sample;
use crate::metrics::DestHour;
use crate::sync::LockExt;

mod judge;
mod report;
mod tracker;

pub use judge::*;
pub use tracker::*;

use report::num;

/// 閾の名前 (`PROXY_SLO` の綴りと `/slo` の `thresholds` のキー。ビットの順)。
pub const NAMES: [&str; 4] = [
    "connect_p50_ms",
    "connect_p95_ms",
    "error_rate",
    "dns_miss_per_connect",
];

/// 集計を残す時間数 (31 日)。1 時間 = 72 B (T29.33) なので約 52 KiB。
pub(crate) const HOURS: usize = 24 * 31;

/// `/slo?days=` の既定と上限 ([`HOURS`] と揃える)。
pub const DEFAULT_DAYS: u64 = 7;
pub const MAX_DAYS: u64 = 31;

/// 応答の上限 (バイト。本文 T14.50 の「64 KiB 以下」)。
pub const MAX_BODY: usize = 64 * 1024;

pub(super) const HOUR: u64 = 3600;
pub(super) const DAY: u64 = 86_400;

/// 4 つの閾 (`PROXY_SLO`)。**満たす = 値がこれ以下**。
///
/// 既定は本文 (T14.50) のとおり。`crates/config` の `Slo` が同じ既定を持っている
/// (層が下なのであちらからこの型は見えない。食い違ったら `tests/slo_test.rs` が落ちる)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// CONNECT 1 本ごとの「確立 − カーネルの RTT」の p50 (ms。T28.45 から。T29.12 から名前解決を除く。
    /// 綴りは今までのまま)
    pub connect_p50_ms: f64,
    /// 同じ窓の p95 (ms)
    pub connect_p95_ms: f64,
    /// 名前解決に成功した試みあたりのエラー (`(errors − dns) ÷ (確立 + 転送 + errors − dns)`。T28.45)。
    /// T30.3 から**宛先のポートが 443・80 の試みだけ** (綴りは今までのまま)
    pub error_rate: f64,
    /// 名前解決のミス (失敗した試みのぶんを除く。T28.45) ÷ 確立
    pub dns_miss_per_connect: f64,
}

/// `/slo` の `basis` (T28.45)。**閾の綴りは変えずに読み方を変えた**ので、読む側
/// (雪像を並べる道具) が前後を取り違えないように、何を判定したかを隣に出す
/// (T28.45 より前の `/slo` にはこの鍵が無い = 確立そのもの・全試み・全ミス)。
/// `connect` は T28.45 〜 T29.11 が `establish_minus_rtt` (`dns + connect − RTT`)、
/// T29.12 から `connect_minus_rtt` (名前解決を除く)。
/// 比の 2 つは T29.33 から **1 時間の合計**で判定する (`_hourly_sum` が付く。それより前は 5 秒の標本ごと)。
/// `error_rate` は T29.33 〜 T30.2 が `resolved_attempts_hourly_sum` (全部のポート)、T30.3 が
/// `resolved_web_attempts_hourly_sum` (宛先のポートが 443・80 の試みだけ)、T30.4 から
/// `resolved_web_attempts_hourly_sum_excluding_top_destination` (1 つの宛先に偏ったエラーを外す。[`excludes`])。
pub(crate) const BASIS_JSON: &str = "{\"connect\":\"connect_minus_rtt\",\
                              \"error_rate\":\"resolved_web_attempts_hourly_sum_excluding_top_destination\",\
                              \"dns_miss_per_connect\":\"excluding_failed_lookups_hourly_sum\"}";

/// 既定の閾 (`PROXY_SLO=connect_p50_ms=5,connect_p95_ms=50,error_rate=0.005,dns_miss_per_connect=0.2`)。
///
/// 確立の 2 つは T28.45 から **「確立 − カーネルの RTT」** の p50 / p95 で、既定は
/// T28.16 段 1 の模型の値 (5 / 50 ms。前は確立そのものの 10 / 100 ms で、tokyo の
/// 1 RTT 18 ms では距離だけで外れた)。
pub const DEFAULT: Thresholds = Thresholds {
    connect_p50_ms: 5.0,
    connect_p95_ms: 50.0,
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

/// 443・80 宛てのエラーのいちばん多い宛先を、その時間の集計に渡す (**履歴スレッドから 1 行**。T30.4)。
/// `prev` は確定した前の時間、`cur` はいまの時間の途中 (`Metrics::web_error_hours`)。
pub fn observe_destinations(prev: Option<&DestHour>, cur: &DestHour) {
    let mut guard = TRACKER.locked();
    guard
        .get_or_insert_with(Tracker::new)
        .observe_destinations(prev, cur);
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

/// その日 (UTC の 0 時 `day_t`) の比ごとの `[満たした時間, 外した時間]` (日次の 1 行が読む。T29.33)。
/// 集計はメモリだけなので、その日の途中で起動し直していれば起動してからの時間だけ。
pub fn rate_hours_of_day(day_t: u64) -> [[u32; 2]; RATES] {
    let th = thresholds();
    let guard = TRACKER.locked();
    guard
        .as_ref()
        .map(|t| t.rate_hours_of_day(&th, day_t))
        .unwrap_or_default()
}

/// 覚えている集計を捨てる (テスト用)。
pub fn reset() {
    *TRACKER.locked() = None;
    *THRESHOLDS.locked() = DEFAULT;
}

/// `slo` の単体試験 (T34.2 で別のファイルに移した)。
#[cfg(test)]
mod tests;
