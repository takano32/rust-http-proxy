//! ダッシュボード用の履歴: 標本を 3 つの解像度の環状バッファに残し、`/history` で
//! JSON にして返す。ブラウザ側で差分からレート (req/s, MB/s) を求める。
//!
//! - 5 秒 × 4,320 (**6 時間**。メモリだけ。T14.32)、1 分 × 1440 (1 日)、1 時間 × 720 (30 日)
//! - 粗い解像度は細かい標本から作る: 累計カウンタは窓の最後の値、ゲージ (接続数・使用量) は平均、
//!   **ゲージの山は最大値** (`active_max` / `threads_max` / `fds_max`)、
//!   **区間の値** (応答時間の分布・エラー・名前解決) は足し合わせ
//! - 状態ファイル (`persist`。1 つ上の層) があれば各解像度をそこにも書き、起動時に読み戻す
//!
//! **累計と区間が混ざっている**のは意図したもの (T12.4 (3))。要求数やバイト数は累計を
//! 置いてブラウザ側で差分を取る (再起動をまたいでも段差が 1 つ出るだけ) が、応答時間の分布は
//! 差分が取れない (区間ごとの分位点が要る) ので、**その 5 秒に起きたぶんだけ**を置く。
//!
//! 混ざっていること自体は変えられない (既存の列の意味は変えない) ので、**どの列を
//! どう読むか**を [`KEY_KINDS`] (`/history` の `key_kinds`) で隣に出す (T15.0 (10))。
//! 通算しか無かった `requests` / `bytes` には区間の値 (`requests_delta` / `bytes_delta`) を
//! 並べて足した。

use crate::sync::LockExt;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;

use crate::recent::ClosedCounts;
use crate::rrd::{Dec, Enc};

// 窓そのものと解像度は下の層 (`proxy-metrics-window`) に置いてある (T14.55)。
// **今までの名前でここから引ける** (`crate::history::Window` / `RESOLUTIONS` …)。
pub use crate::window::{CAPACITY, DEFAULT_N, INTERVAL, RESOLUTIONS, WINDOW_BOUNDS_MS, Window};

/// 標本 1 本の**固定の欄の数** (この順で [`Sample::encode`] が並べる)。
///
/// 版 3 の 1 レコードは 1,024 B ([`crate::rrd::SAMPLE_RECORD`]) で、CRC 4 B を除いた
/// 1,020 B のうち固定の欄が `SAMPLE_ITEMS × 8 B`、**残りが予備**
/// ([`SAMPLE_SPARE_ITEMS`] 項目ぶん)。
///
/// **欄を足すときの作法** (T14.14。これを守る限り `.rrd` の版は上がらない =
/// 通算の統計を捨てなくてよい):
///
/// 1. [`Sample`] の**末尾**に欄を足す (手前の欄の位置は絶対に動かさない。動かすと
///    古いレコードが別の意味で読み戻る)
/// 2. [`Sample::encode`] の末尾に `u64` を 1 つ足す (= 予備の先頭を 1 つ使う)
/// 3. [`Sample::decode`] の末尾で読む (古いレコードはそこがゼロ埋めなので 0 になる)
/// 4. この数を増やす ([`Sample::encode`] の `debug_assert` と下の `const` が見張る)
pub const SAMPLE_ITEMS: usize = 119;

/// **版 3 を始めた時点の**固定の欄の数 (T14.14 のときの [`SAMPLE_ITEMS`])。
///
/// 「予備は 60 項目ぶん持って始まった」は**版 3 の割り付けについての事実**で、
/// 欄を足すたびに減ってよい今の予備 ([`SAMPLE_SPARE_ITEMS`]) の話ではない。
/// T14.14 はそれを `SAMPLE_SPARE_ITEMS >= 60` と書き損ねていて、**4 項目を超えて
/// 足すと組み立てで落ちた** (T15.0 (10) で式を直した)。
const V3_FIXED_ITEMS: usize = 63;

