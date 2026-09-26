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
//! | `dns_slow` | 名前解決のミス 1 回の平均 (直近 5 分) が [`DNS_MISS_MS`] と canary の基準線の [`DNS_CANARY_RATIO`] 倍の大きい方以上、かつ [`DNS_MIN_MISSES`] 回以上 (T17.1) |
//! | `errors` | エラーが 5 分で [`ERRORS_MIN`] 件以上 |
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
            w5.dns_misses >= DNS_MIN_MISSES && w5.dns_miss_avg_ms() >= dns_limit,
            w5.errors >= ERRORS_MIN,
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
        Kind::Errors => format!("{} ({} errors in 5m)", head, w5.errors),
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
        feed_conns(h, d, None, s, c)
    }

    /// 接続の表も渡して 1 本流す (`tunnel_spin` の判定に要る。T15.0 (6))。
    fn feed_conns(
        h: &History,
        d: &mut Detector,
        conns: Option<&ConnTable>,
        s: Sample,
        c: Counters,
    ) -> Vec<Fired> {
        h.push(s);
        d.observe(
            h,
            conns,
            Counters {
                t: s.t,
                evicted_idle: s.evicted_idle,
                ..c
            },
        )
    }

    /// **起動からの秒**を渡せる版 (T15.0 (9))。[`feed`] は [`Counters::default`] を
    /// 渡すので `uptime_secs` が 0 のまま = 暖機中扱いになる。
    fn feed_uptime(h: &History, d: &mut Detector, s: Sample, uptime: u64) -> Vec<Fired> {
        feed_with(
            h,
            d,
            s,
            Counters {
                uptime_secs: uptime,
                ..Counters::default()
            },
        )
    }

    /// cgroup の CPU の**累計**を渡して 1 本流す (T15.0 (6))。
    fn feed_cpu(
        h: &History,
        d: &mut Detector,
        s: Sample,
        throttled: u64,
        periods: u64,
        used_us: u64,
    ) -> Vec<Fired> {
        feed_with(
            h,
            d,
            s,
            Counters {
                cpu_nr_throttled: throttled,
                cpu_nr_periods: periods,
                cpu_quota_cores: 1.0,
                cpu_used_us: used_us,
                ..Counters::default()
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
        assert_eq!(
            fired[0].text,
            // 基準値の窓 (1 時間) に入る本数は、5 秒のリングが 6 時間になった T14.32 以降 1,442
            "connect_p95: connect p95 136 ms over 5m is 13.9x the 1h baseline 9.8 ms (122 of 1442 connects)"
        );
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

    /// 平常時の 1 時間のあと、`misses` 回のミス (1 回 `each_ms`) を 1 分おきに流し、
    /// canary の基準線 `canary` を付けて、出た変わり目を全部返す (T17.1)。
    fn dns_misses_once_a_minute(misses: u64, each_ms: u64, canary: Option<u64>) -> Vec<Fired> {
        let h = History::default();
        let mut d = Detector::new();
        let (mut t, quiet) = calm(&h, &mut d, 0, BASE_SECS);
        assert!(quiet.is_empty(), "{:?}", quiet);
        let mut out = Vec::new();
        // 4 分 (48 標本) のうち、1 分ごとの頭の標本にだけミスを 1 回
        for i in 0..48u64 {
            let mut p = connects(t, 2, 10);
            if i % 12 == 0 && i / 12 < misses {
                p.dns_misses = 1;
                p.dns_ms_sum = each_ms;
            }
            let c = Counters {
                t,
                canary_dns_p50_ms: canary,
                ..Counters::default()
            };
            out.extend(feed_with(&h, &mut d, p, c));
            t += 5;
        }
        out
    }

    /// T17.1: 5 分に 1 回だけの重いミス (400 ms) では `dns_slow` を立てない。
    ///
    /// T16.99 の 29 件はほぼ全部これ (5 分に 1 回、240〜470 ms)。
    #[test]
    fn one_slow_dns_miss_alone_does_not_fire() {
        let out = dns_misses_once_a_minute(1, 400, None);
        assert!(out.is_empty(), "1 回では立たない: {:?}", out);
        assert_eq!(DNS_MIN_MISSES, 3);
    }

    /// T17.1: 5 分に 3 回の 400 ms は立つ (canary が無ければ閾は 100 ms)。
    #[test]
    fn three_slow_dns_misses_fire() {
        let out = dns_misses_once_a_minute(3, 400, None);
        assert_eq!(out.len(), 1, "{:?}", out);
        assert_eq!(out[0].kind, Kind::DnsSlow);
        assert_eq!(
            out[0].text,
            "dns_slow: dns miss 400 ms avg over 5m (threshold 100 ms; 3 misses, 1200 ms total)"
        );
        // 2 回目までは立たない (3 回目の標本で立つ)
        assert_eq!(dns_misses_once_a_minute(2, 400, None), Vec::new());
    }

    /// T17.1: canary の名前解決が 200 ms の網では閾は 3 倍の 600 ms。500 ms は立たない。
    #[test]
    fn a_slow_canary_raises_the_dns_threshold() {
        let out = dns_misses_once_a_minute(3, 500, Some(200));
        assert!(
            out.is_empty(),
            "canary 200 ms なら 500 ms は平常: {:?}",
            out
        );
        // 同じ網で 3 倍を越えれば立ち、説明の閾は基準線から引いた値になる
        let out = dns_misses_once_a_minute(3, 700, Some(200));
        assert_eq!(out.len(), 1, "{:?}", out);
        assert!(
            out[0].text.contains("(threshold 600 ms; 3 misses"),
            "{}",
            out[0].text
        );
        // 速い canary (9 ms) では 100 ms の下限が残る (27 ms には下げない)
        assert_eq!(dns_miss_limit(Some(9)), DNS_MISS_MS);
        assert_eq!(dns_miss_limit(None), DNS_MISS_MS);
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
        // 窓は両端を含むので 5 分 = 61 標本、1 時間 = 721 標本。
        // **T14.32 で 5 秒のリングが 1 時間 (720 本) から 6 時間 (4,320 本) になった**ので、
        // 基準値の窓はリングに切られず 721 本そろう (それ以前は 720 本で頭打ちだった)
        assert_eq!(w5.samples, WINDOW_SECS / 5 + 1);
        assert_eq!(w5.connect.count, 2 * (WINDOW_SECS / 5 + 1));
        assert_eq!(base.samples, BASE_SECS / 5 + 1, "リングが覆うのは 6 時間");
        assert_eq!(base.connect.count, 2 * (BASE_SECS / 5 + 1));
        assert_eq!(base.interval_secs, 5);
    }

    #[test]
    fn the_eight_kinds_have_distinct_names() {
        let mut names: Vec<&str> = KINDS.iter().map(|k| k.name()).collect();
        assert_eq!(names.len(), 8);
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 8, "名前が重なっている");
        // **添字の定数と `KINDS` の並びが合っていること** (ずれても型では落ちない)
        for (i, kind) in [
            (I_CONNECT_P95, Kind::ConnectP95),
            (I_DNS_SLOW, Kind::DnsSlow),
            (I_ERRORS, Kind::Errors),
            (I_ACTIVE_HIGH, Kind::ActiveHigh),
            (I_REJECTED, Kind::Rejected),
            (I_CPU_THROTTLED, Kind::CpuThrottled),
            (I_TUNNEL_SPIN, Kind::TunnelSpin),
            (I_DNS_MISS_RATE, Kind::DnsMissRate),
        ] {
            assert_eq!(KINDS[i], kind, "添字 {} がずれている", i);
        }
        // 既存 5 種の添字は動いていない (T14.9 以来の並び)
        assert_eq!(I_REJECTED, 4);
    }

    // ------------------------------------------------- T15.0 (6)(9) で足した 3 種

    /// (7) CPU の絞りは**割合**で立つ (50% 以上) — 5 分ぶん溜まるまでは判定しない。
    ///
    /// 累計そのものは親の階層の値なので、見るのは 5 分の窓の増分どうしの比。
    #[test]
    fn cpu_throttling_fires_over_half_and_clears_under_a_quarter() {
        let h = History::default();
        let mut d = Detector::new();
        // 起動より前から動いている累計 (0 時間の雪像にも 915 が乗っていた)
        let (mut throttled, mut periods, mut used) = (915u64, 8_000u64, 0u64);
        let mut t = 0u64;
        let mut fired = Vec::new();
        while t <= WINDOW_SECS {
            fired.extend(feed_cpu(&h, &mut d, point(t), throttled, periods, used));
            if t < WINDOW_SECS {
                assert!(
                    fired.is_empty(),
                    "5 分に満たない窓では判定しない: {:?}",
                    fired
                );
            }
            t += 5;
            periods += 50; // 5 秒 = 100 ms の期間 50 個
            throttled += 50; // そのうち全部絞られた (2026-09-18 のデプロイ先は約 100%)
            used += 4_000_000; // 4 秒ぶん = 0.8 コア
        }
        assert_eq!(fired.len(), 1, "立つのは 1 回だけ: {:?}", fired);
        assert_eq!(fired[0].kind, Kind::CpuThrottled);
        assert_eq!(
            fired[0].text,
            "cpu_throttled: 100% of 3000 cpu periods throttled in 5m \
             (threshold 50%; quota 1.00 cores, used 0.80)"
        );
        assert!(fired[0].text.len() <= crate::events::MAX_TEXT);
        // 絞られなくなったら、窓から抜けて 25% を下回り、そこから 5 分で解除
        let mut back = Vec::new();
        while t <= 1_500 {
            back.extend(feed_cpu(&h, &mut d, point(t), throttled, periods, used));
            t += 5;
            periods += 50; // 期間は過ぎ続けるが絞られない
            used += 1_000_000;
        }
        assert_eq!(back.len(), 1, "解除も 1 回だけ: {:?}", back);
        assert!(back[0].cleared);
        assert_eq!(
            back[0].text,
            "cleared: cpu_throttled after 9m (0% of 3000 cpu periods throttled in 5m)"
        );
        assert!(d.firing().is_empty());
    }

    /// (7) **刻みが 1 秒ずれても立つ** (T15.0 単位 4 のレビュー)。
    ///
    /// 履歴スレッドの 1 周は `sleep(5s)` の**あと**に 1 周ぶんの仕事をするので、
    /// 5 秒ちょうどにはならず 6 秒の間隔が周期的に混ざる。「5 分ぶん溜まったか」を
    /// **窓の両端の差**で見ていたころは、この系列では門が 1 度も開かなかった
    /// (差は 296〜299 秒にしかならず、300 に一致する刻みが来ない)。
    #[test]
    fn cpu_throttling_fires_even_when_the_ticks_drift() {
        let h = History::default();
        let mut d = Detector::new();
        let (mut throttled, mut periods, mut used) = (915u64, 8_000u64, 0u64);
        let (mut t, mut step) = (0u64, 0usize);
        let mut fired = Vec::new();
        while t <= 2 * WINDOW_SECS {
            fired.extend(feed_cpu(&h, &mut d, point(t), throttled, periods, used));
            if t < WINDOW_SECS {
                assert!(
                    fired.is_empty(),
                    "5 分に満たない間は判定しない: {:?}",
                    fired
                );
            }
            // 5, 5, 6, 5, 5, 6, … (1 周が 5 秒より長い = 本番の周期)
            let gap = if step % 3 == 2 { 6 } else { 5 };
            step += 1;
            t += gap;
            periods += 10 * gap; // 100 ms の期間が 1 秒に 10 個
            throttled += 10 * gap; // そのうち全部絞られた
            used += 800_000 * gap; // 0.8 コア
        }
        assert_eq!(fired.len(), 1, "立つのは 1 回だけ: {:?}", fired);
        assert_eq!(fired[0].kind, Kind::CpuThrottled);
        assert!(d.firing().contains(&Kind::CpuThrottled), "{:?}", d.firing());
    }

    /// (7) `cpu.stat` が読めない環境 (cgroup v1・Linux 以外) では 1 件も立たない。
    #[test]
    fn a_machine_without_a_cpu_quota_never_fires_cpu_throttled() {
        let h = History::default();
        let mut d = Detector::new();
        let (_, out) = calm(&h, &mut d, 0, 2 * BASE_SECS);
        assert!(out.is_empty(), "{:?}", out);
        assert!(
            !d.firing().contains(&Kind::CpuThrottled),
            "{:?}",
            d.firing()
        );
    }

    /// (8) 回りっ放しのトンネルは **1 分続いてから** 1 件だけ立つ。
    #[test]
    fn a_spinning_tunnel_fires_once_after_a_minute() {
        let h = History::default();
        let mut d = Detector::new();
        let conns = ConnTable::new();
        let slot = conns
            .register(77, "198.51.100.5", std::time::Instant::now())
            .expect("枠ができる");
        slot.begin_tunnel("origin.example:443");
        slot.set_half_closed(crate::recent::CLIENT_SIDE);
        conns.update_rates(0); // 1 回目は控えるだけ (起動直後の周期)
        // 「起こされたのに 1 バイトも進まなかった」が 1 周期で 45,000 回
        slot.set_relaying(0, 45_000, 0);
        conns.update_rates(5_000); // 2 回目で差分が出る
        assert_eq!(slot.spins_delta(), 45_000);

        let mut t = 0u64;
        let mut fired = Vec::new();
        while t <= TUNNEL_SPIN_SECS {
            fired.extend(feed_conns(
                &h,
                &mut d,
                Some(&conns),
                point(t),
                Counters::default(),
            ));
            if t < TUNNEL_SPIN_SECS {
                assert!(
                    fired.is_empty(),
                    "1 分に満たないうちは立たない: {:?}",
                    fired
                );
            }
            t += 5;
        }
        assert_eq!(fired.len(), 1, "立つのは 1 回だけ: {:?}", fired);
        assert_eq!(fired[0].kind, Kind::TunnelSpin);
        assert_eq!(
            fired[0].text,
            "tunnel_spin: conn#77 spun 45000 times/tick for 60s \
             (origin.example:443, age 0s, half_closed client)"
        );
        assert!(fired[0].text.len() <= crate::events::MAX_TEXT);
        // 回らなくなったら 5 分で解除 (差分は 0 に戻る)
        slot.set_relaying(1_000, 45_000, 0);
        conns.update_rates(5_000);
        assert_eq!(slot.spins_delta(), 0);
        let mut back = Vec::new();
        while t <= TUNNEL_SPIN_SECS + CLEAR_SECS + 10 {
            back.extend(feed_conns(
                &h,
                &mut d,
                Some(&conns),
                point(t),
                Counters::default(),
            ));
            t += 5;
        }
        assert_eq!(back.len(), 1, "{:?}", back);
        assert_eq!(
            back[0].text,
            "cleared: tunnel_spin after 6m (no tunnel spinning for 5m)"
        );
    }

    /// (8) 表を渡さなければ (テストと `--lite`) `tunnel_spin` は判定しない。
    #[test]
    fn without_a_conn_table_the_spin_rule_is_quiet() {
        let h = History::default();
        let mut d = Detector::new();
        let (_, out) = calm(&h, &mut d, 0, 600);
        assert!(out.is_empty(), "{:?}", out);
    }

    /// (9) ミスの**率**が 0.40/接続 以上で立つ。ただし**起動から 6 時間**は立てない。
    ///
    /// 再起動の直後はどの名前も warm でないので、率は構造的に高い (実測 0.291)。
    #[test]
    fn a_high_dns_miss_rate_fires_after_the_warmup() {
        let h = History::default();
        let mut d = Detector::new();
        // 1 標本 20 本の確立に 11 回のミス = 0.55/接続 (Phase 13 と同じ高さ)。
        // ミス 1 回は 10 ms なので `dns_slow` (100 ms) は立たない
        let sample = |t: u64| {
            let mut p = connects(t, 20, 10);
            p.dns_misses = 11;
            p.dns_ms_sum = 11 * 10;
            p
        };
        // 起動から 3 時間: 率は 0.55 でも立たない (暖機中)
        let mut t = 0u64;
        let mut warmup = Vec::new();
        while t <= BASE_SECS {
            warmup.extend(feed_uptime(&h, &mut d, sample(t), 3 * 3600));
            t += 5;
        }
        assert!(warmup.is_empty(), "暖機中は書かない: {:?}", warmup);
        // 6 時間を越えると同じ率で立つ
        let fired = feed_uptime(&h, &mut d, sample(t), DNS_MISS_RATE_WARMUP_SECS);
        assert_eq!(fired.len(), 1, "{:?}", fired);
        assert_eq!(fired[0].kind, Kind::DnsMissRate);
        assert_eq!(
            fired[0].text,
            "dns_miss_rate: dns misses 0.55/connect over 1h \
             (threshold 0.20; 7931 misses, 14420 connects)"
        );
        assert!(fired[0].text.len() <= crate::events::MAX_TEXT);
        t += 5;
        // 同じ種類は収まるまで 1 回だけ
        assert!(feed_uptime(&h, &mut d, sample(t), DNS_MISS_RATE_WARMUP_SECS).is_empty());
    }

    /// (9) いまの切り方 (0.094〜0.171) では立たない。
    #[test]
    fn a_normal_dns_miss_rate_never_fires() {
        let h = History::default();
        let mut d = Detector::new();
        // 1 標本 20 本の確立に 3 回のミス = 0.15/接続 (2026-09-18 の実測の上の方)
        let mut t = 0u64;
        let mut out = Vec::new();
        while t <= BASE_SECS + 600 {
            let mut p = connects(t, 20, 10);
            p.dns_misses = 3;
            p.dns_ms_sum = 3 * 10;
            out.extend(feed_uptime(&h, &mut d, p, 24 * 3600));
            t += 5;
        }
        assert!(out.is_empty(), "0.15/接続 では書かない: {:?}", out);
    }

    /// (9) 確立が 20 本に満たない 5 分の窓では `connect_p95` を立てない。
    ///
    /// p95 は 250 ms 超が 1〜2 本しかない窓では**窓の最大値に潰れる**
    /// (仕様なので触らない)。257 / 265 / 275 ms で立った件はどれもこれだった。
    #[test]
    fn connect_p95_needs_twenty_samples_in_the_window() {
        let h = History::default();
        let mut d = Detector::new();
        // 基準値 (1 時間で 10 ms) を作る
        let (mut t, quiet) = calm(&h, &mut d, 0, BASE_SECS);
        assert!(quiet.is_empty(), "{:?}", quiet);
        // 5 分ぶん静かにして、直近の窓から確立を抜く
        while t < BASE_SECS + WINDOW_SECS + 5 {
            assert!(feed(&h, &mut d, point(t)).is_empty());
            t += 5;
        }
        // 19 本の 250 ms では立たない (窓の最大値に潰れた p95 で立てない)
        let mut p = connects(t, 19, 250);
        p.t = t;
        assert!(feed(&h, &mut d, p).is_empty(), "19 本では立ってはいけない");
        t += 5;
        // 20 本目で立つ
        let fired = feed(&h, &mut d, connects(t, 1, 250));
        assert_eq!(fired.len(), 1, "{:?}", fired);
        assert_eq!(fired[0].kind, Kind::ConnectP95);
        assert_eq!(CONNECT_MIN_SAMPLES, 20);
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

    // ------------------------------------------------------------ 規則 6 (T14.54)

    /// 接続元 1 件を 1 要求ぶん記録する (本番と同じ口だけを使う)。
    fn visit(m: &Metrics, client: &str, agent: Option<&str>, target: &str) {
        if let Some(a) = agent {
            // `User-Agent` を読むのは接続の最初の要求だけ (T14.7)
            m.record_client_agent(client, a);
        }
        m.record_client(
            client,
            crate::metrics::HostOutcome::Bypass,
            0,
            (0, 0),
            None,
            Some(target),
        );
    }

    /// いま入っている接続元の `first_seen` (**本物の壁時計**)。
    ///
    /// 窓はこの値に合わせて置く: 標本の時刻は注入できても `first_seen` は
    /// [`crate::cache::now_epoch`] なので、勝手な時刻で窓を切ると秒の境目で落ちる。
    fn first_seen_of(m: &Metrics, client: &str) -> u64 {
        m.clients_first_seen_in(0, u64::MAX, 100)
            .0
            .iter()
            .find(|c| c.client == client)
            .unwrap_or_else(|| panic!("{} が居ない", client))
            .first_seen
    }

    /// (6) 初めて見た接続元が 1 回だけ出る (2 回目の要求では増えない)。
    #[test]
    fn f_a_new_client_is_written_once_per_client() {
        let m = Metrics::new();
        let mut d = Detector::new();
        visit(
            &m,
            "198.51.100.7",
            Some("t1454/1.0"),
            "connect://a.example:443",
        );
        let ta = first_seen_of(&m, "198.51.100.7");
        assert!(
            d.observe_clients(&m, ta - 1).is_empty(),
            "1 本目は基準にするだけ"
        );
        assert_eq!(
            d.observe_clients(&m, ta),
            vec![
                "new_client: 198.51.100.7 first seen \
                 (1 req, first target port 443 (name), agent \"t1454/1.0\")"
                    .to_string()
            ]
        );
        // 2 回目の要求では増えない。窓 (`[ta, tb]`) はこの接続元の `first_seen` を
        // また含むので、「もう書いた」を覚えていないと 2 件目が出る
        visit(
            &m,
            "198.51.100.7",
            Some("t1454/1.0"),
            "connect://b.example:443",
        );
        // 別の接続元は出る (IP リテラル宛て・`User-Agent` 無し)
        visit(&m, "198.51.100.8", None, "connect://203.0.113.9:8443");
        let tb = first_seen_of(&m, "198.51.100.8");
        assert_eq!(
            d.observe_clients(&m, tb),
            vec![
                "new_client: 198.51.100.8 first seen \
                 (1 req, first target port 8443 (literal), no agent)"
                    .to_string()
            ]
        );
    }

    /// (6) 読み戻した接続元 (`first_seen == 0`) は「新しい」ではない。
    #[test]
    fn f_restored_clients_are_not_new() {
        let m = Metrics::new();
        let mut d = Detector::new();
        let t0 = crate::cache::now_epoch();
        m.restore(
            Vec::new(),
            vec![(
                "198.51.100.1".to_string(),
                crate::metrics::HostStats {
                    requests: 42,
                    ..Default::default()
                },
            )],
        );
        assert!(d.observe_clients(&m, t0 - 1).is_empty());
        assert!(
            d.observe_clients(&m, t0 + 300).is_empty(),
            "前から居た接続元は新しくない"
        );
    }

    /// (6) 1 周期に何百も現れたら (走査) 上限で切って、残りは 1 行にまとめる。
    #[test]
    fn f_a_scan_is_folded_into_one_line() {
        let m = Metrics::new();
        let mut d = Detector::new();
        for i in 1..=(NEW_CLIENT_MAX + 3) {
            visit(
                &m,
                &format!("198.51.100.{}", i),
                None,
                "connect://a.example:443",
            );
        }
        let seen: Vec<u64> = m
            .clients_first_seen_in(0, u64::MAX, 100)
            .0
            .iter()
            .map(|c| c.first_seen)
            .collect();
        let (lo, hi) = (*seen.iter().min().unwrap(), *seen.iter().max().unwrap());
        assert!(d.observe_clients(&m, lo - 1).is_empty());
        let out = d.observe_clients(&m, hi);
        assert_eq!(out.len(), NEW_CLIENT_MAX + 1, "{:?}", out);
        assert!(
            out[NEW_CLIENT_MAX].starts_with("new_client: 3 more clients first seen in the same "),
            "{}",
            out[NEW_CLIENT_MAX]
        );
        assert!(
            out[NEW_CLIENT_MAX].ends_with(&format!("window (wrote {})", NEW_CLIENT_MAX)),
            "{}",
            out[NEW_CLIENT_MAX]
        );
        // まとめた分も「もう書いた」に入るので、次の周期では出てこない
        assert!(d.observe_clients(&m, hi).is_empty());
    }

    /// 履歴スレッドの口 ([`check`]) が `/events` に `new_client` で 1 件書き、
    /// 長い `User-Agent` でも 1 件 128 B に収まる (接続元と数字は残る)。
    #[test]
    fn check_writes_one_new_client_event() {
        let _g = crate::events::TEST_LOCK.locked();
        crate::events::clear();
        reset();
        let metrics = Metrics::new();
        let mut sample = Sample {
            t: crate::cache::now_epoch(),
            ..Sample::default()
        };
        check(&metrics, &sample);
        assert!(crate::events::is_empty(), "1 本目では書かない");
        visit(
            &metrics,
            "198.51.100.9",
            Some(&"u".repeat(200)),
            "connect://a.example:443",
        );
        sample.t = first_seen_of(&metrics, "198.51.100.9");
        check(&metrics, &sample);
        let (events, _) = crate::events::select(0, 10);
        assert_eq!(events.len(), 1, "{:?}", events);
        assert_eq!(events[0].kind, crate::events::EventKind::NewClient);
        assert!(
            events[0]
                .text
                .starts_with("new_client: 198.51.100.9 first seen (1 req, first target port 443"),
            "{}",
            events[0].text
        );
        assert!(
            events[0].text.len() <= crate::events::MAX_TEXT,
            "{} B",
            events[0].text.len()
        );
        // 同じ接続元は 1 回だけ (同じ秒の窓をもう一度見ても増えない)
        check(&metrics, &sample);
        assert_eq!(crate::events::len(), 1);
        crate::events::clear();
        reset();
    }
}
