//! 異常の自動検知 (`/events` の `anomaly`。T14.23)。
//!
//! 24 時間の図を人が全部読むのは手間がかかる (T14.0 は 72.7 時間ぶんを手で読んだ)。
//! ここは**「いつから」を機械に印させる**もので、履歴スレッドが 5 秒ごとに取る標本
//! ([`crate::history::Sample`]) を直近 5 分の窓に畳み、**直近 1 時間の基準値と比べて
//! 外れた瞬間**を [`crate::events`] のリングに 1 件書く。
//!
//! **費用は 0**: 判定するのは履歴スレッドの周期だけで、要求ごとの経路には 1 命令も無い。
//! 読むのも標本 1 本と原子 2 つ + 山の写真の通算だけで、システムコールは増えない。
//!
//! 判定は **5 種で固定** ([`Kind`])。閾は本文 (T14.23) のとおり:
//!
//! | 種類 | 立つ条件 |
//! |---|---|
//! | `connect_p95` | CONNECT 確立の p95 (直近 5 分) が直近 1 時間の p95 の [`CONNECT_RATIO`] 倍以上、かつ [`CONNECT_MIN_MS`] 以上 |
//! | `dns_slow` | 名前解決のミス 1 回の平均 (直近 5 分) が [`DNS_MISS_MS`] 以上 |
//! | `errors` | エラーが 5 分で [`ERRORS_MIN`] 件以上 |
//! | `active_high` | 同時接続の山が `max_conns` の [`ACTIVE_PERCENT`]% 以上 (T14.6 の写真と同じ閾。写真があればその番号) |
//! | `rejected` | `rejected_overload` / `evicted_idle` / `rejected_client_acl` が増えた |
//!
//! **同じ種類は収まるまで 1 回だけ**書く。条件を外れたまま [`CLEAR_SECS`] 秒
//! (5 分) 続いたら `cleared: <種類>` で始まる解除の 1 件を書き、また立てるようになる。
//! 説明には必ず数字 (何が・いくつ・基準値) を入れる。
//!
//! `connect_p95` だけは**立っている間は「立った瞬間の基準値」と比べる**。基準値の窓
//! (1 時間) は直近の 5 分を含むので、山が 1 時間続くと基準値そのものが山になって倍率が
//! 1 に戻り、**山の最中に解除を書いてしまう** (T14.0 の 2026-09-11 17〜23 時のような
//! 6 時間の山がこれに当たる。単体テストで実際に 2 件目が出た)。
//!
//! 窓を畳むのは **T14.24 の [`crate::history::summary::of`] 1 か所だけ** (`/history` の
//! `?summary=1` と同じ関数・同じ切り方)。標本を自前に写し取ると、同じ畳み方が 2 つ
//! できて食い違うし、1 時間ぶん (720 標本 × 504 B = 350 KiB) の持ち直しになる。
//! 判定がここで覚えているのは**リングから読めないものだけ** ([`Counters`] = 断った数の
//! 累計と山の写真の通算) と、種類ごとの立ち上がり ([`KindState`])。

use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::history::summary::{self, Params, Summary};
use crate::history::{History, Sample};
use crate::metrics::{ERR_CAUSE_NAMES, ERR_CAUSES, Metrics};
use crate::sync::LockExt;

/// (1) CONNECT 確立の p95 が基準値の何倍で立つか。
pub const CONNECT_RATIO: f64 = 3.0;
/// (1) かつ、この ms 以上のときだけ (速いところの 3 倍は異常ではない)。
pub const CONNECT_MIN_MS: f64 = 50.0;
/// (2) 名前解決のミス 1 回の平均 (ms)。
pub const DNS_MISS_MS: f64 = 100.0;
/// (3) 5 分のエラー件数。
pub const ERRORS_MIN: u64 = 5;
/// (4) 同時接続の山が `max_conns` のこの割合 (%) 以上で立つ (T14.6 の写真と同じ閾)。
pub const ACTIVE_PERCENT: usize = 50;

/// 直近の窓 (秒)。判定はこの窓の値を [`BASE_SECS`] の窓と比べる。
pub const WINDOW_SECS: u64 = 300;
/// 基準値を取る窓 (秒)。直近の窓もこの中に入っている。
pub const BASE_SECS: u64 = 3600;
/// 条件を外れてから解除の 1 件を書くまで (秒)。
pub const CLEAR_SECS: u64 = WINDOW_SECS;