/// 予備に入る項目数。**これを使い切ったら**版を上げ、[`crate::rrd`] に変換をもう 1 本
/// 足すことになる (T14.14 の版 2 → 版 3 と同じ手順)。
pub const SAMPLE_SPARE_ITEMS: usize = ((crate::rrd::SAMPLE_RECORD - 4) - SAMPLE_ITEMS * 8) / 8;
/// 版 3 を始めた時点の予備 (64 項目)。
pub const V3_SPARE_ITEMS: usize = ((crate::rrd::SAMPLE_RECORD - 4) - V3_FIXED_ITEMS * 8) / 8;
const _: () = assert!(SAMPLE_ITEMS * 8 <= crate::rrd::SAMPLE_RECORD - 4);
const _: () = assert!(
    V3_SPARE_ITEMS >= 60,
    "T14.14: 版 3 の割り付けは 60 項目ぶんの予備を持って始まった"
);
const _: () = assert!(SAMPLE_SPARE_ITEMS >= 1, "予備を使い切った (版を上げること)");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sample {
    pub t: u64,
    pub requests: u64,
    pub bytes: u64,
    pub active: usize,
    pub hits: u64,
    pub misses: u64,
    pub stores: u64,
    pub evictions: u64,
    pub mem_used: u64,
    pub mem_limit: u64,
    pub disk_used: u64,
    pub disk_limit: u64,
    pub rss: u64,
    /// その区間に確立した CONNECT トンネルの確立時間 (T12.4 (3)。Phase 13 の主指標)
    pub connect: Window,
    /// その区間に転送した要求の初バイトまでの時間
    pub forward: Window,
    /// その区間のエラー件数と、その原因別の内訳
    pub errors: u64,
    pub errors_by_cause: [u64; crate::metrics::ERR_CAUSES],
    /// その区間に OS へ問い合わせた名前解決の回数と、その合計 ms
    pub dns_misses: u64,
    pub dns_ms_sum: u64,
    /// プロセス全体のスレッド数 (`/proc/self/status` の `Threads`)
    pub threads: u64,
    /// 開いている記述子の数 (`/proc/self/fd`) と、その上限 (`RLIMIT_NOFILE`)
    pub fds: u64,
    pub max_fds: u64,
    /// ゲージの山。**粗い解像度では平均に畳むと山が消える**ので最大値も持つ
    pub active_max: u64,
    pub threads_max: u64,
    pub fds_max: u64,
    /// 上限に当たって暇なトンネルを追い出した回数の累計 (T13.2)。
    ///
    /// **レコードの末尾に足した** (T14.2)。版 2 では 63 項目 × 8 B = 504 B が
    /// 領域の 508 B ぎりぎりだったが、版 3 (T14.14) で 1 レコードが 1,024 B になり、
    /// **ここから先に予備が 64 項目ぶん**ある ([`SAMPLE_SPARE_ITEMS`])。
    /// 版 2 で書かれた古いレコードはこの位置がゼロ埋めなので 0 として読み戻る
    pub evicted_idle: u64,
    /// その区間の**利用者が待つ時間** (`queue + client_read + dns + connect` の ms。
    /// CONNECT だけ。T15.0 (2) が [`crate::metrics::Interval::wait`] を埋め、
    /// T15.0 (10) がここに列として出した)。
    ///
    /// [`Self::connect`] は `open()` の入口 (要求行を読んだ後) からしか測らないので、
    /// accept してワーカーが動き出すまでの待ちも名前解決も入っていない。
    /// **見出しの「利用者が待った時間」はこちらを読むこと**
    pub wait: Window,
    /// その標本の瞬間の **warm な名前の件数** (`/status` の `dns.warm`。T15.0 (8))。
    ///
    /// **件数のゲージ**で、`config.dns_warm` (窓の**秒数**) とは意味が違う。
    /// 裏の引き直しの速さ (`refreshes/h`) をこの数で割ると 1 名前あたりになる
    /// (通算の平均で割ると実勢を 35〜60% 過小に見せる)
    pub dns_warm: u64,
    /// **その区間に増えた**要求数とバイト数 (T15.0 (10))。
    ///
    /// [`Self::requests`] / [`Self::bytes`] は**通算を標本化した値**なので、
    /// 同じ表の中で [`Self::connect`] や [`Self::errors`] などの区間の値と混ざっていた。
    /// 差は**標本を作るときに**取る (リングから引き算で作ると、再起動の段差と
    /// 欠けた標本で狂う)。既存の 2 列は消していない
    pub requests_delta: u64,
    pub bytes_delta: u64,
    /// その区間の同時接続の**真の山** (T15.0 (10))。
    ///
    /// [`Self::active_max`] は 5 秒ごとの瞬間値の最大なので、5 秒より短い山を
    /// 取りこぼす。こちらは接続を数え上げるたびに更新した値
    /// (`Metrics::inc_active_conn`) を 5 秒ごとに読み取ったもの
    pub active_peak: u64,
    /// その区間の **warm な名前の件数の最大** (T16.0)。
    ///
    /// [`Self::dns_warm`] は平均で畳むので、`MAX_WARM` (32) がバーストで埋まったかは
    /// 60 秒 / 1 時間の窓からは読めなかった (5 秒のリングは 6 時間ぶんしか無い)。
    /// 5 秒の標本では `dns_warm` と同じ値
    pub dns_warm_max: u64,
    /// その区間の `dns_warm` の**合計**と、畳んだ 5 秒の標本の数 (T16.0)。
    ///
    /// 平均を `u64` の割り算で 2 段 (5 秒 → 60 秒 → 1 時間) 切り捨てると下に偏るので、
    /// 平均は `round(Σ dns_warm_sum / Σ gauge_n)` で出す。`gauge_n` はほかのゲージ 8 つ
    /// (`active` / `mem_used` / … / `fds`) の重みにも使う。5 秒の標本は `sum = 値, n = 1`。
    /// **古いレコード (T16.0 より前) はどちらも 0** で読み戻るので、`gauge_n == 0` は
    /// `(値, 1)` として読む ([`Sample::gauge_weight`])
    pub dns_warm_sum: u64,
    pub gauge_n: u64,
    /// その区間に閉じた CONNECT 1 本ごとの **「確立 − オリジン側のカーネルの RTT」** (ms。T.28.45)。
    ///
    /// 確立は `dns + connect` の段 (入口の ACL が先に引いた名前解決も含む = `/recent` の
    /// `ms.dns + ms.connect`)、RTT はトンネルの終わりに読む `tcpi_rtt` (`/recent` の
    /// `rtt_ms.origin`)。差を ms に**切り上げ**、負なら 0。**RTT が読めなかった接続と
    /// 失敗した接続は入れない**ので `excess.count <= connect.count`。
    /// [`Self::connect`] の p50 は 1 RTT = 距離で決まる (tokyo は 18 ms) ので、`/slo` の
    /// `connect_p50_ms` / `connect_p95_ms` はこちらで判定する (T.28.16 段 1 の模型)。
    /// **T.28.45 より前のレコードはゼロ埋め** = 空の窓で読み戻る
    pub excess: Window,
    /// その区間の [`Self::dns_misses`] のうち、**名前解決に失敗した試み** (`errors_by_cause`
    /// の `dns`) が払ったミス (T.28.45)。`/slo` の `dns_miss_per_connect` は
    /// `(dns_misses − dns_fail_misses) ÷ 確立` (メール宛の総当たりの NXDOMAIN を数えない)。
    /// T.28.45 より前のレコードは 0
    pub dns_fail_misses: u64,
    /// その区間に閉じた CONNECT 1 本ごとの **「確立 (`connect` の段だけ) − オリジン側のカーネルの RTT」**
    /// (ms。T.29.12)。[`Self::excess`] から**名前解決を除いた**もので、本数は同じ。
    /// 10-26 の tokyo で excess が 50 ms を越えた成功 36 本の excess の合計 19,789 ms のうち 15,159 ms (77%) が
    /// 名前解決 (34 本がメール宛) だったので、`/slo` の `connect_p50_ms` / `connect_p95_ms` はこちらで判定し、
    /// 名前解決の遅れは `dns_slow` (異常検知) に任せる。**T.29.12 より前のレコードはゼロ埋め** = 空の窓
    pub conn_excess: Window,
}

