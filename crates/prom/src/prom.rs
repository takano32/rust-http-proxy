//! `/metrics` 用の Prometheus テキスト形式 (text/plain; version=0.0.4) の生成。

use std::fmt::Write as _;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use crate::cache::Cache;
use crate::metrics::{Concurrency, Metrics};

// `render` の節は仕事ごとに割ってある (T34.27)。出す順は `render` が呼ぶ順で、1 行も変えていない。
// プロセス全体の数え物 (名前解決・IPv6・ブロックリスト・要求・オリジン・確立・canary・ロック・エラー)
mod process;
// ホスト別・接続元別の表
mod tables;
// キャッシュとシステム
mod caching;
// カーネルと cgroup
mod kernel_stats;

/// 段階ごとの待ち時間 (CONNECT 7 段 / forward 6 段。T14.3) を**起動からの累計**で
/// 出す (T14.19)。Prometheus のヒストグラムは単調増加が前提なので、`/profile` の窓
/// (1 時間 / 1 日で古い標本が落ちる) の和ではなく
/// [`crate::profile::Profile::stage_totals`] を読む。
///
/// **1 本も観測していないうちは 1 系列も出さない**。`--lite` は段階の時計を読まないので
/// ずっとここを通らない (`kernel` と同じ判断: 源が無いときに 0 を出すと「一瞬で済んだ」と
/// 読めてしまう)。窓へ渡るのは 5 秒ごとなので、**最後の 5 秒ぶんは次の収集で入る**。
///
/// **ホスト別には出さない** (系列が増えすぎる。T12.4 と同じ判断)。
fn stage_metrics(out: &mut String, m: &Metrics) {
    let stages = m.profile.stage_totals();
    if stages.is_empty() {
        return;
    }
    header(
        out,
        "stage_seconds",
        "histogram",
        "Time in each stage of a CONNECT tunnel (kind=connect) or a forwarded request (kind=forward)",
    );
    let connect = stages.folded_connect();
    for (stage, w) in crate::profile::CONNECT_STAGES.iter().zip(connect.iter()) {
        let l = format!("kind=\"connect\",stage=\"{}\"", stage);
        window_histogram(out, "stage_seconds", &l, w);
    }
    let forward = stages.folded_forward();
    for (stage, w) in crate::profile::FORWARD_STAGES.iter().zip(forward.iter()) {
        let l = format!("kind=\"forward\",stage=\"{}\"", stage);
        window_histogram(out, "stage_seconds", &l, w);
    }
}

/// 12 段のヒストグラム 1 本を書く (区間は `/history` と同じ
/// [`crate::history::WINDOW_BOUNDS_MS`]、`le` は秒。最後は `+Inf`)。
/// `labels` が空でなければ `le` の前に置く。
fn window_histogram(out: &mut String, name: &str, labels: &str, w: &crate::history::Window) {
    let mut cum = 0u64;
    for (i, n) in w.buckets.iter().enumerate() {
        cum += n;
        let le = match crate::history::WINDOW_BOUNDS_MS.get(i) {
            Some(b) => format!("le=\"{}\"", *b as f64 / 1000.0),
            None => "le=\"+Inf\"".to_string(),
        };
        let l = if labels.is_empty() {
            le
        } else {
            format!("{},{}", labels, le)
        };
        line(out, &format!("{}_bucket", name), &l, cum);
    }
    line(
        out,
        &format!("{}_sum", name),
        labels,
        w.ms_sum as f64 / 1000.0,
    );
    line(out, &format!("{}_count", name), labels, w.count);
}

/// ラベル値のエスケープ (RFC: `\`、`"`、改行)。
fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn line(out: &mut String, name: &str, labels: &str, value: impl std::fmt::Display) {
    if labels.is_empty() {
        let _ = writeln!(out, "sorahost_{} {}", name, value);
    } else {
        let _ = writeln!(out, "sorahost_{}{{{}}} {}", name, labels, value);
    }
}

fn header(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP sorahost_{} {}", name, help);
    let _ = writeln!(out, "# TYPE sorahost_{} {}", name, kind);
}

/// メトリクス一式を描く。`cache` が `None` ならキャッシュ関連は出さない。
///
/// `conc` (上限といまのスレッドの数) を**呼び出し側から受け取る**のは、`/status` の
/// [`crate::metrics::StatusExtras`] と同じ理由: 数えるには `Config` と `Workers` が要り、
/// ここ (指標を読むだけの層) から呼ぶと依存が輪になる。数えるのに全接続スレッドで
/// 共有している鍵を取るので、**`/metrics` を組み立てるときだけ**引くこと (熱い経路に乗せない)。
pub fn render(m: &Metrics, cache: Option<&Cache>, conc: Concurrency) -> String {
    let mut out = String::with_capacity(4096);
    header(
        &mut out,
        "uptime_seconds",
        "gauge",
        "Seconds since the proxy started",
    );
    line(
        &mut out,
        "uptime_seconds",
        "",
        m.start_time.elapsed().as_secs(),
    );
    process::dns_metrics(&mut out);
    process::ipv6_metrics(&mut out);
    process::blocklist_metrics(&mut out);
    process::request_metrics(&mut out, m, conc);
    process::origin_metrics(&mut out, m);
    // 起動からの累計 (確立のヒストグラムとエラーの原因の 2 つの節で使う)
    let totals = m.totals();
    process::connect_metrics(&mut out, m, &totals);
    process::canary_metrics(&mut out);
    // 段階ごとの待ち時間 (T14.3 → T14.19)
    stage_metrics(&mut out, m);
    process::lock_metrics(&mut out);
    process::error_metrics(&mut out, m, &totals);
    // ホストと接続元の上位は 1 回だけ取って渡す (数え物とヒストグラムの 2 つの節で使う)
    let hosts: Vec<_> = m.hosts_sorted().into_iter().take(100).collect();
    tables::host_metrics(&mut out, &hosts);
    let clients: Vec<_> = m.clients_sorted().into_iter().take(100).collect();
    tables::client_metrics(&mut out, &clients);
    tables::host_duration_metrics(&mut out, &hosts);

    kernel_stats::kernel_metrics(&mut out);

    let Some(c) = cache else {
        return out;
    };
    let snap = caching::cache_metrics(&mut out, c);
    caching::system_metrics(&mut out, &snap);
    // 起動の切り分けと heap の基準 (T25.3)。既にある行の後ろに足すだけで、上の並びは動かさない。
    boot_metrics(&mut out);
    if let Some(c) = cache {
        heap_metrics(&mut out, c);
    }
    out
}

