//! `/metrics` のうちプロセス全体の数え物 (名前解決・IPv6・ブロックリスト・要求・オリジン・確立・canary・ロック・エラー)。
//!
//! T34.27 で `prom.rs` の `render` から割った。行を動かしただけで、出す順も文面も変えていない。

use super::*;
use crate::metrics::Interval;

/// 名前解決の数え物 (`dns_lookups_total`・`warm_names`)。
pub(super) fn dns_metrics(out: &mut String) {
    header(
        out,
        "dns_lookups_total",
        "counter",
        "Name resolutions by result (hit = served from the DNS cache, refresh = re-resolved in the background)",
    );
    let [hits, misses, stale, negative, refresh] = crate::dns::counters();
    line(out, "dns_lookups_total", "result=\"hit\"", hits);
    line(out, "dns_lookups_total", "result=\"miss\"", misses);
    line(out, "dns_lookups_total", "result=\"stale\"", stale);
    line(out, "dns_lookups_total", "result=\"negative\"", negative);
    // 期限前に裏で引き直した回数 (T13.1)。利用者は待っていないので `miss` とは別系列
    line(out, "dns_lookups_total", "result=\"refresh\"", refresh);
    // keep-warm で裏から引き直し続けている名前の数 (T14.1)。
    // `PROXY_DNS_WARM_SECS=0` (keep-warm を止めた) と `--lite` では 0
    header(
        out,
        "warm_names",
        "gauge",
        "Names kept warm by background re-resolution (PROXY_DNS_WARM_SECS)",
    );
    line(out, "warm_names", "", crate::dns::warm_count());
}

/// Happy Eyeballs の族ごとの勝敗 (`ipv6_*`) と、確立の時間切れの表 (`connect_negative_*`。T36.8)。
pub(super) fn ipv6_metrics(out: &mut String) {
    // Happy Eyeballs の族ごとの勝敗 (T12.1)。`v4_first` は `/status` に出す
    header(
        out,
        "ipv6_attempts_total",
        "counter",
        "Connections where an IPv6 candidate was tried (Happy Eyeballs)",
    );
    let [attempts, wins, losses] = crate::net::ipv6_counters();
    line(out, "ipv6_attempts_total", "", attempts);
    header(
        out,
        "ipv6_wins_total",
        "counter",
        "Connections established by the IPv6 candidate",
    );
    line(out, "ipv6_wins_total", "", wins);
    header(
        out,
        "ipv6_losses_total",
        "counter",
        "Connections where IPv6 was tried but IPv4 established first",
    );
    line(out, "ipv6_losses_total", "", losses);
    // 続けて時間切れになった宛先を待たずに断る表 (T36.8。`PROXY_CONNECT_NEGATIVE_SECS`)
    let [entries, skipped] = crate::net::connect_negative_counters();
    header(
        out,
        "connect_negative_entries",
        "gauge",
        "Targets remembered after connect timeouts (PROXY_CONNECT_NEGATIVE_SECS)",
    );
    line(out, "connect_negative_entries", "", entries);
    header(
        out,
        "connect_negative_skipped_total",
        "counter",
        "Connects refused with 504 without trying, after the target timed out twice in a row",
    );
    line(out, "connect_negative_skipped_total", "", skipped);
}

/// ブロックリスト (`blocklist_*`)。
pub(super) fn blocklist_metrics(out: &mut String) {
    header(
        out,
        "blocklist_entries",
        "gauge",
        "Domains in the blocklist",
    );
    line(out, "blocklist_entries", "", crate::blocklist::len());
    header(
        out,
        "blocklist_blocked_total",
        "counter",
        "Requests refused because the host is on the blocklist",
    );
    line(
        out,
        "blocklist_blocked_total",
        "",
        crate::blocklist::blocked_total(),
    );
}

