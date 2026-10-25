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
//! | `connect_p95` | CONNECT 確立の p95 (直近 5 分) が直近 1 時間の p95 の [`CONNECT_RATIO`] 倍以上、かつ [`CONNECT_MIN_MS`] 以上、かつ [`CONNECT_MIN_SAMPLES`] 本以上 |
//! | `dns_slow` | 名前解決のミス 1 回の平均 (直近 5 分) が [`DNS_MISS_MS`] と canary の基準線の [`DNS_CANARY_RATIO`] 倍の大きい方以上、かつ [`DNS_MIN_MISSES`] 回以上 (T17.1)、かつ窓のミスの合計が [`DNS_MIN_SUM_MS`] 以上 (T22.2) |
//! | `errors` | エラーが 5 分で [`ERRORS_MIN`] 件以上 (**`nxdomain` は数えない**。T.28.17) |
//! | `active_high` | 同時接続の山が `max_conns` の [`ACTIVE_PERCENT`]% 以上 (T14.6 の写真と同じ閾。写真があればその番号) |
//! | `rejected` | `rejected_overload` / `evicted_idle` / `rejected_client_acl` が増えた |
//! | `cpu_throttled` | 直近 5 分に絞られた期間の割合が [`CPU_THROTTLED`] 以上 (T15.0 (6)) |
//! | `tunnel_spin` | 5 秒に [`TUNNEL_SPIN_DELTA`] 回以上空回りするトンネルが [`TUNNEL_SPIN_SECS`] 秒続いた (T15.0 (6)) |
//! | `dns_miss_rate` | 名前解決のミスが直近 1 時間で [`DNS_MISS_RATE`] 回/接続 以上 (T15.0 (9)) |
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

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::history::summary::{self, Params, Summary};
use crate::history::{History, Sample};
use crate::metrics::{ERR_CAUSE_NAMES, ERR_CAUSES, Metrics, NewClient};
use crate::recent::{ConnTable, SpinningConn};
use crate::sync::LockExt;

