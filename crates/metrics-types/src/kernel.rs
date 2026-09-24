//! カーネルと cgroup の統計を **5 秒の標本のときだけ**読み、メモリ上の窓に残す (T14.12)。
//!
//! バーストのとき**カーネル側で何が起きていたか**が無かった (T14.0 の 09-11 の山)。
//! ここで残すのは 6 つ:
//!
//! - 受け入れ待ち行列の溢れ (`ListenOverflows` / `ListenDrops`)。溢れるとクライアントは
//!   SYN を 1〜3 秒後に再送するので、**プロキシの統計には「遅い接続」としてすら残らない**
//! - 再送 (`RetransSegs` / `TCPSynRetrans` / `TCPTimeouts` / `TCPAbortOnTimeout`)
//! - TIME_WAIT の本数 (loopback の CONNECT のベンチを律速していたもの。TODO.md §1)
//! - cgroup の CPU の絞り (`cpu.stat` の `nr_periods` / `nr_throttled` / `throttled_usec`。
//!   **割合で読む**ので分母の `nr_periods` も要る。T15.0 (6))
//! - PSI (`cpu.pressure` / `memory.pressure` / `io.pressure` の `some` / `full` の avg10)
//! - 一緒に取ると読みやすいもの: 名前解決のミス (`/healthz` のリゾルバの検査) と
//!   状態ファイルの書込エラー
//!
//! **`.rrd` (状態ファイル) には書かない**: 標本のレコードの余白は 4 B しか残っていない
//! (T14.2 (3))。窓はメモリだけで、5 秒 × 720 (1 時間) と 60 秒 × 1,440 (1 日)。
//! 1 標本 200 B なので合わせて約 420 KiB (環状バッファの伸び方しだいで最大 600 KiB)。
//! 再起動で消えてよい (残したいものは `.rrd` の版を上げる T14.14 で)。
//!
//! 累計のものは**増分**、値のものは値で窓に入れる。読めなかった源の項目は
//! `/status` と `/history` で `null` (Linux 以外、`/proc/net` が無いコンテナ、cgroup v1)。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;

use crate::sync::LockExt;
use crate::sysinfo::cgroup::{cgroup_cpu, cgroup_pressure};
use crate::sysinfo::net::tcp_stats;

/// 解像度 (秒) と本数。5 秒 × 720 = 1 時間、60 秒 × 1,440 = 1 日。
pub const RESOLUTIONS: [(u64, usize); 2] = [(5, 720), (60, 1440)];

/// 読めた源の旗 ([`Sample::avail`] / [`Latest::avail`])。立っていない項目は `null`。
///
/// 8 ビットが埋まったので T16.0 で `u16` に広げた (窓はメモリだけで `.rrd` には書かないので、
/// 幅を変えても読み継ぎの問題は無い)。
pub const SRC_NETSTAT: u16 = 1 << 0;
pub const SRC_SNMP: u16 = 1 << 1;
pub const SRC_SOCKSTAT: u16 = 1 << 2;
pub const SRC_CGROUP_CPU: u16 = 1 << 3;
pub const SRC_PSI_CPU: u16 = 1 << 4;
pub const SRC_PSI_MEM: u16 = 1 << 5;
pub const SRC_PSI_IO: u16 = 1 << 6;
/// 状態ファイルがあるか (無ければ書込エラーの検査は `null`)
pub const SRC_STATE_FILE: u16 = 1 << 7;
/// `cpu.stat` に `usage_usec` / `user_usec` / `system_usec` の行があるか (T16.0)。
/// 無いファイル (古いカーネル) ではこの 3 欄を `null`
pub const SRC_CGROUP_USAGE: u16 = 1 << 8;

/// `/history` の `kernel` の 1 標本の列名 ([`Sample::push_row`] がこの順で並べる)。
pub const KEYS: [&str; 26] = [
    "t",
    // 累計の**増分** (この窓で何回起きたか)
    "listen_overflows",
    "listen_drops",
    "tcp_timeouts",
    "syn_retrans",
    "abort_on_timeout",
    "retrans_segs",
    // 値 (この窓の最後 / 粗い解像度では最大)
    "curr_estab",
    "sockets_inuse",
    "time_wait",
    "sockets_alloc",
    "sockets_mem",
    // cgroup の CPU の絞り (増分)
    "cpu_nr_throttled",
    "cpu_throttled_usec",
    // PSI (値、%)
    "psi_cpu_some_avg10",
    "psi_cpu_full_avg10",
    "psi_mem_some_avg10",
    "psi_mem_full_avg10",
    "psi_io_some_avg10",
    "psi_io_full_avg10",
    // カーネルではないが 5 秒の標本で一緒に取るもの
    "dns_misses",
    "dns_miss_ms",
    "state_file_errors",
    // **末尾に足したもの** (古い読み手は位置で開くので、途中に入れない)。
    // `cpu_nr_throttled` の分母 (T15.0 (6))
    "cpu_nr_periods",
    // cgroup の CPU のユーザー空間とカーネル側 (増分 us。T16.0)
    "cpu_user_usec",
    "cpu_system_usec",
];

