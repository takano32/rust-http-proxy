//! 異常の自動検知 (`/events` の `anomaly`。T14.23)。
//!
//! 24 時間の図を人が全部読むのは手間がかかる (T14.0 は 72.7 時間ぶんを手で読んだ)。
//! ここは**「いつから」を機械に印させる**もので、履歴スレッドが 5 秒ごとに取る標本
//! ([`crate::history::Sample`]) を直近 5 分の窓に畳み、**直近 1 時間の基準値と比べて
//! 外れた瞬間**を [`crate::events`] のリングに 1 件書く。
//!
//! **費用は 0**: 判定するのは履歴スレッドの周期だけで、要求ごとの経路には 1 命令も無い。
//! 5 秒に 1 回、原子 2 つと山の写真の通算を読み、`/history` の 5 秒のリング (4,320 標本。
//! T14.32 で 6 時間ぶんになった) を 2 回畳むだけで、システムコールは増えない
//! (畳むのは直近 5 分と直近 1 時間の標本だけで、残りは時刻の比較 1 回で飛ばす)。
//!
//! 判定は **8 種で固定** ([`Kind`]) + **規則 6** ([`NewClients`])。閾は本文のとおり:
//!
//! | 種類 | 立つ条件 |
//! |---|---|
//! | `connect_p95` | CONNECT 1 本ごとの「確立 − オリジン側の RTT」(名前解決を除く。T29.11) の p95 (直近 5 分) が直近 1 時間の p95 の [`CONNECT_RATIO`] 倍以上、かつ [`CONNECT_MIN_MS`] 以上、かつ [`CONNECT_MIN_SAMPLES`] 本以上 |
//! | `dns_slow` | **443・80 宛ての接続のための** (T33.3)、**名前解決に成功した**ミス (T32.16) 1 回の平均 (直近 5 分) が [`DNS_MISS_MS`] と canary の基準線の [`DNS_CANARY_RATIO`] 倍の大きい方以上、かつ [`DNS_MIN_MISSES`] 回以上 (T17.1)、かつ窓のミスの合計が [`DNS_MIN_SUM_MS`] 以上 (T22.2) |
//! | `errors` | エラーが 5 分で [`ERRORS_MIN`] 件以上 (**`/slo` の `error_rate` と同じ数え方**: 名前解決の失敗 (原因 `dns` の全部) と 443・80 以外の宛先のエラーは数えない。T32.36) |
//! | `active_high` | 同時接続の山が `max_conns` の [`ACTIVE_PERCENT`]% 以上 (T14.6 の写真と同じ閾。写真があればその番号) |
//! | `rejected` | `rejected_overload` / `evicted_idle` / `rejected_client_acl` が増えた |
//! | `cpu_throttled` | 直近 5 分に絞られた期間の割合が [`CPU_THROTTLED`] 以上 (T15.0 (6)) |
//! | `tunnel_spin` | 5 秒に [`TUNNEL_SPIN_DELTA`] 回以上空回りするトンネルが [`TUNNEL_SPIN_SECS`] 秒続いた (T15.0 (6)) |
//! | `dns_miss_rate` | 名前解決に成功したミス (`dns_misses − dns_fail_misses`。`/slo` と同じ。T32.36) が直近 1 時間で [`DNS_MISS_RATE`] 回/接続 以上 (T15.0 (9)) |
//! | `new_client` | `/clients` の `first_seen` がこの周期の窓の中 (**規則 6**。T14.54) |
//!
//! **規則 6 だけは形が違う**: 上の 8 種が「立つ / 収まる」の状態を持つ (同じ種類は
//! 収まるまで 1 回) のに対し、規則 6 は**接続元ごとに 1 回**で、立ちっ放しにも
//! 解除にもならない。出来事の種類も `anomaly` ではなく **`new_client`**
//! ([`crate::events::EventKind::NewClient`]。[`crate::events::KINDS`] の末尾に足したので
//! T14.9 の永続化の符号 0〜10 は動いていない)。上の 8 種はどれも `anomaly` のまま。
//!
//! **同じ種類は収まるまで 1 回だけ**書く。条件を外れたまま [`CLEAR_SECS`] 秒
//! (5 分) 続いたら `cleared: <種類>` で始まる解除の 1 件を書き、また立てるようになる。
//! 説明には必ず数字 (何が・いくつ・基準値) を入れる。
//!
//! **`connect_p95` は T29.11 から確立そのものではなく「確立 − RTT」(`conn_excess`) で読む**。
//! 0.28.48 の 24 時間で 6 件立ち (0.27.8 は 0)、再現できた窓は成功 36 本のうち上位 3 本が全部 `:993`
//! (名前解決 296 / 532 ms と遠い RTT) で、同じ分に `dns_slow` も立っていた = 遠い宛先の割合が動くだけで
//! 鳴っていた。確立の p95 は距離 (1 RTT) で決まるので、RTT を引いた残り (プロキシとカーネルの取り分・SYN の
//! 再送) で読み、名前解決の遅れは `dns_slow` に任せる。T28.45 の `excess` (名前解決を含む) で読むと、
//! 同じ 24 時間の `history["60"]` の再生で 8 回立った (名前解決の遅いメール宛) ので、名前解決を除いた
//! T29.12 の窓を使う。閾 (50 ms・3 倍・20 本) はそのまま。基準値の p95 は 0〜1 ms のことが多いので、
//! 「基準値が 0 なら立たない」は「1 時間の窓に 1 本も無ければ立たない」に変えた。
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
//! 累計と山の写真の通算) と、種類ごとの立ち上がり ([`KindState`])、
//! それに規則 6 の「もう書いた接続元」 ([`NewClients`]) だけ。

