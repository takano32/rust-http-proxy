//! `/status` の `memory` (T14.21。T.34.32 で `status.rs` から移した)。
//!
//! 180 行の `memory_json` を、記録のリングの見積もり ([`RingBytes::full`])・いま埋まっているぶんの
//! 実測 ([`RingBytes::used`])・描画 ([`memory_json`]) に割った。出す JSON は 1 バイトも変えていない
//! (鍵の順・数の式・読む順は前と同じ)。

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
}

/// 記録のリングの 11 項目の名前 (`memory.rings` と `memory.rings_used` の鍵の順)。
///
/// **`profile` は `total` の手前に足した** (`readers` までの並びは 1 つも動かさず、
/// `total` は今までどおり最後)。T15.0 (13)
pub(super) const RING_NAMES: [&str; 11] = [
    "recent",
    "errors",
    "bursts",
    "log",
    "events",
    "trace",
    "history",
    "hostseries",
    "quantiles",
    "readers",
    "profile",
];

/// 1 件あたりの見積もり (T15.0 (13))。**これを控えてから**「満杯 = 上限 × 1 件」「いま = 件数 × 1 件」を
/// 出すので、同じ数から作った `rings_used <= rings` が必ず成り立つ。
struct PerItem {
    recent: usize,
    error: usize,
    burst: usize,
    log: usize,
    event: usize,
    trace: usize,
    host_slot: usize,
    reader: usize,
    quantile_slot: usize,
    closed: usize,
    transfer: usize,
}

impl PerItem {
    fn new() -> Self {
        use crate::events::{Event, MAX_TEXT};
        use crate::log::{Line, MAX_LOG_LINE};
        use crate::recent::{
            BurstShot, ClosedCounts, ErrorEntry, MAX_CLIENT, MAX_RECENT_TARGET, MAX_SHOT_CLIENTS,
            MAX_SHOT_TARGETS, MAX_TARGET, RecentEntry,
        };
        use crate::transfer::TransferCounts;
        let name = size_of::<(String, u32)>();
        PerItem {
            recent: size_of::<RecentEntry>() + MAX_RECENT_TARGET + MAX_CLIENT,
            error: size_of::<ErrorEntry>() + MAX_TARGET + MAX_CLIENT,
            burst: size_of::<BurstShot>()
                + MAX_SHOT_CLIENTS * (name + MAX_CLIENT)
                + MAX_SHOT_TARGETS * (name + MAX_TARGET),
            log: size_of::<Line>() + MAX_LOG_LINE,
            event: size_of::<Event>() + MAX_TEXT,
            // 接続元 1 つの追跡 (T14.27)。**追跡していなければ 1 バイトも確保していない**ので、
            // これも他と同じ「満杯のとき」の見積もり
            trace: crate::trace::MAX_LINE_ESTIMATE,
            // ホスト別の時系列は固定長 (上位 16 ホスト × 288 標本 × 5 項目 × 8 B。T14.22)。
            // **上位が 1 つ決まるまでは確保しない**ので、これも「満杯のとき」の見積もり
            host_slot: crate::hostseries::SAMPLES * crate::hostseries::FIELDS * size_of::<u64>(),
            // 内部エンドポイントを引いた接続元の表 (T14.53)。環状ではないが、同じ「満杯のとき」の
            // 見積もり (最大 256 行 × (鍵 + パス))。**1 行も引かれていなければ 1 バイトも確保しない**
            reader: size_of::<(String, Reader)>() + MAX_CLIENT + MAX_READER_PATH,
            // 直近の標本の環状は固定長 (3 系統 × 1,024 本 × 8 B = 24 KiB。T14.31、T15.0 (2))。
            // 1 本目を書くまで確保しないので、これも「満杯のとき」の見積もり
            quantile_slot: crate::quantiles::BYTES / (3 * crate::quantiles::SAMPLES),
            closed: size_of::<(u64, ClosedCounts)>(),
            transfer: size_of::<(u64, TransferCounts)>(),
        }
    }
}

/// 記録のリングの 11 項目のバイト数 (並びは [`RING_NAMES`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RingBytes(pub(super) [u64; 11]);