/// 窓の 1 標本。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sample {
    pub t: u64,
    pub avail: u16,
    pub listen_overflows: u64,
    pub listen_drops: u64,
    pub tcp_timeouts: u64,
    pub syn_retrans: u64,
    pub abort_on_timeout: u64,
    pub retrans_segs: u64,
    pub curr_estab: u64,
    pub sockets_inuse: u64,
    pub time_wait: u64,
    pub sockets_alloc: u64,
    pub sockets_mem: u64,
    pub cpu_nr_throttled: u64,
    pub cpu_throttled_usec: u64,
    pub psi_cpu_some: f64,
    pub psi_cpu_full: f64,
    pub psi_mem_some: f64,
    pub psi_mem_full: f64,
    pub psi_io_some: f64,
    pub psi_io_full: f64,
    /// この窓に OS へ問い合わせた回数と、その 1 回の平均 ms
    pub dns_misses: u64,
    pub dns_miss_ms: u64,
    /// この窓に増えた状態ファイルの書込エラー
    pub state_file_errors: u64,
    /// この窓に過ぎた CPU の期間の数 (`cpu.stat` の `nr_periods` の増分。T15.0 (6))。
    /// [`Sample::cpu_nr_throttled`] の**分母**
    pub cpu_nr_periods: u64,
    /// この窓に cgroup が使った CPU のユーザー空間とカーネル側 (`cpu.stat` の
    /// `user_usec` / `system_usec` の増分 us。T16.0)
    pub cpu_user_usec: u64,
    pub cpu_system_usec: u64,
}

impl Sample {
    fn has(&self, src: u16) -> bool {
        self.avail & src != 0
    }

    /// 1 標本を配列 1 行として書く ([`KEYS`] の順。読めない源は `null`)。
    fn push_row(&self, out: &mut String) {
        let net = self.has(SRC_NETSTAT);
        let snmp = self.has(SRC_SNMP);
        let sock = self.has(SRC_SOCKSTAT);
        let cpu = self.has(SRC_CGROUP_CPU);
        let _ = write!(out, "[{}", self.t);
        for (v, ok) in [
            (self.listen_overflows, net),
            (self.listen_drops, net),
            (self.tcp_timeouts, net),
            (self.syn_retrans, net),
            (self.abort_on_timeout, net),
            (self.retrans_segs, snmp),
            (self.curr_estab, snmp),
            (self.sockets_inuse, sock),
            (self.time_wait, sock),
            (self.sockets_alloc, sock),
            (self.sockets_mem, sock),
            (self.cpu_nr_throttled, cpu),
            (self.cpu_throttled_usec, cpu),
        ] {
            out.push(',');
            push_u64(out, v, ok);
        }
        for (v, ok) in [
            (self.psi_cpu_some, self.has(SRC_PSI_CPU)),
            (self.psi_cpu_full, self.has(SRC_PSI_CPU)),
            (self.psi_mem_some, self.has(SRC_PSI_MEM)),
            (self.psi_mem_full, self.has(SRC_PSI_MEM)),
            (self.psi_io_some, self.has(SRC_PSI_IO)),
            (self.psi_io_full, self.has(SRC_PSI_IO)),
        ] {
            out.push(',');
            push_f64(out, v, ok);
        }
        let _ = write!(out, ",{},", self.dns_misses);
        // ミスが無かった窓は「1 回の ms」が存在しない
        push_u64(out, self.dns_miss_ms, self.dns_misses > 0);
        out.push(',');
        push_u64(out, self.state_file_errors, self.has(SRC_STATE_FILE));
        // **[`KEYS`] の末尾に足したもの** (T15.0 (6))
        out.push(',');
        push_u64(out, self.cpu_nr_periods, cpu);
        // T16.0 で末尾に足したもの (`user_usec` の行が無い `cpu.stat` では `null`)
        let usage = self.has(SRC_CGROUP_USAGE);
        out.push(',');
        push_u64(out, self.cpu_user_usec, usage);
        out.push(',');
        push_u64(out, self.cpu_system_usec, usage);
        out.push(']');
    }

    /// 窓の標本をひとつにまとめる: 増分は足し合わせ、値と PSI は**最大**
    /// (平均に畳むと山が消える。見たいのはバーストのときの姿)。
    fn downsample(window: &[Sample], t: u64) -> Sample {
        let sum = |f: fn(&Sample) -> u64| window.iter().map(f).sum::<u64>();
        let max = |f: fn(&Sample) -> u64| window.iter().map(f).max().unwrap_or(0);
        let fmax = |f: fn(&Sample) -> f64| window.iter().map(f).fold(0.0f64, f64::max);
        let misses = sum(|s| s.dns_misses);
        Sample {
            t,
            // 源が読めた標本が 1 つでもあれば、その列は数で出す
            avail: window.iter().fold(0, |a, s| a | s.avail),
            listen_overflows: sum(|s| s.listen_overflows),
            listen_drops: sum(|s| s.listen_drops),
            tcp_timeouts: sum(|s| s.tcp_timeouts),
            syn_retrans: sum(|s| s.syn_retrans),
            abort_on_timeout: sum(|s| s.abort_on_timeout),
            retrans_segs: sum(|s| s.retrans_segs),
            curr_estab: max(|s| s.curr_estab),
            sockets_inuse: max(|s| s.sockets_inuse),
            time_wait: max(|s| s.time_wait),
            sockets_alloc: max(|s| s.sockets_alloc),
            sockets_mem: max(|s| s.sockets_mem),
            cpu_nr_throttled: sum(|s| s.cpu_nr_throttled),
            cpu_throttled_usec: sum(|s| s.cpu_throttled_usec),
            psi_cpu_some: fmax(|s| s.psi_cpu_some),
            psi_cpu_full: fmax(|s| s.psi_cpu_full),
            psi_mem_some: fmax(|s| s.psi_mem_some),
            psi_mem_full: fmax(|s| s.psi_mem_full),
            psi_io_some: fmax(|s| s.psi_io_some),
            psi_io_full: fmax(|s| s.psi_io_full),
            dns_misses: misses,
            // ミスの回数で重みを付けた平均 (窓ごとの「1 回の値段」を薄めない)。
            // ミスが 1 回も無い窓は `checked_div` が `None` を返すので 0
            dns_miss_ms: window
                .iter()
                .map(|s| s.dns_miss_ms * s.dns_misses)
                .sum::<u64>()
                .checked_div(misses)
                .unwrap_or(0),
            state_file_errors: sum(|s| s.state_file_errors),
            // 増分なので足し合わせ (T15.0 (6))
            cpu_nr_periods: sum(|s| s.cpu_nr_periods),
            // 同じく増分 (T16.0)
            cpu_user_usec: sum(|s| s.cpu_user_usec),
            cpu_system_usec: sum(|s| s.cpu_system_usec),
        }
    }
}

