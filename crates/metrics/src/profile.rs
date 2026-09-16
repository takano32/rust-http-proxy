//! プロファイル (`/profile`。T14.3): **1 要求 (1 本) の時間がどの段階に消えたか**を
//! メモリ上の窓に残す。
//!
//! `/status` は「ホスト別の合計」、`/history` は「時系列の合計」までしか持たないので、
//! デプロイ先で「どこで待っているか」を推定する材料が無かった (Phase 10〜11 の計測は
//! `perf` / `strace` を手元で回して得たもので、Pterodactyl では両方使えない)。
//!
//! # 持ち方
//!
//! - 窓は **5 秒 × 720 (1 時間) と 60 秒 × 1,440 (1 日)**。[`crate::history`] と同じ形だが、
//!   **`.rrd` には書かない** (メモリだけ。再起動で消えてよい)。
//! - 1 つの段階は [`crate::history::Window`] (件数・合計 ms・最大・12 段の区間)。
//!   区間は [`crate::history::WINDOW_BOUNDS_MS`] と同じ 12 段。
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
//! `/proc/self/task/*/syscall` (いま居るシステムコールの番号) を読み、
//! **役割 × (CPU、状態の割合)** に束ねる。読めない環境 (seccomp / `hidepid` / Linux 以外)
//! では `sampler` が `"partial"` か `"off"` に落ちる。
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
//! 窓のメモリは 1 標本 2,340 B × (720 + 1,440) ≈ **5.1 MB**。`--lite` では標本を 1 本も
//! 作らないので 0 (環状バッファは空のまま)。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::cache::now_epoch;
use crate::history::Window;
use crate::metrics::{Detail, Metrics};
use crate::sync::LockExt;

/// 段階の窓を畳む間隔と本数 (5 秒 × 720 = 1 時間、60 秒 × 1,440 = 1 日)。
pub const RESOLUTIONS: [(u64, usize); 2] = [(5, 720), (60, 1440)];

/// 細かい方の刻み。
pub const TICK: Duration = Duration::from_secs(RESOLUTIONS[0].0);

/// CONNECT 1 本の段階 ([`Stages::connect`] の順)。
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

/// 転送した 1 要求の段階 ([`Stages::forward`] の順)。
///
/// `origin` はオリジンを掴むまで (プール命中なら 0、それ以外は `dns` + `connect`)、
/// `send` は要求をオリジンへ送り終えるまで、`ttfb` は応答ヘッダーを読み終えるまで
/// (キャッシュ HIT では 0)、`body` は本文を流し終えるまで。
pub const FORWARD_STAGES: [&str; 6] = ["queue", "client_read", "origin", "send", "ttfb", "body"];

/// スレッドの役割 ([`Threads`] の順。T14.3 (2))。
///
/// `accept` は主スレッド (tid == pid)、残りはスレッド名 (`/proc/<pid>/task/<tid>/stat` の
/// 2 番目の項目) で決める。表に無い名前 (`env-reload` / `blocklist` / `shutdown` …) は `other`。
pub const ROLES: [&str; 9] = [
    "accept",
    "conn",
    "idle-watch",
    "dns-refresh",
    "history",
    "persist",
    "cache-probe",
    "profile-sample",
    "other",
];

/// 番号が分かるシステムコールの名前 ([`SYSCALL_NRS`] と同じ順)。
pub const SYSCALL_NAMES: [&str; 19] = [
    "recvfrom",
    "sendto",
    "ppoll",
    "epoll_pwait",
    "futex",
    "splice",
    "accept4",
    "connect",
    "close",
    "read",
    "write",
    "nanosleep",
    "clock_nanosleep",
    "getsockopt",
    "setsockopt",
    "socket",
    "shutdown",
    "openat",
    "fstat",
];

/// aarch64 (asm-generic) のシステムコール番号 ([`SYSCALL_NAMES`] の順)。
#[cfg(target_arch = "aarch64")]
const SYSCALL_NRS: [i64; SYSCALL_NAMES.len()] = [
    207, 206, 73, 22, 98, 76, 242, 203, 57, 63, 64, 101, 115, 209, 208, 198, 210, 56, 80,
];

/// x86_64 のシステムコール番号 ([`SYSCALL_NAMES`] の順)。
#[cfg(target_arch = "x86_64")]
const SYSCALL_NRS: [i64; SYSCALL_NAMES.len()] = [
    45, 44, 271, 281, 202, 275, 288, 42, 3, 0, 1, 35, 230, 55, 54, 41, 48, 257, 5,
];

/// 表を持っていない機械では名前を出さない (全部 `other` に落ちる)。
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
const SYSCALL_NRS: [i64; SYSCALL_NAMES.len()] = [-1; SYSCALL_NAMES.len()];

/// 走行中の枠 ([`STATES`] の先頭)。
const STATE_RUNNING: usize = 0;
/// 休眠の枠 (`syscall` が読めなかったときの受け皿)。
const STATE_SLEEPING: usize = 1 + SYSCALL_NAMES.len();
/// 表に無いシステムコール・分からない状態の枠。
const STATE_OTHER: usize = STATE_SLEEPING + 1;
/// 状態の数。
pub const NSTATES: usize = STATE_OTHER + 1;

