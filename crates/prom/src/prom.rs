//! `/metrics` 用の Prometheus テキスト形式 (text/plain; version=0.0.4) の生成。

use std::fmt::Write as _;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use crate::cache::Cache;
use crate::metrics::{Concurrency, Metrics};

/// カーネルと cgroup の統計 (T14.12)。**5 秒の標本で読んだ最新の値**をそのまま出す
/// (`/metrics` を引いた瞬間に `/proc` を読み直したりはしない)。
///
/// 読めなかった源 (Linux 以外、`/proc/net` の無いコンテナ、cgroup v1、PSI 無しのカーネル) と、
/// まだ 1 本も標本が無いとき (`--lite` / `PROXY_STATS_PERSIST=off` では履歴スレッドが
/// 動かない) は**その系列を出さない** (0 を出すと「溢れていない」と読めてしまう)。
fn kernel_metrics(out: &mut String) {
    use crate::kernel;
    let Some(k) = kernel::latest() else {
        return;
    };
    if k.has(kernel::SRC_NETSTAT) {
        for (name, help, v) in [
            (
                "kernel_listen_overflows_total",
                "Connections dropped because the accept queue was full (/proc/net/netstat)",
                k.listen_overflows,
            ),
            (
                "kernel_listen_drops_total",
                "Connections dropped on a listening socket for any reason",
                k.listen_drops,
            ),
            (
                "kernel_tcp_timeouts_total",
                "TCP retransmission timers that expired",
                k.tcp_timeouts,
            ),
            (
                "kernel_syn_retrans_total",
                "SYN segments retransmitted (the client waited 1-3 s)",
                k.syn_retrans,
            ),
            (
                "kernel_abort_on_timeout_total",
                "Connections given up after retransmitting",
                k.abort_on_timeout,
            ),
        ] {
            header(out, name, "counter", help);
            line(out, name, "", v);
        }
    }
    if k.has(kernel::SRC_SNMP) {
        header(
            out,
            "kernel_retrans_segs_total",
            "counter",
            "TCP segments retransmitted, machine wide (/proc/net/snmp)",
        );
        line(out, "kernel_retrans_segs_total", "", k.retrans_segs);
        header(
            out,
            "kernel_curr_estab",
            "gauge",
            "Sockets in ESTABLISHED or CLOSE-WAIT, machine wide",
        );
        line(out, "kernel_curr_estab", "", k.curr_estab);
    }
    if k.has(kernel::SRC_SOCKSTAT) {
        // TIME_WAIT は `tcp_max_tw_buckets` に当たると新しい接続が張れなくなる
        header(
            out,
            "kernel_time_wait",
            "gauge",
            "Sockets in TIME_WAIT, machine wide (/proc/net/sockstat)",
        );
        line(out, "kernel_time_wait", "", k.time_wait);
        header(
            out,
            "kernel_sockets_inuse",
            "gauge",
            "TCP sockets in use (IPv4 table)",
        );
        line(out, "kernel_sockets_inuse", "", k.sockets_inuse);
        header(
            out,
            "kernel_sockets_alloc",
            "gauge",
            "TCP sockets allocated",
        );
        line(out, "kernel_sockets_alloc", "", k.sockets_alloc);
    }
    if k.has(kernel::SRC_CGROUP_CPU) {
        header(
            out,
            "cgroup_cpu_throttled_seconds_total",
            "counter",
            "Time this cgroup was throttled by its CPU quota (cpu.stat)",
        );
        line(
            out,
            "cgroup_cpu_throttled_seconds_total",
            "",
            format!("{:.6}", k.cpu_throttled_usec as f64 / 1e6),
        );
        header(
            out,
            "cgroup_cpu_throttled_periods_total",
            "counter",
            "Periods in which this cgroup was throttled (cpu.stat nr_throttled)",
        );
        line(
            out,
            "cgroup_cpu_throttled_periods_total",
            "",
            k.cpu_nr_throttled,
        );
        // 絞られた**割合**を出すための分母 (T15.0 (6))。
        // `rate(throttled_periods[5m]) / rate(periods[5m])` で読む
        header(
            out,
            "cgroup_cpu_periods_total",
            "counter",
            "CPU quota periods that elapsed for this cgroup (cpu.stat nr_periods)",
        );
        line(out, "cgroup_cpu_periods_total", "", k.cpu_nr_periods);
    }
    if k.cpu_quota_cores > 0.0 {
        header(
            out,
            "cgroup_cpu_quota_cores",
            "gauge",
            "CPU cores this cgroup may use (cpu.max quota / period)",
        );
        line(
            out,
            "cgroup_cpu_quota_cores",
            "",
            format!("{:.3}", k.cpu_quota_cores),
        );
    }
    // PSI: 直近 10 秒のうち、その資源を待って進めなかった時間の割合 (%)
    let psi = [
        ("cpu", kernel::SRC_PSI_CPU, k.psi_cpu_some, k.psi_cpu_full),
        (
            "memory",
            kernel::SRC_PSI_MEM,
            k.psi_mem_some,
            k.psi_mem_full,
        ),
        ("io", kernel::SRC_PSI_IO, k.psi_io_some, k.psi_io_full),
    ];
    if psi.iter().any(|(_, src, _, _)| k.has(*src)) {
        header(
            out,
            "psi_some_avg10",
            "gauge",
            "Share of the last 10 s in which at least one task stalled on this resource (%)",
        );
        for (res, src, some, _) in psi {
            if k.has(src) {
                line(
                    out,
                    "psi_some_avg10",
                    &format!("resource=\"{}\"", res),
                    format!("{:.2}", some),
                );
            }
        }
        header(
            out,
            "psi_full_avg10",
            "gauge",
            "Share of the last 10 s in which every task stalled on this resource (%)",
        );
        for (res, src, _, full) in psi {
            if k.has(src) {
                line(
                    out,
                    "psi_full_avg10",
                    &format!("resource=\"{}\"", res),
                    format!("{:.2}", full),
                );
            }
        }
    }
}