/// 判定の種類 (**5 種で固定**。出来事の種類はどれも `anomaly` で、これは説明の頭に出る)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// CONNECT の確立が急に遅くなった
    ConnectP95,
    /// 名前解決のミス 1 回が重い
    DnsSlow,
    /// エラーが続けて出ている
    Errors,
    /// 同時接続が上限に近い
    ActiveHigh,
    /// 上限や接続元の一覧で断った / 追い出した
    Rejected,
}

/// 全種類 (README の一覧と同じ並び)。
pub const KINDS: [Kind; 5] = [
    Kind::ConnectP95,
    Kind::DnsSlow,
    Kind::Errors,
    Kind::ActiveHigh,
    Kind::Rejected,
];

impl Kind {
    /// 説明の頭に出す名前 (`cleared:` のあとに出るのもこれ)。
    pub fn name(self) -> &'static str {
        match self {
            Kind::ConnectP95 => "connect_p95",
            Kind::DnsSlow => "dns_slow",
            Kind::Errors => "errors",
            Kind::ActiveHigh => "active_high",
            Kind::Rejected => "rejected",
        }
    }
}

/// 標本 (`/history` のリング) から読めない値。**テストはここへ直接流し込む**
/// (時刻も `t` で注入する)。どれも**累計**で、判定は 1 本前との差だけを見る。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// いつ (epoch 秒。標本の `t` と同じ)
    pub t: u64,
    /// 追い出した数の累計 (T13.2)
    pub evicted_idle: u64,
    /// 上限で断った数の累計 (T8.5)
    pub rejected_overload: u64,
    /// 接続元の一覧で断った数の累計 (T14.18)
    pub rejected_client_acl: u64,
    /// 山の写真の通算 (T14.6)。増えていれば「この周期で 1 枚撮れた」= その番号
    pub shots: u64,
}

impl Counters {
    /// 標本 1 本ぶん集める (履歴スレッドから)。原子 2 つと写真の通算 1 つだけ。
    fn take(metrics: &Metrics, s: &Sample) -> Counters {
        Counters {
            t: s.t,
            // 標本が既に持っている (`/history` の列。T14.2 (3))
            evicted_idle: s.evicted_idle,
            rejected_overload: metrics.rejected_overload.load(Ordering::Relaxed),
            rejected_client_acl: metrics.rejected_client_acl.load(Ordering::Relaxed),
            // 次に撮る番号の 1 つ前 = いままでに撮った枚数 (= 最後の 1 枚の `seq`)
            shots: metrics.bursts.next_seq().saturating_sub(1),
        }
    }
}

/// 立った / 解除された 1 件 (説明はそのまま `/events` の `text` になる)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fired {
    pub kind: Kind,
    /// `true` なら解除 (説明は `cleared: <種類>` で始まる)
    pub cleared: bool,
    pub text: String,
}

/// 1 種類ぶんの状態。
#[derive(Clone, Copy, Debug, Default)]
struct KindState {
    /// いま立っているか (立っている間は同じ種類を二度書かない)
    firing: bool,
    /// 立った時刻
    since: u64,
    /// 条件を外れた最初の時刻 (外れたままここから [`CLEAR_SECS`] 秒で解除)
    calm_since: Option<u64>,
    /// 立った瞬間の基準値 (`connect_p95` だけ使う。下の [`Detector::observe`] を参照)
    base_p95: f64,
}

/// 判定そのもの。**時刻は [`Counters::t`] だけを見る** (テストから注入できるように)。
pub struct Detector {
    /// 同時接続数の上限 (`PROXY_MAX_CONNS`。0 なら (4) を判定しない)
    max_conns: usize,
    /// 山と見なす本数 (T14.6 の写真と同じ閾)
    threshold: usize,
    /// 1 本前の累計 (`None` = まだ 1 本も見ていない)
    last_totals: Option<[u64; 3]>,
    /// 1 本前の山の写真の通算
    last_shots: Option<u64>,
    state: [KindState; KINDS.len()],
}

impl Default for Detector {
    fn default() -> Self {
        Detector::new()
    }
}

impl Detector {
    pub fn new() -> Detector {
        Detector {
            max_conns: 0,
            threshold: 0,
            last_totals: None,
            last_shots: None,
            state: [KindState::default(); KINDS.len()],
        }
    }

    /// 同時接続の上限と、山と見なす本数を教える ([`configure`] が渡す値)。
    pub fn set_limits(&mut self, max_conns: usize, threshold: usize) {
        self.max_conns = max_conns;
        self.threshold = threshold;
    }