mod detector;
#[cfg(test)]
mod detector_tests;
mod text;

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::history::Sample;
use crate::metrics::Metrics;
use crate::sync::LockExt;

pub(crate) use detector::Detector;

/// (1) CONNECT 確立の p95 が基準値の何倍で立つか。
pub(crate) const CONNECT_RATIO: f64 = 3.0;
/// (1) かつ、この ms 以上のときだけ (速いところの 3 倍は異常ではない)。
pub(crate) const CONNECT_MIN_MS: f64 = 50.0;
/// (1) かつ、5 分の窓にこの本数以上あるときだけ (T15.0 (9))。
///
/// p95 は 250 ms 超が 1〜2 本しかない窓では**窓の最大値に潰れる**
/// ([`crate::window`] の `.min(self.ms_max)`。**これは仕様で、触らない**)。
/// 257 / 265 / 275 ms で立った件はどれも「1 本の 250 ms 級」だった。
pub(crate) const CONNECT_MIN_SAMPLES: u64 = 20;
/// (2) 名前解決のミス 1 回の平均 (ms)。これより低い閾にはしない (canary が速くても)。
pub(crate) const DNS_MISS_MS: f64 = 100.0;
/// (2) かつ、5 分の窓にこの回数以上のミスがあるときだけ (T17.1)。
///
/// [`CONNECT_MIN_SAMPLES`] と同じ考え方。keep-warm (T14.1 / T15.4) で安いミスが消え、
/// 残ったのは**たまにしか引かない名前の高いミス** (平均 98 ms) なので、5 分に 1 回の
/// ミスが 240〜470 ms だとそれだけで平均が閾を越えた (T16.99: 47 時間で 29 件)。
pub(crate) const DNS_MIN_MISSES: u64 = 3;
/// (2) かつ、5 分の窓のミスの**合計**がこの ms 以上のときだけ (T22.2)。
///
/// T21.3 (serve-stale) で `expired` のミスが減り、窓に残るのは初めて引く名前 (`cold`) の
/// 3〜5 回だけになった。その中の遅い 1 回で平均が 100 ms を越え、tokyo では 24 時間で
/// 9 件立った (`scripts/dns-slow-replay.py`)。合計 1,000 ms を足すと、ミス 10 回未満で合計の
/// 小さい窓は立たず、本当に遅い一続き (ミス 10 回以上で平均 100 ms 超え) は 4 件とも残る。
/// ミスの回数の下限を上げる案 (10 回) は採らない — 量の少ない網では 5 分で 10 回に届かず、
/// ほぼ鳴らなくなる。合計なら 3 回でも平均 334 ms 以上なら立つ。
pub(crate) const DNS_MIN_SUM_MS: u64 = 1_000;
/// (2) canary の名前解決の中央値 (直近 1 時間) のこの倍を閾にする (T17.1)。
///
/// リゾルバそのものが遅い網では 100 ms を常に越えるので、「いつもより遅い」を
/// 見るための基準線。canary が `off` か 1 回も回っていなければ [`DNS_MISS_MS`] だけ。
pub(crate) const DNS_CANARY_RATIO: f64 = 3.0;
/// (3) 5 分のエラー件数。
pub(crate) const ERRORS_MIN: u64 = 5;
/// (4) 同時接続の山が `max_conns` のこの割合 (%) 以上で立つ (T14.6 の写真と同じ閾)。
pub(crate) const ACTIVE_PERCENT: usize = 50;
/// (7) 直近 5 分に絞られた期間の割合がこれ以上で立つ (T15.0 (6))。
///
/// 2026-09-18 のデプロイ先は約 100%、0 時間の雪像ではほぼ 0% だったので、
/// この 2 つを分けられる所に置く。
pub const CPU_THROTTLED: f64 = 0.50;
/// (7) 立っている間はこの割合を下回るまで収まらない (`connect_p95` と同じ形)。
pub(crate) const CPU_THROTTLED_CLEAR: f64 = 0.25;
/// (8) 1 周期にこの回数以上空回りしたトンネルを「回っている」とみなす (T15.0 (6))。
///
/// 本番の周期は 5 秒なので「5 秒で 1,000 回以上」= 200 回/秒。T15.5 の空回りは
/// 秒あたり数万で増え続ける形だったので、平常時とは桁が違う。
pub(crate) const TUNNEL_SPIN_DELTA: u64 = 1_000;
/// (8) 回りっ放しがこの秒だけ続いたら 1 件 (T15.0 (6))。
pub(crate) const TUNNEL_SPIN_SECS: u64 = 60;
/// (9) 名前解決のミスが直近 1 時間でこの回数/接続 以上で立つ (T15.0 (9))。
///
/// 閾の根拠: Phase 13 (keep-warm が効かない世界) が 0.55、いまの切り方 9 通りの幅が
/// 0.094〜0.171、再起動後の最初の 6 時間だけが 0.291。T15.4 (窓 3,600 秒) のあとの
/// 平常時は 0.05 で、0.40 (その 8 倍) では 0.30 に戻っても立たないので 0.20 に下げた (T17.1)。
pub(crate) const DNS_MISS_RATE: f64 = 0.20;
/// (9) 立っている間はこの値を下回るまで収まらない (閾の半分。`cpu_throttled` と同じ比)。
pub(crate) const DNS_MISS_RATE_CLEAR: f64 = 0.10;
/// (9) 1 時間の窓にこの本数以上の確立があるときだけ判定する (率の分母)。
pub(crate) const DNS_MISS_RATE_MIN_CONNECTS: u64 = 30;
/// (9) 起動からこの秒が経つまでは判定しない (T15.0 (9))。
///
/// 再起動の直後は**どの名前もまだ warm でない**ので、ミス率は構造的に高い
/// (実測 0.291)。それを「異常」と書くと、再起動のたびに 1 件出る。
pub(crate) const DNS_MISS_RATE_WARMUP_SECS: u64 = 6 * 3600;
/// (6) 1 周期に `new_client` を書く上限 (越えた分は 1 件にまとめる)。
///
/// 走査を受けると 1 周期に何百もの接続元が初めて現れうる。**出来事のリングは 512 件**
/// (T14.11) なので、上限を置かないと 1 回の走査で今までの出来事が全部流れてしまう。
pub(crate) const NEW_CLIENT_MAX: usize = 8;
/// (6) 「もう書いた接続元」を覚えておく数の上限。
///
/// 接続元の表そのものが [`crate::metrics::MAX_CLIENTS`] 件で頭打ち (あふれた分は
/// `other` の 1 行) なので、ここもその数で足りる。越えたら**忘れずに数える**のを
/// やめる (窓の判定だけが残るので、境目の秒に現れた接続元が二度出ることがある)。
pub(crate) const NEW_CLIENT_REMEMBER: usize = crate::metrics::MAX_CLIENTS;
/// (6) 1 周期に鍵の内側から写す上限 ([`NEW_CLIENT_MAX`] より多めに取る)。
///
/// 窓の境目の秒に現れた接続元は「もう書いた」側に落ちるので、[`NEW_CLIENT_MAX`] ちょうどで
/// 取ると**書いたものだけで埋まって新しいものが落ちる**ことがある。4 倍を写して、
/// 落としてから上限を当てる。
const NEW_CLIENT_SCAN: usize = NEW_CLIENT_MAX * 4;
/// (6) 説明に入れる `User-Agent` の長さ (バイト)。
///
/// 出来事 1 件は [`crate::events::MAX_TEXT`] (128 B) で切られる。`User-Agent` は
/// **説明の最後**に置いてあるので、切られるのはここだけ (接続元と数字は残る)。
const NEW_CLIENT_AGENT_BYTES: usize = 48;

