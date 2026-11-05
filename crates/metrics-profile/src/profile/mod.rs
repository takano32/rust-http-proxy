//! プロファイル (`/profile`。T14.3): **1 要求 (1 本) の時間がどの段階に消えたか**を
//! メモリ上の窓に残す。
//!
//! `/status` は「ホスト別の合計」、`/history` は「時系列の合計」までしか持たないので、
//! デプロイ先で「どこで待っているか」を推定する材料が無かった (Phase 10〜11 の計測は
//! `perf` / `strace` を手元で回して得たもので、Pterodactyl では両方使えない)。
//!
//! # 持ち方
//!
//! - 窓は **5 秒 × 720 (1 時間) と 60 秒 × 1,440 (1 日)**。`proxy_metrics_window::history` と同じ形だが、
//!   **`.rrd` には書かない** (メモリだけ。再起動で消えてよい)。
//! - 1 つの段階は [`crate::window::Window`] (件数・合計 ms・最大・12 段の区間)。
//!   区間は [`crate::window::WINDOW_BOUNDS_MS`] と同じ 12 段。
//! - 段階の値を運ぶのは [`crate::metrics::Detail`] の [`crate::metrics::StageMs`] で、
//!   **書くのは `Metrics::record` が既に取っている鍵の内側**。原子操作は 1 つも増えない。
//!
//! # 熱い経路の費用
//!
//! 足すのは**境目の `Instant::now()` だけ** (vDSO。システムコール 0)。
//! **要求ごとに増えるのは forward で 2 回** (要求行が届いた直後 / オリジンへ送り終えた直後)、
//! **CONNECT で 3 回** (要求行 / `200` を書いた直後 / 最初の中継バイト)、
//! それに**接続ごとに 1 回** (accept)。段階の終わりは、下の層が入口で既に読んでいる時計
//! (`http::handle_http_with_headers` の `started`、`tunnel::open` の `started`、
//! `Ctx::log` の `took`、`report` の `alive`) をそのまま使うので増やしていない。
//! **`--lite` では時計も読まない** ([`on`] が偽なら [`mark`] が `None` を返すだけ。T1.4)。
//!
//! # スレッドの標本 (T14.3 (2))
//!
//! `profile-sample` スレッド 1 本が `PROXY_PROFILE_SAMPLE_MS` (既定 1,000、`0` で止める)
//! ごとに `/proc/self/task/*/stat` (名前 / 状態 / utime / stime) と
//! `/proc/self/task/*/syscall` (いま居るシステムコールの番号) と
//! `/proc/self/task/*/schedstat` (走れるのに走れなかった時間) を読み、
//! **役割 × (CPU、状態の割合、走れずに待った時間)** に束ねる。読めない環境
//! (seccomp / `hidepid` / Linux 以外) では `sampler` が `"partial"` か `"off"` に落ちる。
//!
//! 役割の集計だけでは「どのスレッドが回っているか」が出ないので、**その窓で CPU を
//! 多く使ったスレッド上位 8 本** ([`TopThread`]) も一緒に持つ (T15.0 (5))。
//!
//! **状態の割合は「どこで待っているか」であって「どこで CPU を使っているか」ではない。**
//! `/proc/<tid>/syscall` は、そのスレッドが CPU に乗っている間は中身に関係なく
//! `running` を返す (実測: `splice` で 3.6 GB/s を運んでいるスレッドは `splice` ではなく
//! `running` に出る。カーネル時間が 93% でも同じ)。**CPU の行き先は役割ごとの CPU を、
//! 待ちの行き先は状態の割合を**読むこと。
//!
//! `conn` 役には**仕事を待っているワーカー**も入る (`Workers` の空き置き場で
//! `recv_timeout` = `futex`)。短い仕事を大量にさばく経路 (`--only connect`) では
//! こちらが標本の大半を占めるので、状態を読むときは `futex` を「待機列」と見ること。
//!
//! 窓のメモリは `size_of::<Sample>()` × (720 + 1,440)。**数字はここに書き写さない**
//! (型を足すたびに古くなる。実際の値は単体テスト
//! `a_sample_stays_small_enough_for_the_rings` を `-- --nocapture` で回すと出る)。
//! `--lite` では標本を 1 本も作らないので 0 (環状バッファは空のまま)。

mod procstat;
mod rings;
mod sample;
mod sampler;
mod stages;
mod threads;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::cache::now_epoch;