/// `/history` の 1 標本の列名 (この順で [`Sample::push_row`] が値を並べる)。
/// **キーを標本ごとに繰り返さない**ため、JSON は配列の配列にしてある (T12.4 (3))。
pub const KEYS: [&str; 52] = [
    "t",
    "requests",
    "bytes",
    "active",
    "active_max",
    "hits",
    "misses",
    "stores",
    "evictions",
    "mem_used",
    "mem_limit",
    "disk_used",
    "disk_limit",
    "rss",
    "connects",
    "connect_ms_sum",
    "connect_ms_max",
    "connect_buckets",
    "forwards",
    "forward_ms_sum",
    "forward_ms_max",
    "forward_buckets",
    "errors",
    "errors_by_cause",
    "dns_misses",
    "dns_ms_sum",
    "threads",
    "threads_max",
    "fds",
    "fds_max",
    "max_fds",
    "evicted_idle",
    "waits",
    "wait_ms_sum",
    "wait_ms_max",
    "wait_buckets",
    "dns_warm",
    "requests_delta",
    "bytes_delta",
    "active_peak",
    // T16.0 で末尾に足したもの
    "dns_warm_max",
    "dns_warm_sum",
    "gauge_n",
    // T.28.45 で末尾に足したもの (`connect` と同じ 4 列の形 + 1 列)
    "excesses",
    "excess_ms_sum",
    "excess_ms_max",
    "excess_buckets",
    "dns_fail_misses",
    // T.29.12 で末尾に足したもの (名前解決を除いた「確立 − RTT」。`connect` と同じ 4 列の形)
    "conn_excesses",
    "conn_excess_ms_sum",
    "conn_excess_ms_max",
    "conn_excess_buckets",
];

/// [`KEYS`] と**同じ長さ・同じ並び**の「その列の読み方」(`/history` の `key_kinds`。T15.0 (10))。
///
/// 累計と区間が同じ表に混ざっている (T12.4 (3) で意図してそうした) ので、読む側が
/// 取り違える — T14.99 では分析役が 2 か所で間違えた。**列の名前からは読み方が分からない**
/// (`requests` は通算、`connects` は区間) ので、綴りを変えずに読み方を隣に出す:
///
/// - `time` … 窓の先頭の時刻 (epoch 秒)
/// - `cumulative` … **起動からの通算**。レートが欲しければ隣の標本との差を取る
///   (粗い解像度へは窓の最後の値で畳む。再起動をまたぐと段差が 1 つ出る)
/// - `delta` … **その区間に起きたぶん**だけ (粗い解像度へは足し合わせ)
/// - `gauge` … その瞬間の値 (粗い解像度へは平均。山は消える)。
///   **上限の列 (`max_fds`) だけは窓の最後の値**で畳む ([`Sample::downsample`]。
///   `RLIMIT_NOFILE` はほぼ定数で、平均にすると上限が変わった瞬間だけ中途半端な数になる)
/// - `peak` … その区間の最大 (粗い解像度へも最大)
/// - `buckets` … **入れ子の配列**。要素ごとに足して畳む
///   (区間は `bounds_ms`、`errors_by_cause` は `causes` が名前を持つ)
pub const KEY_KINDS: [&str; KEYS.len()] = [
    "time",       // t
    "cumulative", // requests
    "cumulative", // bytes
    "gauge",      // active
    "peak",       // active_max
    "cumulative", // hits
    "cumulative", // misses
    "cumulative", // stores
    "cumulative", // evictions
    "gauge",      // mem_used
    "gauge",      // mem_limit
    "gauge",      // disk_used
    "gauge",      // disk_limit
    "gauge",      // rss
    "delta",      // connects
    "delta",      // connect_ms_sum
    "peak",       // connect_ms_max
    "buckets",    // connect_buckets
    "delta",      // forwards
    "delta",      // forward_ms_sum
    "peak",       // forward_ms_max
    "buckets",    // forward_buckets
    "delta",      // errors
    "buckets",    // errors_by_cause
    "delta",      // dns_misses
    "delta",      // dns_ms_sum
    "gauge",      // threads
    "peak",       // threads_max
    "gauge",      // fds
    "peak",       // fds_max
    "gauge",      // max_fds
    "cumulative", // evicted_idle
    "delta",      // waits
    "delta",      // wait_ms_sum
    "peak",       // wait_ms_max
    "buckets",    // wait_buckets
    "gauge",      // dns_warm
    "delta",      // requests_delta
    "delta",      // bytes_delta
    "peak",       // active_peak
    "peak",       // dns_warm_max
    "delta",      // dns_warm_sum
    "delta",      // gauge_n
    "delta",      // excesses
    "delta",      // excess_ms_sum
    "peak",       // excess_ms_max
    "buckets",    // excess_buckets
    "delta",      // dns_fail_misses
    "delta",      // conn_excesses
    "delta",      // conn_excess_ms_sum
    "peak",       // conn_excess_ms_max
    "buckets",    // conn_excess_buckets
];

