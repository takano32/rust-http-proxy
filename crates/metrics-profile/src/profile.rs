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

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::cache::now_epoch;
use crate::metrics::Detail;
use crate::sync::LockExt;
use crate::window::Window;

/// 段階の窓を畳む間隔と本数 (5 秒 × 720 = 1 時間、60 秒 × 1,440 = 1 日)。
pub const RESOLUTIONS: [(u64, usize); 2] = [(5, 720), (60, 1440)];

/// 標本 1 本のバイト (`/status` の `memory.rings` の見積もりが使う。T15.0 (13))。
///
/// **数字をここに書き写さない** (型を足すたびに古くなる)。実際の値は単体テスト
/// `a_sample_stays_small_enough_for_the_rings` を `-- --nocapture` で回すと出る。
pub const fn sample_bytes() -> usize {
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
    /// そのうちユーザー空間 (us。utime の増分。T16.0)。カーネル側は `cpu_us - user_us`
    pub user_us: u64,
    /// 標本の数 (スレッド数 × 標本回数)。割合はこれで割る
    pub samples: u64,
    /// 状態の内訳 ([`state_names`] の順)
    pub states: [u32; NSTATES],
}

impl RoleWindow {
    fn merge(&mut self, o: &RoleWindow) {
        self.cpu_us += o.cpu_us;
        self.user_us += o.user_us;
        self.samples += o.samples;
        for (a, b) in self.states.iter_mut().zip(o.states.iter()) {
            *a += *b;
        }
    }
}

/// 役割ごとの窓。
pub type Threads = [RoleWindow; ROLES.len()];

/// `threads_top` に残すスレッドの数 (CPU の多い順)。
pub const TOP_THREADS: usize = 8;

/// **その窓で CPU を多く使ったスレッド 1 本** (T15.0 (5))。
///
/// 役割ごとの集計では「`conn` 役の `running` がいつも 2.0 本」までしか読めず、
/// **どのスレッドが回っているか**が出ない。空回り (T15.5) や確立の尾を追うときは
/// tid まで要るので、CPU の多い順に [`TOP_THREADS`] 本だけ残す。
///
/// **`String` は入れない** ([`Sample`] が `Copy` を失うと [`Profile::roll`] の `.copied()` と
/// [`Profile::recent_totals`] が壊れる)。名前は `/proc` の `comm` と同じ**固定長 16 バイト**
/// (`comm` は 15 文字で切られる)。空の枠は `tid == 0`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TopThread {
    /// その窓にこのスレッドが使った CPU (us)
    pub cpu_us: u64,
    /// スレッド番号 (`0` = 空の枠)
    pub tid: u32,
    /// [`state_slot`] が `running` を返した標本の数
    pub running: u32,
    /// [`ROLES`] の添字
    pub role: u8,
    /// スレッド名 (`/proc/<pid>/task/<tid>/stat` の 2 番目。末尾は `0` 詰め)
    pub comm: [u8; 16],
}

impl TopThread {
    /// 名前を `[u8; 16]` に詰める (16 バイトを超えるぶんは捨てる)。
    fn comm_bytes(comm: &str) -> [u8; 16] {
        let mut out = [0u8; 16];
        for (o, b) in out.iter_mut().zip(comm.as_bytes()) {
            *o = *b;
        }
        out
    }

    /// 名前 (`0` 詰めを外し、JSON に出せない文字は `.` に落とす)。
    pub fn comm_str(&self) -> String {
        self.comm
            .iter()
            .take_while(|b| **b != 0)
            .map(|b| match b {
                // `"` と `\` を書かない = 逃がし方を考えなくてよい。`comm` は
                // カーネルが任意のバイトを許すので、印字できない文字も落とす
                0x20..=0x7e if *b != b'"' && *b != b'\\' => *b as char,
                _ => '.',
            })
            .collect()
    }
}