/// 直近の標本で読めた**生の値** (カーネルの累計や今の値そのまま)。`/status` と `/metrics` 用。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Latest {
    /// この値を読んだ時刻 (epoch 秒)
    pub at: u64,
    pub avail: u16,
    pub listen_overflows: u64,
    pub listen_drops: u64,
    pub tcp_timeouts: u64,
    pub syn_retrans: u64,
    pub abort_on_timeout: u64,
    pub retrans_segs: u64,
    pub curr_estab: u64,
    pub sockets_inuse: u64,
    pub time_wait: u64,
    pub sockets_alloc: u64,
    pub sockets_mem: u64,
    pub cpu_nr_throttled: u64,
    pub cpu_throttled_usec: u64,
    /// `cpu.stat` の `nr_periods` の累計 (T15.0 (6))。絞られた**割合**の分母
    pub cpu_nr_periods: u64,
    /// `cpu.stat` の `usage_usec` / `user_usec` / `system_usec` の累計 (T16.0。
    /// [`SRC_CGROUP_USAGE`] が立っているときだけ意味がある)
    pub cpu_usage_usec: u64,
    pub cpu_user_usec: u64,
    pub cpu_system_usec: u64,
    /// `cpu.max` の quota ÷ period (0 = 無制限か読めない)
    pub cpu_quota_cores: f64,
    pub psi_cpu_some: f64,
    pub psi_cpu_full: f64,
    pub psi_mem_some: f64,
    pub psi_mem_full: f64,
    pub psi_io_some: f64,
    pub psi_io_full: f64,
    /// 名前解決のミスの累計と、その合計 us (増分を出すために持つ)
    pub dns_misses: u64,
    pub dns_us_sum: u64,
    /// 状態ファイルの書込エラーの累計
    pub state_file_errors: u64,
}

impl Latest {
    pub fn has(&self, src: u16) -> bool {
        self.avail & src != 0
    }
}

/// `/healthz` の検査に渡す値 (どれも読めなければ `None` = その検査をしない)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Health {
    /// 直近 5 分に受け入れ待ち行列が溢れた回数
    pub listen_overflows_5m: Option<u64>,
    /// 直近 5 分に増えた状態ファイルの書込エラー
    pub state_file_errors_5m: Option<u64>,
    /// 直近 5 分でいちばん新しい標本の「名前解決のミス 1 回の ms」
    pub dns_miss_ms: Option<u64>,
    /// 直近 5 分の cgroup の CPU の絞り `(nr_throttled の増分, nr_periods の増分)`
    /// (T15.0 (6))。**割合は呼ぶ側が割る** (分母が 0 の窓を型で持たないため)
    pub cpu_throttled_5m: Option<(u64, u64)>,
    /// **いちばん最初に読んだ標本からの秒** (T15.0 (6))。まだ 1 本も無ければ `None`。
    ///
    /// 上の 5 分の値は「いちばん新しい標本から 300 秒ぶん」を足しただけなので、
    /// 起動 10 秒後 (標本 2 本) でもその 5 秒ぶんの割合が出てしまう。**5 分に
    /// 満たない窓で割合を出さない**ための物差しがこれで、異常の規則と同じ
    /// 関数 (`anomaly::cpu_window_is_full`) を通す
    pub sampled_secs: Option<u64>,
}

impl Health {
    /// 直近 5 分に絞られた期間の割合 (0.0〜1.0)。読めないか期間が 0 なら `None`。
    pub fn cpu_throttled_ratio(&self) -> Option<f64> {
        match self.cpu_throttled_5m {
            Some((throttled, periods)) if periods > 0 => Some(throttled as f64 / periods as f64),
            _ => None,
        }
    }
}

#[derive(Default)]
struct State {
    rings: [VecDeque<Sample>; 2],
    /// 前回の生の値 (増分を出すため)。まだ 1 回も読んでいなければ `None`
    last: Option<Latest>,
    /// **いちばん最初に読んだ生の値** (T15.0 (6))。`/status` の `since_start` は
    /// 「いまの累計 − これ」で出す。起動直後の雪像に `nr_throttled` 915 が
    /// 乗っていたのは、累計が**このプロセスより前から**動いている親の階層の値だから
    first: Option<Latest>,
}

impl State {
    const fn new() -> State {
        State {
            rings: [VecDeque::new(), VecDeque::new()],
            last: None,
            first: None,
        }
    }
}

static STATE: Mutex<State> = Mutex::new(State::new());

/// 状態ファイルの書込エラーの数を読む口 (**上の層 `persist` が状態ファイルを開いたときに
/// 1 回だけ預ける**。T14.55)。
///
/// クレートを割る前はここから `persist::write_errors()` を直に呼んでいたが、
/// 状態ファイルはいちばん上の層なので下のここからは呼べない。**預ける前は `None`** で、
/// これは `persist::write_errors()` が状態ファイルを開く前に返していた値と同じ。
static STATE_FILE_ERRORS: std::sync::OnceLock<fn() -> Option<u64>> = std::sync::OnceLock::new();