/// スレッドの状態の名前 (走行中 / システムコール名 / 休眠 / その他)。
pub fn state_names() -> [&'static str; NSTATES] {
    let mut out = ["other"; NSTATES];
    out[STATE_RUNNING] = "running";
    for (i, n) in SYSCALL_NAMES.iter().enumerate() {
        out[1 + i] = n;
    }
    out[STATE_SLEEPING] = "sleeping";
    out
}

/// スレッド名から役割を決める (主スレッドは tid で見分ける)。
pub fn role_of(tid: u32, main_tid: u32, comm: &str) -> usize {
    if tid == main_tid {
        return 0;
    }
    ROLES
        .iter()
        .position(|r| *r == comm)
        .filter(|i| *i > 0)
        .unwrap_or(ROLES.len() - 1)
}

/// 標本 1 つを [`STATES`] の枠に落とす。表に無い番号は (`other`, その番号) を返す。
pub fn state_slot(syscall: Option<i64>, state: char) -> (usize, Option<i64>) {
    match syscall {
        Some(nr) if nr >= 0 => match SYSCALL_NRS.iter().position(|n| *n == nr) {
            Some(i) => (1 + i, None),
            None => (STATE_OTHER, Some(nr)),
        },
        // `running` / `-1` = システムコールの中に居ない
        Some(_) => (STATE_RUNNING, None),
        // `syscall` が読めない環境は `stat` の状態だけ (`sampler: "partial"`)
        None => match state {
            'R' => (STATE_RUNNING, None),
            'S' | 'D' | 'I' => (STATE_SLEEPING, None),
            _ => (STATE_OTHER, None),
        },
    }
}

/// 1 つの役割の窓 (CPU と状態の内訳)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoleWindow {
    /// その窓にこの役割のスレッドが使った CPU (us)
    pub cpu_us: u64,
    /// 標本の数 (スレッド数 × 標本回数)。割合はこれで割る
    pub samples: u64,
    /// 状態の内訳 ([`state_names`] の順)
    pub states: [u32; NSTATES],
}

impl RoleWindow {
    fn merge(&mut self, o: &RoleWindow) {
        self.cpu_us += o.cpu_us;
        self.samples += o.samples;
        for (a, b) in self.states.iter_mut().zip(o.states.iter()) {
            *a += *b;
        }
    }
}

/// 役割ごとの窓。
pub type Threads = [RoleWindow; ROLES.len()];

/// スレッドの標本がどこまで取れているか (`/profile` の `sampler`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplerState {
    /// `PROXY_PROFILE_SAMPLE_MS=0`、または `/proc/self/task` が読めない
    Off = 0,
    /// スレッドは読めるが `syscall` が読めない (状態は `running` / `sleeping` だけ)
    Partial = 1,
    /// CPU も状態もシステムコールまで読めている
    On = 2,
}

impl SamplerState {
    pub fn name(self) -> &'static str {
        match self {
            SamplerState::Off => "off",
            SamplerState::Partial => "partial",
            SamplerState::On => "on",
        }
    }

    fn from_u8(v: u8) -> SamplerState {
        match v {
            2 => SamplerState::On,
            1 => SamplerState::Partial,
            _ => SamplerState::Off,
        }
    }
}

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

/// その区間に観測した段階 (CONNECT 7 段 + forward 6 段)。
///
/// **[`crate::metrics::Metrics::record`] が取っている鍵の内側でだけ書く。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stages {
    /// **0 ms だった観測の数** (CONNECT の 7 段 → forward の 6 段の順)。
    ///
    /// 0 ms は [`Window`] の `count` と `buckets[0]` (`≤ 1 ms` の区間) にしか効かないので、
    /// 熱い経路ではこの `u32` を 1 つ増やすだけにして、読むときに足し戻す
    /// ([`Stages::folded_connect`])。**要求ごとに触る共有のバイト数が 624 B → 52 B** に減る
    /// (段階がすべて 1 ms 未満の loopback はこちらしか通らない。`Metrics::record` の鍵は
    /// 全接続スレッドが共有しているので、書く範囲が広いほどキャッシュ行が飛び交う)
    zeros: [u32; CONNECT_STAGES.len() + FORWARD_STAGES.len()],
    connect: [Window; CONNECT_STAGES.len()],
    forward: [Window; FORWARD_STAGES.len()],
}

impl Default for Stages {
    fn default() -> Self {
        Stages {
            zeros: [0; CONNECT_STAGES.len() + FORWARD_STAGES.len()],
            connect: [Window::default(); CONNECT_STAGES.len()],
            forward: [Window::default(); FORWARD_STAGES.len()],
        }
    }
}

impl Stages {
    /// CONNECT 1 本を足す ([`CONNECT_STAGES`] の順)。
    pub fn observe_connect(&mut self, d: &Detail) {
        let v = [
            d.stages.queue as u64,
            d.stages.client_read as u64,
            d.dns_ms,
            d.connect_ms,
            d.stages.first_relay as u64,
            d.stages.relay as u64,
            d.stages.park as u64,
        ];
        for (i, ms) in v.into_iter().enumerate() {
            if ms == 0 {
                self.zeros[i] += 1;
            } else {
                self.connect[i].observe(ms);
            }
        }
    }