/// 直近の窓 (秒)。判定はこの窓の値を [`BASE_SECS`] の窓と比べる。
pub(crate) const WINDOW_SECS: u64 = 300;
/// 基準値を取る窓 (秒)。直近の窓もこの中に入っている。
pub(crate) const BASE_SECS: u64 = 3600;
/// 条件を外れてから解除の 1 件を書くまで (秒)。
pub(crate) const CLEAR_SECS: u64 = WINDOW_SECS;

/// CPU の絞りを判定してよいか (**5 分ぶん標本が溜まったか**。T15.0 (6))。
///
/// 渡すのは「**いちばん最初に読めた標本からの秒**」で、窓の両端の差ではない。
/// 標本の時刻は生の epoch 秒 (`now_epoch()`) で、履歴スレッドの 1 周は
/// `sleep(5s)` の**あと**に 1 周ぶんの仕事をするので必ず 5 秒より長く、6 秒の
/// 間隔が周期的に混ざる。畳んだ窓の両端の差は構造上 [`WINDOW_SECS`] を越えられない
/// ([`Detector::cpu_window`] の枝打ち) ので、それを `>= WINDOW_SECS` で見ると
/// **整数がちょうど一致する刻みでしか開かない門**になり、1 周が 17 ms 伸びただけで
/// 二度と開かなくなる (しかも絞られている最中は 1 周が伸びる側)。
/// **起点からの経過**で見れば刻みのずれに左右されない。
///
/// `/healthz` の検査 `cpu` ([`crate::kernel::Health::sampled_secs`]) も同じ関数を通る。
pub fn cpu_window_is_full(sampled_secs: u64) -> bool {
    sampled_secs >= WINDOW_SECS
}