/// 上の層が読み口を預ける (`persist` が状態ファイルを開いた 1 回だけ)。
pub fn set_state_file_errors(f: fn() -> Option<u64>) {
    let _ = STATE_FILE_ERRORS.set(f);
}

/// 5 秒の標本のときに 1 回だけ呼ぶ (`/proc` を 3 つ + cgroup を 4 つ読む)。
///
/// **要求ごとには呼ばないこと。** 呼ぶのは `history` の記録スレッドだけで、
/// `--lite` や `PROXY_STATS_PERSIST=off` では記録スレッド自体が動かないので窓は空になる。
pub fn sample(t: u64) {
    let now = read_now(t);
    let mut st = STATE.locked();
    let sample = match st.last {
        Some(prev) => diff(&prev, &now),
        // 1 本目は増分が出せない (前が無い) ので、値だけ入れて増分は 0
        None => diff(&now, &now),
    };
    // 起動からの増分を出すための最初の 1 本 (T15.0 (6))
    if st.first.is_none() {
        st.first = Some(now);
    }
    st.last = Some(now);
    push(&mut st, sample);
}

/// いまの生の値を読む (源ごとに読めたかを旗で持つ)。
fn read_now(t: u64) -> Latest {
    let tcp = tcp_stats();
    let cpu = cgroup_cpu();
    let psi = cgroup_pressure();
    let (dns_us_sum, dns_misses) = crate::dns::resolve_cost_total();
    let state_file = STATE_FILE_ERRORS.get().and_then(|f| f());
    let mut l = Latest {
        at: t,
        dns_misses,
        dns_us_sum,
        ..Latest::default()
    };
    if let Some(e) = tcp.ext {
        l.avail |= SRC_NETSTAT;
        l.listen_overflows = e.listen_overflows;
        l.listen_drops = e.listen_drops;
        l.tcp_timeouts = e.tcp_timeouts;
        l.syn_retrans = e.syn_retrans;
        l.abort_on_timeout = e.abort_on_timeout;
    }
    if let Some(s) = tcp.snmp {
        l.avail |= SRC_SNMP;
        l.retrans_segs = s.retrans_segs;
        l.curr_estab = s.curr_estab;
    }
    if let Some(s) = tcp.sock {
        l.avail |= SRC_SOCKSTAT;
        l.sockets_inuse = s.inuse;
        l.time_wait = s.tw;
        l.sockets_alloc = s.alloc;
        l.sockets_mem = s.mem;
    }
    // cgroup v2 で cpu.stat が読めたときだけ (v1 と読めない環境は `null`)
    if cpu.nr_throttled.is_some() || cpu.throttled_usec.is_some() || cpu.nr_periods.is_some() {
        l.avail |= SRC_CGROUP_CPU;
        l.cpu_nr_throttled = cpu.nr_throttled.unwrap_or(0);
        l.cpu_throttled_usec = cpu.throttled_usec.unwrap_or(0);
        l.cpu_nr_periods = cpu.nr_periods.unwrap_or(0);
        // 同じファイルの `usage_usec` / `user_usec` / `system_usec` (T16.0)。
        // カーネルは 3 行を揃えて書くので、3 つとも読めたときだけ数で出す
        if let (Some(usage), Some(user), Some(system)) =
            (cpu.usage_usec, cpu.user_usec, cpu.system_usec)
        {
            l.avail |= SRC_CGROUP_USAGE;
            l.cpu_usage_usec = usage;
            l.cpu_user_usec = user;
            l.cpu_system_usec = system;
        }
    }
    l.cpu_quota_cores = cpu.quota_cores.unwrap_or(0.0);
    if let Some(p) = psi.cpu {
        l.avail |= SRC_PSI_CPU;
        l.psi_cpu_some = p.some_avg10;
        l.psi_cpu_full = p.full_avg10;
    }
    if let Some(p) = psi.memory {
        l.avail |= SRC_PSI_MEM;
        l.psi_mem_some = p.some_avg10;
        l.psi_mem_full = p.full_avg10;
    }
    if let Some(p) = psi.io {
        l.avail |= SRC_PSI_IO;
        l.psi_io_some = p.some_avg10;
        l.psi_io_full = p.full_avg10;
    }
    if let Some(n) = state_file {
        l.avail |= SRC_STATE_FILE;
        l.state_file_errors = n;
    }
    l
}

/// 前の生の値との差から 1 標本を作る (累計は増分、値はそのまま)。
///
/// カウンタが減っていたら (カーネルの折り返し・`/proc` の入れ替え) 0 にする。
fn diff(prev: &Latest, now: &Latest) -> Sample {
    let d = |a: u64, b: u64| a.saturating_sub(b);
    let misses = d(now.dns_misses, prev.dns_misses);
    Sample {
        t: now.at,
        avail: now.avail,
        listen_overflows: d(now.listen_overflows, prev.listen_overflows),
        listen_drops: d(now.listen_drops, prev.listen_drops),
        tcp_timeouts: d(now.tcp_timeouts, prev.tcp_timeouts),
        syn_retrans: d(now.syn_retrans, prev.syn_retrans),
        abort_on_timeout: d(now.abort_on_timeout, prev.abort_on_timeout),
        retrans_segs: d(now.retrans_segs, prev.retrans_segs),
        curr_estab: now.curr_estab,
        sockets_inuse: now.sockets_inuse,
        time_wait: now.time_wait,
        sockets_alloc: now.sockets_alloc,
        sockets_mem: now.sockets_mem,
        cpu_nr_throttled: d(now.cpu_nr_throttled, prev.cpu_nr_throttled),
        cpu_throttled_usec: d(now.cpu_throttled_usec, prev.cpu_throttled_usec),
        cpu_nr_periods: d(now.cpu_nr_periods, prev.cpu_nr_periods),
        cpu_user_usec: d(now.cpu_user_usec, prev.cpu_user_usec),
        cpu_system_usec: d(now.cpu_system_usec, prev.cpu_system_usec),
        psi_cpu_some: now.psi_cpu_some,
        psi_cpu_full: now.psi_cpu_full,
        psi_mem_some: now.psi_mem_some,
        psi_mem_full: now.psi_mem_full,
        psi_io_some: now.psi_io_some,
        psi_io_full: now.psi_io_full,
        dns_misses: misses,
        // ミスのあった窓だけ「1 回の ms」が出る (0 回の窓は `checked_div` が `None`)
        dns_miss_ms: d(now.dns_us_sum, prev.dns_us_sum)
            .checked_div(misses)
            .map_or(0, |us| (us + 500) / 1000),
        state_file_errors: d(now.state_file_errors, prev.state_file_errors),
    }
}