    /// 転送した 1 要求を足す ([`FORWARD_STAGES`] の順)。
    pub fn observe_forward(&mut self, d: &Detail) {
        let v = [
            d.stages.queue as u64,
            d.stages.client_read as u64,
            // プールに当たった要求は `origin_detail` を呼ばないので 0 のまま
            d.dns_ms + d.connect_ms,
            d.stages.send as u64,
            d.first_byte_ms.unwrap_or(0),
            d.stages.body as u64,
        ];
        for (i, ms) in v.into_iter().enumerate() {
            if ms == 0 {
                self.zeros[CONNECT_STAGES.len() + i] += 1;
            } else {
                self.forward[i].observe(ms);
            }
        }
    }

    /// 0 ms のぶんを足し戻した CONNECT の窓 (読むときに 1 回だけ組み立てる)。
    pub fn folded_connect(&self) -> [Window; CONNECT_STAGES.len()] {
        let mut out = self.connect;
        for (i, w) in out.iter_mut().enumerate() {
            fold_zeros(w, self.zeros[i]);
        }
        out
    }

    /// 0 ms のぶんを足し戻した forward の窓。
    pub fn folded_forward(&self) -> [Window; FORWARD_STAGES.len()] {
        let mut out = self.forward;
        for (i, w) in out.iter_mut().enumerate() {
            fold_zeros(w, self.zeros[CONNECT_STAGES.len() + i]);
        }
        out
    }

    /// 粗い解像度へ畳むときは足し合わせる (区間の値なので平均でも最後の値でもない)。
    pub fn merge(&mut self, o: &Stages) {
        for (a, b) in self.zeros.iter_mut().zip(o.zeros.iter()) {
            *a += *b;
        }
        for (a, b) in self.connect.iter_mut().zip(o.connect.iter()) {
            a.merge(b);
        }
        for (a, b) in self.forward.iter_mut().zip(o.forward.iter()) {
            a.merge(b);
        }
    }

    /// 1 本でも観測したか (JSON を小さくするための判定)。
    pub fn is_empty(&self) -> bool {
        self.zeros.iter().all(|z| *z == 0)
            && self
                .connect
                .iter()
                .chain(self.forward.iter())
                .all(|w| w.count == 0)
    }
}

/// 0 ms の観測を窓に足し戻す (`count` と `≤ 1 ms` の区間だけ)。
fn fold_zeros(w: &mut Window, zeros: u32) {
    if zeros == 0 {
        return;
    }
    w.count += zeros as u64;
    w.buckets[0] += zeros as u64;
}

/// 1 つの窓 (5 秒 または 60 秒) の中身。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sample {
    /// 窓の先頭 (epoch 秒)
    pub t: u64,
    /// その窓に処理した要求数 (`total_requests` の増分)
    pub requests: u64,
    /// その窓にプロセスが使った CPU (us。`/proc/self/stat` の utime + stime の増分)
    pub cpu_us: u64,
    pub stages: Stages,
    /// 役割ごとの CPU と状態 (T14.3 (2))
    pub threads: Threads,
    /// その窓にロックが取り合いになった回数 ([`crate::sync::LOCK_NAMES`] の順。T14.3 (3))
    pub locks: [u64; crate::sync::LOCK_NAMES.len()],
    /// その窓にワーカーの待ち行列で待った仕事の数・合計 ms・最大 ms
    pub queue_waited: u64,
    pub queue_ms_sum: u64,
    pub queue_ms_max: u64,
}

impl Sample {
    /// **プロセスの CPU/要求** (us)。§2 の loopback の 41 us/要求 と同じ物差し。
    pub fn cpu_per_request_us(&self) -> Option<f64> {
        (self.requests > 0).then(|| self.cpu_us as f64 / self.requests as f64)
    }

    fn merge_into(&mut self, o: &Sample) {
        self.requests += o.requests;
        self.cpu_us += o.cpu_us;
        self.stages.merge(&o.stages);
        for (a, b) in self.threads.iter_mut().zip(o.threads.iter()) {
            a.merge(b);
        }
        for (a, b) in self.locks.iter_mut().zip(o.locks.iter()) {
            *a += *b;
        }
        self.queue_waited += o.queue_waited;
        self.queue_ms_sum += o.queue_ms_sum;
        self.queue_ms_max = self.queue_ms_max.max(o.queue_ms_max);
    }

    /// `[t,requests,cpu_us,[connect...],[forward...],[roles...],[locks...],[queue...]]`。
    /// **件数 0 の段階と標本 0 の役割は `0` 1 文字**で書く (静かな窓を小さくするため)。
    fn push_row(&self, out: &mut String) {
        let _ = write!(out, "[{},{},{},[", self.t, self.requests, self.cpu_us);
        push_windows(out, &self.stages.folded_connect());
        out.push_str("],[");
        push_windows(out, &self.stages.folded_forward());
        out.push_str("],[");
        for (i, r) in self.threads.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            if r.samples == 0 {
                out.push('0');
                continue;
            }
            let _ = write!(out, "[{},{},[", r.cpu_us, r.samples);
            for (j, c) in r.states.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{}", c);
            }
            out.push_str("]]");
        }
        out.push_str("],[");
        for (i, l) in self.locks.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", l);
        }
        let _ = write!(
            out,
            "],[{},{},{}]]",
            self.queue_waited, self.queue_ms_sum, self.queue_ms_max
        );
    }
}