/// 判定の種類 (**8 種で固定**。出来事の種類はどれも `anomaly` で、これは説明の頭に出る)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
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
    /// cgroup の CPU の上限で絞られ続けている (T15.0 (6))
    CpuThrottled,
    /// 起こされても 1 バイトも進まないトンネルが回り続けている (T15.0 (6))
    TunnelSpin,
    /// 名前解決のミスの**率**が高い (T15.0 (9))
    DnsMissRate,
}

/// 全種類 (README の一覧と同じ並び)。
///
/// **[`Detector`] の `state` と [`Detector::hits`] の戻りは添字でここに対応する**
/// (ずれても型では落ちない)。**足すのは末尾だけ** — 既存 5 種の添字を動かすと、
/// 立っている状態が別の種類のものとして読み継がれる。
pub(crate) const KINDS: [Kind; 8] = [
    Kind::ConnectP95,
    Kind::DnsSlow,
    Kind::Errors,
    Kind::ActiveHigh,
    Kind::Rejected,
    Kind::CpuThrottled,
    Kind::TunnelSpin,
    Kind::DnsMissRate,
];

/// [`KINDS`] の添字 (**直値で引かない**。足すたびにずれるため)。
pub(crate) const I_CONNECT_P95: usize = 0;
pub(crate) const I_DNS_SLOW: usize = 1;
pub(crate) const I_ERRORS: usize = 2;
pub(crate) const I_ACTIVE_HIGH: usize = 3;
pub(crate) const I_REJECTED: usize = 4;
pub(crate) const I_CPU_THROTTLED: usize = 5;
pub(crate) const I_TUNNEL_SPIN: usize = 6;
pub(crate) const I_DNS_MISS_RATE: usize = 7;