/// (1) CONNECT 確立の p95 が基準値の何倍で立つか。
pub const CONNECT_RATIO: f64 = 3.0;
/// (1) かつ、この ms 以上のときだけ (速いところの 3 倍は異常ではない)。
pub const CONNECT_MIN_MS: f64 = 50.0;
/// (1) かつ、5 分の窓にこの本数以上あるときだけ (T15.0 (9))。
///
/// p95 は 250 ms 超が 1〜2 本しかない窓では**窓の最大値に潰れる**
/// ([`crate::window`] の `.min(self.ms_max)`。**これは仕様で、触らない**)。
/// 257 / 265 / 275 ms で立った件はどれも「1 本の 250 ms 級」だった。
pub const CONNECT_MIN_SAMPLES: u64 = 20;
/// (2) 名前解決のミス 1 回の平均 (ms)。これより低い閾にはしない (canary が速くても)。
pub const DNS_MISS_MS: f64 = 100.0;
/// (2) かつ、5 分の窓にこの回数以上のミスがあるときだけ (T17.1)。
///
/// [`CONNECT_MIN_SAMPLES`] と同じ考え方。keep-warm (T14.1 / T15.4) で安いミスが消え、
/// 残ったのは**たまにしか引かない名前の高いミス** (平均 98 ms) なので、5 分に 1 回の
/// ミスが 240〜470 ms だとそれだけで平均が閾を越えた (T16.99: 47 時間で 29 件)。
pub const DNS_MIN_MISSES: u64 = 3;
/// (2) かつ、5 分の窓のミスの**合計**がこの ms 以上のときだけ (T22.2)。
///
/// T21.3 (serve-stale) で `expired` のミスが減り、窓に残るのは初めて引く名前 (`cold`) の
/// 3〜5 回だけになった。その中の遅い 1 回で平均が 100 ms を越え、tokyo では 24 時間で
/// 9 件立った (`scripts/dns-slow-replay.py`)。合計 1,000 ms を足すと、ミス 10 回未満で合計の
/// 小さい窓は立たず、本当に遅い一続き (ミス 10 回以上で平均 100 ms 超え) は 4 件とも残る。
/// ミスの回数の下限を上げる案 (10 回) は採らない — 量の少ない網では 5 分で 10 回に届かず、
/// ほぼ鳴らなくなる。合計なら 3 回でも平均 334 ms 以上なら立つ。
pub const DNS_MIN_SUM_MS: u64 = 1_000;
/// (2) canary の名前解決の中央値 (直近 1 時間) のこの倍を閾にする (T17.1)。
///
/// リゾルバそのものが遅い網では 100 ms を常に越えるので、「いつもより遅い」を
/// 見るための基準線。canary が `off` か 1 回も回っていなければ [`DNS_MISS_MS`] だけ。
pub const DNS_CANARY_RATIO: f64 = 3.0;
/// (3) 5 分のエラー件数。
pub const ERRORS_MIN: u64 = 5;
/// (4) 同時接続の山が `max_conns` のこの割合 (%) 以上で立つ (T14.6 の写真と同じ閾)。
pub const ACTIVE_PERCENT: usize = 50;
/// (7) 直近 5 分に絞られた期間の割合がこれ以上で立つ (T15.0 (6))。
///
/// 2026-09-18 のデプロイ先は約 100%、0 時間の雪像ではほぼ 0% だったので、
/// この 2 つを分けられる所に置く。
pub const CPU_THROTTLED: f64 = 0.50;
/// (7) 立っている間はこの割合を下回るまで収まらない (`connect_p95` と同じ形)。
pub const CPU_THROTTLED_CLEAR: f64 = 0.25;
/// (8) 1 周期にこの回数以上空回りしたトンネルを「回っている」とみなす (T15.0 (6))。
///
/// 本番の周期は 5 秒なので「5 秒で 1,000 回以上」= 200 回/秒。T15.5 の空回りは
/// 秒あたり数万で増え続ける形だったので、平常時とは桁が違う。
pub const TUNNEL_SPIN_DELTA: u64 = 1_000;
/// (8) 回りっ放しがこの秒だけ続いたら 1 件 (T15.0 (6))。
pub const TUNNEL_SPIN_SECS: u64 = 60;
/// (9) 名前解決のミスが直近 1 時間でこの回数/接続 以上で立つ (T15.0 (9))。
///
/// 閾の根拠: Phase 13 (keep-warm が効かない世界) が 0.55、いまの切り方 9 通りの幅が
/// 0.094〜0.171、再起動後の最初の 6 時間だけが 0.291。T15.4 (窓 3,600 秒) のあとの
/// 平常時は 0.05 で、0.40 (その 8 倍) では 0.30 に戻っても立たないので 0.20 に下げた (T17.1)。
pub const DNS_MISS_RATE: f64 = 0.20;
/// (9) 立っている間はこの値を下回るまで収まらない (閾の半分。`cpu_throttled` と同じ比)。
pub const DNS_MISS_RATE_CLEAR: f64 = 0.10;
/// (9) 1 時間の窓にこの本数以上の確立があるときだけ判定する (率の分母)。
pub const DNS_MISS_RATE_MIN_CONNECTS: u64 = 30;
/// (9) 起動からこの秒が経つまでは判定しない (T15.0 (9))。
///
/// 再起動の直後は**どの名前もまだ warm でない**ので、ミス率は構造的に高い
/// (実測 0.291)。それを「異常」と書くと、再起動のたびに 1 件出る。
pub const DNS_MISS_RATE_WARMUP_SECS: u64 = 6 * 3600;
/// (6) 1 周期に `new_client` を書く上限 (越えた分は 1 件にまとめる)。
///
/// 走査を受けると 1 周期に何百もの接続元が初めて現れうる。**出来事のリングは 512 件**
/// (T14.11) なので、上限を置かないと 1 回の走査で今までの出来事が全部流れてしまう。
pub const NEW_CLIENT_MAX: usize = 8;
/// (6) 「もう書いた接続元」を覚えておく数の上限。
///
/// 接続元の表そのものが [`crate::metrics::MAX_CLIENTS`] 件で頭打ち (あふれた分は
/// `other` の 1 行) なので、ここもその数で足りる。越えたら**忘れずに数える**のを
/// やめる (窓の判定だけが残るので、境目の秒に現れた接続元が二度出ることがある)。
pub const NEW_CLIENT_REMEMBER: usize = crate::metrics::MAX_CLIENTS;
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
pub const WINDOW_SECS: u64 = 300;
/// 基準値を取る窓 (秒)。直近の窓もこの中に入っている。
pub const BASE_SECS: u64 = 3600;
/// 条件を外れてから解除の 1 件を書くまで (秒)。
pub const CLEAR_SECS: u64 = WINDOW_SECS;

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
    /// cgroup の CPU の上限で絞られ続けている (T15.0 (6))
    CpuThrottled,
    /// 起こされても 1 バイトも進まないトンネルが回り続けている (T15.0 (6))
    TunnelSpin,
    /// 名前解決のミスの**率**が高い (T15.0 (9))
    DnsMissRate,
}