/// 段階ごとの待ち時間 (CONNECT 7 段 / forward 6 段。T14.3 (1)) を**起動からの累計**で
/// 出す (T14.19)。Prometheus のヒストグラムは単調増加が前提なので、`/profile` の窓
/// (1 時間 / 1 日で古い標本が落ちる) の和ではなく
/// [`crate::profile::Profile::stage_totals`] を読む。
///
/// **1 本も観測していないうちは 1 系列も出さない**。`--lite` は段階の時計を読まないので
/// ずっとここを通らない (`kernel` と同じ判断: 源が無いときに 0 を出すと「一瞬で済んだ」と
/// 読めてしまう)。窓へ渡るのは 5 秒ごとなので、**最後の 5 秒ぶんは次の収集で入る**。
///
/// **ホスト別には出さない** (系列が増えすぎる。T12.4 (4) と同じ判断)。
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
    header(
        &mut out,
        "dns_lookups_total",
        "counter",
        "Name resolutions by result (hit = served from the DNS cache, refresh = re-resolved in the background)",
    );
    let [hits, misses, stale, negative, refresh] = crate::dns::counters();
    line(&mut out, "dns_lookups_total", "result=\"hit\"", hits);
    line(&mut out, "dns_lookups_total", "result=\"miss\"", misses);
    line(&mut out, "dns_lookups_total", "result=\"stale\"", stale);
    line(
        &mut out,
        "dns_lookups_total",
        "result=\"negative\"",
        negative,
    );
    // 期限前に裏で引き直した回数 (T13.1)。利用者は待っていないので `miss` とは別系列
    line(&mut out, "dns_lookups_total", "result=\"refresh\"", refresh);
    // keep-warm で裏から引き直し続けている名前の数 (T14.1)。
    // `PROXY_DNS_WARM_SECS=0` (keep-warm を止めた) と `--lite` では 0
    header(
        &mut out,
        "warm_names",
        "gauge",
        "Names kept warm by background re-resolution (PROXY_DNS_WARM_SECS)",
    );
    line(&mut out, "warm_names", "", crate::dns::warm_count());
    // Happy Eyeballs の族ごとの勝敗 (T12.1)。`v4_first` は `/status` に出す
    header(
        &mut out,
        "ipv6_attempts_total",
        "counter",
        "Connections where an IPv6 candidate was tried (Happy Eyeballs)",
    );
    let [attempts, wins, losses] = crate::net::ipv6_counters();
    line(&mut out, "ipv6_attempts_total", "", attempts);
    header(
        &mut out,
        "ipv6_wins_total",
        "counter",
        "Connections established by the IPv6 candidate",
    );
    line(&mut out, "ipv6_wins_total", "", wins);
    header(
        &mut out,
        "ipv6_losses_total",
        "counter",
        "Connections where IPv6 was tried but IPv4 established first",
    );
    line(&mut out, "ipv6_losses_total", "", losses);
    header(
        &mut out,
        "blocklist_entries",
        "gauge",
        "Domains in the blocklist",
    );
    line(&mut out, "blocklist_entries", "", crate::blocklist::len());
    header(
        &mut out,
        "blocklist_blocked_total",
        "counter",
        "Requests refused because the host is on the blocklist",
    );
    line(
        &mut out,
        "blocklist_blocked_total",
        "",
        crate::blocklist::blocked_total(),
    );
    header(&mut out, "requests_total", "counter", "Requests received");
    line(
        &mut out,
        "requests_total",
        "",
        m.total_requests.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "active_connections",
        "gauge",
        "Open client connections",
    );
    line(
        &mut out,
        "active_connections",
        "",
        m.active_connections.load(Ordering::Relaxed),
    );
    // 上限といまのスレッドの数 (`/status` に出しているのと同じ値。T11.5)。
    // `auto` で決まった上限を、起動ログを見なくても監視側から確かめられるようにするためのもの
    header(
        &mut out,
        "max_connections",
        "gauge",
        "Client connection limit (PROXY_MAX_CONNS after auto sizing; 0 = unlimited)",
    );
    line(&mut out, "max_connections", "", conc.max_conns);
    header(
        &mut out,
        "max_threads",
        "gauge",
        "Connection thread limit (PROXY_MAX_THREADS after auto sizing; 0 = unlimited)",
    );
    line(&mut out, "max_threads", "", conc.max_threads);
    // 接続スレッドの数はラベル付きの 1 系列にそろえた (T11.5 の候補 → T12.4 (4))。
    // **旧名 (`sorahost_live_threads` / `sorahost_idle_threads`) も出し続ける** (はじめは 1 版だけのつもりだった。T.28.23 で残すと決めた)
    // ので、監視側は次の版までに `sorahost_threads{state=...}` へ移せる
    header(
        &mut out,
        "threads",
        "gauge",
        "Connection threads by state (replaces sorahost_live_threads / sorahost_idle_threads)",
    );
    line(&mut out, "threads", "state=\"live\"", conc.live_threads);
    line(&mut out, "threads", "state=\"idle\"", conc.idle_threads);
    header(
        &mut out,
        "live_threads",
        "gauge",
        "Connection threads alive (deprecated: use sorahost_threads{state=\"live\"})",
    );
    line(&mut out, "live_threads", "", conc.live_threads);
    header(
        &mut out,
        "idle_threads",
        "gauge",
        "Connection threads waiting for work (deprecated: use sorahost_threads{state=\"idle\"})",
    );
    line(&mut out, "idle_threads", "", conc.idle_threads);
    // プロセス全体の数え物 (接続スレッドとは別。`/proc` を読むのはこのパスだけ。T12.4 (4))
    if let Some(n) = crate::sysinfo::process_threads() {
        header(
            &mut out,
            "process_threads",
            "gauge",
            "Threads in the process (including the watcher, history and connect-attempt threads)",
        );
        line(&mut out, "process_threads", "", n);
    }
    if let Some((fds, max_fds)) = crate::sysinfo::process_fds() {
        header(&mut out, "fds", "gauge", "Open file descriptors");
        line(&mut out, "fds", "", fds);
        header(
            &mut out,
            "max_fds",
            "gauge",
            "File descriptor limit (RLIMIT_NOFILE soft)",
        );
        line(&mut out, "max_fds", "", max_fds);
    }
    header(
        &mut out,
        "queued_jobs",
        "gauge",
        "Jobs waiting because the connection thread limit was reached",
    );
    line(&mut out, "queued_jobs", "", conc.queued_jobs);
    header(
        &mut out,
        "rejected_overload_total",
        "counter",
        "Connections refused with 503 because PROXY_MAX_CONNS was reached",
    );
    line(
        &mut out,
        "rejected_overload_total",
        "",
        m.rejected_overload.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "evicted_idle_total",
        "counter",
        "Idle CONNECT tunnels closed to make room when PROXY_MAX_CONNS was reached",
    );
    line(
        &mut out,
        "evicted_idle_total",
        "",
        m.evicted_idle.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "rejected_client_acl_total",
        "counter",
        "Connections closed right after accept because the peer is not in PROXY_ALLOW_CLIENTS",
    );
    line(
        &mut out,
        "rejected_client_acl_total",
        "",
        m.rejected_client_acl.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "rejected_per_client_total",
        "counter",
        "Connections refused with 503 because the peer was over PROXY_MAX_CONNS_PER_CLIENT",
    );
    line(
        &mut out,
        "rejected_per_client_total",
        "",
        m.rejected_per_client.load(Ordering::Relaxed),
    );
    // 読めずに断った要求の理由別 (T14.28)。8 種で固定なので系列は 8 本きり
    header(
        &mut out,
        "rejected_requests_total",
        "counter",
        "Requests refused before proxying them, by reason (400, 414 or 431)",
    );
    for (i, name) in crate::metrics::BAD_REQUEST_REASON_NAMES.iter().enumerate() {
        line(
            &mut out,
            "rejected_requests_total",
            &format!("reason=\"{}\"", name),
            m.rejected_requests[i].load(Ordering::Relaxed),
        );
    }
    header(
        &mut out,
        "bytes_forwarded_total",
        "counter",
        "Bytes sent to clients and origins",
    );
    line(
        &mut out,
        "bytes_forwarded_total",
        "",
        m.bytes_forwarded.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "origin_connections_total",
        "counter",
        "Origin connections by state",
    );
    line(
        &mut out,
        "origin_connections_total",
        "state=\"new\"",
        m.origin_new.load(Ordering::Relaxed),
    );
    line(
        &mut out,
        "origin_connections_total",
        "state=\"reused\"",
        m.origin_reused.load(Ordering::Relaxed),
    );

    // CONNECT 確立の全体のヒストグラム (Phase 13 が最初に見る数字。T12.4 (4))。
    // ホスト別の `sorahost_host_request_duration_seconds` と違い、**区間は 12 段**で
    // 履歴 (`/history`) と同じ刻み。合計は起動からの累計
    let totals = m.totals();
    header(
        &mut out,
        "connect_seconds",
        "histogram",
        "Time to establish a CONNECT tunnel (name resolution + TCP)",
    );
    window_histogram(&mut out, "connect_seconds", "", &totals.connect);
    // 名前解決のミス 1 回の値段 (デプロイ先ではミス率 26%。Phase 13 の候補 1 の分子)
    let (dns_us, dns_misses) = crate::dns::resolve_cost_total();
    header(
        &mut out,
        "dns_seconds",
        "summary",
        "Time spent in getaddrinfo (cache misses only)",
    );
    let _ = writeln!(out, "sorahost_dns_seconds_sum {}", dns_us as f64 / 1e6);
    let _ = writeln!(out, "sorahost_dns_seconds_count {}", dns_misses);
    // カーネルの平滑化 RTT (`TCP_INFO`。T14.5)。標本は**接続 1 本の終わりに 1 つ**で、
    // **ホスト別は出さない** (系列が増えすぎる)。利用者側 (`client`) と
    // オリジン側 (`origin`) を分けると「物理」と「自分」が切り分けられる
    header(
        &mut out,
        "rtt_seconds",
        "summary",
        "Smoothed kernel RTT (TCP_INFO), sampled once per closed connection",
    );
    let rtt = m.rtt_totals();
    for (side, (us_sum, count)) in [
        ("client", rtt[crate::recent::CLIENT_SIDE]),
        ("origin", rtt[crate::recent::ORIGIN_SIDE]),
    ] {
        let _ = writeln!(
            out,
            "sorahost_rtt_seconds_sum{{side=\"{}\"}} {}",
            side,
            us_sum as f64 / 1e6
        );
        let _ = writeln!(
            out,
            "sorahost_rtt_seconds_count{{side=\"{}\"}} {}",
            side, count
        );
    }
    // canary (T14.10): 利用者の要求が無い時間帯も測っている**最後の 1 回**。
    // まだ 1 回も回っていない (`off`、起動直後、履歴スレッドが無い) ときは 1 行も出さない
    // — 0 秒を出すと「一瞬で繋がった」と読めてしまう
    if let Some(p) = crate::canary::last() {
        header(
            &mut out,
            "canary_seconds",
            "gauge",
            "Last canary probe to the busiest CONNECT target (no TLS, no request)",
        );
        line(
            &mut out,
            "canary_seconds",
            "stage=\"dns\"",
            p.dns_ms as f64 / 1000.0,
        );
        line(
            &mut out,
            "canary_seconds",
            "stage=\"connect\"",
            p.connect_ms as f64 / 1000.0,
        );
        // IPv6 側だけの 1 本 (T14.37)。**繋がらなかった回は 1 行も出さない**
        // (0 秒を出すと「一瞬で繋がった」と読めてしまう。`stage="ipv6_connect"` が
        // 消えていること自体が「コンテナの IPv6 が死んでいる」の印)
        if let Some(v6) = p.ipv6_connect_ms {
            line(
                &mut out,
                "canary_seconds",
                "stage=\"ipv6_connect\"",
                v6 as f64 / 1000.0,
            );
        }
    }
    // 段階ごとの待ち時間 (T14.3 (1) → T14.19)
    stage_metrics(&mut out, m);
    // ロックの取り合い (T14.3 (3))。`try_lock` が失敗した = **本当に待たされた**回数の
    // 累計で、4 つのロックは別々のクレート (統計 / 名前解決 / 預かり所 / ワーカー) に居る
    header(
        &mut out,
        "lock_contention_total",
        "counter",
        "Times a shared lock was already held when a thread took it (try_lock failed)",
    );
    for (lock, n) in crate::sync::LOCK_NAMES
        .iter()
        .zip(crate::sync::lock_contended())
    {
        line(
            &mut out,
            "lock_contention_total",
            &format!("lock=\"{}\"", lock),
            n,
        );
    }
    // エラーの原因 (デプロイ先の「エラー 12 件、原因は不明」を無くす)
    header(
        &mut out,
        "errors_total",
        "counter",
        "Failed requests by cause (loop = 508 Loop Detected)",
    );
    for (i, name) in crate::metrics::ERR_CAUSE_NAMES.iter().enumerate() {
        line(
            &mut out,
            "errors_total",
            &format!("cause=\"{}\"", name),
            totals.errors_by_cause[i],
        );
    }
    // DNS の失敗の内訳 (T26.1)。**先頭 8 つの後ろ**に足すだけで、系列の名前・HELP・
    // 既存のラベルの並びは 1 つも変えていない (式を書いている監視側はそのまま動く)。
    // 値は `/status` の `errors_by_cause` の 9・10 番目と同じで、`dns` (合計) が
    // 動いていても「引き直せば直るのか、名前が無いのか」を監視側から切り分けられる。
    // **0 のときは行ごと出さない**: まだ 1 件も無い内訳に 0 を出すと「見えている」と
    // 読めてしまう (ホスト別の `host_blocked_total` と同じ約束で、`absent` で気づける)
    for (name, n) in crate::metrics::DNS_CAUSE_NAMES
        .iter()
        .zip(m.dns_cause_totals())
    {
        if n > 0 {
            line(&mut out, "errors_total", &format!("cause=\"{}\"", name), n);
        }
    }
    header(
        &mut out,
        "host_requests_total",
        "counter",
        "Requests per origin host",
    );
    header(
        &mut out,
        "host_hits_total",
        "counter",
        "Cache hits per origin host",
    );
    header(
        &mut out,
        "host_misses_total",
        "counter",
        "Cache misses per origin host",
    );
    header(
        &mut out,
        "host_bypass_total",
        "counter",
        "Uncacheable requests per origin host",
    );
    header(
        &mut out,
        "host_errors_total",
        "counter",
        "5xx/502 responses per origin host",
    );
    header(
        &mut out,
        "host_bytes_total",
        "counter",
        "Bytes per origin host",
    );
    let hosts: Vec<_> = m.hosts_sorted().into_iter().take(100).collect();
    for (host, s) in &hosts {
        let l = format!("host=\"{}\"", escape(host));
        line(&mut out, "host_requests_total", &l, s.requests);
        line(&mut out, "host_hits_total", &l, s.hits);
        line(&mut out, "host_misses_total", &l, s.misses);
        line(&mut out, "host_bypass_total", &l, s.bypass);
        line(&mut out, "host_errors_total", &l, s.errors);
        line(&mut out, "host_bytes_total", &l, s.bytes);
    }
    header(
        &mut out,
        "host_blocked_total",
        "counter",
        "Requests refused by the ACL or blocklist per host",
    );
    for (host, s) in &hosts {
        if s.blocked > 0 {
            line(
                &mut out,
                "host_blocked_total",
                &format!("host=\"{}\"", escape(host)),
                s.blocked,
            );
        }
    }
    header(
        &mut out,
        "client_requests_total",
        "counter",
        "Requests per client address",
    );
    header(
        &mut out,
        "client_bytes_total",
        "counter",
        "Bytes per client address",
    );
    header(
        &mut out,
        "client_blocked_total",
        "counter",
        "Refused requests per client address",
    );
    let clients: Vec<_> = m.clients_sorted().into_iter().take(100).collect();
    for (client, c) in &clients {
        let l = format!("client=\"{}\"", escape(client));
        line(&mut out, "client_requests_total", &l, c.requests);
        line(&mut out, "client_bytes_total", &l, c.bytes);
        line(&mut out, "client_blocked_total", &l, c.blocked);
    }
    header(
        &mut out,
        "host_request_duration_seconds",
        "histogram",
        "Response time per origin host (CONNECT: time to establish the tunnel)",
    );
    // 区間が 10 段から 24 段になったので、**ヒストグラムは上位 50 ホストまで**にする
    // (`/status` の `hosts[]` と同じ数)。100 ホスト全部だと上限まで埋まったとき
    // `/metrics` が 389 KiB になり、400 KiB の予算に 1 KiB しか残らなかった (実測)。
    // 数え上げ (`host_requests_total` など) はこれまでどおり上位 100 ホスト
    for (host, s) in hosts.iter().take(50) {
        if s.timed == 0 {
            continue;
        }
        let h = escape(host);
        let mut cum = 0u64;
        for (i, n) in s.buckets.iter().enumerate() {
            cum += n;
            let le = match crate::metrics::LATENCY_BOUNDS_MS.get(i) {
                Some(b) => format!("{}", *b as f64 / 1000.0),
                None => "+Inf".to_string(),
            };
            let _ = writeln!(
                out,
                "sorahost_host_request_duration_seconds_bucket{{host=\"{}\",le=\"{}\"}} {}",
                h, le, cum
            );
        }
        let _ = writeln!(
            out,
            "sorahost_host_request_duration_seconds_sum{{host=\"{}\"}} {}",
            h,
            s.duration_ms_sum as f64 / 1000.0
        );
        let _ = writeln!(
            out,
            "sorahost_host_request_duration_seconds_count{{host=\"{}\"}} {}",
            h, s.timed
        );
    }

    kernel_metrics(&mut out);

    let Some(c) = cache else {
        return out;
    };
    let (mem_bytes, mem_entries) = c.mem_usage();
    let (disk_bytes, disk_entries) = c.disk_usage();
    header(
        &mut out,
        "cache_hits_total",
        "counter",
        "Cache hits by tier",
    );
    line(
        &mut out,
        "cache_hits_total",
        "tier=\"memory\"",
        c.hits_mem.load(Ordering::Relaxed),
    );
    line(
        &mut out,
        "cache_hits_total",
        "tier=\"disk\"",
        c.hits_disk.load(Ordering::Relaxed),
    );
    header(&mut out, "cache_misses_total", "counter", "Cache misses");
    line(
        &mut out,
        "cache_misses_total",
        "",
        c.misses.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_stores_total",
        "counter",
        "Responses stored",
    );
    line(
        &mut out,
        "cache_stores_total",
        "",
        c.stores.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_revalidations_total",
        "counter",
        "Successful revalidations (304)",
    );
    line(
        &mut out,
        "cache_revalidations_total",
        "",
        c.revalidations.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_background_revalidations_total",
        "counter",
        "Revalidations completed in the background",
    );
    line(
        &mut out,
        "cache_background_revalidations_total",
        "",
        c.background_revalidations.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_revalidations_dropped_total",
        "counter",
        "Background revalidations skipped because every worker thread was busy",
    );
    line(
        &mut out,
        "cache_revalidations_dropped_total",
        "",
        c.revalidations_dropped.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_stale_served_total",
        "counter",
        "Expired entries served (grace, origin failure, slow origin)",
    );
    line(
        &mut out,
        "cache_stale_served_total",
        "",
        c.stale_served.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_revalidating",
        "gauge",
        "Background revalidations in flight",
    );
    line(&mut out, "cache_revalidating", "", c.revalidating_count());
    header(
        &mut out,
        "cache_coalesced_total",
        "counter",
        "Requests that waited for an in-flight fetch instead of contacting the origin",
    );
    line(
        &mut out,
        "cache_coalesced_total",
        "",
        c.coalesced.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_not_stored_rotations_total",
        "counter",
        "Rotations of the memory of keys a leader did not store (two rotations forget a key)",
    );
    line(
        &mut out,
        "cache_not_stored_rotations_total",
        "",
        c.not_stored_rotations(),
    );
    header(
        &mut out,
        "cache_inflight",
        "gauge",
        "Origin fetches in flight (coalescing table)",
    );
    line(&mut out, "cache_inflight", "", c.inflight_count());
    header(
        &mut out,
        "cache_admission_rejected_total",
        "counter",
        "Responses not stored because the URL was seen for the first time while the cache was full",
    );
    line(
        &mut out,
        "cache_admission_rejected_total",
        "",
        c.admission_rejected.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_evictions_total",
        "counter",
        "Entries evicted",
    );
    line(
        &mut out,
        "cache_evictions_total",
        "",
        c.evictions.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_bytes_served_total",
        "counter",
        "Bytes served from cache",
    );
    line(
        &mut out,
        "cache_bytes_served_total",
        "",
        c.bytes_served.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_used_bytes",
        "gauge",
        "Bytes held by cache entries",
    );
    line(&mut out, "cache_used_bytes", "tier=\"memory\"", mem_bytes);
    line(&mut out, "cache_used_bytes", "tier=\"disk\"", disk_bytes);
    header(
        &mut out,
        "cache_limit_bytes",
        "gauge",
        "Current cache budget",
    );
    line(
        &mut out,
        "cache_limit_bytes",
        "tier=\"memory\"",
        c.mem_capacity(),
    );
    line(
        &mut out,
        "cache_limit_bytes",
        "tier=\"disk\"",
        c.disk_capacity(),
    );
    header(
        &mut out,
        "cache_reserved_bytes",
        "gauge",
        "Ballast reserved ahead of use",
    );
    line(
        &mut out,
        "cache_reserved_bytes",
        "tier=\"memory\"",
        c.mem_reserved(),
    );
    line(
        &mut out,
        "cache_reserved_bytes",
        "tier=\"disk\"",
        c.disk_reserved(),
    );
    header(&mut out, "cache_entries", "gauge", "Entries per tier");
    line(&mut out, "cache_entries", "tier=\"memory\"", mem_entries);
    line(&mut out, "cache_entries", "tier=\"disk\"", disk_entries);
    let snap = c.snapshot();
    header(
        &mut out,
        "cache_keep_free_bytes",
        "gauge",
        "Dynamic safety margin",
    );
    line(
        &mut out,
        "cache_keep_free_bytes",
        "tier=\"memory\"",
        snap.mem_keep_free,
    );
    line(
        &mut out,
        "cache_keep_free_bytes",
        "tier=\"disk\"",
        snap.disk_keep_free,
    );
    if let Some(mem) = &snap.mem {
        header(
            &mut out,
            "system_memory_bytes",
            "gauge",
            "System memory as seen by the proxy",
        );
        line(&mut out, "system_memory_bytes", "kind=\"total\"", mem.total);
        line(
            &mut out,
            "system_memory_bytes",
            "kind=\"available\"",
            mem.available,
        );
        line(
            &mut out,
            "system_memory_bytes",
            "kind=\"active_file\"",
            mem.active_file,
        );
        if let (Some(l), Some(u)) = (mem.cgroup_limit(), mem.cgroup_usage()) {
            line(&mut out, "system_memory_bytes", "kind=\"cgroup_limit\"", l);
            line(&mut out, "system_memory_bytes", "kind=\"cgroup_usage\"", u);
        }
        header(
            &mut out,
            "system_memory_pressure",
            "gauge",
            "1 when PSI indicates memory pressure",
        );
        line(
            &mut out,
            "system_memory_pressure",
            "",
            u8::from(mem.under_pressure()),
        );
    }
    if let Some(fs) = snap.fs {
        header(
            &mut out,
            "system_disk_bytes",
            "gauge",
            "Disk (or quota) as seen by the proxy",
        );
        line(&mut out, "system_disk_bytes", "kind=\"total\"", fs.total);
        line(
            &mut out,
            "system_disk_bytes",
            "kind=\"available\"",
            fs.available,
        );
    }
    if let Some(rss) = snap.rss {
        header(
            &mut out,
            "process_rss_bytes",
            "gauge",
            "Resident set size of the proxy",
        );
        line(&mut out, "process_rss_bytes", "", rss);
    }
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