/// 要求と接続とスレッドの数、断った数 (`requests_total` 〜 `silent_closes_total`)。
pub(super) fn request_metrics(out: &mut String, m: &Metrics, conc: Concurrency) {
    header(out, "requests_total", "counter", "Requests received");
    line(
        out,
        "requests_total",
        "",
        m.total_requests.load(Ordering::Relaxed),
    );
    header(
        out,
        "active_connections",
        "gauge",
        "Open client connections",
    );
    line(
        out,
        "active_connections",
        "",
        m.active_connections.load(Ordering::Relaxed),
    );
    // 上限といまのスレッドの数 (`/status` に出しているのと同じ値。T11.5)。
    // `auto` で決まった上限を、起動ログを見なくても監視側から確かめられるようにするためのもの
    header(
        out,
        "max_connections",
        "gauge",
        "Client connection limit (PROXY_MAX_CONNS after auto sizing; 0 = unlimited)",
    );
    line(out, "max_connections", "", conc.max_conns);
    header(
        out,
        "max_threads",
        "gauge",
        "Connection thread limit (PROXY_MAX_THREADS after auto sizing; 0 = unlimited)",
    );
    line(out, "max_threads", "", conc.max_threads);
    // 接続スレッドの数はラベル付きの 1 系列にそろえた (T11.5 の候補 → T12.4)。
    // **旧名 (`sorahost_live_threads` / `sorahost_idle_threads`) も出し続ける** (はじめは 1 版だけのつもりだった。T28.23 で残すと決めた)
    // ので、監視側は次の版までに `sorahost_threads{state=...}` へ移せる
    header(
        out,
        "threads",
        "gauge",
        "Connection threads by state (replaces sorahost_live_threads / sorahost_idle_threads)",
    );
    line(out, "threads", "state=\"live\"", conc.live_threads);
    line(out, "threads", "state=\"idle\"", conc.idle_threads);
    header(
        out,
        "live_threads",
        "gauge",
        "Connection threads alive (deprecated: use sorahost_threads{state=\"live\"})",
    );
    line(out, "live_threads", "", conc.live_threads);
    header(
        out,
        "idle_threads",
        "gauge",
        "Connection threads waiting for work (deprecated: use sorahost_threads{state=\"idle\"})",
    );
    line(out, "idle_threads", "", conc.idle_threads);
    // プロセス全体の数え物 (接続スレッドとは別。`/proc` を読むのはこのパスだけ。T12.4)
    if let Some(n) = crate::sysinfo::process_threads() {
        header(
            out,
            "process_threads",
            "gauge",
            "Threads in the process (including the watcher, history and connect-attempt threads)",
        );
        line(out, "process_threads", "", n);
    }
    if let Some((fds, max_fds)) = crate::sysinfo::process_fds() {
        header(out, "fds", "gauge", "Open file descriptors");
        line(out, "fds", "", fds);
        header(
            out,
            "max_fds",
            "gauge",
            "File descriptor limit (RLIMIT_NOFILE soft)",
        );
        line(out, "max_fds", "", max_fds);
    }
    header(
        out,
        "queued_jobs",
        "gauge",
        "Jobs waiting because the connection thread limit was reached",
    );
    line(out, "queued_jobs", "", conc.queued_jobs);
    header(
        out,
        "rejected_overload_total",
        "counter",
        "Connections refused with 503 because PROXY_MAX_CONNS was reached",
    );
    line(
        out,
        "rejected_overload_total",
        "",
        m.rejected_overload.load(Ordering::Relaxed),
    );
    header(
        out,
        "evicted_idle_total",
        "counter",
        "Idle CONNECT tunnels closed to make room when PROXY_MAX_CONNS was reached",
    );
    line(
        out,
        "evicted_idle_total",
        "",
        m.evicted_idle.load(Ordering::Relaxed),
    );
    header(
        out,
        "rejected_client_acl_total",
        "counter",
        "Connections closed right after accept because the peer is not in PROXY_ALLOW_CLIENTS",
    );
    line(
        out,
        "rejected_client_acl_total",
        "",
        m.rejected_client_acl.load(Ordering::Relaxed),
    );
    header(
        out,
        "rejected_per_client_total",
        "counter",
        "Connections refused with 503 because the peer was over PROXY_MAX_CONNS_PER_CLIENT",
    );
    line(
        out,
        "rejected_per_client_total",
        "",
        m.rejected_per_client.load(Ordering::Relaxed),
    );
    // 読めずに断った要求の理由別 (T14.28)。10 種で固定なので系列は 10 本きり
    header(
        out,
        "rejected_requests_total",
        "counter",
        "Requests refused before proxying them, by reason (400, 414 or 431)",
    );
    for (i, name) in crate::metrics::BAD_REQUEST_REASON_NAMES.iter().enumerate() {
        line(
            out,
            "rejected_requests_total",
            &format!("reason=\"{}\"", name),
            m.rejected_requests[i].load(Ordering::Relaxed),
        );
    }
    // 1 バイトも来ないまま締め切りを過ぎ、408 を返さずに閉じた接続 (先張りの接続。T33.0)
    header(
        out,
        "silent_closes_total",
        "counter",
        "Connections closed without a 408 because not a byte arrived within PROXY_HEADER_TIMEOUT_SECS",
    );
    line(
        out,
        "silent_closes_total",
        "",
        m.silent_closes.load(Ordering::Relaxed),
    );
}