    /// 標本 1 本ぶんを入れて、**変わり目 (立った / 解除された) だけ**を返す。
    ///
    /// 窓を畳むのは [`summary::of`] (T14.24) で、`/history?summary=1` と同じ切り方
    /// (`since <= t <= until`、5 秒の解像度)。**本番では `h` に今の標本が積まれた
    /// あとに呼ぶ** (履歴スレッドの `push` の直後)。
    pub fn observe(&mut self, h: &History, c: Counters) -> Vec<Fired> {
        let now = c.t;
        let totals = [c.rejected_overload, c.evicted_idle, c.rejected_client_acl];
        // 1 本目は比べる相手が無いので増分 0 (途中から見始めても古い数で立てない)
        let delta = match self.last_totals.replace(totals) {
            Some(prev) => [
                totals[0].saturating_sub(prev[0]),
                totals[1].saturating_sub(prev[1]),
                totals[2].saturating_sub(prev[2]),
            ],
            None => [0; 3],
        };
        let shot = match self.last_shots.replace(c.shots) {
            Some(prev) if c.shots > prev => Some(c.shots),
            _ => None,
        };
        let w5 = fold(h, now, WINDOW_SECS);
        let base = fold(h, now, BASE_SECS);

        let p95 = w5.connect.quantile_ms(0.95);
        let p95_base = base.connect.quantile_ms(0.95);
        let peak = w5.active_max;
        // **立っている間は「立った瞬間の基準値」と比べる。** 基準値の窓 (1 時間) は
        // 直近の窓を含むので、山が 1 時間続くと基準値そのものが山になって倍率が 1 に戻り、
        // 山の最中に解除を書いてしまう (T14.0 の 17〜23 時のような 6 時間の山がこれ)。
        // 「収まるまで 1 回だけ」の「収まる」は**元の基準値に戻ること**にする
        let p95_ref = if self.state[0].firing {
            self.state[0].base_p95
        } else {
            p95_base
        };
        let hits = [
            // 直近の窓は基準値の中にも入っているので、起動直後 (1 時間ぶんが全部この
            // 5 分) は倍率がちょうど 1 になり、立たない
            p95 >= CONNECT_MIN_MS && p95_ref > 0.0 && p95 >= CONNECT_RATIO * p95_ref,
            w5.dns_miss_avg_ms() >= DNS_MISS_MS,
            w5.errors >= ERRORS_MIN,
            (self.threshold > 0 && peak >= self.threshold as u64) || shot.is_some(),
            delta.iter().any(|&d| d > 0),
        ];

        // 説明を組むのに要るだけ (下で `self.state` を可変に借りるので先に写しておく)
        let limits = (self.max_conns, self.threshold);
        let mut out = Vec::new();
        for (i, &hit) in hits.iter().enumerate() {
            let kind = KINDS[i];
            let st = &mut self.state[i];
            if hit {
                st.calm_since = None;
                if !st.firing {
                    st.firing = true;
                    st.since = now;
                    st.base_p95 = p95_base;
                    out.push(Fired {
                        kind,
                        cleared: false,
                        text: fired_text(kind, &w5, &base, limits, delta, totals, shot),
                    });
                }
                continue;
            }
            if !st.firing {
                continue;
            }
            let calm = *st.calm_since.get_or_insert(now);
            if now.saturating_sub(calm) >= CLEAR_SECS {
                let mins = now.saturating_sub(st.since).div_ceil(60);
                st.firing = false;
                st.calm_since = None;
                out.push(Fired {
                    kind,
                    cleared: true,
                    text: cleared_text(kind, &w5, mins),
                });
            }
        }
        out
    }

    /// いま立っている種類 (テストと、途中経過を見るため)。
    pub fn firing(&self) -> Vec<Kind> {
        KINDS
            .iter()
            .enumerate()
            .filter(|(i, _)| self.state[*i].firing)
            .map(|(_, k)| *k)
            .collect()
    }
}

/// 直近 `secs` 秒を畳む (T14.24 の関数をそのまま使う。5 秒の標本 = リング 720 本)。
fn fold(h: &History, now: u64, secs: u64) -> Summary {
    summary::of(
        h,
        &Params {
            since: now.saturating_sub(secs),
            until: now,
            res: Some(5),
            // バーストを外すのは「平常時どうしを比べる」ための切り方 (T14.0)。
            // ここは**バーストそのものを見つける**のが仕事なので外さない
            normal_hours_only: false,
        },
    )
}

/// ms の書き方 (10 ms 未満は小数 1 桁。「0 ms」と書かないため)。
fn ms(v: f64) -> String {
    if v < 10.0 {
        format!("{:.1}", v)
    } else {
        format!("{:.0}", v)
    }
}