impl RingBytes {
    /// **満杯のときの見積もり** (固定部 + 文字列の上限。T13.4 / T14.4 / T14.6 / T14.11 / T14.22 /
    /// T14.25 / T14.27 / T14.31 / T15.0)。0 時間でも 36.6 時間でも 1 バイトも変わらない。
    pub(super) fn full() -> Self {
        use crate::events::MAX_EVENTS;
        use crate::history::{History, RESOLUTIONS, Sample};
        use crate::log::MAX_LOG_LINES;
        use crate::recent::{MAX_BURSTS, MAX_ERRORS, MAX_RECENT};
        let p = PerItem::new();
        let recent = (MAX_RECENT * p.recent) as u64;
        let errors = (MAX_ERRORS * p.error) as u64;
        let bursts = (MAX_BURSTS * p.burst) as u64;
        let log = (MAX_LOG_LINES * p.log) as u64;
        let events = (MAX_EVENTS * p.event) as u64;
        let trace = (crate::trace::MAX_TRACE * p.trace) as u64;
        let hostseries = (crate::hostseries::SLOTS * p.host_slot) as u64;
        let readers = (MAX_READERS * p.reader) as u64;
        let quantiles = crate::quantiles::BYTES as u64;
        // 履歴は 3 解像度の標本 (T12.4。**5 秒はメモリだけ 6 時間 = 4,320 本**。T14.32) と、
        // 閉じた接続の分布の窓 2 つ (T14.6)、速さと半閉じの窓 2 つ (T14.25)。
        // 窓の方は 5 秒 × 720 のままなので [`RESOLUTIONS`] を使う
        let samples: usize = (0..RESOLUTIONS.len()).map(History::capacity).sum();
        let windows = RESOLUTIONS[0].1 + RESOLUTIONS[1].1;
        let history =
            (samples * size_of::<Sample>() + windows * p.closed + windows * p.transfer) as u64;
        // 段階とスレッドの窓 (`/profile`。T14.3)。**T14.21 はこれを落としていた**ので、
        // `rings.total` が `/status` の見出しの RSS と説明が合わなかった (T15.0 (13))
        let profile = crate::profile::capacity_bytes() as u64;
        RingBytes([
            recent, errors, bursts, log, events, trace, history, hostseries, quantiles, readers,
            profile,
        ])
    }

    /// **いま埋まっているぶんの実バイト** (T15.0 (13))。[`RingBytes::full`] と同じ 1 件あたりの
    /// 見積もりに、いま入っている件数を掛けたもの。読むのは `/status` に来たときだけで、
    /// 数えるのはどれも `len()` (鍵の内側で長さを 1 つ読むだけ)。
    pub(super) fn used(me: &Metrics) -> Self {
        use crate::history::{RESOLUTIONS, Sample};
        let p = PerItem::new();
        let (quantile_slots, host_slots) = me.rings_used_counts();
        let recent_used = (me.closed.len() * p.recent) as u64;
        let errors_used = (me.errors.len() * p.error) as u64;
        let bursts_used = (me.bursts.len() * p.burst) as u64;
        let log_used = (crate::log::recent_len() * p.log) as u64;
        let events_used = (crate::events::len() * p.event) as u64;
        let trace_used = (crate::trace::len() * p.trace) as u64;
        let hostseries_used = (host_slots * p.host_slot) as u64;
        let readers_used = (me.readers.locked().len() * p.reader) as u64;
        let quantiles_used = (quantile_slots * p.quantile_slot) as u64;
        let (closed_fine, closed_min, _) = me.history.closed.counts();
        let (transfer_fine, transfer_min, _) = me.history.transfer.counts();
        let history_used = ((0..RESOLUTIONS.len())
            .map(|r| me.history.len_res(r))
            .sum::<usize>()
            * size_of::<Sample>()
            + (closed_fine + closed_min) * p.closed
            + (transfer_fine + transfer_min) * p.transfer) as u64;
        let profile_used = me.profile.used_bytes() as u64;
        RingBytes([
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
        ])
    }

    /// 11 項目の和 (`total`)。足す順は [`RING_NAMES`] の順。
    pub(super) fn total(&self) -> u64 {
        self.0.iter().sum()
    }

    /// `{"recent":N,…,"profile":N,"total":N}` を書く。
    pub(super) fn push_json(&self, out: &mut String) {
        out.push('{');
        for (name, n) in RING_NAMES.iter().zip(self.0) {
            let _ = write!(out, "\"{}\":{},", name, n);
        }
        let _ = write!(out, "\"total\":{}}}", self.total());
    }
}

