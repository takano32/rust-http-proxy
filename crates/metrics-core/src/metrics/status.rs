//! `/status` の組み立て (`metrics` の一部。T.28.27 で `metrics.rs` から移した)。
//!
//! [`Metrics::to_json_with_cache`] とその部品 (`push_*`・`boot`・`canary`・`memory`) を置く。
//!
//! 行を動かしただけで中身は変えていない。外から見える名前は `metrics` の
//! `pub use` でそのまま (T.28.27)。

use super::*;

impl Metrics {
    /// `memory.rings_used` が要る 2 つの件数 (分位点の標本と、追っているホストの枠)。
    ///
    /// どちらも**ホスト表と同じ鍵の中**に居る (T14.22 / T14.31) ので、1 回で両方読む。
    /// 鍵の内側でするのは長さを 2 つ読むことだけ (T15.0 (13))。
    fn rings_used_counts(&self) -> (usize, usize) {
        let h = self.hosts.locked();
        (h.quantiles.used_slots(), h.series.tracked())
    }

    pub fn to_json(&self) -> String {
        self.to_json_with_cache(None, StatusExtras::default())
    }

    /// キャッシュ統計と、上の層が用意した部品を含めた `/status` 用 JSON を生成する。
    ///
    /// `settings` / `blocklist` / `state_file` を**呼び出し側から受け取る**のは、
    /// ここで `reload::status_json()` のように直接呼ぶと、下の層 (指標) が上の層を
    /// 呼ぶ形になって依存が輪になるため。`/status` を組み立てるのは `proxy-endpoints`
    /// の仕事で、ここはその部品を並べるだけにする。
    pub fn to_json_with_cache(&self, cache: Option<&Cache>, extra: StatusExtras<'_>) -> String {
        let uptime = self.start_time.elapsed().as_secs();
        let requests = self.total_requests.load(Ordering::Relaxed);
        let active = self.active_connections.load(Ordering::Relaxed);
        let bytes = self.bytes_forwarded.load(Ordering::Relaxed);

        let cache_json = match cache {
            Some(c) => c.to_json(),
            None => "null".to_string(),
        };

        // 上位 50 を切り出す鍵だけが `?sort=` で変わる (JSON の形は変わらない。T13.3)。
        // `?hosts=0` のときは**並べ替えも写しも作らない** (T25.1 段 2): 50 行を組む費用と
        // 応答の 8 割を落とすのがこの口の目的なので、ここで `hosts_sorted_by` を呼ばない
        let hosts_json: Vec<String> = if extra.hosts {
            self.hosts_sorted_by(extra.sort)
                .into_iter()
                .take(50)
                .map(|(h, s)| {
                    format!(
                        "{{\"host\":\"{}\",{}}}",
                        crate::json::escape(&h),
                        stats_json(&s, true)
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        // ホスト別統計は `.rrd` で再起動をまたいで通算されるので、**いつからの通算か**を出す
        // (`total_requests` は起動から、`hosts[]` は通算という窓の混在が読めなかった)。
        // `hosts[]` を省いても値は同じ (表を 1 回舐めるだけ。T25.1 段 2)
        let restored_since = self.restored_since();
        // `/proc` を読むのはこのパスに来たときだけ (要求ごとには読まない)
        let threads = crate::sysinfo::process_threads().unwrap_or(0);
        let (fds, max_fds) = crate::sysinfo::process_fds().unwrap_or((0, 0));
        // RSS は、キャッシュのプローブ (既定 1 秒ごと) が回っているならその値を使う。
        // **同じ `/status` の `cache.system.process_rss_bytes` と 1 バイトも違わない**ように
        // するため (1 枚の中に食い違う RSS が 2 つ並ぶと、どちらを信じるかが分からない)。
        // プローブが止まっている (`PROXY_CACHE_PROBE_SECS=0` / キャッシュ無効 / `--lite`)
        // ときは古い値しか無いので、その場で `/proc/self/status` を読む
        let rss = cache
            .filter(|c| !c.config().probe_interval.is_zero())
            .and_then(|c| c.snapshot().rss)
            .or_else(crate::sysinfo::process_rss);
        let clients_json: Vec<String> = self
            .clients_sorted_by(ClientSort::Requests)
            .into_iter()
            .take(50)
            .map(|(c, s)| {
                format!(
                    "{{\"client\":\"{}\",{}{}}}",
                    crate::json::escape(&c),
                    stats_json(&s.stats, false),
                    // T14.7 の 4 つは**末尾に足す** (既存の鍵の順は変えない)
                    s.status_json()
                )
            })
            .collect();
        // **部ごとに分けて書く** (T14.55)。1 つの巨大な `format!` (50 引数) は
        // `rustc` がこのクレートを抱える量をひとりで押し上げていたため。
        // **出す JSON は 1 バイトも変えていない**: 書式の並びも引数の順もそのままで、
        // 区切りが `,` のところで切っただけ。
        let mut out = String::with_capacity(4096);
        push_head(&mut out, &extra, uptime, requests, restored_since);
        self.push_capacity(&mut out, &extra, threads, fds, max_fds, active);
        self.push_io(&mut out, bytes);
        push_tables(&mut out, extra.hosts, &hosts_json, &clients_json);
        push_env(&mut out, &extra, &cache_json);
        self.push_tail(
            &mut out,
            rss,
            threads,
            extra.concurrency.live_threads as u64,
            cache,
            // 状態ファイルの「起動 60 秒の heap」(T24.4)。上の層が組んだ JSON をそのまま入れる
            extra.baseline,
        );
        out
    }

    /// `/status` の `threads` 〜 `rejected_per_client` (席と、断った数)。
    fn push_capacity(
        &self,
        out: &mut String,
        extra: &StatusExtras<'_>,
        threads: u64,
        fds: u64,
        max_fds: u64,
        active: usize,
    ) {
        let _ = write!(
            out,
            concat!(
                "\"threads\":{},\"fds\":{},\"max_fds\":{},",
                "\"active_connections\":{},\"max_conns\":{},",
                "\"parked_connections\":{},\"parked_tunnels\":{},",
                "\"parking\":{},",
                "\"live_threads\":{},\"idle_threads\":{},\"queued_jobs\":{},\"max_threads\":{},",
                "\"rejected_overload\":{},\"evicted_idle\":{},\"rejected_client_acl\":{},",
                "\"rejected_per_client\":{},"
            ),
            threads,
            fds,
            max_fds,
            active,
            extra.concurrency.max_conns,
            self.parked_connections.load(Ordering::Relaxed),
            self.parked_tunnels.load(Ordering::Relaxed),
            self.park_watcher_alive.load(Ordering::Relaxed),
            extra.concurrency.live_threads,
            extra.concurrency.idle_threads,
            extra.concurrency.queued_jobs,
            extra.concurrency.max_threads,
            self.rejected_overload.load(Ordering::Relaxed),
            self.evicted_idle.load(Ordering::Relaxed),
            self.rejected_client_acl.load(Ordering::Relaxed),
            self.rejected_per_client.load(Ordering::Relaxed)
        );
    }

    /// `/status` の `bytes_forwarded` 〜 `origin_connections` (流した量)。
    fn push_io(&self, out: &mut String, bytes: u64) {
        let _ = write!(
            out,
            concat!(
                "\"bytes_forwarded\":{},",
                "\"cache_hits\":{},\"cache_misses\":{},",
                "\"origin_connections\":{{\"new\":{},\"reused\":{},\"pool_hit_ratio\":{:.4}}},"
            ),
            bytes,
            self.cache_hits.load(Ordering::Relaxed),
            self.cache_misses.load(Ordering::Relaxed),
            self.origin_new.load(Ordering::Relaxed),
            self.origin_reused.load(Ordering::Relaxed),
            self.pool_hit_ratio()
        );
    }

    /// `/status` の末尾 (**鍵を足すときはここの末尾に足す**。既存の鍵の順は変えない)。
    ///
    /// `kernel` は T14.12、`memory` は T14.21、`recent_quantiles` は T14.31、
    /// `rate_bps_total` は T14.39、`rejected_requests` は T14.28、`sni_mismatches` は
    /// T14.38、`self_bench` は T14.43、`readers` は T14.53、`syn_retrans_total` は
    /// T14.46、`records` は T14.41、`heavy_rejected` は T14.51 で末尾に足したもの。
    /// `boot` は T24.0 で `kernel` の隣 (直後) に足した (**既存の鍵の順は動かさない**)。
    fn push_tail(
        &self,
        out: &mut String,
        rss: Option<u64>,
        threads: u64,
        conn_threads: u64,
        cache: Option<&Cache>,
        baseline: &str,
    ) {
        let _ = write!(
            out,
            concat!(
                "\"kernel\":{},\"boot\":{},\"memory\":{},\"recent_quantiles\":{},\"rate_bps_total\":{},",
                "\"rejected_requests\":{},\"sni_mismatches\":{},\"self_bench\":{},",
                "\"readers\":{},\"syn_retrans_total\":{},\"records\":\"{}\",",
                "\"heavy_rejected\":{}}}"
            ),
            // カーネルと cgroup の統計 (5 秒の標本で読んだ最新の値。T14.12)。
            // ここから直に呼べるのは `dns` / `ipv6` と同じ**下の層**だから
            // (`settings` のような上の層の部品は `extra` で受け取る)
            crate::kernel::status_json(),
            // ホストの起動ごとに変わる `boot_id` と、このプロセスの `pid` (T24.0)
            boot_json(),
            // RSS の内訳 (T14.21)。`mallinfo2` を読むのはこの経路だけ。
            // `baseline` は状態ファイルが持っている「起動 60 秒の heap」(T24.4)
            memory_json(self, rss, threads, conn_threads, cache, baseline),
            // 直近 1,024 本の正確な分位点 (T14.31)。区間の補間ではない実測の並び
            self.recent_quantiles_json(),
            // いま流れているバイト/秒の合計 (T14.39)。history スレッドが 5 秒ごとに
            // 書いた値を原子 1 回読むだけ (`/connections` の `rate_bps` の和)
            self.conns.rate_bps_total(),
            // 読めずに断った要求の理由別 (T14.28)。原子 7 本を読むだけ
            self.rejected_requests_json(),
            // CONNECT のホストと SNI が食い違った本数 (T14.38)
            self.sni_mismatches.load(Ordering::Relaxed),
            // 起動直後に loopback だけで測った CPU/要求 と CPU/本 (T14.43)。
            // `PROXY_SELF_BENCH=off` (既定) なら `null` (覚えている結果が無い)
            crate::selfbench::status_json(),
            // 内部エンドポイントを引いた接続元の上位 20 (T14.53)。全部は `/readers`。
            // **プロキシとして通した要求は入らない** (`clients[]` とは別の表)
            self.readers_json(),
            // 確立までに SYN を送り直した回数の合計 (T14.46)
            self.syn_retrans_total.load(Ordering::Relaxed),
            // 記録の一括 off とハッシュ化 (T14.41)。旗を原子 1 回読むだけ
            crate::records::mode().name(),
            // 重い口が 1 本走っている最中に来て 503 で断った数 (T14.51)
            self.heavy_rejected.load(Ordering::Relaxed)
        );
    }
}

/// `/status` の `boot` (T24.0)。**ホストの起動ごとに変わる `id`** と、このプロセスの `pid`。
///
/// `id` (`/proc/sys/kernel/random/boot_id`) が**前の起動と変わっていればホストの再起動**、
/// 同じならプロセスだけの再起動。2026-10-21 の 2 回の無言の再起動は、`/events` の `start` に
/// `shutdown` が無いという事実だけでは外からの再起動と自分の落ちを区別できなかった
/// (利用者に聞いてホストの管理者によるものと分かった)。
///
/// **値はプロセスの生存中に変わらない**ので起動に 1 回だけ読み ([`OnceLock`])、読めない環境
/// (procfs が無い) は `id` を `null` にする (**`/status` は落とさない**)。`/events` の `start` の
/// 文面にも先頭 12 文字を載せる (`crates/run/src/lib.rs`。1 件 128 B の枠に収めるため)。
fn boot_json() -> String {
    static BOOT: OnceLock<Option<String>> = OnceLock::new();
    let pid = std::process::id();
    match BOOT.get_or_init(boot_id) {
        Some(id) => format!("{{\"id\":\"{}\",\"pid\":{}}}", crate::json::escape(id), pid),
        None => format!("{{\"id\":null,\"pid\":{pid}}}"),
    }
}

/// `boot_id` を読む口 (Linux 以外は `None`。`crates/sys` の実装をそのまま使う)。
#[cfg(target_os = "linux")]
fn boot_id() -> Option<String> {
    crate::sys::boot_id()
}

#[cfg(not(target_os = "linux"))]
fn boot_id() -> Option<String> {
    None
}

/// `/status` の先頭 (版と、起動からの窓の目印)。
///
/// **応答の形の版は先頭の鍵** (T14.49)。読む道具が先頭 64 バイトで分岐できる。
/// 窓の目印 (T12.4 (4)) は、`since_start_secs` から下が起動から、`restored_since` が
/// `hosts[]` / `clients[]` の通算の始まり (epoch 秒、`0` = 無し)。
fn push_head(
    out: &mut String,
    extra: &StatusExtras<'_>,
    uptime: u64,
    requests: u64,
    restored_since: u64,
) {
    let _ = write!(
        out,
        concat!(
            "{{\"schema\":{},\"status\":\"ok\",\"version\":\"{}\",\"uptime_secs\":{},\"total_requests\":{},",
            "\"since_start_secs\":{},\"restored_since\":{},"
        ),
        SCHEMA,
        crate::json::escape(extra.version),
        uptime,
        requests,
        uptime,
        restored_since
    );
}

/// `/status` の `hosts[]` と `clients[]` (どちらも上位 50 を組み終えたもの)。
///
/// `hosts` が偽 (`/status?hosts=0`。T25.1 段 2) のときは **`hosts` の鍵ごと省く**:
/// `"clients":[…]` だけを書く。既定 (真) の応答は 1 バイトも変わらない。
fn push_tables(out: &mut String, hosts: bool, hosts_json: &[String], clients_json: &[String]) {
    if hosts {
        let _ = write!(out, "\"hosts\":[{}],", hosts_json.join(","));
    }
    let _ = write!(out, "\"clients\":[{}],", clients_json.join(","));
}

/// 預かっていないときに `/status` の `canary` に出す文字列 (T15.12)。
///
/// **canary が `off` で 1 回も回っていないときの `canary::status_json()` と 1 バイト同じ**
/// (`secs` は `canary::SECS` = 60 の既定)。縛っているのは
/// `proxy-metrics-watch` の単体テスト
/// `canary::tests::the_default_canary_status_is_byte_identical_to_off`。
///
/// **canary 側の静的な既定は `Mode::Auto`** (`canary.rs` の `static MODE`) なので、誰も預けないまま
/// `/status` を組む処理系では、`auto` ではなくここの `off` の形が出る (製品のバイナリは
/// `crates/run/src/lib.rs` が待ち受けを立てる前に必ず `canary::configure` を呼ぶので `/status` は変わらない)。
pub const CANARY_STATUS_UNSET: &str = concat!(
    "{\"mode\":\"off\",\"secs\":60,\"runs\":0,\"failures\":0,",
    "\"at\":0,\"host\":\"\",\"dns_ms\":0,\"connect_ms\":0,\"ipv6_connect_ms\":null,\"error\":null,",
    // IPv6 側の 1 本を試した回数と試さなかった回数 (T18.1。末尾に足しただけ)
    "\"ipv6_runs\":0,\"ipv6_skipped\":0}"
);

/// `/status` の `canary` を組む口 (**上の層 `canary` が 1 回だけ預ける**。T15.12)。
///
/// canary はクレートを割って 1 つ上の層 (`proxy-metrics-watch`) へ上げたので、下のここからは
/// 直に呼べない (`kernel::set_state_file_errors` と同じ形)。**預ける前は
/// [`CANARY_STATUS_UNSET`]** = canary が `off` のときと同じ文字列。
static CANARY_STATUS: std::sync::OnceLock<fn() -> String> = std::sync::OnceLock::new();

/// 上の層が組み立ての口を預ける (`canary::configure` / `canary::tick` から)。
pub fn set_canary_status(f: fn() -> String) {
    let _ = CANARY_STATUS.set(f);
}

/// `/status` の `canary` の中身 (預かっていなければ [`CANARY_STATUS_UNSET`])。
pub(super) fn canary_status_json() -> String {
    match CANARY_STATUS.get() {
        Some(f) => f(),
        None => CANARY_STATUS_UNSET.to_string(),
    }
}

/// `/status` の「この環境で何が読めるか」(T14.15) と canary (T14.10)。
///
/// `settings` / `blocklist` / `state_file` は**上の層が組んだものを受け取る**
/// ([`StatusExtras`])。ここから直に呼べるのは下の層だけ。
pub(super) fn push_env(out: &mut String, extra: &StatusExtras<'_>, cache_json: &str) {
    let _ = write!(
        out,
        "\"log_level\":\"{}\",\"settings\":{},\"dns\":{},\"canary\":{},\"ipv6\":{},\"blocklist\":{},\"state_file\":{},\"capabilities\":{},\"cache\":{},",
        crate::log::current_level().as_str().trim(),
        extra.settings,
        crate::dns::status_json(),
        // 利用者の要求が無い時間帯の名前解決と TCP 接続 (最後の 1 回。T14.10)
        canary_status_json(),
        crate::net::ipv6_status_json(),
        extra.blocklist,
        extra.state_file,
        crate::sysinfo::capabilities::status_json(),
        cache_json
    );
}

/// `/status` の `memory` (RSS が何でできているか。T14.21)。
///
/// 256 MiB のコンテナで「RSS の内訳」を `/status` 1 枚から読むためのもの。読むのは
/// **`/status` に来たときだけ**で、要求の経路には 1 命令も足さない (`mallinfo2` は
/// アリーナの鍵を順に取るので数 us)。
///
/// **足して RSS になる形ではない** (README の `/status` の節にも同じ注意を書いてある):
///
/// - `rss` は呼び出し側が渡す 1 つの値 (キャッシュのプローブが読んだもの、または今読んだもの)。
///   同じ `/status` の `cache.system.process_rss_bytes` と食い違わせない
/// - `heap_used` / `heap_free` / `mmap` は `mallinfo2(3)` の `uordblks` / `fordblks` /
///   `hblkhd`。`heap_free` は「返していないだけ」で、`MADV_DONTNEED` 済みなら常駐していない。
///   `mmap` も確保しただけで触っていないページは常駐しない (`calloc` の大きな塊など)
/// - `stacks_estimate` は**予約**の合計 (実際に触ったページとの差は出せない)。接続スレッド
///   (`conn`) は 256 KiB、それ以外は Rust の既定 2 MiB
/// - `cache_memory` はキャッシュの本体 (`cache.memory.used_bytes`) と先行確保
///   (`cache.memory.reserved_bytes`) の合計 = キャッシュがヒープに持っている量
/// - `rings` は記録のリングが**満杯のときの見積もり** (固定部 + 文字列の上限。T13.4 / T14.4 /
///   T14.6 / T14.11 / T14.22 / T14.25 / T14.27 / T14.31 / T15.0)。0 時間でも 36.6 時間でも
///   1 バイトも変わらない。`readers` だけは環状ではなく表 (最大 256 行。T14.53) だが、
///   同じ「満杯のとき」の見積もりで並べてある
/// - `arenas` は `PROXY_MALLOC_ARENAS` で掛けた上限 (`0` = glibc の既定のまま。T5.6)
/// - `rings_used` は同じ鍵で**いま埋まっているぶんの実バイト** (T15.0 (13))。
///   `rings` と同じ 1 件あたりの見積もりに、いま入っている件数を掛けたもので、
///   必ず `rings_used.<名前> <= rings.<名前>`。割合は出さない (`used ÷ capacity` で
///   出せるが、割合からは実バイトを戻せない)
/// - `rings_touched` は起動時に `rings` の置き場を確保して触ったか (T17.8)。`true` なら
///   `rss` は起動直後から天井の近くに居る (`--lite` と Linux 以外は `false`)
///
/// `mallinfo2` が無い環境 (musl / glibc 2.32 以下 / Linux 以外) では 3 つとも `null`。
fn memory_json(
    me: &Metrics,
    rss: Option<u64>,
    threads: u64,
    conn_threads: u64,
    cache: Option<&Cache>,
    baseline: &str,
) -> String {
    use crate::events::{Event, MAX_EVENTS, MAX_TEXT};
    use crate::history::{History, RESOLUTIONS, Sample};
    use crate::log::{Line, MAX_LOG_LINE, MAX_LOG_LINES};
    use crate::recent::{
        BurstShot, ClosedCounts, ErrorEntry, MAX_BURSTS, MAX_CLIENT, MAX_ERRORS, MAX_RECENT,
        MAX_RECENT_TARGET, MAX_SHOT_CLIENTS, MAX_SHOT_TARGETS, MAX_TARGET, RecentEntry,
    };
    use crate::transfer::TransferCounts;

    /// 接続スレッドのスタック (`crates/workers` の `STACK_SIZE` と同じ値)。
    /// あちらは private なので写してある (変えるときは両方)。
    const CONN_STACK: u64 = 256 * 1024;
    /// それ以外のスレッド (`std::thread` の既定)。
    const THREAD_STACK: u64 = 2 * 1024 * 1024;

    // **1 件あたりの見積もりを控えてから**「満杯 = 上限 × 1 件」「いま = 件数 × 1 件」を
    // 出す (T15.0 (13))。同じ数から作るので `rings_used <= rings` が必ず成り立つ
    let name = size_of::<(String, u32)>();
    let per_recent = size_of::<RecentEntry>() + MAX_RECENT_TARGET + MAX_CLIENT;
    let per_error = size_of::<ErrorEntry>() + MAX_TARGET + MAX_CLIENT;
    let per_burst = size_of::<BurstShot>()
        + MAX_SHOT_CLIENTS * (name + MAX_CLIENT)
        + MAX_SHOT_TARGETS * (name + MAX_TARGET);
    let per_log = size_of::<Line>() + MAX_LOG_LINE;
    let per_event = size_of::<Event>() + MAX_TEXT;
    // 接続元 1 つの追跡 (T14.27)。**追跡していなければ 1 バイトも確保していない**ので、
    // これも他と同じ「満杯のとき」の見積もり
    let per_trace = crate::trace::MAX_LINE_ESTIMATE;
    // ホスト別の時系列は固定長 (上位 16 ホスト × 288 標本 × 5 項目 × 8 B。T14.22)。
    // **上位が 1 つ決まるまでは確保しない**ので、これも「満杯のとき」の見積もり
    let per_host_slot = crate::hostseries::SAMPLES * crate::hostseries::FIELDS * size_of::<u64>();
    // 内部エンドポイントを引いた接続元の表 (T14.53)。環状ではないが、同じ「満杯のとき」の
    // 見積もり (最大 256 行 × (鍵 + パス))。**1 行も引かれていなければ 1 バイトも確保しない**
    let per_reader = size_of::<(String, Reader)>() + MAX_CLIENT + MAX_READER_PATH;
    // 直近の標本の環状は固定長 (3 系統 × 1,024 本 × 8 B = 24 KiB。T14.31、T15.0 (2))。
    // 1 本目を書くまで確保しないので、これも「満杯のとき」の見積もり
    let per_quantile_slot = crate::quantiles::BYTES / (3 * crate::quantiles::SAMPLES);

    let recent = (MAX_RECENT * per_recent) as u64;
    let errors = (MAX_ERRORS * per_error) as u64;
    let bursts = (MAX_BURSTS * per_burst) as u64;
    let log = (MAX_LOG_LINES * per_log) as u64;
    let events = (MAX_EVENTS * per_event) as u64;
    let trace = (crate::trace::MAX_TRACE * per_trace) as u64;
    let hostseries = (crate::hostseries::SLOTS * per_host_slot) as u64;
    let readers = (MAX_READERS * per_reader) as u64;
    let quantiles = crate::quantiles::BYTES as u64;
    // 履歴は 3 解像度の標本 (T12.4。**5 秒はメモリだけ 6 時間 = 4,320 本**。T14.32) と、
    // 閉じた接続の分布の窓 2 つ (T14.6)、速さと半閉じの窓 2 つ (T14.25)。
    // 窓の方は 5 秒 × 720 のままなので [`RESOLUTIONS`] を使う
    let samples: usize = (0..RESOLUTIONS.len()).map(History::capacity).sum();
    let windows = RESOLUTIONS[0].1 + RESOLUTIONS[1].1;
    let per_closed = size_of::<(u64, ClosedCounts)>();
    let per_transfer = size_of::<(u64, TransferCounts)>();
    let history =
        (samples * size_of::<Sample>() + windows * per_closed + windows * per_transfer) as u64;
    // 段階とスレッドの窓 (`/profile`。T14.3)。**T14.21 はこれを落としていた**ので、
    // `rings.total` が `/status` の見出しの RSS と説明が合わなかった (T15.0 (13))
    let profile = crate::profile::capacity_bytes() as u64;

    // ここから「いま埋まっているぶん」(T15.0 (13))。読むのは `/status` に来たときだけで、
    // 数えるのはどれも `len()` (鍵の内側で長さを 1 つ読むだけ)
    let (quantile_slots, host_slots) = me.rings_used_counts();
    let recent_used = (me.closed.len() * per_recent) as u64;
    let errors_used = (me.errors.len() * per_error) as u64;
    let bursts_used = (me.bursts.len() * per_burst) as u64;
    let log_used = (crate::log::recent_len() * per_log) as u64;
    let events_used = (crate::events::len() * per_event) as u64;
    let trace_used = (crate::trace::len() * per_trace) as u64;
    let hostseries_used = (host_slots * per_host_slot) as u64;
    let readers_used = (me.readers.locked().len() * per_reader) as u64;
    let quantiles_used = (quantile_slots * per_quantile_slot) as u64;
    let (closed_fine, closed_min, _) = me.history.closed.counts();
    let (transfer_fine, transfer_min, _) = me.history.transfer.counts();
    let history_used = ((0..RESOLUTIONS.len())
        .map(|r| me.history.len_res(r))
        .sum::<usize>()
        * size_of::<Sample>()
        + (closed_fine + closed_min) * per_closed
        + (transfer_fine + transfer_min) * per_transfer) as u64;
    let profile_used = me.profile.used_bytes() as u64;

    let conn = conn_threads.min(threads);
    let other = threads.saturating_sub(conn_threads);
    let stacks = (threads > 0).then(|| conn * CONN_STACK + other * THREAD_STACK);
    let heap = crate::sysinfo::malloc_info();
    let opt = |v: Option<u64>| v.map_or_else(|| "null".to_string(), |x| x.to_string());

    format!(
        concat!(
            "{{\"rss\":{},\"heap_used\":{},\"heap_free\":{},\"mmap\":{},",
            "\"stacks_estimate\":{},\"cache_memory\":{},",
            "\"rings\":{{\"recent\":{},\"errors\":{},\"bursts\":{},\"log\":{},",
            "\"events\":{},\"trace\":{},\"history\":{},\"hostseries\":{},\"quantiles\":{},",
            "\"readers\":{},\"profile\":{},\"total\":{}}},\"arenas\":{},",
            "\"rings_used\":{{\"recent\":{},\"errors\":{},\"bursts\":{},\"log\":{},",
            "\"events\":{},\"trace\":{},\"history\":{},\"hostseries\":{},\"quantiles\":{},",
            "\"readers\":{},\"profile\":{},\"total\":{}}},\"rings_touched\":{},\"baseline\":{}}}"
        ),
        opt(rss),
        opt(heap.map(|h| h.used)),
        opt(heap.map(|h| h.free)),
        opt(heap.map(|h| h.mmap)),
        opt(stacks),
        cache.map_or(0, |c| c.mem_usage().0.saturating_add(c.mem_reserved())),
        recent,
        errors,
        bursts,
        log,
        events,
        trace,
        history,
        hostseries,
        quantiles,
        readers,
        // **`profile` は `total` の手前に足した** (`readers` までの並びは 1 つも動かさず、
        // `total` は今までどおり `rings` の最後)。T15.0 (13)
        profile,
        recent
            + errors
            + bursts
            + log
            + events
            + trace
            + history
            + hostseries
            + quantiles
            + readers
            + profile,
        crate::sysinfo::arena_max(),
        // いま埋まっているぶん (T15.0 (13))。**`memory` の鍵の末尾に足した**ので、
        // `arenas` までの並びは 1 つも動いていない
        recent_used,
        errors_used,
        bursts_used,
        log_used,
        events_used,
        trace_used,
        history_used,
        hostseries_used,
        quantiles_used,
        readers_used,
        profile_used,
        recent_used
            + errors_used
            + bursts_used
            + log_used
            + events_used
            + trace_used
            + history_used
            + hostseries_used
            + quantiles_used
            + readers_used
            + profile_used,
        // 起動時にリングを触ったか (T17.8)。**`memory` の鍵の末尾に足した**
        me.rings_touched(),
        // 起動 60 秒の heap の基準 (T24.4)。**この起動のぶんだけ**。これも末尾に足した
        // (無ければ `null`。古い版の雪像と同じ形)
        baseline,
    )
}