/// 5 秒の環へ 1 本足し、1 分の窓が閉じていればそれも作る ([`crate::history`] と同じ形)。
fn push(st: &mut State, s: Sample) {
    append(&mut st.rings[0], s, RESOLUTIONS[0].1);
    let step = RESOLUTIONS[1].0;
    let window_start = (s.t / step) * step;
    let Some(prev) = window_start.checked_sub(step) else {
        return;
    };
    if st.rings[1].back().is_some_and(|m| m.t >= prev) {
        return;
    }
    let window: Vec<Sample> = st.rings[0]
        .iter()
        .filter(|w| w.t >= prev && w.t < window_start)
        .copied()
        .collect();
    if !window.is_empty() {
        let agg = Sample::downsample(&window, prev);
        append(&mut st.rings[1], agg, RESOLUTIONS[1].1);
    }
}

fn append(ring: &mut VecDeque<Sample>, s: Sample, cap: usize) {
    if ring.len() >= cap {
        ring.pop_front();
    }
    ring.push_back(s);
}

/// 直近の生の値 (まだ 1 回も読んでいなければ `None`)。
pub fn latest() -> Option<Latest> {
    STATE.locked().last
}

/// **起動して最初に**読んだ生の値 (T15.0 (6))。まだ 1 回も読んでいなければ `None`。
///
/// cgroup の累計はプロセスより長生き (親の階層の値) なので、`nr_throttled` の
/// 生の値だけでは「自分が絞られたか」が分からない。引き算の相手がこれ。
pub fn first() -> Option<Latest> {
    STATE.locked().first
}

/// `/healthz` の検査に渡す直近 5 分の値。
pub fn health() -> Health {
    let st = STATE.locked();
    let ring = &st.rings[0];
    let Some(newest) = ring.back().map(|s| s.t) else {
        return Health::default();
    };
    let since = newest.saturating_sub(300);
    let recent = ring.iter().filter(|s| s.t >= since);
    let mut h = Health {
        // 起動して最初に読んだ標本からの秒 (T15.0 (6))。窓が 5 分ぶん溜まったかの物差し
        sampled_secs: st.first.map(|f| newest.saturating_sub(f.at)),
        ..Health::default()
    };
    for s in recent {
        if s.has(SRC_NETSTAT) {
            h.listen_overflows_5m = Some(h.listen_overflows_5m.unwrap_or(0) + s.listen_overflows);
        }
        if s.has(SRC_STATE_FILE) {
            h.state_file_errors_5m =
                Some(h.state_file_errors_5m.unwrap_or(0) + s.state_file_errors);
        }
        // 絞られた期間と過ぎた期間の増分 (T15.0 (6))。割合は読む側が割る
        if s.has(SRC_CGROUP_CPU) {
            let (throttled, periods) = h.cpu_throttled_5m.unwrap_or((0, 0));
            h.cpu_throttled_5m = Some((throttled + s.cpu_nr_throttled, periods + s.cpu_nr_periods));
        }
        // いちばん新しい「ミスのあった窓」の値 (古い順に見るので最後の 1 つが残る)
        if s.dns_misses > 0 {
            h.dns_miss_ms = Some(s.dns_miss_ms);
        }
    }
    h
}