/// 版とビルドの種類を **info 型** (値は常に `1`) で出す (T.29.19)。
///
/// `sorahost_boot_id_info` はあるのに版の系列が無く、監視からは「いつ版が変わったか」が読めなかった。
/// ラベルは `/status` の `version` (`0.29.35+b2b26c1` の形) と `build` (`release` / `dist` / `debug`。T.28.42) の
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
mod tests {
    use super::*;
    use crate::metrics::HostOutcome;

    #[test]
    fn renders_counters_and_escaped_labels() {
        let m = Metrics::new();
        m.inc_requests();
        m.record_host("http://a\"b:80", HostOutcome::Hit, 10);
        let text = render(&m, None, Concurrency::default());
        assert!(
            text.contains("# TYPE sorahost_requests_total counter\nsorahost_requests_total 1\n")
        );
        assert!(
            text.contains("sorahost_host_hits_total{host=\"http://a\\\"b:80\"} 1\n"),
            "{}",
            text
        );
        assert!(
            !text.contains("sorahost_cache_hits_total"),
            "no cache section without a cache"
        );
        assert_eq!(escape("x\\y\n"), "x\\\\y\\n");
    }

    /// 上限といまのスレッド数が gauge として出ること (T11.5)。
    /// `/status` に出しているのと同じ 5 つで、値は上の層から渡ったものがそのまま出る。
    #[test]
    fn renders_the_limits_and_the_thread_counts_as_gauges() {
        let m = Metrics::new();
        let text = render(
            &m,
            None,
            Concurrency {
                max_conns: 137,
                max_threads: 41,
                live_threads: 7,
                idle_threads: 3,
                queued_jobs: 2,
            },
        );
        for (name, value) in [
            ("max_connections", 137),
            ("max_threads", 41),
            ("live_threads", 7),
            ("idle_threads", 3),
            ("queued_jobs", 2),
        ] {
            assert!(
                text.contains(&format!("# TYPE sorahost_{} gauge\n", name)),
                "{} の TYPE が無い:\n{}",
                name,
                text
            );
            assert!(
                text.contains(&format!("sorahost_{} {}\n", name, value)),
                "{} の値が違う:\n{}",
                name,
                text
            );
        }
        // 渡されなければ 0 (= 無制限・数えていない)
        let zeroed = render(&m, None, Concurrency::default());
        assert!(
            zeroed.contains("sorahost_max_connections 0\n"),
            "{}",
            zeroed
        );
    }