impl Kind {
    /// 説明の頭に出す名前 (`cleared:` のあとに出るのもこれ)。
    pub(crate) fn name(self) -> &'static str {
        match self {
            Kind::ConnectP95 => "connect_p95",
            Kind::DnsSlow => "dns_slow",
            Kind::Errors => "errors",
            Kind::ActiveHigh => "active_high",
            Kind::Rejected => "rejected",
            Kind::CpuThrottled => "cpu_throttled",
            Kind::TunnelSpin => "tunnel_spin",
            Kind::DnsMissRate => "dns_miss_rate",
        }
    }
}

/// 標本 (`/history` のリング) から読めない値。**テストはここへ直接流し込む**
/// (時刻も `t` で注入する)。累計のものは判定が 1 本前 (か 5 分前) との差だけを見る。
///
/// `Eq` を持たないのは [`Counters::cpu_quota_cores`] が `f64` だから (T15.0 (6))。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Counters {
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
    /// cgroup の `cpu.stat` の累計 (T15.0 (6))。**親の階層の値のことがある**ので、
    /// 判定が見るのは 5 分の窓の**増分どうしの比**だけ
    pub cpu_nr_throttled: u64,
    pub cpu_nr_periods: u64,
    /// `cpu.max` の quota ÷ period (0 = 無制限か読めない)
    pub cpu_quota_cores: f64,
    /// このプロセスが使った CPU の累計 (us。`/proc/self/stat` の utime + stime)
    pub cpu_used_us: u64,
    /// 直近の窓でいちばん CPU を使ったスレッド (`/profile` の `threads_top` の先頭。
    /// `0` = 標本が無い / `--lite`) と、その [`crate::profile::ROLES`] の添字
    pub cpu_top_tid: u32,
    pub cpu_top_role: u8,
    /// 起動からの秒 (T15.0 (9))。**`Detector` が最初の `t` を覚える案は採らない** —
    /// `.rrd` から読み戻した直後の窓を「起動直後」と区別できないため
    pub uptime_secs: u64,
    /// canary の名前解決の中央値 (ms。直近 [`BASE_SECS`] 秒。T17.1)。
    /// `None` = canary が `off` か、窓に 1 回も無い
    pub canary_dns_p50_ms: Option<u64>,
}