/// ホストの起動ごとに変わる `boot_id` を **info 型** (値は常に `1`) で出す (T25.3)。
///
/// `/status` の `boot.id` と同じ値 (`/proc/sys/kernel/random/boot_id`) で、意味は「ホストが
/// 起動し直すと変わり、プロセスを起動し直しただけなら変わらない」。**ラベルは `id` の 1 つだけ**
/// なので系列は 1 本ままで、増えるのはホストを再起動したときだけ (`id` が変わった古い系列は
/// 止まる)。読めない環境 (procfs が無い) では**行ごと出さない** — `sorahost_kernel_*` と同じ判断で、
/// 空の `id` や 0 を出すと「起動が変わっていない」と読めてしまう。
fn boot_id_info(out: &mut String, id: Option<&str>) {
    let Some(id) = id else {
        return;
    };
    header(
        out,
        "boot_id_info",
        "gauge",
        "Kernel boot id: a new value means the host rebooted (always 1)",
    );
    line(out, "boot_id_info", &format!("id=\"{}\"", escape(id)), 1);
}

/// 版とビルドの種類を **info 型** (値は常に `1`) で出す (T29.19)。
///
/// `sorahost_boot_id_info` はあるのに版の系列が無く、監視からは「いつ版が変わったか」が読めなかった。
/// ラベルは `/status` の `version` (`0.29.35+b2b26c1` の形) と `build` (`release` / `dist` / `debug`。T28.42) の
/// 2 つで、系列は 1 本 (版を入れ替えると古い系列は止まる)。`/metrics` の口が末尾に足す
/// ([`render`] は版を知らない層なので、引数で受ける)。
pub fn build_info(out: &mut String, version: &str, build: &str) {
    header(
        out,
        "build_info",
        "gauge",
        "Version and build profile of this binary (always 1)",
    );
    line(
        out,
        "build_info",
        &format!(
            "version=\"{}\",build=\"{}\"",
            escape(version),
            escape(build)
        ),
        1,
    );
}

/// [`boot_id_info`] に渡す `boot_id` を起動に 1 回だけ読む (`/status` の `boot` と同じ理由。
/// 値はプロセスの生存中に変わらないので、収集のたびに `/proc` を読み直さない)。
fn boot_metrics(out: &mut String) {
    static BOOT: OnceLock<Option<String>> = OnceLock::new();
    boot_id_info(out, BOOT.get_or_init(boot_id).as_deref());
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

/// いまの heap と、起動 60 秒の基準を出す (T25.3)。
///
/// **基準だけでは「増え」が読めない** (RSS は別の数) ので、増えを
/// `sorahost_heap_bytes − sorahost_heap_baseline_bytes` で引けるようにいまの値も出す。
/// 引き算は `/status` の `memory` と同じ [`crate::persist::heap_bytes`] を使う
/// (片方だけ直すと 2 つの口が食い違う)。
fn heap_metrics(out: &mut String, cache: &Cache) {
    if let Some(bytes) = crate::persist::heap_bytes(cache) {
        header(
            out,
            "heap_bytes",
            "gauge",
            "Heap in use: malloc used + mmap minus the cache's own memory",
        );
        line(out, "heap_bytes", "", bytes);
    }
    heap_baseline(out, crate::persist::baseline());
}

/// 起動 60 秒の heap の基準 (`/status` の `memory.baseline`) を 2 本で出す (T25.3)。
///
/// まだ 60 秒たっていない・`mallinfo2` が読めない・永続化していない
/// (`PROXY_STATS_PERSIST=off` / `--lite`) のどれかなら**行ごと出さない**
/// (`/status` が `baseline: null` になるのと同じ判断)。
fn heap_baseline(out: &mut String, base: Option<(u64, u64)>) {
    let Some((at, bytes)) = base else {
        return;
    };
    header(
        out,
        "heap_baseline_bytes",
        "gauge",
        "Heap 60 s after start, taken once per boot",
    );
    line(out, "heap_baseline_bytes", "", bytes);
    header(
        out,
        "heap_baseline_at_seconds",
        "gauge",
        "Unix time when the baseline was taken",
    );
    line(out, "heap_baseline_at_seconds", "", at);
}

#[cfg(test)]
mod tests;