    /// 接続スレッドの数が新旧両方の名前で出ること (T12.4 (4))。旧名は 1 版だけ残す。
    #[test]
    fn thread_gauges_are_rendered_under_both_the_old_and_the_new_name() {
        let m = Metrics::new();
        let text = render(
            &m,
            None,
            Concurrency {
                live_threads: 7,
                idle_threads: 3,
                ..Concurrency::default()
            },
        );
        for pat in [
            "sorahost_threads{state=\"live\"} 7\n",
            "sorahost_threads{state=\"idle\"} 3\n",
            "sorahost_live_threads 7\n",
            "sorahost_idle_threads 3\n",
        ] {
            assert!(text.contains(pat), "{} が無い:\n{}", pat, text);
        }
    }

    /// CONNECT 確立のヒストグラム・名前解決・エラーの原因が出ること (T12.4 (4))。
    #[test]
    fn renders_the_connect_histogram_the_dns_summary_and_the_error_causes() {
        use std::time::Duration;
        let m = Metrics::new();
        m.record_host_timed(
            "connect://a:443",
            HostOutcome::Bypass,
            0,
            Duration::from_millis(257),
        );
        // 繋がらなかった 1 本 (30 秒で `timeout`)。**確立のヒストグラムには入らない**
        // (T20.4。`errors_total` には残る)
        m.record_host_detail(
            "connect://b:443",
            HostOutcome::Error,
            0,
            Some(Duration::from_millis(30_000)),
            crate::metrics::Detail {
                cause: Some(crate::metrics::ErrCause::Timeout),
                ..crate::metrics::Detail::default()
            },
        );
        let text = render(&m, None, Concurrency::default());
        assert!(
            text.contains("# TYPE sorahost_connect_seconds histogram\n"),
            "{}",
            text
        );
        // 12 段 + `+Inf` で、累積になっていること
        assert!(
            text.contains("sorahost_connect_seconds_bucket{le=\"0.25\"} 0\n"),
            "{}",
            text
        );
        assert!(
            text.contains("sorahost_connect_seconds_bucket{le=\"0.5\"} 1\n"),
            "{}",
            text
        );
        assert!(
            text.contains("sorahost_connect_seconds_bucket{le=\"+Inf\"} 1\n"),
            "{}",
            text
        );
        assert!(
            text.contains("sorahost_connect_seconds_count 1\n"),
            "{}",
            text
        );
        assert!(
            text.contains("sorahost_connect_seconds_sum 0.257\n"),
            "{}",
            text
        );
        assert!(text.contains("sorahost_dns_seconds_count "), "{}", text);
        assert!(
            text.contains("sorahost_errors_total{cause=\"timeout\"} 1\n"),
            "{}",
            text
        );
        assert!(
            text.contains("sorahost_errors_total{cause=\"loop\"} 0\n"),
            "{}",
            text
        );
    }