/// 全種類 (README の一覧と同じ並び)。
///
/// **[`Detector::state`] と [`Detector::observe`] の `hits` は添字でここに対応する**
/// (ずれても型では落ちない)。**足すのは末尾だけ** — 既存 5 種の添字を動かすと、
/// 立っている状態が別の種類のものとして読み継がれる。
pub const KINDS: [Kind; 8] = [
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
pub const I_CONNECT_P95: usize = 0;
pub const I_DNS_SLOW: usize = 1;
pub const I_ERRORS: usize = 2;
pub const I_ACTIVE_HIGH: usize = 3;
pub const I_REJECTED: usize = 4;
pub const I_CPU_THROTTLED: usize = 5;
pub const I_TUNNEL_SPIN: usize = 6;
pub const I_DNS_MISS_RATE: usize = 7;

impl Kind {
    /// 説明の頭に出す名前 (`cleared:` のあとに出るのもこれ)。
    pub fn name(self) -> &'static str {
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
    /// 名前解決の失敗のうち `nxdomain` の通算 ([`Metrics::dns_cause_totals`] の 1 番目。T.28.17)。
    ///
    /// 客が無い名前を頼んだ (= `blocked` と同じ客の都合) ので `errors` の判定から外す。
    /// 標本 (`.rrd`) は DNS の内訳を持たない (T25.0) ので、判定が 5 分ぶんの点を覚えて増分を出す
    pub nxdomain: u64,
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
            // ホスト別の表を 1 回舐める (最大 1,000 行。5 秒に 1 回、`/status` を組むのと同じ数え方)
            nxdomain: metrics.dns_cause_totals()[crate::metrics::DnsCause::Nxdomain as usize],
        }
    }
}

/// [`Detector`] が覚えておく CPU の 1 点 (5 分の増分を出すため。T15.0 (6))。
#[derive(Clone, Copy, Debug, Default)]
struct CpuPoint {
    t: u64,
    throttled: u64,
    periods: u64,
    used_us: u64,
}

/// 5 分の窓ぶんの CPU の増分 (いちばん古い点と今の点の差)。
#[derive(Clone, Copy, Debug, Default)]
struct CpuWindow {
    /// 窓の秒 (点が 1 つしか無ければ 0)
    secs: u64,
    /// **5 分ぶん溜まったか** ([`cpu_window_is_full`]。窓の両端の差ではなく、
    /// いちばん最初に読めた標本からの経過で見る)
    full: bool,
    throttled: u64,
    periods: u64,
    used_us: u64,
    /// `cpu.max` の quota ÷ period (0 = 無制限か読めない)
    quota_cores: f64,
    top_tid: u32,
    top_role: u8,
}

impl CpuWindow {
    /// 絞られた期間の割合 (0.0〜1.0)。期間が 1 つも過ぎていなければ `None`。
    fn ratio(&self) -> Option<f64> {
        (self.periods > 0).then(|| self.throttled as f64 / self.periods as f64)
    }