/// `/status` の `kernel` の節 (最新の値と累計、直近 5 分の増分)。まだ標本が無ければ `"null"`。
pub fn status_json() -> String {
    let Some(l) = latest() else {
        return "null".to_string();
    };
    let h = health();
    let (net, snmp, sock) = (l.has(SRC_NETSTAT), l.has(SRC_SNMP), l.has(SRC_SOCKSTAT));
    let mut out = String::with_capacity(768);
    let _ = write!(out, "{{\"at\":{},\"tcp\":{{", l.at);
    for (i, (k, v, ok)) in [
        ("listen_overflows", l.listen_overflows, net),
        ("listen_drops", l.listen_drops, net),
        ("tcp_timeouts", l.tcp_timeouts, net),
        ("syn_retrans", l.syn_retrans, net),
        ("abort_on_timeout", l.abort_on_timeout, net),
        ("retrans_segs", l.retrans_segs, snmp),
        ("curr_estab", l.curr_estab, snmp),
        ("sockets_inuse", l.sockets_inuse, sock),
        ("time_wait", l.time_wait, sock),
        ("sockets_alloc", l.sockets_alloc, sock),
        ("sockets_mem", l.sockets_mem, sock),
    ]
    .into_iter()
    .enumerate()
    {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\":", k);
        push_u64(&mut out, v, ok);
    }
    out.push_str("},\"cgroup_cpu\":");
    if l.has(SRC_CGROUP_CPU) {
        let _ = write!(
            out,
            "{{\"nr_throttled\":{},\"throttled_usec\":{},\"quota_cores\":",
            l.cpu_nr_throttled, l.cpu_throttled_usec
        );
        push_f64(&mut out, l.cpu_quota_cores, l.cpu_quota_cores > 0.0);
        // **ここから下は T15.0 (6) で末尾に足したもの** (既存の 3 鍵の順は動かさない)。
        // `nr_periods` は割合の分母、`path` はどの階層を読んでいるか、
        // `since_start` は**このプロセスが始まってからの**増分 (累計は親の値を含む)
        let _ = write!(out, ",\"nr_periods\":{},\"path\":", l.cpu_nr_periods);
        // 道を引くのはここ (`/status` を組むとき) と `--check` だけ。5 秒ごとには引かない
        match crate::sysinfo::cgroup::cgroup_cpu_path() {
            Some(p) => {
                let _ = write!(out, "\"{}\"", crate::json::escape(&p.display().to_string()));
            }
            None => out.push_str("null"),
        }
        out.push_str(",\"since_start\":");
        let usage = l.has(SRC_CGROUP_USAGE);
        match first() {
            Some(f) => {
                let _ = write!(
                    out,
                    "{{\"nr_periods\":{},\"nr_throttled\":{},\"throttled_usec\":{}",
                    l.cpu_nr_periods.saturating_sub(f.cpu_nr_periods),
                    l.cpu_nr_throttled.saturating_sub(f.cpu_nr_throttled),
                    l.cpu_throttled_usec.saturating_sub(f.cpu_throttled_usec),
                );
                // T16.0 で末尾に足したもの (起動からの増分)。最初の 1 本で読めていなければ
                // 引き算の相手が無いので `null`
                let since_ok = usage && f.has(SRC_CGROUP_USAGE);
                push_usage(
                    &mut out,
                    [
                        l.cpu_usage_usec.saturating_sub(f.cpu_usage_usec),
                        l.cpu_user_usec.saturating_sub(f.cpu_user_usec),
                        l.cpu_system_usec.saturating_sub(f.cpu_system_usec),
                    ],
                    since_ok,
                );
                out.push('}');
            }
            None => out.push_str("null"),
        }
        // T16.0 で末尾に足したもの (累計 us。`/proc/self/stat` と違いコンテナ全体)
        push_usage(
            &mut out,
            [l.cpu_usage_usec, l.cpu_user_usec, l.cpu_system_usec],
            usage,
        );
        out.push('}');
    } else {
        out.push_str("null");
    }
    out.push_str(",\"psi\":{");
    for (i, (k, some, full, ok)) in [
        ("cpu", l.psi_cpu_some, l.psi_cpu_full, l.has(SRC_PSI_CPU)),
        ("memory", l.psi_mem_some, l.psi_mem_full, l.has(SRC_PSI_MEM)),
        ("io", l.psi_io_some, l.psi_io_full, l.has(SRC_PSI_IO)),
    ]
    .into_iter()
    .enumerate()
    {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\":", k);
        if ok {
            out.push_str("{\"some_avg10\":");
            push_f64(&mut out, some, true);
            out.push_str(",\"full_avg10\":");
            push_f64(&mut out, full, true);
            out.push('}');
        } else {
            out.push_str("null");
        }
    }
    // 直近 5 分の増分 (`/healthz` が見ているのと同じ値)
    out.push_str("},\"last_5m\":{\"listen_overflows\":");
    push_u64(
        &mut out,
        h.listen_overflows_5m.unwrap_or(0),
        h.listen_overflows_5m.is_some(),
    );
    out.push_str(",\"state_file_errors\":");
    push_u64(
        &mut out,
        h.state_file_errors_5m.unwrap_or(0),
        h.state_file_errors_5m.is_some(),
    );
    out.push_str(",\"dns_miss_ms\":");
    push_u64(
        &mut out,
        h.dns_miss_ms.unwrap_or(0),
        h.dns_miss_ms.is_some(),
    );
    let _ = write!(out, "}},\"samples\":{}}}", STATE.locked().rings[0].len());
    out
}

/// `/history` に足す `kernel` の配列 (`res` は 0 = 5 秒、1 = 60 秒)。
/// 1 時間 (3600) の解像度はこの窓に無いので `"null"`。
pub fn history_json(res: usize) -> String {
    if res >= RESOLUTIONS.len() {
        return "null".to_string();
    }
    let st = STATE.locked();
    let ring = &st.rings[res];
    let mut out = String::with_capacity(128 + ring.len() * 160);
    let _ = write!(out, "{{\"interval_secs\":{},\"keys\":[", RESOLUTIONS[res].0);
    for (i, k) in KEYS.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", k);
    }
    out.push_str("],\"samples\":[");
    for (i, s) in ring.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        s.push_row(&mut out);
    }
    out.push_str("]}");
    out
}

/// `,"usage_usec":…,"user_usec":…,"system_usec":…` (T16.0。読めなければ 3 つとも `null`)。
fn push_usage(out: &mut String, v: [u64; 3], ok: bool) {
    for (k, v) in ["usage_usec", "user_usec", "system_usec"].iter().zip(v) {
        let _ = write!(out, ",\"{}\":", k);
        push_u64(out, v, ok);
    }
}

fn push_u64(out: &mut String, v: u64, ok: bool) {
    if ok {
        let _ = write!(out, "{}", v);
    } else {
        out.push_str("null");
    }
}