/// エラーの原因の内訳 (多い順に 4 つまで。`/status` と同じ綴り)。
fn causes(counts: &[u64; ERR_CAUSES]) -> String {
    let mut idx: Vec<usize> = (0..ERR_CAUSES).filter(|&i| counts[i] > 0).collect();
    idx.sort_by_key(|&i| std::cmp::Reverse(counts[i]));
    idx.truncate(4);
    let mut s = String::new();
    for (n, &i) in idx.iter().enumerate() {
        if n > 0 {
            s.push_str(", ");
        }
        let _ = write!(s, "{} {}", ERR_CAUSE_NAMES[i], counts[i]);
    }
    s
}

/// 立ったときの説明 (**数字を必ず入れる**: 何が・いくつ・基準値)。
fn fired_text(
    kind: Kind,
    w5: &Summary,
    base: &Summary,
    limits: (usize, usize),
    delta: [u64; 3],
    totals: [u64; 3],
    shot: Option<u64>,
) -> String {
    let (max_conns, threshold) = limits;
    let head = kind.name();
    match kind {
        Kind::ConnectP95 => {
            let p95 = w5.connect.quantile_ms(0.95);
            let p95_base = base.connect.quantile_ms(0.95);
            format!(
                "{}: connect p95 {} ms over 5m is {:.1}x the 1h baseline {} ms ({} of {} connects)",
                head,
                ms(p95),
                p95 / p95_base.max(f64::MIN_POSITIVE),
                ms(p95_base),
                w5.connect.count,
                base.connect.count
            )
        }
        Kind::DnsSlow => format!(
            "{}: dns miss {} ms avg over 5m (threshold {} ms; {} misses, {} ms total)",
            head,
            ms(w5.dns_miss_avg_ms()),
            DNS_MISS_MS as u64,
            w5.dns_misses,
            w5.dns_ms_sum
        ),
        Kind::Errors => {
            let by = causes(&w5.errors_by_cause);
            format!(
                "{}: {} errors in 5m (threshold {}){}{}",
                head,
                w5.errors,
                ERRORS_MIN,
                if by.is_empty() { "" } else { ": " },
                by
            )
        }
        Kind::ActiveHigh => {
            // 標本 (5 秒ごと) の山が閾に届いていないのに立つのは、**写真の方が先に
            // 気づいた**とき (accept の経路は越えた瞬間に旗を立てるが、標本はその 5 秒の
            // 断面しか見ない)。そのときは「写真が越えた」と書く (山の数字が食い違って見えないように)
            let sampled = w5.active_max;
            if threshold > 0 && sampled >= threshold as u64 {
                let mut s = format!("{}: active connections peaked at {} in 5m", head, sampled);
                if max_conns > 0 {
                    let _ = write!(
                        s,
                        " of max_conns {} ({}%, threshold {})",
                        max_conns,
                        sampled.saturating_mul(100) / max_conns as u64,
                        threshold
                    );
                }
                if let Some(seq) = shot {
                    let _ = write!(s, ", burst shot #{}", seq);
                }
                return s;
            }
            format!(
                "{}: burst shot #{} crossed the threshold {} of max_conns {} (sampled peak {} in 5m)",
                head,
                shot.unwrap_or(0),
                threshold,
                max_conns,
                sampled
            )
        }
        Kind::Rejected => {
            let mut s = format!("{}:", head);
            for (name, d, total) in [
                ("rejected_overload", delta[0], totals[0]),
                ("evicted_idle", delta[1], totals[1]),
                ("rejected_client_acl", delta[2], totals[2]),
            ] {
                if d > 0 {
                    let _ = write!(s, " {} +{} (total {}),", name, d, total);
                }
            }
            s.pop();
            s
        }
    }
}

/// 解除の説明 (**`cleared: <種類>` で始める**。いまの値も入れる)。
fn cleared_text(kind: Kind, w5: &Summary, mins: u64) -> String {
    let head = format!("cleared: {} after {}m", kind.name(), mins);
    match kind {
        Kind::ConnectP95 => format!(
            "{} (connect p95 {} ms over 5m, {} connects)",
            head,
            ms(w5.connect.quantile_ms(0.95)),
            w5.connect.count
        ),
        Kind::DnsSlow => format!(
            "{} (dns miss {} ms avg over 5m, {} misses)",
            head,
            ms(w5.dns_miss_avg_ms()),
            w5.dns_misses
        ),
        Kind::Errors => format!("{} ({} errors in 5m)", head, w5.errors),
        Kind::ActiveHigh => format!("{} (active peaked at {} in 5m)", head, w5.active_max),
        Kind::Rejected => format!("{} (no new rejects for {}m)", head, CLEAR_SECS / 60),
    }
}