/// 上位のスレッドを tid を鍵に足し合わせ、CPU の多い順に [`TOP_THREADS`] 本へ切り直す。
///
/// 60 秒の窓は 5 秒の標本 12 本を畳むので、同じスレッドが何度も出てくる。
/// **確保はしない** (最大 2 × [`TOP_THREADS`] の局所配列だけ)。
///
/// `a` が古い側・`b` が新しい側 ([`Sample::merge_into`] は古い順に呼ぶ)。同じ tid の
/// **名前と役割は新しい方を採る** — 生まれたばかりのスレッドは `pthread_setname_np` を
/// 呼ぶまで親の `comm` を名乗るので、古い方を残すと親の名前が 60 秒の窓まで残る。
fn merge_top(
    a: &[TopThread; TOP_THREADS],
    b: &[TopThread; TOP_THREADS],
) -> [TopThread; TOP_THREADS] {
    let mut buf = [TopThread::default(); TOP_THREADS * 2];
    let mut n = 0usize;
    for t in a.iter().chain(b.iter()).filter(|t| t.tid != 0) {
        if let Some(e) = buf[..n].iter_mut().find(|e| e.tid == t.tid) {
            e.cpu_us += t.cpu_us;
            e.running += t.running;
            e.comm = t.comm;
            e.role = t.role;
            continue;
        }
        buf[n] = *t;
        n += 1;
    }
    buf[..n].sort_unstable_by(|x, y| y.cpu_us.cmp(&x.cpu_us).then(x.tid.cmp(&y.tid)));
    let mut out = [TopThread::default(); TOP_THREADS];
    for (o, t) in out.iter_mut().zip(buf[..n].iter()) {
        *o = *t;
    }
    out
}

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
    /// その窓に CPU を多く使ったスレッド (多い順。空の枠は `tid == 0`。T15.0 (5))
    pub threads_top: [TopThread; TOP_THREADS],
    /// **走れるのに走れなかった時間**の増分 (us。[`ROLES`] の順)。
    /// `None` = `schedstat` が読めない環境 (`/profile` では `null`)
    pub run_delay_us: Option<[u64; ROLES.len()]>,
    /// その窓にプロセスが使った CPU のうち**ユーザー空間** (us。`/proc/self/stat` の utime の増分。T16.0)。
    /// カーネル側は `cpu_us - cpu_user_us` (sys は持たない)。役割ごとは [`RoleWindow::user_us`]
    pub cpu_user_us: u64,
}

impl Sample {
    /// **プロセスの CPU/要求** (us)。§2 の loopback の 41 us/要求 と同じ物差し。
    pub fn cpu_per_request_us(&self) -> Option<f64> {
        (self.requests > 0).then(|| self.cpu_us as f64 / self.requests as f64)
    }

    fn merge_into(&mut self, o: &Sample) {
        self.requests += o.requests;
        self.cpu_us += o.cpu_us;
        self.cpu_user_us += o.cpu_user_us;
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
        self.threads_top = merge_top(&self.threads_top, &o.threads_top);
        // 片方でも読めていれば足す (読めない窓は無かったことにする)
        self.run_delay_us = match (self.run_delay_us, o.run_delay_us) {
            (Some(mut a), Some(b)) => {
                for (x, y) in a.iter_mut().zip(b.iter()) {
                    *x += *y;
                }
                Some(a)
            }
            (a, b) => a.or(b),
        };
    }

