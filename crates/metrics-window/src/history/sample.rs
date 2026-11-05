//! `/history` の標本 1 本 ([`Sample`]) と列の名前・読み方 ([`KEYS`]・[`KEY_KINDS`])、
//! 1 行の書き出しと粗い解像度への畳み方 (T.34.28 で `history.rs` から移した)。

use super::*;

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
    /// その区間の、名前解決に成功した試み (確立・転送の成功と、原因 `dns` 以外のエラー) のうち
    /// **宛先のポートが 443・80 以外**の本数と、そのうちエラーで終わった本数 (T.30.3)。
    /// `/slo` の `error_rate` は今までの分子と分母からこの 2 つを引いた **443・80 宛てだけ**で判定する
    /// (利用者の決定。10-28 の tokyo で外れの大半はメール宛 `:993`・`:995`・`:143` の確立の時間切れ)。
    /// **T.30.3 より前のレコードはゼロ埋め** = 引くものが無いので、旧の数え方 (全部のポート) で読み戻る
    pub other_port_attempts: u64,
    pub other_port_errors: u64,
    /// その区間の [`Self::dns_ms_sum`] のうち、**名前解決に失敗した試み**が払った ms (T.32.16)。
    /// 異常検知の `dns_slow` は `(dns_ms_sum − dns_fail_ms_sum) ÷ (dns_misses − dns_fail_misses)` で読む
    /// (0.30.4 の 62 件は nxdomain の 2 秒級の失敗で鳴っていた)。**T.32.16 より前のレコードは 0**
    /// = 今までどおり失敗のぶんの ms も成功したミスに入ったまま読み戻る
    pub dns_fail_ms_sum: u64,
    /// その区間の、名前解決に**成功した**ミス (`dns_misses − dns_fail_misses`) のうち、**宛先のポートが 443・80**
    /// のものの回数と ms の合計 (T.33.3。鍵の末尾の見分けは [`Self::other_port_attempts`] と同じ)。
    /// 異常検知の `dns_slow` はこの 2 つで判定する (利用者の決定。0.32.39 の 48 時間の 15 件はどれも NAT の接続元が
    /// 初めて引いたメールのホスト (`:993`・`:995`・`:143`) の遅い名前解決で鳴っていた)。
    /// **T.33.3 より前のレコードは 0** で読み戻る (= `dns_slow` はその標本では立たない)
    pub web_dns_misses: u64,
    pub web_dns_ms_sum: u64,
}

/// `/history` の 1 標本の列名 (この順で [`Sample::push_row`] が値を並べる)。
/// **キーを標本ごとに繰り返さない**ため、JSON は配列の配列にしてある (T12.4 (3))。
pub const KEYS: [&str; 57] = [
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
    // T.30.3 で末尾に足したもの (443・80 以外の宛先への試みと、そのエラー。`/slo` の `error_rate` が引く)
    "other_port_attempts",
    "other_port_errors",
    // T.32.16 で末尾に足したもの (名前解決に失敗した試みが払った ms。`dns_slow` が引く)
    "dns_fail_ms_sum",
    // T.33.3 で末尾に足したもの (443・80 宛ての、名前解決に成功したミスの回数と ms。`dns_slow` が読む)
    "web_dns_misses",
    "web_dns_ms_sum",
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
    "delta",      // other_port_attempts
    "delta",      // other_port_errors
    "delta",      // dns_fail_ms_sum
    "delta",      // web_dns_misses
    "delta",      // web_dns_ms_sum
];

impl Sample {
    /// 1 標本を配列 1 行として書く ([`KEYS`] の順)。
    pub(super) fn push_row(&self, out: &mut String) {
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
        // T.30.3 で末尾に足したもの
        let _ = write!(
            out,
            ",{},{}",
            self.other_port_attempts, self.other_port_errors
        );
        // T.32.16 で末尾に足したもの
        let _ = write!(out, ",{}", self.dns_fail_ms_sum);
        // T.33.3 で末尾に足したもの
        let _ = write!(out, ",{},{}]", self.web_dns_misses, self.web_dns_ms_sum);
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

    /// 窓の標本をひとつにまとめる: 累計は最後の値、ゲージは平均 (山は最大値)、
    /// 区間の値は足し合わせ、時刻は窓の先頭。
    ///
    /// **ゲージの平均は標本の数で重みを付けて四捨五入する** (T16.0)。前は `Σ / 行数` の
    /// 切り捨てで、5 秒 → 60 秒 → 1 時間 の 2 段で最大 2 近く下に偏っていた (1 時間の行は
    /// 60 秒の行をどれも同じ重みで平均していたので、途中で欠けた分も同じ重みだった)。
    /// `dns_warm` は合計 (`dns_warm_sum`) を持つので `round(Σ合計 / Σ標本数)` で**真の平均 ±0.5**。
    /// ほかの 8 つは 60 秒の行の平均が丸めてあるので、1 時間の行は最悪 ±1.0。
    pub(super) fn downsample(window: &[Sample], t: u64) -> Sample {
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
        // 区間の値 (応答時間の分布・エラーの原因) は足し合わせる (畳む段。T.34.28)
        let Folded {
            connect,
            forward,
            wait,
            excess,
            conn_excess,
            errors_by_cause,
        } = Folded::of(window);
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
            other_port_attempts: sum(|s| s.other_port_attempts),
            other_port_errors: sum(|s| s.other_port_errors),
            dns_fail_ms_sum: sum(|s| s.dns_fail_ms_sum),
            web_dns_misses: sum(|s| s.web_dns_misses),
            web_dns_ms_sum: sum(|s| s.web_dns_ms_sum),
        }
    }
}

/// 粗い解像度へ畳むときの、区間の値の足し合わせ (T.34.28 で [`Sample::downsample`] から割った「畳む段」)。
struct Folded {
    connect: Window,
    forward: Window,
    wait: Window,
    excess: Window,
    conn_excess: Window,
    errors_by_cause: [u64; crate::metrics::ERR_CAUSES],
}

impl Folded {
    fn of(window: &[Sample]) -> Folded {
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
        Folded {
            connect,
            forward,
            wait,
            excess,
            conn_excess,
            errors_by_cause,
        }
    }
}