/// 窓の並びを JSON に書く (件数 0 は `0`、それ以外は `[count,sum,max,[buckets]]`)。
fn push_windows(out: &mut String, ws: &[Window]) {
    for (i, w) in ws.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if w.count == 0 {
            out.push('0');
            continue;
        }
        let _ = write!(out, "[{},{},{},[", w.count, w.ms_sum, w.ms_max);
        for (j, b) in w.buckets.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", b);
        }
        out.push_str("]]");
    }
}

/// 段階とスレッドの窓の環状バッファ (5 秒 × 720 と 60 秒 × 1,440)。
#[derive(Default)]
pub struct Profile {
    rings: [Mutex<VecDeque<Sample>>; 2],
    /// **起動からの累計**の段階 (`/metrics` の `sorahost_stage_seconds`。T14.19)。
    ///
    /// Prometheus のヒストグラムは単調増加が前提なので、窓 (環状バッファ) の和ではなく
    /// ここに積む。足すのは 5 秒に 1 回 ([`Profile::push`]) だけで、熱い経路は通らない。
    stage_totals: Mutex<Stages>,
    /// スレッドの標本の状態 ([`SamplerState`])
    sampler: std::sync::atomic::AtomicU8,
    /// 表に無いシステムコール番号と回数 (`sys_N` で出す。最大 [`MAX_UNKNOWN`] 種)
    unknown: Mutex<Vec<(i64, u64)>>,
}

/// 覚えておく「表に無いシステムコール番号」の種類数。
pub const MAX_UNKNOWN: usize = 16;

impl Profile {
    /// 5 秒の標本を足し、1 分の窓が閉じていればそれも作る。
    pub fn push(&self, s: Sample) {
        self.stage_totals.locked().merge(&s.stages);
        Self::append(&self.rings[0], s, RESOLUTIONS[0].1);
        self.roll(s.t);
    }

    /// 起動からの段階の累計 (`/metrics` の `sorahost_stage_seconds`。T14.19)。
    ///
    /// 窓と違って 0 に戻らない。最後の 5 秒の標本まで (= [`TICK`] の遅れ) が入る。
    pub fn stage_totals(&self) -> Stages {
        *self.stage_totals.locked()
    }

    /// 直前の 1 分の窓がまだ無ければ、5 秒の標本から作る ([`crate::history::History`] と同じ形)。
    fn roll(&self, now: u64) {
        let step = RESOLUTIONS[1].0;
        let window_start = (now / step) * step;
        let Some(prev) = window_start.checked_sub(step) else {
            return;
        };
        if self.rings[1].locked().back().is_some_and(|s| s.t >= prev) {
            return;
        }
        let src = self.rings[0].locked();
        let window: Vec<Sample> = src
            .iter()
            .filter(|s| s.t >= prev && s.t < window_start)
            .copied()
            .collect();
        drop(src);
        if window.is_empty() {
            return;
        }
        let mut agg = Sample {
            t: prev,
            ..Sample::default()
        };
        for s in &window {
            agg.merge_into(s);
        }
        Self::append(&self.rings[1], agg, RESOLUTIONS[1].1);
    }

    fn append(ring: &Mutex<VecDeque<Sample>>, s: Sample, cap: usize) {
        let mut q = ring.locked();
        if q.len() >= cap {
            q.pop_front();
        }
        q.push_back(s);
    }

    /// スレッドの標本の状態を書く (`profile-sample` スレッドだけが呼ぶ)。
    pub fn set_sampler(&self, state: SamplerState) {
        self.sampler.store(state as u8, Ordering::Relaxed);
    }

    /// スレッドの標本の状態。
    pub fn sampler(&self) -> SamplerState {
        SamplerState::from_u8(self.sampler.load(Ordering::Relaxed))
    }

    /// 表に無いシステムコール番号を 1 回数える (`/profile` に `sys_N` で出す)。
    pub fn note_unknown_syscall(&self, nr: i64) {
        let mut v = self.unknown.locked();
        if let Some(e) = v.iter_mut().find(|e| e.0 == nr) {
            e.1 += 1;
        } else if v.len() < MAX_UNKNOWN {
            v.push((nr, 1));
        }
    }