    /// DNS の失敗の内訳が `errors_total` の**うしろ**に出ること (T26.1)。
    ///
    /// - 先頭 8 つの並びは今までどおりで、内訳は `cause="other"` の後ろ
    /// - 値はホスト別の内訳の**合計** (2 ホストに 1 件ずつ入れて 2 になる)
    /// - **0 のときは行ごと出さない** (既存の `host_blocked_total` と同じ約束)
    #[test]
    fn renders_the_dns_breakdown_after_the_eight_causes() {
        use crate::metrics::{Detail, DnsCause, ErrCause};
        let m = Metrics::new();
        for (host, dns) in [
            ("connect://a:443", DnsCause::Nxdomain),
            ("connect://b:443", DnsCause::Nxdomain),
        ] {
            m.record_host_detail(
                host,
                HostOutcome::Error,
                0,
                Some(std::time::Duration::from_millis(1)),
                Detail {
                    cause: Some(ErrCause::Dns),
                    dns_cause: Some(dns),
                    ..Detail::default()
                },
            );
        }
        let text = render(&m, None, Concurrency::default());
        // `dns` は合計のまま 2、内訳がその後ろに 2
        assert!(
            text.contains("sorahost_errors_total{cause=\"dns\"} 2\n"),
            "{}",
            text
        );
        assert!(
            text.contains("sorahost_errors_total{cause=\"nxdomain\"} 2\n"),
            "{}",
            text
        );
        assert!(
            !text.contains("cause=\"dns_temp\""),
            "0 の内訳は行ごと出さない: {}",
            text
        );
        // 並び: 8 つの最後 (`other`) → `nxdomain` → `dns_temp` (出るとき)
        let other = text
            .lines()
            .position(|l| l == "sorahost_errors_total{cause=\"other\"} 0")
            .expect("`other` が無い");
        let nx = text
            .lines()
            .position(|l| l == "sorahost_errors_total{cause=\"nxdomain\"} 2")
            .expect("`nxdomain` が無い");
        assert_eq!(nx, other + 1, "内訳は 8 つの**すぐ後ろ**:\n{}", text);
        // `/status` の側 (ホスト別の `errors_by_cause` の 9 番目) と同じ値
        let hosts = m.hosts_sorted();
        assert_eq!(hosts.len(), 2);
        let per_host: u64 = hosts.iter().map(|(_, s)| s.dns_by_cause[0]).sum();
        assert_eq!(per_host, 2, "ホスト別は 1 件ずつ:\n{}", text);
        assert_eq!(m.dns_cause_totals(), [2, 0]);

        // 続けて `dns_temp` も 1 件 → 2 本そろって 8 つの後ろに並ぶ
        m.record_host_detail(
            "connect://c:443",
            HostOutcome::Error,
            0,
            Some(std::time::Duration::from_millis(1)),
            Detail {
                cause: Some(ErrCause::Dns),
                dns_cause: Some(DnsCause::DnsTemp),
                ..Detail::default()
            },
        );
        let text = render(&m, None, Concurrency::default());
        let nx = text
            .lines()
            .position(|l| l == "sorahost_errors_total{cause=\"nxdomain\"} 2")
            .expect("`nxdomain` が無い");
        assert!(
            text.lines()
                .nth(nx + 1)
                .is_some_and(|l| l == "sorahost_errors_total{cause=\"dns_temp\"} 1"),
            "`nxdomain` の次が `dns_temp` でない:\n{}",
            text
        );
        assert_eq!(m.dns_cause_totals(), [2, 1]);
    }