impl Sample {
    /// 1 標本を配列 1 行として書く ([`KEYS`] の順)。
    fn push_row(&self, out: &mut String) {
        let _ = write!(
            out,
            "[{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            self.t,
            self.requests,
            self.bytes,
            self.active,
            self.active_max,
            self.hits,
            self.misses,
            self.stores,
            self.evictions,
            self.mem_used,
            self.mem_limit,
            self.disk_used,
            self.disk_limit,
            self.rss
        );
        self.connect.push_json(out);
        self.forward.push_json(out);
        let _ = write!(out, ",{},[", self.errors);
        for (i, c) in self.errors_by_cause.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", c);
        }
        let _ = write!(
            out,
            "],{},{},{},{},{},{},{},{}",
            self.dns_misses,
            self.dns_ms_sum,
            self.threads,
            self.threads_max,
            self.fds,
            self.fds_max,
            self.max_fds,
            self.evicted_idle
        );
        // **末尾に足すこと** (T15.0 (10))。`connect` / `forward` と同じ 4 列の形
        self.wait.push_json(out);
        let _ = write!(
            out,
            ",{},{},{},{},{},{},{}",
            self.dns_warm,
            self.requests_delta,
            self.bytes_delta,
            self.active_peak,
            // T16.0 で末尾に足したもの
            self.dns_warm_max,
            self.dns_warm_sum,
            self.gauge_n
        );
        // T.28.45 で末尾に足したもの (`connect` と同じ 4 列の形 + 1 列)
        self.excess.push_json(out);
        let _ = write!(out, ",{}", self.dns_fail_misses);
        // T.29.12 で末尾に足したもの
        self.conn_excess.push_json(out);
        out.push(']');
    }

    /// ゲージを畳むときの重み (= その行に畳んだ 5 秒の標本の数。T16.0)。
    /// 古いレコード (`gauge_n` が 0) は 1 本ぶんとして読む。
    fn gauge_weight(&self) -> u64 {
        self.gauge_n.max(1)
    }

    /// `dns_warm` の合計 (T16.0)。古いレコード (`gauge_n` が 0) は値そのものを 1 本ぶんの合計として読む。
    fn dns_warm_total(&self) -> u64 {
        if self.gauge_n == 0 {
            self.dns_warm
        } else {
            self.dns_warm_sum
        }
    }

    /// 状態ファイルのレコード (先頭が時刻)。
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u64(self.t)
            .u64(self.requests)
            .u64(self.bytes)
            .u64(self.active as u64)
            .u64(self.hits)
            .u64(self.misses)
            .u64(self.stores)
            .u64(self.evictions)
            .u64(self.mem_used)
            .u64(self.mem_limit)
            .u64(self.disk_used)
            .u64(self.disk_limit)
            .u64(self.rss);
        self.connect.encode(&mut e);
        self.forward.encode(&mut e);
        e.u64(self.errors);
        for c in self.errors_by_cause {
            e.u64(c);
        }
        e.u64(self.dns_misses)
            .u64(self.dns_ms_sum)
            .u64(self.threads)
            .u64(self.fds)
            .u64(self.max_fds)
            .u64(self.active_max)
            .u64(self.threads_max)
            .u64(self.fds_max)
            // **末尾に足すこと** (T14.2)。前からある項目の位置が動くと、古いレコードが
            // 別の意味で読み戻る。足したら `SAMPLE_ITEMS` も 1 つ増やす (T14.14)
            .u64(self.evicted_idle);
        // T15.0 (10) で足した 20 項目。**`decode` と同じ順に並べること** —
        // `/history` の列数しか見ていない検査は順番の食い違いを通してしまい、
        // 統計が別の意味で読み戻る
        self.wait.encode(&mut e);
        e.u64(self.dns_warm)
            .u64(self.requests_delta)
            .u64(self.bytes_delta)
            .u64(self.active_peak)
            // T16.0 で足した 3 項目 (`decode` と同じ順)
            .u64(self.dns_warm_max)
            .u64(self.dns_warm_sum)
            .u64(self.gauge_n);
        // T.28.45 で足した 17 項目 (`decode` と同じ順)
        self.excess.encode(&mut e);
        e.u64(self.dns_fail_misses);
        // T.29.12 で足した 16 項目 (`decode` と同じ順)
        self.conn_excess.encode(&mut e);
        debug_assert_eq!(e.0.len(), SAMPLE_ITEMS * 8, "固定の欄の数と食い違っている");
        e.0
    }

    pub fn decode(payload: &[u8]) -> Option<Sample> {
        let mut d = Dec(payload);
        let t = d.u64();
        if t == 0 {
            return None;
        }
        let mut s = Sample {
            t,
            requests: d.u64(),
            bytes: d.u64(),
            active: d.u64() as usize,
            hits: d.u64(),
            misses: d.u64(),
            stores: d.u64(),
            evictions: d.u64(),
            mem_used: d.u64(),
            mem_limit: d.u64(),
            disk_used: d.u64(),
            disk_limit: d.u64(),
            rss: d.u64(),
            ..Sample::default()
        };
        s.connect = Window::decode(&mut d);
        s.forward = Window::decode(&mut d);
        s.errors = d.u64();
        for c in s.errors_by_cause.iter_mut() {
            *c = d.u64();
        }
        s.dns_misses = d.u64();
        s.dns_ms_sum = d.u64();
        s.threads = d.u64();
        s.fds = d.u64();
        s.max_fds = d.u64();
        s.active_max = d.u64();
        s.threads_max = d.u64();
        s.fds_max = d.u64();
        // 古いレコードはここから先がゼロ埋めなので 0 になる (`Dec` は足りなければ 0)。
        // **ここから下が予備** — 欄を足すならこの位置から (T14.14)
        s.evicted_idle = d.u64();
        // T15.0 (10)。**`encode` と同じ順**。版 3 の 63 項目のレコードはここが
        // ゼロ埋めなので、新しい欄は 0 で読み戻り、手前の欄は 1 つもずれない
        s.wait = Window::decode(&mut d);
        s.dns_warm = d.u64();
        s.requests_delta = d.u64();
        s.bytes_delta = d.u64();
        s.active_peak = d.u64();
        // T16.0。T16.0 より前のレコードはここがゼロ埋めなので 0 で読み戻る
        // (畳むときは `gauge_n == 0` を `(値, 1)` として読む)
        s.dns_warm_max = d.u64();
        s.dns_warm_sum = d.u64();
        s.gauge_n = d.u64();
        // T.28.45。それより前のレコードはここがゼロ埋めなので空の窓と 0 で読み戻る
        s.excess = Window::decode(&mut d);
        s.dns_fail_misses = d.u64();
        // T.29.12。それより前のレコードはここがゼロ埋めなので空の窓で読み戻る
        s.conn_excess = Window::decode(&mut d);
        Some(s)
    }

    /// 窓の標本をひとつにまとめる: 累計は最後の値、ゲージは平均 (山は最大値)、
    /// 区間の値は足し合わせ、時刻は窓の先頭。
    ///
    /// **ゲージの平均は標本の数で重みを付けて四捨五入する** (T16.0)。前は `Σ / 行数` の
    /// 切り捨てで、5 秒 → 60 秒 → 1 時間 の 2 段で最大 2 近く下に偏っていた (1 時間の行は
    /// 60 秒の行をどれも同じ重みで平均していたので、途中で欠けた分も同じ重みだった)。
    /// `dns_warm` は合計 (`dns_warm_sum`) を持つので `round(Σ合計 / Σ標本数)` で**真の平均 ±0.5**。
    /// ほかの 8 つは 60 秒の行の平均が丸めてあるので、1 時間の行は最悪 ±1.0。
    fn downsample(window: &[Sample], t: u64) -> Sample {
        let last = window.last().copied().unwrap_or_default();
        // この行に畳む 5 秒の標本の数 (古いレコードは 1 行 = 1 本)
        let gauge_n: u64 = window.iter().map(Sample::gauge_weight).sum();
        let n = gauge_n.max(1) as u128;
        let round = |total: u128| ((total + n / 2) / n) as u64;
        let avg = |f: fn(&Sample) -> u64| {
            round(
                window
                    .iter()
                    .map(|s| f(s) as u128 * s.gauge_weight() as u128)
                    .sum::<u128>(),
            )
        };
        let dns_warm_sum: u64 = window.iter().map(Sample::dns_warm_total).sum();
        let max = |f: fn(&Sample) -> u64| window.iter().map(f).max().unwrap_or(0);
        let sum = |f: fn(&Sample) -> u64| window.iter().map(f).sum::<u64>();
        let mut connect = Window::default();
        let mut forward = Window::default();
        // 利用者が待つ時間も区間の値なので `connect` と同じ畳み方 (T15.0 (10))
        let mut wait = Window::default();
        // 「確立 − RTT」も区間の値 (T.28.45)
        let mut excess = Window::default();
        let mut conn_excess = Window::default();
        let mut errors_by_cause = [0u64; crate::metrics::ERR_CAUSES];
        for s in window {
            connect.merge(&s.connect);
            forward.merge(&s.forward);
            wait.merge(&s.wait);
            excess.merge(&s.excess);
            conn_excess.merge(&s.conn_excess);
            for (a, b) in errors_by_cause.iter_mut().zip(s.errors_by_cause.iter()) {
                *a += *b;
            }
        }
        Sample {
            t,
            requests: last.requests,
            bytes: last.bytes,
            active: avg(|s| s.active as u64) as usize,
            hits: last.hits,
            misses: last.misses,
            stores: last.stores,
            evictions: last.evictions,
            mem_used: avg(|s| s.mem_used),
            mem_limit: avg(|s| s.mem_limit),
            disk_used: avg(|s| s.disk_used),
            disk_limit: avg(|s| s.disk_limit),
            rss: avg(|s| s.rss),
            connect,
            forward,
            errors: sum(|s| s.errors),
            errors_by_cause,
            dns_misses: sum(|s| s.dns_misses),
            dns_ms_sum: sum(|s| s.dns_ms_sum),
            threads: avg(|s| s.threads),
            fds: avg(|s| s.fds),
            // 上限は `gauge` だが**窓の最後の値** (`RLIMIT_NOFILE` はほぼ定数。
            // [`KEY_KINDS`] の `gauge` の行に但し書きがある)
            max_fds: last.max_fds,
            active_max: max(|s| s.active_max),
            threads_max: max(|s| s.threads_max),
            fds_max: max(|s| s.fds_max),
            // 累計カウンタなので窓の最後の値 (`requests` と同じ。ブラウザ側で差分を取る)
            evicted_idle: last.evicted_idle,
            wait,
            // warm な名前の**件数のゲージ**。平均で畳むのは、受け入れ基準の
            // 「`refreshes/h ÷ その時間の平均 warm`」がそのまま読めるようにするため。
            // T16.0 から平均は合計 ÷ 標本数 (行ごとの丸めを重ねない)
            dns_warm: round(dns_warm_sum as u128),
            // 区間の値なので足し合わせ (通算の `requests` / `bytes` とは畳み方が違う)
            requests_delta: sum(|s| s.requests_delta),
            bytes_delta: sum(|s| s.bytes_delta),
            // 山なので最大 (平均に畳むと消える。`active_max` と同じ扱い)
            active_peak: max(|s| s.active_peak),
            // T16.0。古いレコードは `dns_warm_max` が 0 なので、平均の値を下限にする
            dns_warm_max: max(|s| s.dns_warm_max.max(s.dns_warm)),
            dns_warm_sum,
            gauge_n,
            excess,
            dns_fail_misses: sum(|s| s.dns_fail_misses),
            conn_excess,
        }
    }
}