fn push_f64(out: &mut String, v: f64, ok: bool) {
    if ok && v.is_finite() {
        let _ = write!(out, "{:.2}", v);
    } else {
        out.push_str("null");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 窓の中身を直に組み立てる (グローバルの [`STATE`] はテストの間で共有なので触らない)。
    fn raw(t: u64, overflows: u64, tw: u64) -> Latest {
        Latest {
            at: t,
            avail: SRC_NETSTAT | SRC_SNMP | SRC_SOCKSTAT | SRC_PSI_CPU | SRC_STATE_FILE,
            listen_overflows: overflows,
            time_wait: tw,
            psi_cpu_some: 43.02,
            psi_cpu_full: 2.99,
            ..Latest::default()
        }
    }

    /// 累計のものは**増分**、値のものは値で窓に入る。
    #[test]
    fn counters_become_deltas_and_gauges_stay_values() {
        let a = raw(1_000_000, 100, 30_912);
        let b = raw(1_000_005, 103, 31_000);
        let first = diff(&a, &a);
        assert_eq!(first.listen_overflows, 0, "1 本目は増分が出せない");
        assert_eq!(first.time_wait, 30_912, "値はそのまま");
        let next = diff(&a, &b);
        assert_eq!(next.listen_overflows, 3);
        assert_eq!(next.time_wait, 31_000);
        // カウンタが減っていたら 0 (折り返し・入れ替え)
        assert_eq!(diff(&b, &a).listen_overflows, 0);
    }

    /// 名前解決のミスは「この窓の回数」と「1 回の平均 ms」になる。
    #[test]
    fn dns_misses_become_the_price_of_one_miss() {
        let mut a = raw(1_000_000, 0, 0);
        a.dns_misses = 10;
        a.dns_us_sum = 100_000;
        let mut b = a;
        b.at = 1_000_005;
        b.dns_misses = 12;
        b.dns_us_sum = 100_000 + 2 * 12_600;
        let s = diff(&a, &b);
        assert_eq!(s.dns_misses, 2);
        assert_eq!(s.dns_miss_ms, 13, "12.6 ms は 13 に丸まる");
        assert_eq!(diff(&a, &a).dns_miss_ms, 0, "ミスが無ければ 0");
    }

    /// 1 分へ畳むと増分は足し合わせ、値と PSI は**最大** (山が消えない)。
    #[test]
    fn downsampling_sums_the_deltas_and_keeps_the_peaks() {
        let mut a = diff(&raw(0, 0, 0), &raw(1_000_000, 3, 1_000));
        let mut b = diff(&raw(0, 0, 0), &raw(1_000_005, 4, 30_912));
        a.dns_misses = 1;
        a.dns_miss_ms = 10;
        b.dns_misses = 3;
        b.dns_miss_ms = 30;
        b.psi_cpu_some = 90.0;
        let agg = Sample::downsample(&[a, b], 1_000_000);
        assert_eq!(agg.listen_overflows, 7);
        assert_eq!(agg.time_wait, 30_912, "山は最大値");
        assert_eq!(agg.psi_cpu_some, 90.0);
        assert_eq!(agg.dns_misses, 4);
        assert_eq!(agg.dns_miss_ms, 25, "回数で重みを付けた平均 (10+90)/4");
    }

    /// CPU の絞りは**割合**で読む: 期間の数が分母、絞られた期間が分子 (T15.0 (6))。
    ///
    /// 累計はこのプロセスより前から動いている (親の階層の値) ので、生の値ではなく
    /// 増分どうしを割る。1 分へ畳むときは分子も分母も足し合わせる。
    #[test]
    fn the_cpu_periods_are_the_denominator_of_the_throttling() {
        let raw = |t: u64, periods: u64, throttled: u64| Latest {
            at: t,
            avail: SRC_CGROUP_CPU,
            cpu_nr_periods: periods,
            cpu_nr_throttled: throttled,
            ..Latest::default()
        };
        // 起動より前から動いている累計 (8,000 期間のうち 915 絞られていた)
        let a = raw(1_000_000, 8_000, 915);
        let b = raw(1_000_005, 8_050, 965);
        assert_eq!(diff(&a, &a).cpu_nr_periods, 0, "1 本目は増分が出せない");
        let s = diff(&a, &b);
        assert_eq!(s.cpu_nr_periods, 50);
        assert_eq!(s.cpu_nr_throttled, 50, "この 5 秒は 50/50 = 100%");
        // 畳むと分子も分母も足し合わせ (割合が保たれる)
        let c = diff(&b, &raw(1_000_010, 8_100, 1_015));
        let agg = Sample::downsample(&[s, c], 1_000_000);
        assert_eq!((agg.cpu_nr_throttled, agg.cpu_nr_periods), (100, 100));
        // 列は [`KEYS`] の**末尾**に出る (古い読み手は位置で開く)
        let mut out = String::new();
        s.push_row(&mut out);
        let cols: Vec<&str> = out.trim_matches(['[', ']']).split(',').collect();
        assert_eq!(cols.len(), KEYS.len(), "{}", out);
        let at = |k: &str| cols[KEYS.iter().position(|n| *n == k).unwrap()];
        assert_eq!(at("cpu_nr_periods"), "50");
        // cgroup が読めない環境では `null`
        let mut out = String::new();
        Sample::default().push_row(&mut out);
        assert!(out.ends_with(",null]"), "{}", out);
    }

    /// cgroup のユーザー空間とカーネル側は**増分**で窓に入り、畳むと足し合わせ、
    /// 行の末尾 2 列に出る。`user_usec` の行が無ければ `null` (T16.0)。
    #[test]
    fn the_cgroup_user_and_system_time_are_deltas_at_the_end() {
        let raw = |t: u64, user: u64, system: u64| Latest {
            at: t,
            avail: SRC_CGROUP_CPU | SRC_CGROUP_USAGE,
            cpu_usage_usec: user + system,
            cpu_user_usec: user,
            cpu_system_usec: system,
            ..Latest::default()
        };
        let a = raw(1_000_000, 13_000_000, 18_000_000);
        let b = raw(1_000_005, 13_300_000, 18_700_000);
        let c = raw(1_000_010, 13_400_000, 18_800_000);
        let s1 = diff(&a, &b);
        assert_eq!((s1.cpu_user_usec, s1.cpu_system_usec), (300_000, 700_000));
        let s2 = diff(&b, &c);
        let agg = Sample::downsample(&[s1, s2], 1_000_000);
        assert_eq!((agg.cpu_user_usec, agg.cpu_system_usec), (400_000, 800_000));
        assert_eq!(
            agg.avail & SRC_CGROUP_USAGE,
            SRC_CGROUP_USAGE,
            "旗は 9 ビット目も残る"
        );
        let mut out = String::new();
        s1.push_row(&mut out);
        let cols: Vec<&str> = out.trim_matches(['[', ']']).split(',').collect();
        assert_eq!(cols.len(), KEYS.len(), "{}", out);
        assert_eq!(KEYS[KEYS.len() - 2..], ["cpu_user_usec", "cpu_system_usec"]);
        assert!(out.ends_with(",300000,700000]"), "{}", out);
        // 行の無いファイル (旗が立たない) は、絞りの列は数のまま、この 2 列だけ `null`
        let mut old = s1;
        old.avail = SRC_CGROUP_CPU;
        let mut out = String::new();
        old.push_row(&mut out);
        assert!(out.ends_with(",0,null,null]"), "{}", out);
    }

    /// 直近 5 分の絞りは `/healthz` と異常の規則が読む割合になる (T15.0 (6))。
    #[test]
    fn the_five_minute_throttling_is_a_ratio() {
        assert_eq!(Health::default().cpu_throttled_ratio(), None, "読めない");
        let h = Health {
            cpu_throttled_5m: Some((45, 60)),
            ..Health::default()
        };
        assert_eq!(h.cpu_throttled_ratio(), Some(0.75));
        let zero = Health {
            cpu_throttled_5m: Some((0, 0)),
            ..Health::default()
        };
        assert_eq!(zero.cpu_throttled_ratio(), None, "分母 0 は割らない");
    }

    /// 読めない源の列は `null` で出る (Linux 以外・cgroup v1・`/proc/net` の無いコンテナ)。
    #[test]
    fn unreadable_sources_are_null_in_the_row() {
        let mut out = String::new();
        Sample {
            t: 1_770_000_000,
            ..Sample::default()
        }
        .push_row(&mut out);
        let cols: Vec<&str> = out.trim_matches(['[', ']']).split(',').collect();
        assert_eq!(cols.len(), KEYS.len(), "{}", out);
        assert_eq!(cols[0], "1770000000");
        // 時刻と「その窓のミスの回数」以外は全部 null
        for (k, c) in KEYS.iter().zip(&cols).skip(1) {
            let want = if *k == "dns_misses" { "0" } else { "null" };
            assert_eq!(*c, want, "{} が {}", k, c);
        }
    }

    /// 読めた源は数で出る (`time_wait` と `psi_cpu_some_avg10` が窓に出ること)。
    #[test]
    fn a_readable_row_has_the_time_wait_and_the_psi() {
        let s = diff(&raw(1_000_000, 100, 0), &raw(1_000_005, 101, 30_912));
        let mut out = String::new();
        s.push_row(&mut out);
        let cols: Vec<&str> = out.trim_matches(['[', ']']).split(',').collect();
        assert_eq!(cols.len(), KEYS.len());
        let at = |k: &str| cols[KEYS.iter().position(|n| *n == k).unwrap()];
        assert_eq!(at("listen_overflows"), "1");
        assert_eq!(at("time_wait"), "30912");
        assert_eq!(at("psi_cpu_some_avg10"), "43.02");
        assert_eq!(at("psi_io_some_avg10"), "null", "io の PSI は読めていない");
        assert_eq!(at("curr_estab"), "0");
    }

    /// 環は 5 秒 × 720 で頭打ちになり、1 分の窓は閉じたところで 1 本できる。
    #[test]
    fn the_rings_roll_up_and_stay_bounded() {
        let mut st = State::new();
        for i in 0..(RESOLUTIONS[0].1 as u64 + 20) {
            let s = diff(&raw(0, 0, 0), &raw(1_000_000 + i * 5, i, 1_000 + i));
            push(&mut st, s);
        }
        assert_eq!(st.rings[0].len(), RESOLUTIONS[0].1);
        // 740 標本 = 3,700 秒 ≈ 61 分ぶん
        assert!(
            (60..=62).contains(&st.rings[1].len()),
            "1 分の標本が {} 本",
            st.rings[1].len()
        );
        assert_eq!(
            st.rings[1].front().unwrap().t % 60,
            0,
            "窓の先頭は 60 秒境界"
        );
    }

    /// `/history` の `kernel` は `keys` と `samples` を持ち、3600 の解像度には無い。
    #[test]
    fn the_history_array_has_the_keys_and_no_hour_resolution() {
        let json = history_json(0);
        assert!(
            json.starts_with("{\"interval_secs\":5,\"keys\":[\"t\","),
            "{}",
            json
        );
        assert!(json.contains("\"time_wait\""), "{}", json);
        assert!(json.contains("\"psi_cpu_some_avg10\""), "{}", json);
        assert!(json.contains("\"samples\":["), "{}", json);
        assert!(history_json(1).starts_with("{\"interval_secs\":60,"));
        assert_eq!(history_json(2), "null");
    }
}