    /// この窓で使った CPU のコア数 (1.0 = 1 コアを丸ごと)。
    fn cores(&self) -> f64 {
        if self.secs == 0 {
            0.0
        } else {
            self.used_us as f64 / (self.secs as f64 * 1e6)
        }
    }

    /// 上位スレッドの「tid と役割」(標本が無ければ空文字列)。
    fn top(&self) -> String {
        if self.top_tid == 0 {
            return String::new();
        }
        let role = crate::profile::ROLES
            .get(self.top_role as usize)
            .copied()
            .unwrap_or("other");
        format!(", tid {} {}", self.top_tid, role)
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
    /// 規則 6 (初めて見た接続元。T14.54)
    new_clients: NewClients,
    /// 直近 5 分ぶんの CPU の累計 (T15.0 (6))。61 点しか持たない
    cpu_hist: VecDeque<CpuPoint>,
    /// **いちばん最初に読めた** cgroup の標本の時刻 (T15.0 (6))。
    /// 「窓が 5 分ぶん溜まったか」([`cpu_window_is_full`]) の起点
    cpu_since: Option<u64>,
    /// 回りっ放しのトンネルを最初に見つけた時刻 (T15.0 (6))。
    /// [`KindState`] は「立つ / 解除」しか持たないので、「1 分続いたら」は
    /// **[`KINDS`] の外に状態を持つ規則** ([`NewClients`] と同じ形) で数える
    spin_since: Option<u64>,
    /// 直近 5 分ぶんの `nxdomain` の通算 (時刻, 通算)。`errors` から外す数を出す (T.28.17)
    nx_hist: VecDeque<(u64, u64)>,
}

/// 規則 6 の覚えていること (T14.54)。
///
/// 窓は**前に見た時刻から今まで** (両端を含む)。両端を含むのは、周期のちょうどその秒に
/// 現れた接続元を落とさないため (`first_seen` は epoch 秒なので、標本を取った直後に
/// 来た接続元は同じ秒を持つ)。二度書かないのは [`NewClients::seen`] の役目。
#[derive(Debug, Default)]
struct NewClients {
    /// 前に見た時刻 (`None` = まだ 1 度も見ていない)。**1 本目は基準にするだけ**で
    /// 書かない (起動時に状態ファイルから読み戻した接続元を新しいと言わないため。
    /// 読み戻した側は `first_seen == 0` でも外れるので、これは二重の歯止め)
    last_t: Option<u64>,
    /// もう `/events` に書いた接続元 ([`NEW_CLIENT_REMEMBER`] 件まで)
    seen: std::collections::HashSet<String>,
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
            new_clients: NewClients::default(),
            cpu_hist: VecDeque::new(),
            cpu_since: None,
            spin_since: None,
            nx_hist: VecDeque::new(),
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
    ///
    /// `conns` は「いま空回りしているトンネル」を引く先 (T15.0 (6))。
    /// **[`ConnTable::update_rates`] のあと**に呼ぶこと (同じ周期の差分を読むため)。
    /// `None` なら `tunnel_spin` は判定しない。
    pub fn observe(&mut self, h: &History, conns: Option<&ConnTable>, c: Counters) -> Vec<Fired> {
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
        // (3) 窓の中の `nxdomain` (客が無い名前を頼んだ) は `errors` に数えない (T.28.17)
        let nx = self.nxdomain_window(c);
        let errors = w5.errors.saturating_sub(nx);

        let p95 = w5.connect.quantile_ms(0.95);
        let p95_base = base.connect.quantile_ms(0.95);
        let peak = w5.active_max;
        // **立っている間は「立った瞬間の基準値」と比べる。** 基準値の窓 (1 時間) は
        // 直近の窓を含むので、山が 1 時間続くと基準値そのものが山になって倍率が 1 に戻り、
        // 山の最中に解除を書いてしまう (T14.0 の 17〜23 時のような 6 時間の山がこれ)。
        // 「収まるまで 1 回だけ」の「収まる」は**元の基準値に戻ること**にする
        let p95_ref = if self.state[I_CONNECT_P95].firing {
            self.state[I_CONNECT_P95].base_p95
        } else {
            p95_base
        };
        // (7) CPU の絞り: 5 分の窓の**増分どうしの比** (累計は親の階層の値のことがある)
        let cpu = self.cpu_window(c);
        let cpu_limit = if self.state[I_CPU_THROTTLED].firing {
            CPU_THROTTLED_CLEAR
        } else {
            CPU_THROTTLED
        };
        // (8) 回っているトンネル。**1 分続いたら**立てる (回り始めた瞬間ではない)
        let (spinning, spinning_total) = match conns {
            Some(t) => t.spinning(TUNNEL_SPIN_DELTA, 1),
            None => (Vec::new(), 0),
        };
        let spin_secs = if spinning_total > 0 {
            now.saturating_sub(*self.spin_since.get_or_insert(now))
        } else {
            self.spin_since = None;
            0
        };
        // (9) ミスの**率**。窓は 1 時間 (`base`) で、立っている間は `CLEAR` の閾を使う
        let miss_rate = base.dns_miss_per_connect();
        let miss_limit = if self.state[I_DNS_MISS_RATE].firing {
            DNS_MISS_RATE_CLEAR
        } else {
            DNS_MISS_RATE
        };
        // (2) ミスの平均の閾。canary の基準線の 3 倍と 100 ms の大きい方 (T17.1)
        let dns_limit = dns_miss_limit(c.canary_dns_p50_ms);
        let hits = [
            // 直近の窓は基準値の中にも入っているので、起動直後 (1 時間ぶんが全部この
            // 5 分) は倍率がちょうど 1 になり、立たない
            p95 >= CONNECT_MIN_MS
                && p95_ref > 0.0
                && p95 >= CONNECT_RATIO * p95_ref
                && w5.connect.count >= CONNECT_MIN_SAMPLES,
            w5.dns_misses >= DNS_MIN_MISSES
                && w5.dns_miss_avg_ms() >= dns_limit
                && w5.dns_ms_sum >= DNS_MIN_SUM_MS,
            errors >= ERRORS_MIN,
            (self.threshold > 0 && peak >= self.threshold as u64) || shot.is_some(),
            delta.iter().any(|&d| d > 0),
            // 窓が 5 分ぶん溜まるまでは判定しない (「5 分のあいだ絞られ続けた」が条件)
            cpu.full && cpu.ratio().is_some_and(|r| r >= cpu_limit),
            spinning_total > 0 && spin_secs >= TUNNEL_SPIN_SECS,
            c.uptime_secs >= DNS_MISS_RATE_WARMUP_SECS
                && base.connects() >= DNS_MISS_RATE_MIN_CONNECTS
                && miss_rate >= miss_limit,
        ];

        // 説明を組むのに要るだけ (下で `self.state` を可変に借りるので先に写しておく)
        let facts = Facts {
            limits: (self.max_conns, self.threshold),
            delta,
            totals,
            shot,
            cpu,
            spinning: &spinning,
            spinning_total,
            spin_secs,
            dns_limit,
            nx,
        };
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
                        text: fired_text(kind, &w5, &base, &facts),
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
                    text: cleared_text(kind, &w5, &base, &facts, mins),
                });
            }
        }
        out
    }

    /// 直近の窓 (`fold(h, now, WINDOW_SECS)` と同じ 5 分 + 1 周期) の `nxdomain` の増分 (T.28.17)。
    ///
    /// 窓の標本は `now - WINDOW_SECS <= t <= now` で、それぞれが手前の 1 周期 (5 秒) を持つので、
    /// 起点は `now - WINDOW_SECS - 5` 秒の点。点がまだそこまで無ければ (起動直後)、いちばん古い点からの増分
    /// (外す数が少なめに出る = 今までの判定に寄る側)。通算が減ったら (`.rrd` の作り直し) 0 にする
    fn nxdomain_window(&mut self, c: Counters) -> u64 {
        self.nx_hist.push_back((c.t, c.nxdomain));
        let edge = c.t.saturating_sub(WINDOW_SECS + 5);
        while self.nx_hist.len() > 1 && self.nx_hist.get(1).is_some_and(|p| p.0 <= edge) {
            self.nx_hist.pop_front();
        }
        let first = self.nx_hist.front().map_or(c.nxdomain, |p| p.1);
        c.nxdomain.saturating_sub(first)
    }

    /// 直近 [`WINDOW_SECS`] 秒ぶんの CPU の増分 (T15.0 (6))。
    ///
    /// 累計そのものは**このプロセスより前から**動いている (自分の階層に cpu
    /// コントローラが無ければ親の値を読む) ので、生の値では何も言えない。
    /// 覚えておくのは 5 分ぶん = 61 点だけ。
    ///
    /// **「5 分ぶん溜まったか」は窓の両端の差では測らない** ([`cpu_window_is_full`]。
    /// 差は枝打ちの側で [`WINDOW_SECS`] 以下に固定されるので、刻みが 1 秒ずれると
    /// 二度と届かない)。起点はこの [`Detector`] が**最初に読めた**標本の時刻。
    fn cpu_window(&mut self, c: Counters) -> CpuWindow {
        // 読めない標本 (cgroup v1 / Linux 以外 / 上限なし) は窓に入れない。0 を混ぜると
        // 「読めるようになった瞬間」に**累計そのもの** (親の階層の値) が増分に化ける
        if c.cpu_nr_periods == 0 {
            return CpuWindow::default();
        }
        let since = *self.cpu_since.get_or_insert(c.t);
        self.cpu_hist.push_back(CpuPoint {
            t: c.t,
            throttled: c.cpu_nr_throttled,
            periods: c.cpu_nr_periods,
            used_us: c.cpu_used_us,
        });
        while self.cpu_hist.len() > 2
            && self
                .cpu_hist
                .front()
                .is_some_and(|p| c.t.saturating_sub(p.t) > WINDOW_SECS)
        {
            self.cpu_hist.pop_front();
        }
        let (Some(first), Some(last)) = (self.cpu_hist.front(), self.cpu_hist.back()) else {
            return CpuWindow::default();
        };
        CpuWindow {
            secs: last.t.saturating_sub(first.t),
            full: cpu_window_is_full(c.t.saturating_sub(since)),
            throttled: last.throttled.saturating_sub(first.throttled),
            periods: last.periods.saturating_sub(first.periods),
            used_us: last.used_us.saturating_sub(first.used_us),
            quota_cores: c.cpu_quota_cores,
            top_tid: c.cpu_top_tid,
            top_role: c.cpu_top_role,
        }
    }

    /// **規則 6**: この周期に初めて見た接続元を `/events` の 1 行にする (T14.54)。
    ///
    /// 上の 5 種と違って「立つ / 収まる」を持たない (**接続元ごとに 1 回**)。
    /// 返すのは説明だけで、書くのは呼んだ側 ([`check`]。鍵を握ったまま
    /// 出来事のリングに触らないため)。
    ///
    /// 見るのは [`Metrics::clients_first_seen_in`] だけ = **鍵の内側でやるのは
    /// `first_seen` の比較**で、標本のリングは 1 度も畳まない。
    pub fn observe_clients(&mut self, metrics: &Metrics, now: u64) -> Vec<String> {
        // 1 本目は「前に見た時刻」を置くだけ (それ以前から居た接続元は新しくない)
        let Some(from) = self.new_clients.last_t.replace(now) else {
            return Vec::new();
        };
        let (found, mut over) = metrics.clients_first_seen_in(from, now, NEW_CLIENT_SCAN);
        let mut out = Vec::new();
        for c in found {
            if self.new_clients.seen.contains(&c.client) {
                continue; // 窓の境目の秒。同じ接続元は 1 回だけ
            }
            if self.new_clients.seen.len() < NEW_CLIENT_REMEMBER {
                self.new_clients.seen.insert(c.client.clone());
            }
            if out.len() >= NEW_CLIENT_MAX {
                over += 1;
                continue;
            }
            out.push(new_client_text(&c));
        }
        if over > 0 {
            // 走査を受けたとき。出来事のリング (512 件) を 1 回で流さないための 1 行
            out.push(format!(
                "new_client: {} more clients first seen in the same {}s window (wrote {})",
                over,
                now.saturating_sub(from).max(1),
                out.len()
            ));
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

/// 直近 `secs` 秒を畳む (T14.24 の関数をそのまま使う。5 秒の標本のリングは
/// 6 時間ぶん = 4,320 本 (T14.32) だが、足し込むのは `secs` に入る標本だけ)。
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

/// 説明を組むのに要る「窓から読めないもの」をひとまとめにしたもの。
///
/// [`Detector::observe`] が 1 周期ぶん作り、[`fired_text`] と [`cleared_text`] が読む。
struct Facts<'a> {
    /// (`max_conns`, 山と見なす本数)
    limits: (usize, usize),
    /// 断った / 追い出した数の増分と累計
    delta: [u64; 3],
    totals: [u64; 3],
    /// この周期に撮れた山の写真の番号
    shot: Option<u64>,
    /// 直近 5 分の CPU の絞り (T15.0 (6))
    cpu: CpuWindow,
    /// いちばん回っているトンネル (多くて 1 本) と、閾を越えた本数、続いている秒
    spinning: &'a [SpinningConn],
    spinning_total: usize,
    spin_secs: u64,
    /// `dns_slow` の閾 (ms。[`dns_miss_limit`])
    dns_limit: f64,
    /// 直近の窓の `nxdomain` (`errors` に数えなかったもの。T.28.17)
    nx: u64,
}

/// `dns_slow` の閾 (ms): [`DNS_MISS_MS`] と canary の基準線の [`DNS_CANARY_RATIO`] 倍の
/// 大きい方 (T17.1)。基準線が無ければ [`DNS_MISS_MS`]。
fn dns_miss_limit(canary_p50_ms: Option<u64>) -> f64 {
    match canary_p50_ms {
        Some(b) => DNS_MISS_MS.max(DNS_CANARY_RATIO * b as f64),
        None => DNS_MISS_MS,
    }
}

/// 異常の説明に入れる宛先の長さ (バイト)。
///
/// 出来事 1 件は [`crate::events::MAX_TEXT`] (128 B) で切られるので、長い名前で
/// 後ろの数字を押し出さない。
const TARGET_BYTES: usize = 24;

/// 立ったときの説明 (**数字を必ず入れる**: 何が・いくつ・基準値)。
fn fired_text(kind: Kind, w5: &Summary, base: &Summary, f: &Facts<'_>) -> String {
    let (max_conns, threshold) = f.limits;
    let (delta, totals, shot) = (f.delta, f.totals, f.shot);
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
            f.dns_limit as u64,
            w5.dns_misses,
            w5.dns_ms_sum
        ),
        Kind::Errors => {
            let by = causes(&w5.errors_by_cause);
            // 数は判定に使った数 (`nxdomain` を外したもの)。外したものがあればそれも書く (T.28.17。
            // 外していなければ文面は今までどおり)。原因の内訳は標本の 8 つのまま (`dns` に `nxdomain` も入る)
            format!(
                "{}: {} errors in 5m (threshold {}{}){}{}",
                head,
                w5.errors.saturating_sub(f.nx),
                ERRORS_MIN,
                if f.nx > 0 {
                    format!("; {} nxdomain not counted", f.nx)
                } else {
                    String::new()
                },
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
        // CPU の上限で絞られ続けている (T15.0 (6))。**割合**で書く
        Kind::CpuThrottled => format!(
            "{}: {:.0}% of {} cpu periods throttled in {}m (threshold {:.0}%; quota {:.2} cores, used {:.2}{})",
            head,
            f.cpu.ratio().unwrap_or(0.0) * 100.0,
            f.cpu.periods,
            f.cpu.secs.div_ceil(60),
            CPU_THROTTLED * 100.0,
            f.cpu.quota_cores,
            f.cpu.cores(),
            f.cpu.top(),
        ),
        // 起こされても進まないトンネルが回り続けている (T15.0 (6))
        Kind::TunnelSpin => {
            let mut s = format!("{}:", head);
            match f.spinning.first() {
                Some(c) => {
                    let _ = write!(
                        s,
                        " conn#{} spun {} times/tick for {}s ({}, age {}s, half_closed {})",
                        c.id,
                        c.spins_delta,
                        f.spin_secs,
                        crate::recent::clip(&c.target, TARGET_BYTES),
                        c.age_secs,
                        c.half_closed.unwrap_or("no"),
                    );
                }
                // 本数だけ分かって個票が取れない周期 (起こりえないが黙らない)
                None => s.push_str(" a tunnel is spinning"),
            }
            if f.spinning_total > 1 {
                let _ = write!(s, ", {} tunnels", f.spinning_total);
            }
            s
        }
        // 名前解決のミスの**率** (T15.0 (9))。窓は 1 時間
        Kind::DnsMissRate => format!(
            "{}: dns misses {:.2}/connect over 1h (threshold {:.2}; {} misses, {} connects)",
            head,
            base.dns_miss_per_connect(),
            DNS_MISS_RATE,
            base.dns_misses,
            base.connects(),
        ),
    }
}

/// 解除の説明 (**`cleared: <種類>` で始める**。いまの値も入れる)。
fn cleared_text(kind: Kind, w5: &Summary, base: &Summary, f: &Facts<'_>, mins: u64) -> String {
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
        Kind::Errors => format!("{} ({} errors in 5m)", head, w5.errors.saturating_sub(f.nx)),
        Kind::ActiveHigh => format!("{} (active peaked at {} in 5m)", head, w5.active_max),
        Kind::Rejected => format!("{} (no new rejects for {}m)", head, CLEAR_SECS / 60),
        Kind::CpuThrottled => format!(
            "{} ({:.0}% of {} cpu periods throttled in 5m)",
            head,
            f.cpu.ratio().unwrap_or(0.0) * 100.0,
            f.cpu.periods
        ),
        Kind::TunnelSpin => format!("{} (no tunnel spinning for {}m)", head, CLEAR_SECS / 60),
        Kind::DnsMissRate => format!(
            "{} (dns misses {:.2}/connect over 1h, {} connects)",
            head,
            base.dns_miss_per_connect(),
            base.connects()
        ),
    }
}