/// 閉じた接続の分布を残す**メモリ上の窓** (5 秒 × 720 と 60 秒 × 1,440。T14.6)。
///
/// **`.rrd` の標本には足さない。** 標本 1 本の余白は 4 B しか残っていない (T14.2 (3)) ので、
/// 閉じた理由 8 種 + 寿命 13 段 + 上り 13 段 + 下り 13 段 + 合計 5 つ = 52 個の u64 は
/// どうやっても入らない。版を上げれば入るが、上げると統計が全部消える。
/// **ここは再起動で消えてよい**個票と同じ扱い (T14.4 のリングと同じ方針)。
///
/// 書くのは `Metrics::record_closed` = 接続の終了で 1 回だけで、
/// 窓を閉じるのは history スレッド ([`ClosedWindows::roll`]) — `/history` の標本と
/// **同じ周期・同じ境目**で閉じるので、読む側は時刻で突き合わせられる。
///
/// **件数 0 の窓は残さない** (デプロイ先は 43 本/時 なので、残すと 720 本のうち
/// 719 本がゼロの行になる)。行の先頭に窓の始まりの時刻があるので、抜けていても読める。
pub struct ClosedWindows {
    inner: Mutex<ClosedState>,
}

#[derive(Default)]
struct ClosedState {
    /// まだ閉じていない 5 秒の窓と、その始まり (5 秒に丸めた epoch)
    cur: ClosedCounts,
    cur_t: u64,
    /// まだ閉じていない 60 秒の窓 (閉じた 5 秒の窓を足し込む)
    min_cur: ClosedCounts,
    min_t: u64,
    fine: VecDeque<(u64, ClosedCounts)>,
    minute: VecDeque<(u64, ClosedCounts)>,
    /// 起動からの通算 (畳んだ本数)
    total: u64,
}