pub(crate) use procstat::process_cpu_split_us;
pub use procstat::process_cpu_us;
pub use rings::Profile;
pub use sample::Sample;
pub(crate) use sampler::Sampler;
pub use sampler::SamplerState;
pub use stages::Stages;
pub use threads::{NSTATES, ROLES, RoleWindow, TOP_THREADS, Threads, TopThread, state_names};

/// 段階の窓を畳む間隔と本数 (5 秒 × 720 = 1 時間、60 秒 × 1,440 = 1 日)。
pub const RESOLUTIONS: [(u64, usize); 2] = [(5, 720), (60, 1440)];

/// 標本 1 本のバイト (`/status` の `memory.rings` の見積もりが使う。T15.0 (13))。
///
/// **数字をここに書き写さない** (型を足すたびに古くなる)。実際の値は単体テスト
/// `a_sample_stays_small_enough_for_the_rings` を `-- --nocapture` で回すと出る。
pub(crate) const fn sample_bytes() -> usize {
    size_of::<Sample>()
}

/// 2 つの環が**満杯のとき**のバイト (`/status` の `memory.rings.profile`。T15.0 (13))。
///
/// T14.21 の `rings` は「満杯のときの見積もり」で揃えてあるので、ここも同じ数え方。
/// いま埋まっているぶんは [`Profile::used_bytes`]。
pub const fn capacity_bytes() -> usize {
    (RESOLUTIONS[0].1 + RESOLUTIONS[1].1) * sample_bytes()
}

/// 細かい方の刻み。
pub(crate) const TICK: Duration = Duration::from_secs(RESOLUTIONS[0].0);

/// CONNECT 1 本の段階 (`Stages::connect` の順)。
///
/// `queue` accept してからワーカーが動き出すまで / `client_read` 要求行を読んでから
/// `Host` まで読み終えるまで / `dns` 名前解決 / `connect` SYN → 確立 /
/// `first_relay` `200 Connection Established` を書いてから最初の中継バイトまで
/// (= トンネル越しの TLS 握手の往復) / `relay` 中継の合計 / `park` 預けられていた合計。
pub const CONNECT_STAGES: [&str; 7] = [
    "queue",
    "client_read",
    "dns",
    "connect",
    "first_relay",
    "relay",
    "park",
];

/// 転送した 1 要求の段階 (`Stages::forward` の順)。
///
/// `origin` はオリジンを掴むまで (プール命中なら 0、それ以外は `dns` + `connect`)、
/// `send` は要求をオリジンへ送り終えるまで、`ttfb` は応答ヘッダーを読み終えるまで
/// (キャッシュ HIT では 0)、`body` は本文を流し終えるまで。
pub const FORWARD_STAGES: [&str; 6] = ["queue", "client_read", "origin", "send", "ttfb", "body"];

/// 記録するかどうか。`--lite` では偽で、**時計も読まない**。
static ON: AtomicBool = AtomicBool::new(false);

/// 設定されたスレッドの標本の間隔 (ms。`/profile` に出すだけ)。
static SAMPLE_MS: AtomicU64 = AtomicU64::new(0);

/// `PROXY_PROFILE_SAMPLE_MS` の値 (`/profile` の `sample_ms`)。
pub fn sample_ms() -> u64 {
    SAMPLE_MS.load(Ordering::Relaxed)
}

/// 記録するかどうかを決める (起動時に 1 回だけ呼ぶ)。
pub fn set_enabled(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

/// 記録するか (熱い経路で読むのはこの `Relaxed` の 1 回だけ)。
#[inline]
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// 段階の境目で時計を読む。`--lite` では読まない (旗 1 つの分岐だけ)。
#[inline]
pub fn mark() -> Option<Instant> {
    on().then(Instant::now)
}

/// `Duration` を ms の `u32` に落とす (49 日で頭打ち。段階の長さには十分)。
#[inline]
pub fn ms_u32(d: Duration) -> u32 {
    d.as_millis().min(u32::MAX as u128) as u32
}

/// [`spawn`] のスレッドが**上の層 (`Metrics`) から読むもの**。これだけ (T14.55)。
///
/// `Metrics` は 1 つ上のクレート (`proxy-metrics-core`) に居て、そこから下のここを
/// 直に呼ぶことはできても逆はできない。**下から上を呼べない**ので、スレッドが要る
/// 3 つの読み口だけをこの型で受け取る (実装は `Metrics` 側に 1 つだけある)。
pub trait Source: Send + Sync + 'static {
    /// 起動からの要求数 (`Metrics::total_requests`)。
    fn total_requests(&self) -> u64;
    /// 直近の標本以降の段階を読んで 0 に戻す (`Metrics::take_stages`)。
    fn take_stages(&self) -> Stages;
    /// 段階とスレッドの窓 (`Metrics::profile`)。
    fn profile(&self) -> &Profile;
}