    /// 表に無いシステムコール番号の控え (多い順)。
    pub fn unknown_syscalls(&self) -> Vec<(i64, u64)> {
        let mut v = self.unknown.locked().clone();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// 解像度 (秒) から添字を引く。合わなければ 5 秒。
    pub fn index_for(secs: u64) -> usize {
        RESOLUTIONS
            .iter()
            .position(|(s, _)| *s == secs)
            .unwrap_or(0)
    }

    /// その解像度の標本数。
    pub fn len(&self, res: usize) -> usize {
        self.rings[res.min(1)].locked().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len(0) == 0
    }

    /// 直近 `n` 標本を 1 つに足し合わせる (画面の積み上げと CPU/要求 の要約用)。
    pub fn recent_totals(&self, res: usize, n: usize) -> Sample {
        let q = self.rings[res.min(1)].locked();
        let mut out = Sample::default();
        for s in q.iter().skip(q.len().saturating_sub(n)) {
            out.t = s.t;
            out.merge_into(s);
        }
        out
    }

    /// **新しい順に** `budget` バイトまで書けるだけ集め、古い順に並べて返す。
    /// 返すのは (JSON の並び, 書けた件数, 全体の件数, 打ち切ったか)。
    pub fn rows_within(&self, res: usize, budget: usize) -> (String, usize, usize, bool) {
        let q = self.rings[res.min(1)].locked();
        let total = q.len();
        let mut rows: Vec<String> = Vec::new();
        let mut used = 0usize;
        let mut cut = false;
        for s in q.iter().rev() {
            let mut row = String::with_capacity(160);
            s.push_row(&mut row);
            if used + row.len() + 1 > budget {
                cut = true;
                break;
            }
            used += row.len() + 1;
            rows.push(row);
        }
        drop(q);
        rows.reverse();
        let shown = rows.len();
        (rows.join(","), shown, total, cut)
    }
}

/// 1 標本ぶんの「前回との差」を取るための覚え書き。
struct Tick {
    requests: u64,
    cpu_us: u64,
    locks: [u64; crate::sync::LOCK_NAMES.len()],
    queue_waited: u64,
    queue_ms_sum: u64,
}

impl Tick {
    fn now(metrics: &Metrics) -> Tick {
        let [queue_waited, queue_ms_sum, _] = crate::sync::queue_totals();
        Tick {
            requests: metrics.total_requests.load(Ordering::Relaxed),
            cpu_us: process_cpu_us().unwrap_or(0),
            locks: crate::sync::lock_contended(),
            queue_waited,
            queue_ms_sum,
        }
    }
}

/// スレッドの標本を取る側 (`profile-sample` スレッドが 1 つだけ持つ。T14.3 (2))。
///
/// **CPU はスレッドごとの増分**で積む。新しく生まれたスレッドは前回が無いので
/// 「生まれてからのぶん」がそのまま増分になり (正しい)、消えたスレッドは最後の
/// 1 標本ぶんだけ落ちる (役割別の CPU はその分だけ少なめに出る。`cpu_us` =
/// プロセス全体はこの取りこぼしが無いので、**CPU/要求 は正確**)。
pub struct Sampler {
    /// 普通は `/proc/self/task` (テストは差し替える)
    root: std::path::PathBuf,
    /// 主スレッド = accept 役の tid (= pid)
    main_tid: u32,
    /// 前回のスレッド別 utime + stime (clock tick)
    prev: std::collections::HashMap<u32, u64>,
    /// 読み取りの使い回し用
    buf: String,
    /// 1 clock tick の us
    tick_us: u64,
    /// 5 秒の窓へ渡す前の溜め
    pending: Threads,
}

impl Sampler {
    pub fn new(root: std::path::PathBuf, main_tid: u32) -> Sampler {
        Sampler {
            root,
            main_tid,
            prev: std::collections::HashMap::new(),
            buf: String::with_capacity(1024),
            tick_us: 1_000_000 / clock_tick(),
            pending: Threads::default(),
        }
    }

    /// 自プロセス用 (`/proc/self/task`)。
    pub fn for_self() -> Sampler {
        Sampler::new(
            std::path::PathBuf::from("/proc/self/task"),
            std::process::id(),
        )
    }

    /// 1 回ぶん取る。読めなければ [`SamplerState::Off`] を書いて何もしない。
    pub fn sample(&mut self, profile: &Profile) {
        let Some(scan) = crate::sysinfo::scan_tasks(&self.root, &mut self.buf) else {
            profile.set_sampler(SamplerState::Off);
            return;
        };
        let mut next = std::collections::HashMap::with_capacity(scan.tasks.len());
        for t in &scan.tasks {
            let role = role_of(t.tid, self.main_tid, &t.comm);
            let prev = self.prev.get(&t.tid).copied().unwrap_or(0);
            next.insert(t.tid, t.ticks);
            let w = &mut self.pending[role];
            w.cpu_us += t.ticks.saturating_sub(prev) * self.tick_us;
            w.samples += 1;
            let (slot, unknown) = state_slot(t.syscall, t.state);
            w.states[slot] += 1;
            if let Some(nr) = unknown {
                profile.note_unknown_syscall(nr);
            }
        }
        self.prev = next;
        profile.set_sampler(if scan.syscalls_readable {
            SamplerState::On
        } else {
            SamplerState::Partial
        });
    }

    /// 溜めた標本を取り出して 0 に戻す (5 秒の窓へ)。
    fn take(&mut self) -> Threads {
        std::mem::take(&mut self.pending)
    }
}

/// `/proc/self/stat` の utime + stime (us)。読めなければ `None`。
pub fn process_cpu_us() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/stat").ok()?;
    parse_stat_cpu_us(&text)
}