impl Default for ClosedWindows {
    fn default() -> Self {
        ClosedWindows::new()
    }
}

impl ClosedWindows {
    pub fn new() -> ClosedWindows {
        ClosedWindows {
            inner: Mutex::new(ClosedState::default()),
        }
    }

    /// 閉じた接続 1 本を今の窓に足す (**接続の終了で 1 回だけ**。鍵 1 回)。
    pub fn observe(&self, e: &crate::recent::RecentEntry) {
        let mut w = self.inner.locked();
        w.total += 1;
        w.cur.observe(e);
    }

    /// 窓を閉じる (history スレッドが 5 秒ごとに呼ぶ)。
    ///
    /// `/history` の標本と同じ境目 (`now / 5 * 5`、`now / 60 * 60`) で切るので、
    /// 5 秒の窓の時刻は `/history?res=5` の `t` と、60 秒の窓は `res=60` の `t` と揃う。
    pub fn roll(&self, now: u64) {
        let mut w = self.inner.locked();
        let fine_t = (now / RESOLUTIONS[0].0) * RESOLUTIONS[0].0;
        if fine_t != w.cur_t {
            let closed = std::mem::take(&mut w.cur);
            let at = w.cur_t;
            w.cur_t = fine_t;
            if !closed.is_empty() {
                w.min_cur.merge(&closed);
                push_window(&mut w.fine, at, closed, RESOLUTIONS[0].1);
            }
        }
        let min_t = (now / RESOLUTIONS[1].0) * RESOLUTIONS[1].0;
        if min_t != w.min_t {
            let closed = std::mem::take(&mut w.min_cur);
            let at = w.min_t;
            w.min_t = min_t;
            if !closed.is_empty() {
                push_window(&mut w.minute, at, closed, RESOLUTIONS[1].1);
            }
        }
    }

    /// 2 つの窓の置き場を満杯のぶん確保して触る (**起動時に 1 回だけ**。T17.8)。
    /// 返すのは触ったバイト数。
    pub fn prefault(&self) -> usize {
        let mut w = self.inner.locked();
        crate::prefault::deque(&mut w.fine, RESOLUTIONS[0].1)
            + crate::prefault::deque(&mut w.minute, RESOLUTIONS[1].1)
    }

    /// 残してある窓の数 (5 秒 / 60 秒) と、畳んだ本数の通算。
    pub fn counts(&self) -> (usize, usize, u64) {
        let w = self.inner.locked();
        (w.fine.len(), w.minute.len(), w.total)
    }

    /// `/history` の `closed` (解像度の添字は [`RESOLUTIONS`] と同じ)。
    ///
    /// **1 時間の解像度では残していない** (`null`)。閉じた接続の分布は「いま効いている
    /// 設定が長すぎるか短すぎるか」を読むためのもので、30 日ぶんは要らない。
    pub fn to_json_res(&self, res: usize) -> String {
        use crate::recent::{BYTE_BOUNDS, CLOSE_REASON_NAMES, CLOSED_KEYS, LIFE_BOUNDS_SECS};
        if res > 1 {
            return "null".to_string();
        }
        let w = self.inner.locked();
        let ring = if res == 0 { &w.fine } else { &w.minute };
        let mut out = String::with_capacity(256 + ring.len() * 140);
        let _ = write!(out, "{{\"interval_secs\":{},\"keys\":[", RESOLUTIONS[res].0);
        push_str_array(&mut out, &CLOSED_KEYS);
        out.push_str("],\"reasons\":[");
        push_str_array(&mut out, &CLOSE_REASON_NAMES);
        out.push_str("],\"life_bounds_secs\":[");
        push_num_array(&mut out, &LIFE_BOUNDS_SECS);
        out.push_str("],\"byte_bounds\":[");
        push_num_array(&mut out, &BYTE_BOUNDS);
        out.push_str("],\"samples\":[");
        for (i, (t, c)) in ring.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            c.push_row(&mut out, *t);
        }
        let _ = write!(
            out,
            "],\"windows\":{},\"capacity\":{},\"recorded\":{}}}",
            ring.len(),
            RESOLUTIONS[res].1,
            w.total
        );
        out
    }
}