/// 同時接続数の上限と、山と見なす本数 ([`configure`] が入れる)。
static MAX_CONNS: AtomicUsize = AtomicUsize::new(0);
static THRESHOLD: AtomicUsize = AtomicUsize::new(0);

/// 判定の入れ物。**触るのは履歴スレッドだけ** (5 秒に 1 回)。
static DETECTOR: Mutex<Option<Detector>> = Mutex::new(None);

/// 同時接続の山の閾を教える (`src/main.rs` の起動時に 1 回)。
///
/// `burst_at` は T14.6 の写真と同じ閾 (`PROXY_MAX_CONNS × PROXY_BURST_PERCENT`)。
/// 写真を撮らない設定 (`PROXY_BURST_PERCENT=0` と `--lite`) では [`usize::MAX`] が
/// 来るので、そのときだけ本文どおりの [`ACTIVE_PERCENT`]% を自分で当てる。
pub fn configure(max_conns: usize, burst_at: usize) {
    let threshold = if burst_at == 0 || burst_at == usize::MAX {
        max_conns.saturating_mul(ACTIVE_PERCENT) / 100
    } else {
        burst_at
    };
    MAX_CONNS.store(max_conns, Ordering::Relaxed);
    THRESHOLD.store(threshold, Ordering::Relaxed);
}

/// 標本 1 本を判定して、変わり目だけ `/events` に 1 件書く (**履歴スレッドから**)。
///
/// 要求の経路には 1 命令も足さない。ここで増えるのは原子の読み 2 つ・山の写真の
/// 通算 1 つ・直近 5 分の畳み (5 秒周期なら 60 本 × 21 個の加算) だけ。
pub fn check(metrics: &Metrics, sample: &Sample) {
    let fired = {
        let mut guard = DETECTOR.locked();
        let d = guard.get_or_insert_with(Detector::new);
        d.set_limits(
            MAX_CONNS.load(Ordering::Relaxed),
            THRESHOLD.load(Ordering::Relaxed),
        );
        d.observe(&metrics.history, Counters::take(metrics, sample))
    };
    // 書くのは鍵の外 (出来事のリングと判定の鍵を同時に握らない)
    for f in fired {
        crate::events::push(crate::events::EventKind::Anomaly, &f.text);
    }
}