/// 1 標本ぶんの「前回との差」を取るための覚え書き。
struct Tick {
    requests: u64,
    cpu_us: u64,
    /// そのうちユーザー空間 (T16.0。同じ `/proc/self/stat` の 1 回の読みから取る)
    cpu_user_us: u64,
    locks: [u64; crate::sync::LOCK_NAMES.len()],
    queue_waited: u64,
    queue_ms_sum: u64,
}

impl Tick {
    fn now<M: Source>(metrics: &M) -> Tick {
        let [queue_waited, queue_ms_sum, _] = crate::sync::queue_totals();
        // 合計とユーザー空間は**同じ 1 回の読み**から取る (読む回数を増やさない。T16.0)
        let (cpu_us, cpu_user_us) = process_cpu_split_us().unwrap_or((0, 0));
        Tick {
            requests: metrics.total_requests(),
            cpu_us,
            cpu_user_us,
            locks: crate::sync::lock_contended(),
            queue_waited,
            queue_ms_sum,
        }
    }
}

/// スレッドの標本の既定の間隔 (ms)。`PROXY_PROFILE_SAMPLE_MS` で変える (`0` で止める)。
pub const DEFAULT_SAMPLE_MS: u64 = 1000;

/// 標本の間隔の下限と上限 (ms)。下限を置くのは自分の CPU を使い切らないため。
pub(crate) const MIN_SAMPLE_MS: u64 = 50;
pub(crate) const MAX_SAMPLE_MS: u64 = 60_000;

/// 段階の窓を畳み、スレッドの標本を取るスレッドを起こす (`profile-sample`)。
///
/// **`--lite` では呼ばない** (窓を 1 本も作らない)。`sample_ms` が `0` なら
/// スレッドの標本は取らず (`sampler: "off"`)、5 秒ごとに段階の窓だけ畳む。
pub fn spawn<M: Source>(metrics: std::sync::Arc<M>, sample_ms: u64) -> JoinHandle<()> {
    SAMPLE_MS.store(sample_ms, Ordering::Relaxed);
    thread::Builder::new()
        .name("profile-sample".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut prev = Tick::now(&*metrics);
            let mut sampler = (sample_ms > 0).then(Sampler::for_self);
            let step = match sample_ms {
                0 => TICK,
                ms => Duration::from_millis(ms.clamp(MIN_SAMPLE_MS, MAX_SAMPLE_MS)),
            };
            let mut waited = Duration::ZERO;
            loop {
                thread::sleep(step);
                if let Some(s) = sampler.as_mut() {
                    s.sample_unless_idle(metrics.profile(), metrics.total_requests());
                }
                waited += step;
                if waited >= TICK {
                    waited = Duration::ZERO;
                    tick(&*metrics, &mut prev, sampler.as_mut());
                }
            }
        })
        .expect("spawn profile-sample thread")
}

/// 1 標本ぶんを窓へ (`spawn` のループの中身。テストからも呼ぶ)。
fn tick<M: Source>(metrics: &M, prev: &mut Tick, sampler: Option<&mut Sampler>) {
    let now = Tick::now(metrics);
    let mut locks = [0u64; crate::sync::LOCK_NAMES.len()];
    for ((o, a), b) in locks
        .iter_mut()
        .zip(now.locks.iter())
        .zip(prev.locks.iter())
    {
        *o = a.saturating_sub(*b);
    }
    let (threads, threads_top, run_delay_us) = sampler.map(Sampler::take).unwrap_or_default();
    let s = Sample {
        t: now_epoch(),
        requests: now.requests.saturating_sub(prev.requests),
        cpu_us: now.cpu_us.saturating_sub(prev.cpu_us),
        stages: metrics.take_stages(),
        threads,
        locks,
        queue_waited: now.queue_waited.saturating_sub(prev.queue_waited),
        queue_ms_sum: now.queue_ms_sum.saturating_sub(prev.queue_ms_sum),
        queue_ms_max: crate::sync::take_queue_window_max(),
        threads_top,
        run_delay_us,
        cpu_user_us: now.cpu_user_us.saturating_sub(prev.cpu_user_us),
    };
    *prev = now;
    metrics.profile().push(s);
}