impl Counters {
    /// 標本 1 本ぶん集める (履歴スレッドから)。原子 2 つと写真の通算 1 つ、
    /// それに T15.0 (6)(9) の 4 つ (メモリ上の窓 2 つと `/proc/self/stat` 1 回)。
    fn take(metrics: &Metrics, s: &Sample) -> Counters {
        // カーネルの窓は同じ周期で既に進んでいる (`tick.rs` が標本を取るときに読む)
        let k = crate::kernel::latest().unwrap_or_default();
        // 直近の 5 秒の窓の上位スレッド 1 本 (`--lite` と標本が無い窓では tid 0)
        let top = metrics.profile.recent_totals(0, 1).threads_top[0];
        Counters {
            t: s.t,
            // 標本が既に持っている (`/history` の列。T14.2 (3))
            evicted_idle: s.evicted_idle,
            rejected_overload: metrics.rejected_overload.load(Ordering::Relaxed),
            rejected_client_acl: metrics.rejected_client_acl.load(Ordering::Relaxed),
            // 次に撮る番号の 1 つ前 = いままでに撮った枚数 (= 最後の 1 枚の `seq`)
            shots: metrics.bursts.next_seq().saturating_sub(1),
            cpu_nr_throttled: k.cpu_nr_throttled,
            cpu_nr_periods: k.cpu_nr_periods,
            cpu_quota_cores: k.cpu_quota_cores,
            cpu_used_us: crate::profile::process_cpu_us().unwrap_or(0),
            cpu_top_tid: top.tid,
            cpu_top_role: top.role,
            uptime_secs: metrics.start_time.elapsed().as_secs(),
            // 窓は 5 秒 × 720 行を 1 回舐めるだけ。`off` に切り替えたあとは古い行を見ない
            canary_dns_p50_ms: if crate::canary::mode() == crate::canary::Mode::Off {
                None
            } else {
                crate::canaryhist::dns_p50_ms(s.t.saturating_sub(BASE_SECS), s.t)
            },
        }
    }
}

/// 立った / 解除された 1 件 (説明はそのまま `/events` の `text` になる)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fired {
    pub kind: Kind,
    /// `true` なら解除 (説明は `cleared: <種類>` で始まる)
    pub cleared: bool,
    pub text: String,
}

/// 同時接続数の上限と、山と見なす本数 ([`configure`] が入れる)。
static MAX_CONNS: AtomicUsize = AtomicUsize::new(0);
static THRESHOLD: AtomicUsize = AtomicUsize::new(0);

/// 判定の入れ物。**触るのは履歴スレッドだけ** (5 秒に 1 回)。
static DETECTOR: Mutex<Option<Detector>> = Mutex::new(None);

/// 同時接続の山の閾を教える (`crates/run/src/lib.rs` の起動時に 1 回)。
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
/// 通算 1 つ・5 秒のリングを 2 回畳むぶん (720 標本 × 2 = 約 7 万回の加算) だけで、
/// 5 秒に 1 回なら測れない。
pub fn check(metrics: &Metrics, sample: &Sample) {
    let (fired, newcomers) = {
        let mut guard = DETECTOR.locked();
        let d = guard.get_or_insert_with(Detector::new);
        d.set_limits(
            MAX_CONNS.load(Ordering::Relaxed),
            THRESHOLD.load(Ordering::Relaxed),
        );
        // `conns` を渡すのは (8) のため。`update_rates` (`tick.rs` の周期の頭) が
        // **同じ周期で先に**空回りの差分を書いているので、ここで読めば同じ 5 秒の値
        let fired = d.observe(
            &metrics.history,
            Some(&metrics.conns),
            Counters::take(metrics, sample),
        );
        // 規則 6 (初めて見た接続元。T14.54)。標本ではなく `/clients` を見る
        (fired, d.observe_clients(metrics, sample.t))
    };
    // 書くのは鍵の外 (出来事のリングと判定の鍵を同時に握らない)
    for f in fired {
        crate::events::push(crate::events::EventKind::Anomaly, &f.text);
    }
    for text in newcomers {
        crate::events::push(crate::events::EventKind::NewClient, &text);
    }
}

/// 覚えている状態を捨てる (テスト用)。
#[cfg(test)]
pub(crate) fn reset() {
    *DETECTOR.locked() = None;
    MAX_CONNS.store(0, Ordering::Relaxed);
    THRESHOLD.store(0, Ordering::Relaxed);
}

/// `anomaly` の単体テスト (T22.2 で別のファイルに移し、T34.47 で規則ごとに `tests/` へ割った)。
#[cfg(test)]
mod tests;