/// 覚えている状態を捨てる (テスト用)。
pub fn reset() {
    *DETECTOR.locked() = None;
    MAX_CONNS.store(0, Ordering::Relaxed);
    THRESHOLD.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 5 秒周期の標本 1 本。
    fn point(t: u64) -> Sample {
        Sample {
            t,
            ..Sample::default()
        }
    }

    /// `n` 本の CONNECT が `ms` ミリ秒で確立した標本。
    fn connects(t: u64, n: usize, ms: u64) -> Sample {
        let mut p = point(t);
        for _ in 0..n {
            p.connect.observe(ms);
        }
        p
    }

    /// 標本を 1 本流す (**本番と同じ順**: 履歴に積んでから判定する)。
    fn feed(h: &History, d: &mut Detector, s: Sample) -> Vec<Fired> {
        feed_with(h, d, s, Counters::default())
    }

    /// リングから読めない累計 (断った数・写真の通算) も付けて 1 本流す。
    fn feed_with(h: &History, d: &mut Detector, s: Sample, c: Counters) -> Vec<Fired> {
        h.push(s);
        d.observe(
            h,
            Counters {
                t: s.t,
                evicted_idle: s.evicted_idle,
                ..c
            },
        )
    }

    /// 平常時を `secs` 秒ぶん流す (5 秒ごとに 10 ms の CONNECT が 2 本)。
    fn calm(h: &History, d: &mut Detector, from: u64, secs: u64) -> (u64, Vec<Fired>) {
        let mut out = Vec::new();
        let mut t = from;
        while t < from + secs {
            out.extend(feed(h, d, connects(t, 2, 10)));
            t += 5;
        }
        (t, out)
    }

    /// (1) 確立の p95 が基準値の 3 倍以上かつ 50 ms 以上。
    #[test]
    fn a_slow_connect_p95_fires_once_and_clears() {
        let h = History::default();
        let mut d = Detector::new();
        // 1 時間の平常時 (基準値 = 10 ms)
        let (mut t, quiet) = calm(&h, &mut d, 0, BASE_SECS);
        assert!(quiet.is_empty(), "平常時に立ってはいけない: {:?}", quiet);
        // 10 分のバースト (250 ms)
        let mut fired = Vec::new();
        while t < BASE_SECS + 600 {
            fired.extend(feed(&h, &mut d, connects(t, 2, 250)));
            t += 5;
        }
        assert_eq!(fired.len(), 1, "立つのは 1 回だけ: {:?}", fired);
        assert!(!fired[0].cleared);
        assert_eq!(fired[0].kind, Kind::ConnectP95);
        assert!(
            fired[0].text.starts_with("connect_p95: connect p95 "),
            "{}",
            fired[0].text
        );
        assert!(fired[0].text.contains("1h baseline"), "{}", fired[0].text);
        // 平常時に戻して 15 分 (窓から抜けるのに 5 分 + 外れたまま 5 分)
        let (_, back) = calm(&h, &mut d, t, 900);
        assert_eq!(back.len(), 1, "解除も 1 回だけ: {:?}", back);
        assert!(back[0].cleared);
        assert!(
            back[0].text.starts_with("cleared: connect_p95 after "),
            "{}",
            back[0].text
        );
        assert!(d.firing().is_empty());
    }

    /// (2) 名前解決のミス 1 回の平均が 100 ms 以上。
    #[test]
    fn a_slow_dns_miss_fires_once_and_clears() {
        let h = History::default();
        let mut d = Detector::new();
        let (mut t, _) = calm(&h, &mut d, 0, 60);
        let mut fired = Vec::new();
        for _ in 0..12 {
            let mut p = connects(t, 2, 10);
            p.dns_misses = 3;
            p.dns_ms_sum = 3 * 140;
            fired.extend(feed(&h, &mut d, p));
            t += 5;
        }
        assert_eq!(fired.len(), 1, "{:?}", fired);
        assert_eq!(fired[0].kind, Kind::DnsSlow);
        assert_eq!(
            fired[0].text,
            "dns_slow: dns miss 140 ms avg over 5m (threshold 100 ms; 3 misses, 420 ms total)"
        );
        let (_, back) = calm(&h, &mut d, t, 900);
        assert_eq!(back.len(), 1, "{:?}", back);
        assert!(back[0].cleared);
        assert!(
            back[0].text.contains("dns miss 0.0 ms avg over 5m"),
            "{}",
            back[0].text
        );
    }

    /// (3) エラーが 5 分で 5 件以上。
    #[test]
    fn errors_in_five_minutes_fire_once_and_clear() {
        let h = History::default();
        let mut d = Detector::new();
        let (mut t, _) = calm(&h, &mut d, 0, 60);
        let mut fired = Vec::new();
        // 1 標本に 3 件ずつ 2 回 = 6 件 (2 回目で越える)
        for _ in 0..2 {
            let mut p = point(t);
            p.errors = 3;
            p.errors_by_cause[0] = 2;
            p.errors_by_cause[4] = 1;
            fired.extend(feed(&h, &mut d, p));
            t += 5;
        }
        assert_eq!(fired.len(), 1, "{:?}", fired);
        assert_eq!(fired[0].kind, Kind::Errors);
        assert_eq!(
            fired[0].text,
            "errors: 6 errors in 5m (threshold 5): dns 4, reset 2"
        );
        // 窓から抜けるのに 5 分 + 外れたまま 5 分
        let (_, back) = calm(&h, &mut d, t, 900);
        assert_eq!(back.len(), 1, "{:?}", back);
        assert_eq!(back[0].kind, Kind::Errors);
        assert!(
            back[0].text.ends_with("(0 errors in 5m)"),
            "{}",
            back[0].text
        );
    }

    /// (4) 同時接続の山が `max_conns` の 50% 以上。
    #[test]
    fn a_high_active_peak_fires_once_and_clears() {
        let h = History::default();
        let mut d = Detector::new();
        d.set_limits(240, 120);
        let (mut t, _) = calm(&h, &mut d, 0, 60);
        let mut fired = Vec::new();
        for active in [195, 218, 201] {
            let mut p = connects(t, 2, 10);
            p.active_max = active;
            fired.extend(feed(&h, &mut d, p));
            t += 5;
        }
        assert_eq!(fired.len(), 1, "{:?}", fired);
        assert_eq!(fired[0].kind, Kind::ActiveHigh);
        assert_eq!(
            fired[0].text,
            "active_high: active connections peaked at 195 in 5m of max_conns 240 (81%, threshold 120)"
        );
        let (_, back) = calm(&h, &mut d, t, 900);
        assert_eq!(back.len(), 1, "{:?}", back);
        assert!(
            back[0].text.starts_with("cleared: active_high after "),
            "{}",
            back[0].text
        );
    }

    /// (4) 写真が撮れていればその番号を持つ (T14.6 の `seq`)。
    #[test]
    fn a_new_burst_shot_carries_its_number() {
        let h = History::default();
        let mut d = Detector::new();
        d.set_limits(240, 120);
        let shots = |n| Counters {
            shots: n,
            ..Counters::default()
        };
        let empty = feed_with(&h, &mut d, point(0), shots(6));
        assert!(empty.is_empty(), "1 本目は比べる相手が無い");
        let mut p = point(5);
        p.active_max = 130;
        let fired = feed_with(&h, &mut d, p, shots(7));
        assert_eq!(fired.len(), 1);
        assert_eq!(
            fired[0].text,
            "active_high: active connections peaked at 130 in 5m of max_conns 240 (54%, threshold 120), burst shot #7"
        );
    }

    /// 5 秒の標本が山を見逃しても写真が気づく (accept の経路は越えた瞬間に旗を立てる)。
    /// そのときは「写真が越えた」と書く (標本の山と数字が食い違って見えないように)。
    #[test]
    fn a_spike_between_two_samples_is_written_as_the_photo() {
        let h = History::default();
        let mut d = Detector::new();
        d.set_limits(4, 2);
        assert!(feed(&h, &mut d, point(0)).is_empty());
        let mut p = point(5);
        p.active_max = 1;
        let fired = feed_with(
            &h,
            &mut d,
            p,
            Counters {
                shots: 1,
                ..Counters::default()
            },
        );
        assert_eq!(fired.len(), 1);
        assert_eq!(
            fired[0].text,
            "active_high: burst shot #1 crossed the threshold 2 of max_conns 4 (sampled peak 1 in 5m)"
        );
    }

    /// (5) 断った / 追い出した数が増えた。
    #[test]
    fn growing_reject_counters_fire_once_and_clear() {
        let h = History::default();
        let mut d = Detector::new();
        let (mut t, _) = calm(&h, &mut d, 0, 60);
        let rejected = |n| Counters {
            rejected_overload: n,
            ..Counters::default()
        };
        let mut fired = Vec::new();
        let mut p = connects(t, 2, 10);
        p.evicted_idle = 3;
        fired.extend(feed_with(&h, &mut d, p, rejected(12)));
        t += 5;
        // 同じ山でもう 1 回増えても 2 件目は書かない
        let mut p = connects(t, 2, 10);
        p.evicted_idle = 3;
        fired.extend(feed_with(&h, &mut d, p, rejected(20)));
        t += 5;
        assert_eq!(fired.len(), 1, "{:?}", fired);
        assert_eq!(fired[0].kind, Kind::Rejected);
        assert_eq!(
            fired[0].text,
            "rejected: rejected_overload +12 (total 12), evicted_idle +3 (total 3)"
        );
        // 増えなくなってから 5 分で解除 (累計は 20 / 3 のまま)
        let mut back = Vec::new();
        while t < 60 + 700 {
            let mut p = connects(t, 2, 10);
            p.evicted_idle = 3;
            back.extend(feed_with(&h, &mut d, p, rejected(20)));
            t += 5;
        }
        assert_eq!(back.len(), 1, "{:?}", back);
        assert_eq!(
            back[0].text,
            "cleared: rejected after 6m (no new rejects for 5m)"
        );
    }

    /// 5 種が同時に立っても、それぞれ 1 回ずつ (受け入れ基準)。
    #[test]
    fn each_of_the_five_fires_once_and_clears_once() {
        let h = History::default();
        let mut d = Detector::new();
        d.set_limits(240, 120);
        let (mut t, _) = calm(&h, &mut d, 0, BASE_SECS);
        let mut fired = Vec::new();
        let start = t;
        while t < start + 600 {
            let mut p = connects(t, 4, 300);
            p.dns_misses = 2;
            p.dns_ms_sum = 2 * 250;
            p.errors = 3;
            p.errors_by_cause[0] = 3;
            p.active_max = 218;
            let c = Counters {
                rejected_overload: (t - start) / 5,
                ..Counters::default()
            };
            fired.extend(feed_with(&h, &mut d, p, c));
            t += 5;
        }
        let mut kinds: Vec<&str> = fired.iter().map(|f| f.kind.name()).collect();
        kinds.sort_unstable();
        assert_eq!(
            kinds,
            [
                "active_high",
                "connect_p95",
                "dns_slow",
                "errors",
                "rejected"
            ],
            "5 種が 1 回ずつ: {:?}",
            fired
        );
        assert!(fired.iter().all(|f| !f.cleared));
        assert!(
            fired
                .iter()
                .all(|f| f.text.len() <= crate::events::MAX_TEXT)
        );
        // 収まったら 5 種とも解除が 1 回ずつ
        let (_, back) = calm(&h, &mut d, t, 1200);
        let mut kinds: Vec<&str> = back.iter().map(|f| f.kind.name()).collect();
        kinds.sort_unstable();
        assert_eq!(
            kinds,
            [
                "active_high",
                "connect_p95",
                "dns_slow",
                "errors",
                "rejected"
            ],
            "解除も 1 回ずつ: {:?}",
            back
        );
        assert!(back.iter().all(|f| f.cleared));
        assert!(back.iter().all(|f| f.text.len() <= crate::events::MAX_TEXT));
        assert!(d.firing().is_empty());
    }

    /// 静かなプロセスでは 1 件も書かない (2 時間)。
    #[test]
    fn a_quiet_process_writes_nothing() {
        let h = History::default();
        let mut d = Detector::new();
        d.set_limits(240, 120);
        let (t, out) = calm(&h, &mut d, 0, 2 * BASE_SECS);
        assert!(out.is_empty(), "{:?}", out);
        // 標本が 1 本も無い (要求が来ていない) 時間帯も同じ
        let mut t = t;
        let mut out = Vec::new();
        for _ in 0..720 {
            out.extend(feed(&h, &mut d, point(t)));
            t += 5;
        }
        assert!(out.is_empty(), "{:?}", out);
    }

    /// 窓は直近 5 分と直近 1 時間 (どちらも T14.24 の [`summary::of`] が切る)。
    ///
    /// 5 秒の標本しか読まないので、基準値は**リングが覆う 1 時間**そのもの。
    #[test]
    fn the_windows_are_five_minutes_and_one_hour() {
        let h = History::default();
        let mut d = Detector::new();
        let (t, _) = calm(&h, &mut d, 0, 2 * BASE_SECS);
        let w5 = fold(&h, t - 5, WINDOW_SECS);
        let base = fold(&h, t - 5, BASE_SECS);
        // 窓は両端を含むので 5 分 = 61 標本、1 時間 = 721 標本ぶん (リングは 720 本)
        assert_eq!(w5.samples, WINDOW_SECS / 5 + 1);
        assert_eq!(w5.connect.count, 2 * (WINDOW_SECS / 5 + 1));
        assert_eq!(base.samples, BASE_SECS / 5, "リングが覆うのは 1 時間");
        assert_eq!(base.connect.count, 2 * BASE_SECS / 5);
        assert_eq!(base.interval_secs, 5);
    }

    #[test]
    fn the_five_kinds_have_distinct_names() {
        let mut names: Vec<&str> = KINDS.iter().map(|k| k.name()).collect();
        assert_eq!(names.len(), 5);
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 5, "名前が重なっている");
    }

    /// 閾は `PROXY_BURST_PERCENT` と同じ本数。写真を撮らない設定なら本文の 50%。
    #[test]
    fn the_threshold_follows_the_burst_photo() {
        reset();
        configure(240, 120);
        assert_eq!(THRESHOLD.load(Ordering::Relaxed), 120);
        configure(240, usize::MAX);
        assert_eq!(THRESHOLD.load(Ordering::Relaxed), 120, "50% を自分で当てる");
        configure(0, usize::MAX);
        assert_eq!(
            THRESHOLD.load(Ordering::Relaxed),
            0,
            "上限なしなら判定しない"
        );
        reset();
    }

    /// 履歴スレッドの口 ([`check`]) が `/events` に `anomaly` で 1 件書く。
    ///
    /// 出来事のリングは静的に 1 本なので、[`crate::events`] のテストと同じ鍵で 1 つずつ通す。
    #[test]
    fn check_writes_one_anomaly_event() {
        let _g = crate::events::TEST_LOCK.locked();
        crate::events::clear();
        reset();
        let metrics = Metrics::new();
        let mut sample = Sample {
            t: 1_789_251_465,
            ..Sample::default()
        };
        check(&metrics, &sample);
        assert!(crate::events::is_empty(), "1 本目では立たない");
        metrics
            .rejected_client_acl
            .fetch_add(7, std::sync::atomic::Ordering::Relaxed);
        sample.t += 5;
        check(&metrics, &sample);
        let (events, _) = crate::events::select(0, 10);
        assert_eq!(events.len(), 1, "{:?}", events);
        assert_eq!(events[0].kind, crate::events::EventKind::Anomaly);
        assert_eq!(events[0].text, "rejected: rejected_client_acl +7 (total 7)");
        // 同じ種類は収まるまで 1 回だけ
        sample.t += 5;
        check(&metrics, &sample);
        assert_eq!(crate::events::len(), 1);
        crate::events::clear();
        reset();
    }
}