/// `/status` の `memory` (RSS が何でできているか。T14.21)。
///
/// 256 MiB のコンテナで「RSS の内訳」を `/status` 1 枚から読むためのもの。読むのは
/// **`/status` に来たときだけ**で、要求の経路には 1 命令も足さない (`mallinfo2` は
/// アリーナの鍵を順に取るので数 us)。
///
/// **足して RSS になる形ではない** (README の `/status` の節にも同じ注意を書いてある):
///
/// - `rss` は今読んだ `VmRSS` (T.32.4。それまではキャッシュのプローブの値で、起動直後は古かった)。
///   末尾の `rss_anon` / `rss_file` / `rss_shmem` は**同じ 1 回の読み**の `RssAnon` / `RssFile` /
///   `RssShmem` で、足すと `rss` になる (file は本体と libc・libcrypto などの共有ライブラリ)
/// - `heap_used` / `heap_free` / `mmap` は `mallinfo2(3)` の `uordblks` / `fordblks` /
///   `hblkhd`。`heap_free` は**`malloc_trim` で返した頁を含む** (返しても空きの塊のまま) ので、
///   常駐している空きはこれより小さい。
///   `mmap` も確保しただけで触っていないページは常駐しない (`calloc` の大きな塊など)
/// - `stacks_estimate` は**予約**の合計。接続スレッド (`conn`) は 256 KiB、それ以外は Rust の
///   既定 2 MiB。常駐しているのは末尾の `stacks_resident` (smaps のスタックの写像の `Rss` の和。
///   T.32.4) で、1 本 8〜28 KiB しか触っていない
/// - `cache_memory` はキャッシュの本体 (`cache.memory.used_bytes`) と先行確保
///   (`cache.memory.reserved_bytes`) の合計 = キャッシュがヒープに持っている量
/// - `rings` は記録のリングが**満杯のときの見積もり** ([`RingBytes::full`])。`readers` だけは
///   環状ではなく表 (最大 256 行。T14.53) だが、同じ「満杯のとき」の見積もりで並べてある
/// - `arenas` は `PROXY_MALLOC_ARENAS` で掛けた上限 (`0` = glibc の既定のまま。T5.6)
/// - `rings_used` は同じ鍵で**いま埋まっているぶんの実バイト** ([`RingBytes::used`])。
///   必ず `rings_used.<名前> <= rings.<名前>`。割合は出さない (`used ÷ capacity` で
///   出せるが、割合からは実バイトを戻せない)
/// - `trim` は 1 時間に 1 回の `malloc_trim(0)` の通算 (T.30.1)。`count` は呼んだ回数、
///   `skipped` は間隔は来たが `heap_free <= heap_used` で呼ばなかった回数、返した量は RSS の差
/// - `rings_touched` は起動時に `rings` の置き場を確保して触ったか (T17.8)。`true` なら
///   `rss` は起動直後から天井の近くに居る (`--lite` と Linux 以外は `false`)
///
/// `mallinfo2` が無い環境 (musl / glibc 2.32 以下 / Linux 以外) では 3 つとも `null`。
pub(super) fn memory_json(
    me: &Metrics,
    detail: Option<crate::sysinfo::ProcessRss>,
    threads: u64,
    conn_threads: u64,
    cache: Option<&Cache>,
    baseline: &str,
) -> String {
    /// 接続スレッドのスタック (`crates/workers` の `STACK_SIZE` と同じ値)。
    /// あちらは private なので写してある (変えるときは両方)。
    const CONN_STACK: u64 = 256 * 1024;
    /// それ以外のスレッド (`std::thread` の既定)。
    const THREAD_STACK: u64 = 2 * 1024 * 1024;

    // 見積もり (満杯のとき) と実測 (いま埋まっているぶん)
    let full = RingBytes::full();
    let used = RingBytes::used(me);

    let conn = conn_threads.min(threads);
    let other = threads.saturating_sub(conn_threads);
    let stacks = (threads > 0).then(|| conn * CONN_STACK + other * THREAD_STACK);
    let rss = detail.map(|d| d.rss);
    // スタックの常駐 (T.32.4)。smaps を 1 回読む (この口に来たときだけ)
    let stacks_resident = crate::sysinfo::stacks_resident();
    let heap = crate::sysinfo::malloc_info();
    let opt = |v: Option<u64>| v.map_or_else(|| "null".to_string(), |x| x.to_string());

    let mut out = String::new();
    let _ = write!(
        out,
        concat!(
            "{{\"rss\":{},\"heap_used\":{},\"heap_free\":{},\"mmap\":{},",
            "\"stacks_estimate\":{},\"cache_memory\":{},\"rings\":"
        ),
        opt(rss),
        opt(heap.map(|h| h.used)),
        opt(heap.map(|h| h.free)),
        opt(heap.map(|h| h.mmap)),
        opt(stacks),
        cache.map_or(0, |c| c.mem_usage().0.saturating_add(c.mem_reserved())),
    );
    full.push_json(&mut out);
    let _ = write!(
        out,
        ",\"arenas\":{},\"rings_used\":",
        crate::sysinfo::arena_max()
    );
    // いま埋まっているぶん (T15.0 (13))。**`memory` の鍵の末尾に足した**ので、
    // `arenas` までの並びは 1 つも動いていない
    used.push_json(&mut out);
    let _ = write!(
        out,
        concat!(
            ",\"rings_touched\":{},\"baseline\":{},",
            "\"trim\":{},\"rss_anon\":{},\"rss_file\":{},\"rss_shmem\":{},",
            "\"stacks_resident\":{}}}"
        ),
        // 起動時にリングを触ったか (T17.8)。**`memory` の鍵の末尾に足した**
        me.rings_touched(),
        // 起動 60 秒の heap の基準 (T24.4)。**この起動のぶんだけ**。これも末尾に足した
        // (無ければ `null`。古い版の雪像と同じ形)
        baseline,
        // 1 時間に 1 回の `malloc_trim` の通算 (T.30.1)。これも `memory` の末尾に足した。
        // `history` スレッドが動かない (`--lite` / `PROXY_STATS_PERSIST=off`) なら 0 / `null`
        crate::sysinfo::trim_stats().to_json(),
        // 常駐の内訳 (T.32.4)。`rss` と同じ 1 回の読み。**`memory` の末尾に足した**
        opt(detail.and_then(|d| d.anon)),
        opt(detail.and_then(|d| d.file)),
        opt(detail.and_then(|d| d.shmem)),
        opt(stacks_resident),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 11 項目のどれも `used <= full` で、`total` は 11 項目の和 (T.34.32)。
    #[test]
    fn the_rings_in_use_never_exceed_the_full_estimate() {
        let m = Metrics::new();
        for i in 0..300 {
            let host = format!("connect://h{}:443", i % 20);
            m.record_host_timed(&host, HostOutcome::Bypass, 1, Duration::from_millis(3));
            m.record_reader(&format!("192.0.2.{}", i % 7), "/status");
            m.record_canary_error(&host, ErrCause::Timeout, 1, 2);
        }
        let (full, used) = (RingBytes::full(), RingBytes::used(&m));
        assert_eq!(RING_NAMES.len(), 11);
        for (i, name) in RING_NAMES.iter().enumerate() {
            assert!(
                used.0[i] <= full.0[i],
                "{}: {} > {}",
                name,
                used.0[i],
                full.0[i]
            );
            assert!(full.0[i] > 0, "{}", name);
        }
        assert!(
            used.0[1] > 0 && used.0[9] > 0,
            "errors と readers は埋まっている"
        );
        for r in [full, used] {
            assert_eq!(r.total(), r.0.iter().sum::<u64>());
        }
        assert!(used.total() <= full.total());
    }

    /// 書き出しは 11 項目を名前の順に並べ、最後に `total` (T.34.32)。
    #[test]
    fn the_rings_json_lists_eleven_names_then_the_total() {
        let r = RingBytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
        let mut out = String::new();
        r.push_json(&mut out);
        assert_eq!(
            out,
            concat!(
                "{\"recent\":1,\"errors\":2,\"bursts\":3,\"log\":4,\"events\":5,\"trace\":6,",
                "\"history\":7,\"hostseries\":8,\"quantiles\":9,\"readers\":10,\"profile\":11,",
                "\"total\":66}"
            )
        );
    }
}