fn push_window(ring: &mut VecDeque<(u64, ClosedCounts)>, t: u64, c: ClosedCounts, cap: usize) {
    if ring.len() >= cap {
        ring.pop_front();
    }
    ring.push_back((t, c));
}

fn push_str_array(out: &mut String, names: &[&str]) {
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", n);
    }
}

fn push_num_array(out: &mut String, v: &[u64]) {
    for (i, n) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}", n);
    }
}

/// 1 回の記録で各解像度に加わった標本 (状態ファイルへの書込用)。
#[derive(Debug, Default, Clone, Copy)]
pub struct Pushed {
    pub fine: Option<Sample>,
    pub minute: Option<Sample>,
    pub hour: Option<Sample>,
}

#[derive(Default)]
pub struct History {
    rings: [Mutex<VecDeque<Sample>>; 3],
    /// 閉じた接続の分布 (`/history` の `closed`。T14.6)。**`.rrd` には載せない**
    pub closed: ClosedWindows,
    /// 転送速度と半閉じの分布 (`/history` の `transfer`。T14.25)。**`.rrd` には載せない**
    pub transfer: crate::transfer::TransferWindows,
}

impl History {
    /// 5 秒の標本を記録し、分・時の窓が閉じたらそれらも作る。
    pub fn push(&self, s: Sample) -> Pushed {
        let mut out = Pushed {
            fine: Some(s),
            ..Pushed::default()
        };
        Self::append(&self.rings[0], s, Self::capacity(0));
        out.minute = self.roll(0, 1, s.t);
        if out.minute.is_some() {
            out.hour = self.roll(1, 2, s.t);
        }
        out
    }

    /// `from` の標本から、`to` の解像度で前の窓が閉じていればひとつ作る。
    fn roll(&self, from: usize, to: usize, now: u64) -> Option<Sample> {
        let step = RESOLUTIONS[to].0;
        let window_start = (now / step) * step;
        let last_to = self.rings[to].locked().back().map(|s| s.t);
        // 直前の窓 [window_start - step, window_start) がまだ無ければ作る
        let prev = window_start.checked_sub(step)?;
        if last_to.is_some_and(|t| t >= prev) {
            return None;
        }
        let src = self.rings[from].locked();
        let window: Vec<Sample> = src
            .iter()
            .filter(|s| s.t >= prev && s.t < window_start)
            .copied()
            .collect();
        drop(src);
        if window.is_empty() {
            return None;
        }
        let agg = Sample::downsample(&window, prev);
        Self::append(&self.rings[to], agg, Self::capacity(to));
        Some(agg)
    }

    fn append(ring: &Mutex<VecDeque<Sample>>, s: Sample, cap: usize) {
        let mut q = ring.locked();
        if q.len() >= cap {
            q.pop_front();
        }
        q.push_back(s);
    }

    /// 3 つの解像度の標本と `closed` / `transfer` の窓の置き場を、満杯のぶん確保して触る
    /// (**起動時に 1 回だけ**。T17.8)。返すのは触ったバイト数。
    ///
    /// 触っても件数は増えないので、`memory.rings_used.history` (件数 × 1 本) は変わらない。
    /// 読み戻した標本はそのまま残る (状態ファイルを読む前でも後でも呼べる)。
    pub fn prefault(&self) -> usize {
        let rings: usize = (0..RESOLUTIONS.len())
            .map(|res| crate::prefault::deque(&mut self.rings[res].locked(), Self::capacity(res)))
            .sum();
        rings + self.closed.prefault() + self.transfer.prefault()
    }

