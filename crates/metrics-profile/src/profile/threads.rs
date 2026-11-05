//! スレッドの役割と状態の表 (`/proc/<pid>/task/<tid>/{stat,syscall}` の読みを枠に落とす)。

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

/// 番号が分かるシステムコールの名前 (`SYSCALL_NRS` と同じ順)。
pub(crate) const SYSCALL_NAMES: [&str; 19] = [
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

/// `read` の番号 (標本を取るスレッドが自分の `syscall` を読んだときに見える値。T.29.21)。
pub(super) const READ_NR: i64 = {
    let mut i = 0;
    while !matches!(SYSCALL_NAMES[i].as_bytes(), b"read") {
        i += 1;
    }
    SYSCALL_NRS[i]
};

/// aarch64 (asm-generic) のシステムコール番号 ([`SYSCALL_NAMES`] の順)。
#[cfg(target_arch = "aarch64")]
pub(super) const SYSCALL_NRS: [i64; SYSCALL_NAMES.len()] = [
    207, 206, 73, 22, 98, 76, 242, 203, 57, 63, 64, 101, 115, 209, 208, 198, 210, 56, 80,
];

/// x86_64 のシステムコール番号 ([`SYSCALL_NAMES`] の順)。
#[cfg(target_arch = "x86_64")]
pub(super) const SYSCALL_NRS: [i64; SYSCALL_NAMES.len()] = [
    45, 44, 271, 281, 202, 275, 288, 42, 3, 0, 1, 35, 230, 55, 54, 41, 48, 257, 5,
];

/// 表を持っていない機械では名前を出さない (全部 `other` に落ちる)。
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub(super) const SYSCALL_NRS: [i64; SYSCALL_NAMES.len()] = [-1; SYSCALL_NAMES.len()];

/// 同じ枠に数える別の番号 (`(番号, SYSCALL_NAMES の位置)`。T.29.14)。
///
/// x86_64 には `ppoll` / `epoll_pwait` の古い形の `poll` (7) / `epoll_wait` (232) があり、std の
/// 待ちはそちらを使う (aarch64 には無い)。tokyo の `/profile` の `unknown_syscalls` が `sys_7` 129,929・
/// `sys_232` 86,693 標本/24 時間で、idle-watch が `other` 100% に見えていた。**状態の数は増やさず**
/// (標本の形と大きさを変えない) 同じ待ちの枠に入れる
#[cfg(target_arch = "x86_64")]
pub(super) const SYSCALL_ALIASES: [(i64, usize); 2] = [(7, 2), (232, 3)];
#[cfg(not(target_arch = "x86_64"))]
pub(super) const SYSCALL_ALIASES: [(i64, usize); 0] = [];

/// 走行中の枠 ([`state_names`] の先頭)。
pub(super) const STATE_RUNNING: usize = 0;
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
pub(crate) fn role_of(tid: u32, main_tid: u32, comm: &str) -> usize {
    if tid == main_tid {
        return 0;
    }
    ROLES
        .iter()
        .position(|r| *r == comm)
        .filter(|i| *i > 0)
        .unwrap_or(ROLES.len() - 1)
}

/// 標本 1 つを [`state_names`] の枠に落とす。表に無い番号は (`other`, その番号) を返す。
pub(crate) fn state_slot(syscall: Option<i64>, state: char) -> (usize, Option<i64>) {
    match syscall {
        Some(nr) if nr >= 0 => match SYSCALL_NRS.iter().position(|n| *n == nr).or_else(|| {
            SYSCALL_ALIASES
                .iter()
                .find(|(n, _)| *n == nr)
                .map(|(_, i)| *i)
        }) {
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
    pub(super) fn merge(&mut self, o: &RoleWindow) {
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
/// **`String` は入れない** ([`Sample`] が `Copy` を失うと `Profile::roll` の `.copied()` と
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
    pub(super) fn comm_bytes(comm: &str) -> [u8; 16] {
        let mut out = [0u8; 16];
        for (o, b) in out.iter_mut().zip(comm.as_bytes()) {
            *o = *b;
        }
        out
    }

    /// 名前 (`0` 詰めを外し、JSON に出せない文字は `.` に落とす)。
    pub(crate) fn comm_str(&self) -> String {
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
pub(super) fn merge_top(
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