/// `/proc/<pid>/stat` の utime (14) + stime (15) を us で返す。
///
/// **comm は括弧で囲まれていて空白も括弧も含みうる**ので、最後の `)` から後ろを数える。
pub fn parse_stat_cpu_us(text: &str) -> Option<u64> {
    let rest = &text[text.rfind(')')? + 1..];
    let mut it = rest.split_whitespace();
    // 最後の ')' の次は state (3 番目の項目) なので、utime は 11 個先
    let utime: u64 = it.nth(11)?.parse().ok()?;
    let stime: u64 = it.next()?.parse().ok()?;
    Some((utime + stime) * (1_000_000 / clock_tick()))
}

/// `sysconf(_SC_CLK_TCK)`。Linux では実質いつも 100。
fn clock_tick() -> u64 {
    #[cfg(target_os = "linux")]
    {
        const SC_CLK_TCK: i32 = 2;
        unsafe extern "C" {
            fn sysconf(name: i32) -> i64;
        }
        // SAFETY: 引数も戻り値も整数だけの読み取り専用の問い合わせ
        let v = unsafe { sysconf(SC_CLK_TCK) };
        if v > 0 {
            return v as u64;
        }
    }
    100
}

/// スレッドの標本の既定の間隔 (ms)。`PROXY_PROFILE_SAMPLE_MS` で変える (`0` で止める)。
pub const DEFAULT_SAMPLE_MS: u64 = 1000;

/// 標本の間隔の下限と上限 (ms)。下限を置くのは自分の CPU を使い切らないため。
pub const MIN_SAMPLE_MS: u64 = 50;
pub const MAX_SAMPLE_MS: u64 = 60_000;

/// 段階の窓を畳み、スレッドの標本を取るスレッドを起こす (`profile-sample`)。
///
/// **`--lite` では呼ばない** (窓を 1 本も作らない)。`sample_ms` が `0` なら
/// スレッドの標本は取らず (`sampler: "off"`)、5 秒ごとに段階の窓だけ畳む。
pub fn spawn(metrics: std::sync::Arc<Metrics>, sample_ms: u64) -> JoinHandle<()> {
    SAMPLE_MS.store(sample_ms, Ordering::Relaxed);
    thread::Builder::new()
        .name("profile-sample".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut prev = Tick::now(&metrics);
            let mut sampler = (sample_ms > 0).then(Sampler::for_self);
            let step = match sample_ms {
                0 => TICK,
                ms => Duration::from_millis(ms.clamp(MIN_SAMPLE_MS, MAX_SAMPLE_MS)),
            };
            let mut waited = Duration::ZERO;
            loop {
                thread::sleep(step);
                if let Some(s) = sampler.as_mut() {
                    s.sample(&metrics.profile);
                }
                waited += step;
                if waited >= TICK {
                    waited = Duration::ZERO;
                    tick(&metrics, &mut prev, sampler.as_mut());
                }
            }
        })
        .expect("spawn profile-sample thread")
}