    /// 起動時に状態ファイルから読み戻す (`res` は 0=5 秒, 1=1 分, 2=1 時間)。
    pub fn restore(&self, res: usize, samples: impl IntoIterator<Item = Sample>) {
        let mut q = self.rings[res].locked();
        q.clear();
        q.extend(samples);
        while q.len() > Self::capacity(res) {
            q.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.rings[0].locked().len()
    }

    /// その解像度の標本数 (`res` は 0=5 秒, 1=1 分, 2=1 時間)。
    ///
    /// [`Self::len`] は 5 秒しか返さないので、3 つ足した「いま埋まっているぶん」
    /// (`/status` の `memory.rings_used.history`) を出すのに要る (T15.0 (13))。
    pub fn len_res(&self, res: usize) -> usize {
        self.rings[res.min(RESOLUTIONS.len() - 1)].locked().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 解像度 (秒) から添字を引く。合わなければ 5 秒。
    pub fn index_for(secs: u64) -> usize {
        RESOLUTIONS
            .iter()
            .position(|(s, _)| *s == secs)
            .unwrap_or(0)
    }

    /// **メモリ上の**リングが残す本数。5 秒だけ [`CAPACITY`] (6 時間。T14.32) で、
    /// 1 分と 1 時間は [`RESOLUTIONS`] のまま (`.rrd` の本数と同じ)。
    pub const fn capacity(res: usize) -> usize {
        if res == 0 {
            CAPACITY
        } else {
            RESOLUTIONS[res].1
        }
    }

    /// `?n=` を書かないときに返す本数。**5 秒だけ [`DEFAULT_N`] (720 = 1 時間)** で
    /// 頭打ちにして、応答の大きさを今までどおりに保つ (T14.32)。
    pub const fn default_n(res: usize) -> usize {
        if res == 0 {
            DEFAULT_N
        } else {
            RESOLUTIONS[res].1
        }
    }

    /// `?n=` の値を丸める (書いていなければ [`Self::default_n`]、上は
    /// [`Self::capacity`] で頭打ち。`/errors?n=` などと同じ作法で下は 1)。
    pub fn clamp_n(res: usize, n: Option<usize>) -> usize {
        n.unwrap_or_else(|| Self::default_n(res))
            .clamp(1, Self::capacity(res))
    }

    /// `{"interval_secs":N,"keys":[...],"bounds_ms":[...],"samples":[[...],...]}` (古い順)。
    ///
    /// **標本は配列の配列**にしてある (T12.4 (3))。項目が 13 から 31 に増えたので、
    /// 標本ごとにキーを繰り返すと `/history?res=5` (720 標本) が 1 MB を超える。
    /// キーは `keys` に 1 回だけ出し、`connect_buckets` / `forward_buckets` /
    /// `errors_by_cause` はその位置に入れ子の配列で入る。
    ///
    /// 末尾の `closed` は閉じた接続の分布 ([`ClosedWindows`]。T14.6)。**標本とは別の配列**で、
    /// `res=5|60` のときだけ中身がある (1 時間では `null`)。
    pub fn to_json(&self) -> String {
        self.to_json_res(0)
    }

    pub fn to_json_res(&self, res: usize) -> String {
        self.to_json_res_n(res, None)
    }

    /// `?n=` で**新しい方から何本返すか**を決める版 (T14.32)。
    ///
    /// 5 秒のリングは 6 時間ぶん ([`CAPACITY`]) 持つが、`n` を書かなければ今までどおり
    /// 720 本 ([`DEFAULT_N`]) しか返さない (応答の大きさを変えないため)。効くのは
    /// **標本の配列だけ**で、隣に並ぶ `closed` / `transfer` / `canary` / `kernel` の窓は
    /// もともと 720 本までしか持っていないのでそのまま出す。
    pub fn to_json_res_n(&self, res: usize, n: Option<usize>) -> String {
        let res = res.min(RESOLUTIONS.len() - 1);
        let n = Self::clamp_n(res, n);
        let q = self.rings[res].locked();
        // 古い方を落として新しい方から n 本
        let skip = q.len().saturating_sub(n);
        let mut out = String::with_capacity(256 + (q.len() - skip) * 360);
        // 応答の形の版は**いちばん先頭の鍵** (T14.49)。`closed` / `transfer` / `canary` /
        // `kernel` は**この応答の中の配列**なので、版を持つのはここ 1 か所だけ
        out.push_str(crate::metrics::SCHEMA_HEAD);
        let _ = write!(out, "\"interval_secs\":{},\"keys\":[", RESOLUTIONS[res].0);
        for (i, k) in KEYS.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{}\"", k);
        }
        // 列の**読み方**を同じ長さで隣に出す (T15.0 (10))。綴り (`keys`) は 1 つも
        // 変えていないので、読む側は添字でも名前でも今までどおり引ける
        out.push_str("],\"key_kinds\":[");
        for (i, k) in KEY_KINDS.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{}\"", k);
        }
        out.push_str("],\"bounds_ms\":[");
        for (i, b) in WINDOW_BOUNDS_MS.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", b);
        }
        out.push_str("],\"causes\":[");
        for (i, c) in crate::metrics::ERR_CAUSE_NAMES.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{}\"", c);
        }
        out.push_str("],\"samples\":[");
        for (i, s) in q.iter().skip(skip).enumerate() {
            if i > 0 {
                out.push(',');
            }
            s.push_row(&mut out);
        }
        drop(q);
        // 閉じた接続の分布は**標本の後ろに別の配列**で出す (T14.6)。既存の `keys` /
        // `samples` の形と読み方は 1 つも変えない。1 時間の解像度では残していないので `null`
        out.push_str("],\"closed\":");
        out.push_str(&self.closed.to_json_res(res));
        // 転送速度と半閉じの分布は `closed` の隣に**別の配列**で出す (T14.25)。
        // `closed` の `keys` も標本の形も 1 つも変えていない
        out.push_str(",\"transfer\":");
        out.push_str(&self.transfer.to_json_res(res));
        // 利用者の要求が無い時間帯の名前解決と TCP 接続 (T14.10)。**別の配列**に足す
        // ので、既存の `keys` / `samples` を読む側は 1 行も変えなくてよい
        crate::canaryhist::push_history_json(&mut out, res);
        out.push('}');
        out
    }
}

#[cfg(test)]
mod tests;

/// 閉じた接続の分布の窓 (T14.6)。**`.rrd` には載せない**メモリ上の窓。
#[cfg(test)]
mod closed_tests;

/// 期間を切ってサーバー側で畳む要約 (`/history?since=&until=&summary=1`。T14.24)。
///
/// T14.0 と T14.17 は `/history` を丸ごと取って手元で切って集計していた (284 KB を取って
/// 5 行を得る)。同じ畳み方をサーバー側でやれば、調査ページの「起動から」「前のデプロイと
/// 比べる」が **1 要求**で済む。**標本そのものは返さない**ので応答は 1 KiB 前後。
///
/// 求め方は `scripts/snapshot-diff.py` の `aggregate()` と**同じ**にしてある:
/// 区間の値 (確立時間の 12 段のヒストグラム・エラー・名前解決) は期間ぶん**足し合わせ**、
/// ゲージの山は**最大値**、分位点は足し合わせたヒストグラムを [`Window::quantile_ms`] で
/// 補間する (区間内は一様、その期間の最大値で頭打ち)。同じ期間を切れば同じ数字が出る。
///
/// **費用 0**: 読むのは `/history` に来たときだけで、要求の経路には何も足していない。
pub mod summary;

/// 期間を切ってサーバー側で畳む要約 (T14.24)。
///
/// 見るのは 4 つ: **T14.0 の表と同じ数字が出ること** (架空の標本列から p50 / p95 が
/// 手計算と一致)、平常時の切り出しがバーストを外すこと、解像度の自動選択、応答が 4 KiB 以下。
#[cfg(test)]
mod summary_tests;