    /// 上限まで埋めても `/status` 64 KiB / `/metrics` 400 KiB に収まること (T12.4 (3))。
    #[test]
    fn the_endpoints_stay_within_their_size_budget_when_the_tables_are_full() {
        let m = Metrics::new();
        for i in 0..crate::metrics::MAX_HOSTS {
            // 長めのホスト名 + 全部の区間に件数が入っている最悪の形
            let host = format!("connect://very-long-host-name-{:04}.example.com:443", i);
            for ms in [1, 5, 30, 120, 400, 900, 3000, 12_000] {
                m.record_host_timed(
                    &host,
                    HostOutcome::Bypass,
                    1 << 30,
                    std::time::Duration::from_millis(ms),
                );
            }
        }
        for i in 0..crate::metrics::MAX_CLIENTS {
            m.record_client(
                &format!("2001:db8:1234:5678:9abc:def0:1234:{:04x}", i),
                HostOutcome::Bypass,
                1 << 30,
                (1 << 20, 1 << 30),
                Some(std::time::Duration::from_millis(400)),
                Some("very-long-host-name.example.com:443"),
            );
        }
        let status = m.to_json();
        assert!(status.len() <= 64 * 1024, "/status が {} B", status.len());
        let before = render(&m, None, Concurrency::default()).len();
        // 段階の 13 本が全部の区間に埋まった最悪 (T14.19)
        let mut stages = crate::profile::Stages::default();
        for ms in [
            0, 1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 30_000,
        ] {
            let d = crate::metrics::Detail {
                dns_ms: ms,
                connect_ms: ms,
                first_byte_ms: Some(ms),
                stages: crate::metrics::StageMs {
                    queue: ms as u32,
                    client_read: ms as u32,
                    first_relay: ms as u32,
                    relay: ms as u32,
                    park: ms as u32,
                    send: ms as u32,
                    body: ms as u32,
                },
                ..crate::metrics::Detail::default()
            };
            stages.observe_connect(&d);
            stages.observe_forward(&d);
        }
        m.profile.push(crate::profile::Sample {
            t: 1,
            stages,
            ..crate::profile::Sample::default()
        });
        let text = render(&m, None, Concurrency::default());
        assert!(text.len() <= 400 * 1024, "/metrics が {} B", text.len());
        // T14.19 の増分 (段階 + keep-warm + ロック) は 20 KiB を超えない
        assert!(
            text.len() - before <= 20 * 1024,
            "段階の増分が {} B (表が満杯のとき /metrics は {} B)",
            text.len() - before,
            text.len()
        );
    }