    /// `[t,requests,cpu_us,[connect...],[forward...],[roles...],[locks...],[queue...],`
    /// `[threads_top...],[run_delay_us...],[user_us...],cpu_user_us]`。
    /// **件数 0 の段階と標本 0 の役割は `0` 1 文字**で書く (静かな窓を小さくするため)。
    /// 上位のスレッドが 1 本も無い窓も `0`、`schedstat` が読めなければ `run_delay_us` は `null`。
    /// `user_us` (役割ごとのユーザー空間。T16.0) は `roles` と同じ長さの配列で、いつも数を書く。
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
            "],[{},{},{}],",
            self.queue_waited, self.queue_ms_sum, self.queue_ms_max
        );
        // 上位のスレッド: `[[tid,"comm",role,cpu_us,running],…]` (空の窓は `0`)
        if self.threads_top[0].tid == 0 {
            out.push('0');
        } else {
            out.push('[');
            for (i, t) in self
                .threads_top
                .iter()
                .take_while(|t| t.tid != 0)
                .enumerate()
            {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(
                    out,
                    "[{},\"{}\",{},{},{}]",
                    t.tid,
                    t.comm_str(),
                    t.role,
                    t.cpu_us,
                    t.running
                );
            }
            out.push(']');
        }
        match &self.run_delay_us {
            None => out.push_str(",null"),
            Some(v) => {
                out.push_str(",[");
                for (i, d) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    let _ = write!(out, "{}", d);
                }
                out.push(']');
            }
        }
        // 新しい列は末尾に足す (T16.0): 役割ごとのユーザー空間と、プロセス全体のユーザー空間
        out.push_str(",[");
        for (i, r) in self.threads.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", r.user_us);
        }
        let _ = write!(out, "],{}]", self.cpu_user_us);
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

    /// 2 つの環の置き場を満杯のぶん ([`capacity_bytes`]) 確保して触る
    /// (**起動時に 1 回だけ**。T17.8)。返すのは触ったバイト数。
    ///
    /// 件数は増えないので [`Profile::used_bytes`] (`memory.rings_used.profile`) は変わらない。
    pub fn prefault(&self) -> usize {
        (0..RESOLUTIONS.len())
            .map(|res| crate::prefault::deque(&mut self.rings[res].locked(), RESOLUTIONS[res].1))
            .sum()
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

    /// **いま環に入っているぶん**のバイト (`/status` の `memory.rings_used.profile`。T15.0 (13))。
    ///
    /// `--lite` では標本を 1 本も作らないので 0。満杯のときの見積もりは
    /// [`capacity_bytes`]。
    pub fn used_bytes(&self) -> usize {
        (self.len(0) + self.len(1)) * sample_bytes()
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
        self.rows_within_page(res, budget, usize::MAX, 0)
    }

    /// [`Profile::rows_within`] の**ページ送りつき** (`/profile?n=&offset=`。T15.0 (11))。
    ///
    /// 環は時刻で引ける作りになっていない (新しい順に詰めるだけ) ので、`since=` /
    /// `until=` ではなく「新しい方から何本飛ばして、何本返すか」で切る。`offset` は
    /// **新しい順の並び**を飛ばす本数で、返す並びは今までどおり**古い順**。
    /// `budget` はページ 1 枚ぶんに効く (飛ばしたぶんは数えない)。
    ///
    /// 打ち切り (4 つ目の戻り) は**バイト数で切れたときだけ**真で、`n` で切れたぶんは
    /// 「続きがある」= 呼ぶ側が `offset + 書けた件数` で次を引く。
    pub fn rows_within_page(
        &self,
        res: usize,
        budget: usize,
        n: usize,
        offset: usize,
    ) -> (String, usize, usize, bool) {
        let q = self.rings[res.min(1)].locked();
        let total = q.len();
        let mut rows: Vec<String> = Vec::new();
        let mut used = 0usize;
        let mut cut = false;
        for s in q.iter().rev().skip(offset) {
            if rows.len() >= n {
                break;
            }
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
    /// 前回のスレッド別 (utime + stime (clock tick)、utime (clock tick。T16.0)、
    /// `schedstat` の走れずに待った ns)
    prev: std::collections::HashMap<u32, (u64, u64, u64)>,
    /// 読み取りの使い回し用
    buf: String,
    /// 1 clock tick の us
    tick_us: u64,
    /// 5 秒の窓へ渡す前の溜め
    pending: Threads,
    /// 同上、スレッド単位 (tid → その窓の CPU と `running` の数)。**窓ごとに空にする**
    pending_top: std::collections::HashMap<u32, TopThread>,
    /// 同上、役割ごとの「走れずに待った時間」(**ns**)。us へ落とすのは
    /// [`Sampler::take`] で 1 度だけ (標本ごとに割ると 1 us 未満の待ちが毎回消える)
    pending_run_delay: [u64; ROLES.len()],
    /// `schedstat` が 1 本でも読めたか (読めなければ `run_delay_us` は `null`)
    schedstat_readable: bool,
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
            pending_top: std::collections::HashMap::new(),
            pending_run_delay: [0; ROLES.len()],
            schedstat_readable: false,
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
        self.schedstat_readable |= scan.schedstat_readable;
        for t in &scan.tasks {
            let role = role_of(t.tid, self.main_tid, &t.comm);
            let (prev_ticks, prev_utime, prev_delay) =
                self.prev.get(&t.tid).copied().unwrap_or((0, 0, 0));
            let delay_ns = t.run_delay_ns.unwrap_or(0);
            next.insert(t.tid, (t.ticks, t.utime, delay_ns));
            let cpu_us = t.ticks.saturating_sub(prev_ticks) * self.tick_us;
            let w = &mut self.pending[role];
            w.cpu_us += cpu_us;
            // ユーザー空間も同じ流儀の増分 (T16.0。同じ `stat` の 1 回の読みから取るので、
            // 役割ごとには user <= 合計 がいつも成り立つ)
            w.user_us += t.utime.saturating_sub(prev_utime) * self.tick_us;
            w.samples += 1;
            let (slot, unknown) = state_slot(t.syscall, t.state);
            w.states[slot] += 1;
            if let Some(nr) = unknown {
                profile.note_unknown_syscall(nr);
            }
            // **走れるのに走れなかった時間**も増分で積む (はじめて見たスレッドは
            // 「生まれてからのぶん」がそのまま入る。CPU と同じ流儀)。
            // **ns のまま積む** — ここで us に落とすと 1 us 未満の待ちが標本ごと・
            // スレッドごとにまるごと消え、140 スレッド × 5 標本の窓で最大 700 us
            // (平均 350 us) ぶん**下向きに**外れる。割るのは [`Sampler::take`] で 1 度だけ
            self.pending_run_delay[role] += delay_ns.saturating_sub(prev_delay);
            // スレッド単位 (上位 8 本を切り出す元。**窓のあいだだけ持つ**)。
            // **名前と役割は標本ごとに書き直す**: Linux は `clone` のとき子に親の `comm` を
            // 継がせ、子が `pthread_setname_np` を呼ぶまでそのままなので、はじめて見たときの
            // 値を入れっぱなしにすると、生まれたばかりのスレッドが**親の名前**のまま
            // 窓じゅう (60 秒の窓まで) 居座る。この欄は tid と名前で犯人を指すためのもの
            let e = self.pending_top.entry(t.tid).or_default();
            e.tid = t.tid;
            e.role = role as u8;
            e.comm = TopThread::comm_bytes(&t.comm);
            e.cpu_us += cpu_us;
            e.running += u32::from(slot == STATE_RUNNING);
        }
        self.prev = next;
        profile.set_sampler(if scan.syscalls_readable {
            SamplerState::On
        } else {
            SamplerState::Partial
        });
    }

    /// 溜めた標本を取り出して 0 に戻す (5 秒の窓へ)。
    ///
    /// 返すのは (役割ごと, CPU の多い順の上位 [`TOP_THREADS`] 本, 役割ごとの
    /// 走れずに待った時間 (`schedstat` が読めなければ `None`))。
    fn take(
        &mut self,
    ) -> (
        Threads,
        [TopThread; TOP_THREADS],
        Option<[u64; ROLES.len()]>,
    ) {
        // **何もしなかったスレッドは残さない** (暇なとき 140 本のうち 8 本を
        // 「CPU 0 の役立たず」で埋めても読む人の助けにならない)
        let mut all: Vec<TopThread> = self
            .pending_top
            .drain()
            .map(|(_, t)| t)
            .filter(|t| t.cpu_us > 0 || t.running > 0)
            .collect();
        all.sort_unstable_by(|x, y| y.cpu_us.cmp(&x.cpu_us).then(x.tid.cmp(&y.tid)));
        let mut top = [TopThread::default(); TOP_THREADS];
        for (o, t) in top.iter_mut().zip(all.iter()) {
            *o = *t;
        }
        // ns で積んであるので、**ここで 1 度だけ** us に落とす
        let mut run_delay = std::mem::take(&mut self.pending_run_delay);
        for v in run_delay.iter_mut() {
            *v /= 1_000;
        }
        let run_delay = self.schedstat_readable.then_some(run_delay);
        (std::mem::take(&mut self.pending), top, run_delay)
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
/// (異常の見張りと `tunnel_spin_test` が使うので残す。`/profile` は [`parse_stat_cpu_split_us`])
pub fn parse_stat_cpu_us(text: &str) -> Option<u64> {
    parse_stat_cpu_split_us(text).map(|(total, _)| total)
}

/// `/proc/self/stat` の (utime + stime, utime) (us)。読めなければ `None` (T16.0)。
pub fn process_cpu_split_us() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/self/stat").ok()?;
    parse_stat_cpu_split_us(&text)
}

/// `/proc/<pid>/stat` の (utime + stime, utime) を us で返す (T16.0)。
///
/// **持つのはユーザー空間だけ** (カーネル側は合計から引けば出る)。
pub fn parse_stat_cpu_split_us(text: &str) -> Option<(u64, u64)> {
    let rest = &text[text.rfind(')')? + 1..];
    let mut it = rest.split_whitespace();
    // 最後の ')' の次は state (3 番目の項目) なので、utime は 11 個先
    let utime: u64 = it.nth(11)?.parse().ok()?;
    let stime: u64 = it.next()?.parse().ok()?;
    let tick_us = 1_000_000 / clock_tick();
    Some(((utime + stime) * tick_us, utime * tick_us))
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
                    s.sample(metrics.profile());
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
        // 上位のスレッドが 1 本も無い窓も `0` 1 文字、`schedstat` が読めなければ `null`
        assert_eq!(
            row,
            "[7,0,0,[0,0,0,0,0,0,0],[0,0,0,0,0,0],[0,0,0,0,0,0,0,0,0],[0,0,0,0],[0,0,0],0,null,[0,0,0,0,0,0,0,0,0],0]"
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

    /// `rows_within_page` は「新しい方から `offset` 本飛ばして `n` 本」(T15.0 (11))。
    ///
    /// 環は時刻で引けないので頁は本数で切る。縛るのは 3 つ: **返す並びは古い順**のまま、
    /// 頁を継いでいくと**全部が重複なく揃う**、`n` で切れたぶんは打ち切りではない
    /// (バイト数で切れたときだけ `cut`)。
    #[test]
    fn rows_can_be_paged_from_the_newest_end() {
        let p = Profile::default();
        for i in 0..50u64 {
            p.push(Sample {
                t: 1_000_000 + i * 5,
                requests: i,
                ..Sample::default()
            });
        }
        // 1 頁目 = いちばん新しい 10 本 (t は 1000200..1000245)。並びは古い順
        let (page1, shown, total, cut) = p.rows_within_page(0, 1 << 20, 10, 0);
        assert_eq!(
            (shown, total, cut),
            (10, 50, false),
            "n で切るのは打ち切りではない"
        );
        assert!(page1.starts_with("[1000200,40,"), "{}", &page1[..24]);
        assert!(page1.contains("[1000245,49,"), "{}", page1);
        assert!(
            !page1.contains("[1000195,39,"),
            "11 本目が入っている: {}",
            page1
        );

        // 2 頁目 = その次の 10 本
        let (page2, shown2, _, _) = p.rows_within_page(0, 1 << 20, 10, 10);
        assert_eq!(shown2, 10);
        assert!(page2.starts_with("[1000150,30,"), "{}", &page2[..24]);
        assert!(page2.contains("[1000195,39,"), "{}", page2);

        // 5 頁で 50 本が重複なく揃う (1 標本の目印は先頭の `[t,requests,`。
        // 入れ子の配列は 0 埋めの窓なので、この綴りとは当たらない)
        let mut seen: Vec<u64> = Vec::new();
        for page in 0..5 {
            let (rows, n, _, _) = p.rows_within_page(0, 1 << 20, 10, page * 10);
            assert_eq!(n, 10, "{} 頁目", page + 1);
            for i in 0..50u64 {
                if rows.contains(&format!("[{},{},", 1_000_000 + i * 5, i)) {
                    seen.push(i);
                }
            }
        }
        assert_eq!(seen.len(), 50, "同じ標本が 2 つの頁に出た: {:?}", seen);
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 50, "欠けがある");

        // 環の外を指したら空 (エラーにはしない)
        let (empty, shown3, total3, cut3) = p.rows_within_page(0, 1 << 20, 10, 50);
        assert_eq!((empty.as_str(), shown3, total3, cut3), ("", 0, 50, false));

        // 予算はページ 1 枚に効く (飛ばしたぶんは数えない)
        let (tight, shown4, _, cut4) = p.rows_within_page(0, 120, 10, 10);
        assert!(shown4 < 10 && shown4 > 0, "書けた件数: {}", shown4);
        assert!(cut4, "バイト数で切れたら打ち切り");
        assert!(tight.contains("[1000195,39,"), "{}", tight);

        // `rows_within` は今までどおり (= 頁を切らない呼び方と同じ)
        let (all, shown5, _, _) = p.rows_within(0, 1 << 20);
        let (same, shown6, _, _) = p.rows_within_page(0, 1 << 20, usize::MAX, 0);
        assert_eq!((all, shown5), (same, shown6));
    }

    /// `/proc/<pid>/stat` の comm に空白と括弧が入っていても utime / stime を読める。
    #[test]
    fn the_stat_line_is_parsed_after_the_last_paren() {
        let mut line = String::from("42 (weird ) name) S 1 42 42 0 -1 4194304 0 0 0 0 ");
        line.push_str("111 222 0 0 20 0 8 0 100 0 0");
        let us = parse_stat_cpu_us(&line).expect("読めること");
        assert_eq!(us, (111 + 222) * (1_000_000 / clock_tick()));
        // ユーザー空間は utime (項目 14) だけ。合計は `parse_stat_cpu_us` と同じ (T16.0)
        let (total, user) = parse_stat_cpu_split_us(&line).expect("読めること");
        assert_eq!(total, us);
        assert_eq!(user, 111 * (1_000_000 / clock_tick()));
        assert_eq!(parse_stat_cpu_split_us("42 (x) S 1"), None);
        assert_eq!(parse_stat_cpu_split_us("括弧が無い"), None);
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
        // `schedstat` は `"run_time_ns wait_time_ns nr_timeslices"` の 1 行 (`None` で置かない)
        let write =
            |tid: u32, comm: &str, ticks: u64, syscall: Option<&str>, delay_ns: Option<u64>| {
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
                match delay_ns {
                    Some(ns) => {
                        std::fs::write(d.join("schedstat"), format!("999 {} 3\n", ns)).unwrap()
                    }
                    None => {
                        let _ = std::fs::remove_file(d.join("schedstat"));
                    }
                }
            };
        write(1, "proxy", 10, Some("running\n"), Some(1_000));
        write(
            2,
            "conn",
            20,
            Some(format!("{} 0x0\n", SYSCALL_NRS[5]).as_str()),
            Some(5_000),
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
        // 走れずに待った時間も「はじめて見たスレッド」は累計がそのまま増分。
        // **溜めは ns のまま** (us へ落とすのは `take` で 1 度だけ)
        assert_eq!(s.pending_run_delay[0], 1_000);
        assert_eq!(s.pending_run_delay[1], 5_000);
        // 2 回目: 5 tick だけ進め、待ち時間も 3,000 ns 進める
        write(
            2,
            "conn",
            25,
            Some(format!("{} 0x0\n", SYSCALL_NRS[5]).as_str()),
            Some(8_000),
        );
        s.sample(&p);
        let delta = s.pending[1].cpu_us - first;
        assert_eq!(delta, 5 * (1_000_000 / clock_tick()), "増分だけ積む");
        assert_eq!(
            s.pending_run_delay[1],
            5_000 + 3_000,
            "待ち時間も増分だけ積む"
        );
        // `syscall` が無ければ partial
        write(2, "conn", 25, None, Some(8_000));
        write(1, "proxy", 10, None, Some(1_000));
        s.sample(&p);
        assert_eq!(p.sampler(), SamplerState::Partial);
        assert!(s.pending[1].states[names.iter().position(|n| *n == "sleeping").unwrap()] >= 1);
        // 取り出したら 0 に戻る
        let (taken, top, run_delay) = s.take();
        assert!(taken.iter().any(|r| r.samples > 0));
        assert!(s.pending.iter().all(|r| r.samples == 0));
        // 上位のスレッドは CPU の多い順 (conn の tid 2 が先)
        assert_eq!(top[0].tid, 2);
        assert_eq!(top[0].comm_str(), "conn");
        assert_eq!(ROLES[top[0].role as usize], "conn");
        assert_eq!(top[1].tid, 1);
        assert_eq!(top[2], TopThread::default(), "残りは空の枠");
        assert_eq!(top[0].running, 0, "conn は splice の中に居た");
        assert_eq!(top[1].running, 2, "主スレッドは 1・2 回目が running");
        // 役割ごとの待ち時間 (accept 1 us / conn 8 us) が取り出せ、次の窓は 0 から
        let d = run_delay.expect("schedstat が読めている");
        assert_eq!(d[0], 1);
        assert_eq!(d[1], 8);
        assert!(s.pending_run_delay.iter().all(|v| *v == 0));
        assert!(s.pending_top.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **1 us 未満の待ちが標本ごとに消えないこと** (T15.0 単位 3 のレビュー)。
    ///
    /// 溜めを us で持つと 600 ns の増分が毎回 0 に落ち、窓じゅう「待たされていない」と
    /// 読めてしまう。ns で積んで [`Sampler::take`] で 1 度だけ割るので 3 回で 1 us になる。
    /// **名前も標本ごとに書き直す** (生まれたばかりのスレッドは親の `comm` を名乗る)。
    #[test]
    fn sub_microsecond_run_delays_add_up_and_the_name_is_refreshed() {
        let dir = std::env::temp_dir().join(format!("t150-subus-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let write = |comm: &str, delay_ns: u64| {
            let d = dir.join("1");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("stat"),
                format!(
                    "1 ({}) R 1 1 0 0 -1 0 0 0 0 0 0 0 0 0 20 0 8 0 100 0\n",
                    comm
                ),
            )
            .unwrap();
            std::fs::write(d.join("schedstat"), format!("999 {} 3\n", delay_ns)).unwrap();
        };
        let p = Profile::default();
        let mut s = Sampler::new(dir.clone(), 1);
        // 1 本目は「親の名前」、増分は 600 ns
        write("parent-name", 600);
        s.sample(&p);
        // 2・3 本目は本当の名前で、増分はそれぞれ 600 ns (合計 1,800 ns = 1 us)
        write("real-name", 1_200);
        s.sample(&p);
        write("real-name", 1_800);
        s.sample(&p);
        let (_, top, run_delay) = s.take();
        let d = run_delay.expect("schedstat が読めている");
        assert_eq!(d[0], 1, "600 ns × 3 が丸ごと消えている: {:?}", d);
        assert_eq!(
            top[0].comm_str(),
            "real-name",
            "名前がはじめて見たときのまま固まっている"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **役割ごとのユーザー空間**が utime の増分だけで積まれ、合計 (utime + stime) と
    /// 別に読めること (T16.0)。行の末尾は `[user_us...],cpu_user_us`。
    #[test]
    fn the_user_time_is_split_per_role() {
        let dir = std::env::temp_dir().join(format!("t160-user-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let write = |utime: u64, stime: u64| {
            let d = dir.join("2");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("stat"),
                format!(
                    "2 (conn) S 1 1 0 0 -1 0 0 0 0 0 {} {} 0 0 20 0 8 0 100 0\n",
                    utime, stime
                ),
            )
            .unwrap();
        };
        let tick = 1_000_000 / clock_tick();
        let p = Profile::default();
        let mut s = Sampler::new(dir.clone(), 1);
        write(7, 13);
        s.sample(&p);
        // はじめて見たスレッドは累計がそのまま増分 (CPU と同じ流儀)
        assert_eq!(s.pending[1].cpu_us, 20 * tick);
        assert_eq!(s.pending[1].user_us, 7 * tick);
        // 2 回目: user 3 tick、kernel 7 tick 進める
        write(10, 20);
        s.sample(&p);
        assert_eq!(s.pending[1].cpu_us, 30 * tick);
        assert_eq!(s.pending[1].user_us, 10 * tick, "user は utime の増分だけ");
        let (threads, _, _) = s.take();
        assert_eq!(threads[1].user_us, 10 * tick);
        assert!(s.pending.iter().all(|r| r.user_us == 0), "取り出したら 0");
        // 60 秒へ畳むと足し合わせ、行の末尾に役割の順で出る
        let mut a = Sample {
            threads,
            cpu_us: 30 * tick,
            cpu_user_us: 10 * tick,
            ..Sample::default()
        };
        let b = a;
        a.merge_into(&b);
        assert_eq!(a.threads[1].user_us, 20 * tick);
        assert_eq!(a.cpu_user_us, 20 * tick);
        let mut row = String::new();
        a.push_row(&mut row);
        assert!(
            row.ends_with(&format!(
                ",null,[0,{},0,0,0,0,0,0,0],{}]",
                20 * tick,
                20 * tick
            )),
            "{}",
            row
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `schedstat` が読めない環境では `run_delay_us` が `null` になること (受け入れ基準)。
    #[test]
    fn the_run_delay_is_null_without_schedstat() {
        let dir = std::env::temp_dir().join(format!("t150-noschedstat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("1")).unwrap();
        std::fs::write(
            dir.join("1/stat"),
            "1 (proxy) R 1 1 0 0 -1 0 0 0 0 0 10 0 0 0 20 0 8 0 100 0\n",
        )
        .unwrap();
        let p = Profile::default();
        let mut s = Sampler::new(dir.clone(), 1);
        s.sample(&p);
        let (_, top, run_delay) = s.take();
        assert_eq!(run_delay, None, "schedstat が無ければ null");
        assert_eq!(top[0].tid, 1, "上位のスレッドは読めている");
        // JSON も `null` で書かれること (dashboard は位置で開くので数を変えない)
        let mut row = String::new();
        Sample {
            threads_top: top,
            run_delay_us: run_delay,
            ..Sample::default()
        }
        .push_row(&mut row);
        assert!(row.ends_with(",null,[0,0,0,0,0,0,0,0,0],0]"), "{}", row);
        assert!(row.contains("[[1,\"proxy\",0,"), "{}", row);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空の窓は `threads_top` が `0` 1 文字、`run_delay_us` は配列のまま。
    #[test]
    fn an_empty_window_writes_a_bare_zero() {
        let mut row = String::new();
        Sample {
            run_delay_us: Some([0; ROLES.len()]),
            ..Sample::default()
        }
        .push_row(&mut row);
        assert!(
            row.ends_with(",0,[0,0,0,0,0,0,0,0,0],[0,0,0,0,0,0,0,0,0],0]"),
            "{}",
            row
        );
    }

    /// 60 秒へ畳むとき、上位のスレッドは **tid を鍵に**足して上位 8 に切り直す。
    #[test]
    fn the_top_threads_merge_by_tid() {
        let t = |tid: u32, cpu_us: u64| TopThread {
            tid,
            cpu_us,
            running: 1,
            role: 1,
            comm: TopThread::comm_bytes("conn"),
        };
        let mut a = Sample {
            threads_top: [
                t(1, 10),
                t(2, 5),
                t(3, 4),
                t(4, 3),
                t(5, 2),
                t(6, 1),
                t(7, 1),
                t(8, 1),
            ],
            run_delay_us: Some([1; ROLES.len()]),
            ..Sample::default()
        };
        let b = Sample {
            threads_top: [
                t(9, 9),
                t(2, 100),
                t(3, 1),
                t(4, 1),
                t(5, 1),
                t(6, 1),
                t(7, 1),
                t(8, 1),
            ],
            run_delay_us: Some([2; ROLES.len()]),
            ..Sample::default()
        };
        a.merge_into(&b);
        // tid 2 は 5 + 100 = 105 で首位、同じ tid が 2 度出ない
        assert_eq!(a.threads_top[0].tid, 2);
        assert_eq!(a.threads_top[0].cpu_us, 105);
        assert_eq!(a.threads_top[0].running, 2);
        assert_eq!(a.threads_top[1].tid, 1);
        let mut tids: Vec<u32> = a.threads_top.iter().map(|x| x.tid).collect();
        tids.sort_unstable();
        tids.dedup();
        assert_eq!(tids.len(), TOP_THREADS, "8 本とも別のスレッド");
        assert_eq!(a.run_delay_us, Some([3; ROLES.len()]));
        // 片方しか読めていない窓は読めた方をそのまま残す
        let mut c = Sample::default();
        c.merge_into(&b);
        assert_eq!(c.run_delay_us, Some([2; ROLES.len()]));
    }

    /// **実機で**、回り続けているスレッドが `threads_top` の先頭に出ること (T15.0 (5))。
    ///
    /// `sample` は 2 回呼ぶ (CPU は前回との差で積むので、1 回目は「生まれてからのぶん」)。
    /// 空回りの犯人を名前で指せるのがこの欄の目的なので `comm` も見る。
    #[cfg(target_os = "linux")]
    #[test]
    fn the_busiest_thread_comes_first_in_threads_top() {
        use std::sync::Arc;

        let stop = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&stop);
        let spinner = thread::Builder::new()
            .name("t150-spin".into())
            .spawn(move || {
                // 最長 2 秒回し続ける (1 clock tick = 10 ms なので 200 tick ぶん)
                let until = Instant::now() + Duration::from_secs(2);
                let mut x = 0u64;
                while Instant::now() < until && !s2.load(Ordering::Relaxed) {
                    for _ in 0..10_000 {
                        x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    }
                }
                std::hint::black_box(x);
            })
            .expect("spawn t150-spin");

        let p = Profile::default();
        let mut s = Sampler::for_self();
        s.sample(&p);
        thread::sleep(Duration::from_millis(500));
        s.sample(&p);
        stop.store(true, Ordering::Relaxed);
        let (_, top, run_delay) = s.take();
        assert_eq!(
            top[0].comm_str(),
            "t150-spin",
            "回り続けているスレッドが先頭に出ない: {:?}",
            top.iter()
                .map(|t| (t.tid, t.comm_str(), t.cpu_us))
                .collect::<Vec<_>>()
        );
        assert!(top[0].tid != 0 && top[0].tid != std::process::id());
        assert!(top[0].cpu_us > 0);
        assert!(top[0].running > 0, "走っている標本が 1 つも無い");
        // `schedstat` は `CONFIG_SCHEDSTATS` の無いカーネルでは**ファイルごと無い**
        // (この開発機がそれ。受け入れ基準の「読めない環境で `null`」はここで通る)
        let has_schedstat = std::path::Path::new("/proc/thread-self/schedstat").exists();
        assert_eq!(
            run_delay.is_some(),
            has_schedstat,
            "schedstat の読める / 読めないと `run_delay_us` が食い違う"
        );
        let _ = spinner.join();
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

    /// 1 標本の大きさ (窓のメモリ = これ × (720 + 1,440))。
    ///
    /// **数字を doc に書き写さない**ため、知りたいときはこのテストを `-- --nocapture` で
    /// 回す。縛るのは上限だけ (増えたことに気づけるように)。
    #[test]
    fn a_sample_stays_small_enough_for_the_rings() {
        let bytes = std::mem::size_of::<Sample>();
        eprintln!(
            "size_of::<Sample>() = {} B、満杯の窓 = {} B ({:.2} MiB)",
            bytes,
            capacity_bytes(),
            capacity_bytes() as f64 / 1_048_576.0
        );
        assert!(bytes <= 4096, "1 標本が大きすぎる: {} B", bytes);
        assert_eq!(sample_bytes(), bytes);
        assert_eq!(
            capacity_bytes(),
            (RESOLUTIONS[0].1 + RESOLUTIONS[1].1) * bytes
        );
    }

    /// `used_bytes` は**いま入っているぶん**なので、標本を積むと増える (T15.0 (13))。
    ///
    /// 「起動直後と 1 時間後で増える」は待てないので、`push` を 2 回して見る。
    /// 満杯の見積もり ([`capacity_bytes`]) は動かない。
    #[test]
    fn used_bytes_grows_with_the_samples_and_never_passes_the_capacity() {
        let p = Profile::default();
        assert_eq!(p.used_bytes(), 0, "`--lite` と起動直後は 1 本も無い");
        p.push(Sample {
            t: 1_700_000_000,
            requests: 1,
            ..Sample::default()
        });
        let one = p.used_bytes();
        assert_eq!(one, sample_bytes(), "5 秒の環に 1 本");
        p.push(Sample {
            t: 1_700_000_005,
            requests: 2,
            ..Sample::default()
        });
        assert!(p.used_bytes() > one, "{} -> {}", one, p.used_bytes());
        assert!(
            p.used_bytes() <= capacity_bytes(),
            "{} > {}",
            p.used_bytes(),
            capacity_bytes()
        );
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