/// 流した量とオリジンへの接続 (`bytes_forwarded_total`・`origin_connections_total`)。
pub(super) fn origin_metrics(out: &mut String, m: &Metrics) {
    header(
        out,
        "bytes_forwarded_total",
        "counter",
        "Bytes sent to clients and origins",
    );
    line(
        out,
        "bytes_forwarded_total",
        "",
        m.bytes_forwarded.load(Ordering::Relaxed),
    );
    header(
        out,
        "origin_connections_total",
        "counter",
        "Origin connections by state",
    );
    line(
        out,
        "origin_connections_total",
        "state=\"new\"",
        m.origin_new.load(Ordering::Relaxed),
    );
    line(
        out,
        "origin_connections_total",
        "state=\"reused\"",
        m.origin_reused.load(Ordering::Relaxed),
    );
}

/// CONNECT の確立・名前解決・カーネルの RTT (`connect_seconds`・`dns_seconds`・`rtt_seconds`)。
pub(super) fn connect_metrics(out: &mut String, m: &Metrics, totals: &Interval) {
    // CONNECT 確立の全体のヒストグラム (Phase 13 が最初に見る数字。T12.4)。
    // ホスト別の `sorahost_host_request_duration_seconds` と違い、**区間は 12 段**で
    // 履歴 (`/history`) と同じ刻み。合計は起動からの累計
    header(
        out,
        "connect_seconds",
        "histogram",
        "Time to establish a CONNECT tunnel (name resolution + TCP)",
    );
    window_histogram(out, "connect_seconds", "", &totals.connect);
    // 名前解決のミス 1 回の値段 (デプロイ先ではミス率 26%。Phase 13 の候補 1 の分子)
    let (dns_us, dns_misses) = crate::dns::resolve_cost_total();
    header(
        out,
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
        out,
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
}

/// canary の最後の 1 回 (`canary_seconds`)。
pub(super) fn canary_metrics(out: &mut String) {
    // canary (T14.10): 利用者の要求が無い時間帯も測っている**最後の 1 回**。
    // まだ 1 回も回っていない (`off`、起動直後、履歴スレッドが無い) ときは 1 行も出さない
    // — 0 秒を出すと「一瞬で繋がった」と読めてしまう
    if let Some(p) = crate::canary::last() {
        header(
            out,
            "canary_seconds",
            "gauge",
            "Last canary probe to the busiest CONNECT target (no TLS, no request)",
        );
        line(
            out,
            "canary_seconds",
            "stage=\"dns\"",
            p.dns_ms as f64 / 1000.0,
        );
        line(
            out,
            "canary_seconds",
            "stage=\"connect\"",
            p.connect_ms as f64 / 1000.0,
        );
        // IPv6 側だけの 1 本 (T14.37)。**繋がらなかった回は 1 行も出さない**
        // (0 秒を出すと「一瞬で繋がった」と読めてしまう。`stage="ipv6_connect"` が
        // 消えていること自体が「コンテナの IPv6 が死んでいる」の印)
        if let Some(v6) = p.ipv6_connect_ms {
            line(
                out,
                "canary_seconds",
                "stage=\"ipv6_connect\"",
                v6 as f64 / 1000.0,
            );
        }
    }
}

/// ロックの取り合い (`lock_contention_total`)。
pub(super) fn lock_metrics(out: &mut String) {
    // ロックの取り合い (T14.3)。`try_lock` が失敗した = **本当に待たされた**回数の
    // 累計で、4 つのロックは別々のクレート (統計 / 名前解決 / 預かり所 / ワーカー) に居る
    header(
        out,
        "lock_contention_total",
        "counter",
        "Times a shared lock was already held when a thread took it (try_lock failed)",
    );
    for (lock, n) in crate::sync::LOCK_NAMES
        .iter()
        .zip(crate::sync::lock_contended())
    {
        line(
            out,
            "lock_contention_total",
            &format!("lock=\"{}\"", lock),
            n,
        );
    }
}

/// エラーの原因 (`errors_total`。8 つの原因と名前解決の内訳)。
pub(super) fn error_metrics(out: &mut String, m: &Metrics, totals: &Interval) {
    // エラーの原因 (デプロイ先の「エラー 12 件、原因は不明」を無くす)
    header(
        out,
        "errors_total",
        "counter",
        "Failed requests by cause (loop = 508 Loop Detected)",
    );
    for (i, name) in crate::metrics::ERR_CAUSE_NAMES.iter().enumerate() {
        line(
            out,
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
            line(out, "errors_total", &format!("cause=\"{}\"", name), n);
        }
    }
}