    /// 段階の窓を 1 標本ぶん積む (T14.19 のテスト用。CONNECT 1 本 + forward 1 要求)。
    fn push_one_sample(m: &Metrics) {
        use crate::metrics::StageMs;
        let mut stages = crate::profile::Stages::default();
        stages.observe_connect(&crate::metrics::Detail {
            dns_ms: 30,
            connect_ms: 4,
            stages: StageMs {
                // `queue` と `park` は 0 ms = 速い道 (読むときに足し戻される)
                client_read: 1,
                first_relay: 120,
                relay: 900,
                ..StageMs::default()
            },
            ..crate::metrics::Detail::default()
        });
        stages.observe_forward(&crate::metrics::Detail {
            first_byte_ms: Some(60),
            stages: StageMs {
                queue: 2,
                client_read: 1,
                body: 7,
                ..StageMs::default()
            },
            ..crate::metrics::Detail::default()
        });
        m.profile.push(crate::profile::Sample {
            t: 1,
            stages,
            ..crate::profile::Sample::default()
        });
    }

    /// 段階・keep-warm・ロックの取り合いが `/metrics` に出ること (T14.19)。
    /// 段階は **12 段 + `+Inf` の累積**、値は秒、累計は起動から。
    #[test]
    fn renders_the_stage_histograms_the_warm_gauge_and_the_lock_counters() {
        let m = Metrics::new();
        push_one_sample(&m);
        let text = render(&m, None, Concurrency::default());
        assert_eq!(
            text.matches("# TYPE sorahost_stage_seconds histogram\n")
                .count(),
            1,
            "HELP / TYPE は族に 1 組だけ:\n{}",
            text
        );
        for pat in [
            // 0 ms の観測は `≤ 1 ms` の区間に足し戻る (`queue` / `park`)
            "sorahost_stage_seconds_bucket{kind=\"connect\",stage=\"queue\",le=\"0.001\"} 1\n",
            "sorahost_stage_seconds_sum{kind=\"connect\",stage=\"queue\"} 0\n",
            "sorahost_stage_seconds_count{kind=\"connect\",stage=\"queue\"} 1\n",
            // 名前解決 30 ms → `≤ 25 ms` には入らず `≤ 50 ms` に入る
            "sorahost_stage_seconds_bucket{kind=\"connect\",stage=\"dns\",le=\"0.025\"} 0\n",
            "sorahost_stage_seconds_bucket{kind=\"connect\",stage=\"dns\",le=\"0.05\"} 1\n",
            "sorahost_stage_seconds_bucket{kind=\"connect\",stage=\"dns\",le=\"+Inf\"} 1\n",
            "sorahost_stage_seconds_sum{kind=\"connect\",stage=\"dns\"} 0.03\n",
            "sorahost_stage_seconds_count{kind=\"connect\",stage=\"dns\"} 1\n",
            // トンネル越しの TLS 握手 120 ms と中継 900 ms
            "sorahost_stage_seconds_sum{kind=\"connect\",stage=\"first_relay\"} 0.12\n",
            "sorahost_stage_seconds_sum{kind=\"connect\",stage=\"relay\"} 0.9\n",
            // forward の初バイト 60 ms は `≤ 100 ms`
            "sorahost_stage_seconds_bucket{kind=\"forward\",stage=\"ttfb\",le=\"0.05\"} 0\n",
            "sorahost_stage_seconds_bucket{kind=\"forward\",stage=\"ttfb\",le=\"0.1\"} 1\n",
            "sorahost_stage_seconds_sum{kind=\"forward\",stage=\"body\"} 0.007\n",
            // keep-warm な名前の数 (T14.1) と ロックの取り合い (T14.3 (3))
            "# TYPE sorahost_warm_names gauge\nsorahost_warm_names ",
            "# TYPE sorahost_lock_contention_total counter\n",
            "sorahost_lock_contention_total{lock=\"stats\"} ",
            "sorahost_lock_contention_total{lock=\"workers\"} ",
        ] {
            assert!(text.contains(pat), "{} が無い:\n{}", pat, text);
        }
        // 13 段 (CONNECT 7 + forward 6) × (12 段 + `+Inf`) と `_sum` / `_count`
        let stages = crate::profile::CONNECT_STAGES.len() + crate::profile::FORWARD_STAGES.len();
        assert_eq!(stages, 13);
        assert_eq!(
            text.matches("sorahost_stage_seconds_bucket{").count(),
            stages * (crate::history::WINDOW_BOUNDS_MS.len() + 1)
        );
        assert_eq!(text.matches("sorahost_stage_seconds_sum{").count(), stages);
        assert_eq!(
            text.matches("sorahost_stage_seconds_count{").count(),
            stages
        );
        assert_eq!(
            text.matches("sorahost_lock_contention_total{").count(),
            crate::sync::LOCK_NAMES.len()
        );
    }