/// **規則 6** の説明 (T14.54)。接続元・要求数・最初の宛先の種類・`User-Agent` の順。
///
/// `User-Agent` を最後に置いてあるのは、[`crate::events::MAX_TEXT`] (128 B) で切られる
/// ときに**接続元と数字を残す**ため。
fn new_client_text(c: &NewClient) -> String {
    let mut s = format!("new_client: {} first seen ({} req", c.client, c.requests);
    match c.port {
        // `ports` は出た順なので、先頭が最初の宛先のポート (443 / 80 / それ以外)
        Some(p) => {
            let _ = write!(
                s,
                ", first target port {} ({})",
                p,
                if c.literal { "literal" } else { "name" }
            );
        }
        // `User-Agent` だけ先に読めていて、要求がまだ数えられていない瞬間
        None => s.push_str(", no target yet"),
    }
    match &c.agent {
        Some(a) => {
            let _ = write!(
                s,
                ", agent \"{}\")",
                crate::recent::clip(a, NEW_CLIENT_AGENT_BYTES)
            );
        }
        None => s.push_str(", no agent)"),
    }
    s
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
pub fn reset() {
    *DETECTOR.locked() = None;
    MAX_CONNS.store(0, Ordering::Relaxed);
    THRESHOLD.store(0, Ordering::Relaxed);
}

/// `anomaly` の単体テスト (T22.2 で別のファイルに移した。`anomaly.rs` が 2,000 行に届いていたため)。
#[cfg(test)]
#[path = "anomaly_tests.rs"]
mod tests;
