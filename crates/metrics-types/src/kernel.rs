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
//!
//! T.34.1 で 3 つに割った: 源を読むのは `read` (I/O と旗立ての判断を分けてある)、
//! 増分・畳み・環は `window` (I/O なし)、JSON は `json` (引数の値だけを読む)。
//! ここに残るのは型と定数、グローバルの窓 ([`STATE`]) とその口。

use std::sync::Mutex;

use crate::sync::LockExt;

mod json;
mod read;
mod window;

#[cfg(test)]
mod tests;

use window::State;

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

/// グローバルの窓 (5 秒と 1 分の環、前回と最初の生の値)。
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
    let now = read::read_now(t);
    STATE.locked().record(now);
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
    Health::of(&st.rings[0], st.first.as_ref())
}

/// `/status` の `kernel` の節 (最新の値と累計、直近 5 分の増分)。まだ標本が無ければ `"null"`。
pub fn status_json() -> String {
    // 鍵の内側では値を写すだけ (読むのは `/status` に来たときだけ)
    let st = STATE.locked();
    let Some(l) = st.last else {
        return "null".to_string();
    };
    let h = Health::of(&st.rings[0], st.first.as_ref());
    let first = st.first;
    let samples = st.rings[0].len();
    drop(st);
    json::status_json_of(
        &l,
        &h,
        first.as_ref(),
        crate::sysinfo::cgroup::cgroup_cpu_path,
        samples,
    )
}

/// `/history` に足す `kernel` の配列 (`res` は 0 = 5 秒、1 = 60 秒)。
/// 1 時間 (3600) の解像度はこの窓に無いので `"null"`。
pub fn history_json(res: usize) -> String {
    if res >= RESOLUTIONS.len() {
        return "null".to_string();
    }
    let st = STATE.locked();
    json::history_json_of(res, &st.rings[res])
}