    /// **1 本も観測していないうちは段階の系列を 1 本も出さない** (T14.19)。
    /// `--lite` は段階の時計を読まないので、ずっとこの形 (`kernel` と同じ判断)。
    #[test]
    fn the_stage_series_are_absent_until_the_first_observation() {
        let m = Metrics::new();
        let text = render(&m, None, Concurrency::default());
        assert!(!text.contains("sorahost_stage_seconds"), "{}", text);
        // 段階に依らないもの (keep-warm とロック) は `--lite` でも出る
        assert!(text.contains("sorahost_warm_names "), "{}", text);
        assert!(
            text.contains("sorahost_lock_contention_total{lock=\"dns\"} "),
            "{}",
            text
        );
    }

    /// 「合流を飛ばす鍵」の記憶を入れ替えた回数が `/metrics` にも出ること (T12.6)。
    #[test]
    fn renders_the_not_stored_rotations_counter() {
        let m = Metrics::new();
        let cache = Cache::new(crate::cache::CacheConfig::disabled());
        let text = render(&m, Some(&cache), Concurrency::default());
        assert!(
            text.contains(
                "# TYPE sorahost_cache_not_stored_rotations_total counter\nsorahost_cache_not_stored_rotations_total 0\n"
            ),
            "{}",
            text
        );
    }

    /// 版とビルドの種類が `sorahost_build_info{version,build} 1` の 1 行で出ること (T.29.19)。
    #[test]
    fn renders_the_build_info_with_two_labels() {
        let mut out = String::new();
        build_info(&mut out, "0.29.35+b2b26c1", "release");
        assert!(
            out.contains("# TYPE sorahost_build_info gauge\n"),
            "{}",
            out
        );
        assert!(
            out.contains("sorahost_build_info{version=\"0.29.35+b2b26c1\",build=\"release\"} 1\n"),
            "{}",
            out
        );
        let mut quoted = String::new();
        build_info(&mut quoted, "a\"b", "c");
        assert!(quoted.contains("version=\"a\\\"b\""), "{}", quoted);
    }

    /// 起動の id と heap の基準が `/metrics` に出ること (T25.3)。
    ///
    /// **出せないときは行ごと出さない** (`sorahost_kernel_*` と同じ判断)。0 や空の `id` を
    /// 出すと「起動が変わっていない」「増えが 0」と読めてしまう。
    #[test]
    fn renders_the_boot_id_and_the_heap_baseline() {
        // info 型: 値は常に 1、ラベルは `id` の 1 つだけ (系列は増えない)
        let mut out = String::new();
        boot_id_info(&mut out, Some("0f1e2d3c-aaaa-bbbb-cccc-1234567890ab"));
        assert!(
            out.contains("# TYPE sorahost_boot_id_info gauge\n"),
            "{}",
            out
        );
        assert!(
            out.contains("sorahost_boot_id_info{id=\"0f1e2d3c-aaaa-bbbb-cccc-1234567890ab\"} 1\n"),
            "{}",
            out
        );
        // ラベルは `escape` を通す (id に `"` が混ざっても形が壊れない)
        let mut quoted = String::new();
        boot_id_info(&mut quoted, Some("a\"b"));
        assert!(
            quoted.contains("sorahost_boot_id_info{id=\"a\\\"b\"} 1\n"),
            "{}",
            quoted
        );
        let mut none = String::new();
        boot_id_info(&mut none, None);
        assert_eq!(none, "", "読めない環境では系列ごと出さない");

        // 基準: バイトと、それを取った epoch 秒
        let mut heap = String::new();
        heap_baseline(&mut heap, Some((1_767_225_600, 15_572_256)));
        assert!(
            heap.contains(
                "# TYPE sorahost_heap_baseline_bytes gauge\nsorahost_heap_baseline_bytes 15572256\n"
            ),
            "{}",
            heap
        );
        assert!(
            heap.contains(
                "# TYPE sorahost_heap_baseline_at_seconds gauge\nsorahost_heap_baseline_at_seconds 1767225600\n"
            ),
            "{}",
            heap
        );
        let mut no_base = String::new();
        heap_baseline(&mut no_base, None);
        assert_eq!(no_base, "", "まだ 60 秒たっていない起動では出さない");
    }
}