/// 1 標本ぶんを窓へ (`spawn` のループの中身。テストからも呼ぶ)。
fn tick(metrics: &Metrics, prev: &mut Tick, sampler: Option<&mut Sampler>) {
    let now = Tick::now(metrics);
    let mut locks = [0u64; crate::sync::LOCK_NAMES.len()];
    for ((o, a), b) in locks
        .iter_mut()
        .zip(now.locks.iter())
        .zip(prev.locks.iter())
    {
        *o = a.saturating_sub(*b);
    }
    let s = Sample {
        t: now_epoch(),
        requests: now.requests.saturating_sub(prev.requests),
        cpu_us: now.cpu_us.saturating_sub(prev.cpu_us),
        stages: metrics.take_stages(),
        threads: sampler.map(Sampler::take).unwrap_or_default(),
        locks,
        queue_waited: now.queue_waited.saturating_sub(prev.queue_waited),
        queue_ms_sum: now.queue_ms_sum.saturating_sub(prev.queue_ms_sum),
        queue_ms_max: crate::sync::take_queue_window_max(),
    };
    *prev = now;
    metrics.profile.push(s);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{Detail, StageMs};

    fn detail(stages: StageMs, dns: u64, connect: u64, ttfb: Option<u64>) -> Detail {
        Detail {
            dns_ms: dns,
            connect_ms: connect,
            first_byte_ms: ttfb,
            stages,
            ..Detail::default()
        }
    }

    #[test]
    fn a_connect_lands_in_all_seven_stages() {
        let mut s = Stages::default();
        s.observe_connect(&detail(
            StageMs {
                queue: 1,
                client_read: 2,
                first_relay: 30,
                relay: 400,
                park: 5000,
                ..StageMs::default()
            },
            6,
            9,
            None,
        ));
        let got: Vec<u64> = s.folded_connect().iter().map(|w| w.ms_sum).collect();
        assert_eq!(got, vec![1, 2, 6, 9, 30, 400, 5000]);
        assert!(s.folded_connect().iter().all(|w| w.count == 1));
        assert!(s.folded_forward().iter().all(|w| w.count == 0));
    }

    #[test]
    fn a_forward_request_lands_in_all_six_stages() {
        let mut s = Stages::default();
        s.observe_forward(&detail(
            StageMs {
                queue: 1,
                client_read: 2,
                send: 3,
                body: 40,
                ..StageMs::default()
            },
            6,
            9,
            Some(20),
        ));
        let got: Vec<u64> = s.folded_forward().iter().map(|w| w.ms_sum).collect();
        // origin = dns + connect
        assert_eq!(got, vec![1, 2, 15, 3, 20, 40]);
    }

    /// 粗い窓へは足し合わせる (件数も区間も)。
    #[test]
    fn merging_adds_up() {
        let mut a = Stages::default();
        let mut b = Stages::default();
        a.observe_connect(&detail(StageMs::default(), 3, 0, None));
        b.observe_connect(&detail(StageMs::default(), 7, 0, None));
        a.merge(&b);
        assert_eq!(a.folded_connect()[2].count, 2);
        assert_eq!(a.folded_connect()[2].ms_sum, 10);
        assert_eq!(a.folded_connect()[2].ms_max, 7);
        // 0 ms の段階も件数だけは残り、`≤ 1 ms` の区間に入っている
        assert_eq!(a.folded_connect()[0].count, 2);
        assert_eq!(a.folded_connect()[0].buckets[0], 2);
        assert_eq!(a.folded_connect()[0].ms_sum, 0);
    }

    /// 1 分の窓は 5 秒の標本 12 本から作られ、環状バッファは上限で古いものを捨てる。
    #[test]
    fn the_minute_window_is_built_from_the_five_second_ones() {
        let p = Profile::default();
        let base = 1_800_000_000u64; // 分の頭 (60 で割り切れる)
        for i in 0..12 {
            let mut st = Stages::default();
            st.observe_connect(&detail(StageMs::default(), 10, 0, None));
            p.push(Sample {
                t: base + i * 5,
                requests: 2,
                cpu_us: 100,
                stages: st,
                ..Sample::default()
            });
        }
        // まだ次の分に入っていないので 1 分の窓はできない
        assert_eq!(p.len(1), 0);
        p.push(Sample {
            t: base + 60,
            ..Sample::default()
        });
        assert_eq!(p.len(1), 1);
        let minute = p.recent_totals(1, 10).stages;
        assert_eq!(minute.folded_connect()[2].count, 12);
        assert_eq!(minute.folded_connect()[2].ms_sum, 120);
    }

    /// 上限を超えたら古い標本から捨てる。
    #[test]
    fn the_ring_keeps_only_the_capacity() {
        let p = Profile::default();
        for i in 0..(RESOLUTIONS[0].1 as u64 + 10) {
            p.push(Sample {
                t: 1_000_000 + i * 5,
                ..Sample::default()
            });
        }
        assert_eq!(p.len(0), RESOLUTIONS[0].1);
    }

    /// 件数 0 の段階は `0` 1 文字で書く (静かな窓を小さくする)。
    #[test]
    fn empty_stages_are_written_as_a_single_zero() {
        let mut row = String::new();
        Sample {
            t: 7,
            ..Sample::default()
        }
        .push_row(&mut row);
        assert_eq!(
            row,
            "[7,0,0,[0,0,0,0,0,0,0],[0,0,0,0,0,0],[0,0,0,0,0,0,0,0,0],[0,0,0,0],[0,0,0]]"
        );
    }

    /// 予算に入らない古い標本は落とし、`truncated` を立てる。
    #[test]
    fn rows_are_cut_to_the_budget_newest_first() {
        let p = Profile::default();
        for i in 0..50u64 {
            p.push(Sample {
                t: 1_000_000 + i * 5,
                requests: i,
                ..Sample::default()
            });
        }
        let (all, shown, total, cut) = p.rows_within(0, 1 << 20);
        assert_eq!(total, 50);
        assert_eq!(shown, 50);
        assert!(!cut);
        assert!(all.starts_with("[1000000,0,"), "{}", &all[..24]);
        let (small, shown2, _, cut2) = p.rows_within(0, 120);
        assert!(shown2 < 50 && shown2 > 0, "書けた件数: {}", shown2);
        assert!(cut2, "予算に入り切らなければ打ち切る");
        assert!(small.len() <= 120);
        // 残るのは**新しい方**
        assert!(small.contains("[1000245,49,"), "{}", small);
    }

    /// `/proc/<pid>/stat` の comm に空白と括弧が入っていても utime / stime を読める。
    #[test]
    fn the_stat_line_is_parsed_after_the_last_paren() {
        let mut line = String::from("42 (weird ) name) S 1 42 42 0 -1 4194304 0 0 0 0 ");
        line.push_str("111 222 0 0 20 0 8 0 100 0 0");
        let us = parse_stat_cpu_us(&line).expect("読めること");
        assert_eq!(us, (111 + 222) * (1_000_000 / clock_tick()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_has_used_some_cpu() {
        assert!(process_cpu_us().is_some());
    }

    /// 役割は主スレッド (tid == pid) とスレッド名で決まり、知らない名前は `other`。
    #[test]
    fn roles_come_from_the_thread_name() {
        assert_eq!(ROLES[role_of(7, 7, "rust-http-proxy")], "accept");
        assert_eq!(ROLES[role_of(9, 7, "conn")], "conn");
        assert_eq!(ROLES[role_of(9, 7, "idle-watch")], "idle-watch");
        assert_eq!(ROLES[role_of(9, 7, "profile-sample")], "profile-sample");
        assert_eq!(ROLES[role_of(9, 7, "env-reload")], "other");
        // 主スレッドの名前は実行ファイル名なので、名前より tid を先に見る
        assert_eq!(ROLES[role_of(7, 7, "conn")], "accept");
    }

    /// 状態は「走行中 / システムコール名 / 休眠 / その他」に落ちる。
    #[test]
    fn states_map_to_syscall_names() {
        let names = state_names();
        assert_eq!(names[state_slot(Some(-1), 'R').0], "running");
        assert_eq!(names[state_slot(None, 'R').0], "running");
        assert_eq!(names[state_slot(None, 'S').0], "sleeping");
        assert_eq!(names[state_slot(None, 'Z').0], "other");
        // 表にある番号は名前で、無い番号は `other` + 番号を返す
        let nr = SYSCALL_NRS[2]; // ppoll
        assert_eq!(names[state_slot(Some(nr), 'S').0], "ppoll");
        assert_eq!(state_slot(Some(99_999), 'S'), (NSTATES - 1, Some(99_999)));
        assert_eq!(names.len(), NSTATES);
        assert_eq!(names[1], "recvfrom");
    }

    /// 番号の表は名前と同じ長さで、同じ番号が 2 つ無いこと。
    #[test]
    fn the_syscall_table_is_consistent() {
        assert_eq!(SYSCALL_NRS.len(), SYSCALL_NAMES.len());
        for (i, a) in SYSCALL_NRS.iter().enumerate() {
            for b in SYSCALL_NRS.iter().skip(i + 1) {
                assert_ne!(a, b, "番号が重複している: {}", a);
            }
        }
    }

    /// 差し替えたディレクトリから取ると、役割ごとに CPU と状態が積まれる。
    /// 2 回目は**増分だけ**が積まれること (累計をそのまま足さない)。
    #[test]
    fn the_sampler_accumulates_per_role_deltas() {
        let dir = std::env::temp_dir().join(format!("t143-sampler-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let write = |tid: u32, comm: &str, ticks: u64, syscall: Option<&str>| {
            let d = dir.join(tid.to_string());
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("stat"),
                format!(
                    "{} ({}) S 1 1 0 0 -1 0 0 0 0 0 {} 0 0 0 20 0 8 0 100 0\n",
                    tid, comm, ticks
                ),
            )
            .unwrap();
            match syscall {
                Some(v) => std::fs::write(d.join("syscall"), v).unwrap(),
                None => {
                    let _ = std::fs::remove_file(d.join("syscall"));
                }
            }
        };
        write(1, "proxy", 10, Some("running\n"));
        write(
            2,
            "conn",
            20,
            Some(format!("{} 0x0\n", SYSCALL_NRS[5]).as_str()),
        );
        let p = Profile::default();
        let mut s = Sampler::new(dir.clone(), 1);
        s.sample(&p);
        assert_eq!(p.sampler(), SamplerState::On);
        let names = state_names();
        let accept = s.pending[0];
        let conn = s.pending[1];
        assert_eq!(accept.samples, 1);
        assert_eq!(
            accept.states[names.iter().position(|n| *n == "running").unwrap()],
            1
        );
        assert_eq!(
            conn.states[names.iter().position(|n| *n == "splice").unwrap()],
            1
        );
        let first = conn.cpu_us;
        assert!(first > 0, "はじめて見たスレッドは累計がそのまま増分");
        // 2 回目: 5 tick だけ進める
        write(
            2,
            "conn",
            25,
            Some(format!("{} 0x0\n", SYSCALL_NRS[5]).as_str()),
        );
        s.sample(&p);
        let delta = s.pending[1].cpu_us - first;
        assert_eq!(delta, 5 * (1_000_000 / clock_tick()), "増分だけ積む");
        // `syscall` が無ければ partial
        write(2, "conn", 25, None);
        write(1, "proxy", 10, None);
        s.sample(&p);
        assert_eq!(p.sampler(), SamplerState::Partial);
        assert!(s.pending[1].states[names.iter().position(|n| *n == "sleeping").unwrap()] >= 1);
        // 取り出したら 0 に戻る
        let taken = s.take();
        assert!(taken.iter().any(|r| r.samples > 0));
        assert!(s.pending.iter().all(|r| r.samples == 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/proc` が読めない環境では `off` に落ちる (受け入れ基準)。
    #[test]
    fn an_unreadable_proc_turns_the_sampler_off() {
        let p = Profile::default();
        p.set_sampler(SamplerState::On);
        let mut s = Sampler::new(std::path::PathBuf::from("/nonexistent/task"), 1);
        s.sample(&p);
        assert_eq!(p.sampler(), SamplerState::Off);
        assert_eq!(p.sampler().name(), "off");
    }

    /// 表に無いシステムコール番号は `sys_N` 用に控える (上限 [`MAX_UNKNOWN`] 種)。
    #[test]
    fn unknown_syscall_numbers_are_kept() {
        let p = Profile::default();
        for _ in 0..3 {
            p.note_unknown_syscall(999);
        }
        p.note_unknown_syscall(998);
        for i in 0..MAX_UNKNOWN as i64 {
            p.note_unknown_syscall(500 + i);
        }
        let v = p.unknown_syscalls();
        assert_eq!(v[0], (999, 3));
        assert_eq!(v.len(), MAX_UNKNOWN);
    }
}
