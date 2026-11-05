//! ブラウザで開くページ (`/dashboard` `/inspect` `/probe.html`) と、自分宛ての `/` に返す案内。
//!
//! HTML 3 本の実体は葉クレート `proxy-web` (`crates/web/src/*.html`) に置いてある
//! (T15.12 段 1: 163 KB の文字列定数を このクレートの rustc から外に出すため)。
//! 綴りを変えないよう、ここでは今までと同じ名前の私有 const で受ける。

pub(super) const DASHBOARD_HTML: &str = proxy_web::DASHBOARD_HTML;

/// 「調査」ページ (T14.8)。`/dashboard` が「いま」を見る画面なのに対して、
/// **起きたことを時間軸で読む**ための別のページ (個票を描く)。
/// `--lite` でも 200 で返す (記録が無ければページの中で「記録していません」と出る)。
pub(super) const INSPECT_HTML: &str = proxy_web::INSPECT_HTML;

/// 「端末から測る」ページ (T14.33)。プロキシ側の計測は「プロキシに届いてから」しか
/// 見えないので、**利用者のブラウザから** `/status` の往復と、プロキシ経由で小さな URL を
/// 取る時間を測り、`/clients` の自分の行 (T14.7) と `rtt_ms` (T14.5) に並べる。
/// 測った値はサーバーへ送らない (端末の中だけ)。`--lite` でも 200 で返す
/// (接続元を記録していないことはページの中で伝える)。
pub(super) const PROBE_HTML: &str = proxy_web::PROBE_HTML;

/// 自分宛ての `/` に返す案内 (エンドポイントの一覧)。`--lite` では `/dashboard` を載せない。
pub(super) fn endpoint_list(lite: bool) -> String {
    let dashboard = if lite {
        ""
    } else {
        "  /dashboard                                  control panel (graphs, per-host stats)\n"
    };
    format!(
        "rust-http-proxy - an HTTP/HTTPS(CONNECT) forward proxy.\n\n\
         This is the proxy itself, not a web site. Point your browser or client at\n\
         this address as an HTTP proxy (or use /proxy.pac below).\n\n\
         endpoints:\n\
         {}\
         \x20 /inspect                                    control panel: what happened (timeline)\n\
         \x20 /probe.html                                 measure this proxy from your browser\n\
         \x20 /status[?sort=errors|dns|slow&hosts=0]      JSON: counters, hosts, cache, threads\n\
         \x20 /errors?n=100                               JSON: the last errors (who, when, why)\n\
         \x20 /connections                                JSON: the connections open right now\n\
         \x20 /recent?n=200&since=&client=&sort=&offset=  JSON: the connections that closed\n\
         \x20 /bursts?n=50                                JSON: snapshots taken at each spike\n\
         \x20 /trace?n=200&since=                         JSON: one client's requests (PROXY_TRACE_CLIENT)\n\
         \x20 /events?n=200&since=                        JSON: starts, reloads, and other events\n\
         \x20 /snapshot                                   JSON: everything above in one request\n\
         \x20 /snapshots                                  JSON: the daily, start and shutdown snapshots kept on disk\n\
         \x20 /snapshots/<YYYY-MM-DD>                     JSON: one saved day (as taken)\n\
         \x20 /snapshots/<time>-start|shutdown            JSON: one start or shutdown snapshot (as taken)\n\
         \x20 /dns?sort=age|host|misses&offset=          JSON: the resolver cache table\n\
         \x20 /log?n=200                                  JSON: the last warnings and errors\n\
         \x20 /hosts?sort=&limit=200&offset=              JSON: every host (/status keeps 50)\n\
         \x20 /hosts/series?top=16&host=<name>            JSON: per-host series (5 min x 24 h)\n\
         \x20 /clients?sort=&limit=200                    JSON: every client (agent, targets, ports)\n\
         \x20 /readers                                    JSON: who reads these endpoints (scan or monitor)\n\
         \x20 /explain?host=<name> | ?client=<ip>         JSON: one peer, explained in one page\n\
         \x20 /config                                     JSON: effective settings and where they came from\n\
         \x20 /healthz                                    health checks (503 when unhealthy)\n\
         \x20 /history?res=5|60|3600&n=720                JSON: time series (res=5 keeps 6 h)\n\
         \x20 /history?since=&until=&summary=1            JSON: one summary row for a period\n\
         \x20 /profile?res=5|60&n=&offset=&summary=1      JSON: stages, threads, locks\n\
         \x20 /daily?n=365                                JSON: one summary line per day (kept forever)\n\
         \x20 /slo?days=7                                 JSON: how much of the time the SLO was met\n\
         \x20 /metrics                                    Prometheus text format\n\
         \x20 /proxy.pac                                  browser auto-config script\n\
         \x20 /lookup?url=<url>                           cache entry state\n\
         \x20 /purge?url=<url> | /purge?all=1             drop cache entries (also: PURGE <url>)\n\
         \x20 /blocklist?host=&action=block|allow|clear   blocklist decision and overrides\n",
        dashboard
    )
}

#[cfg(test)]
mod tests {
    use super::endpoint_list;

    /// ダッシュボードに Phase 13 が見る図と KPI が載っていること (T12.4 (5)、T13.3)。
    ///
    /// ブラウザが無いので絵は確かめられない。**`/history` と `/status` の読み方が
    /// 合っているか**は `scripts/check-dashboard.js` (Node があるときだけ) が
    /// 実出力を通して見る。ここで見るのは「消えていないこと」だけ。
    #[test]
    fn the_dashboard_has_the_charts_phase_13_reads() {
        let html = super::DASHBOARD_HTML;
        for id in [
            "ch-conn",
            "ch-err",
            "ch-fd",
            "connp50",
            "version",
            "window",
            "dnsmiss",
            "connlimit",
            "bad",
            // 個票の表 (T13.4)
            "errors",
            "errhint",
            "conns",
            "connhint",
            // プロファイル (T14.3)
            "proflead",
            "st-connect-setup",
            "st-connect-after",
            "st-forward-setup",
            "st-forward-after",
            "stages",
            "ch-rolecpu",
            "ch-cpureq",
            "roles",
            "locks",
            "profhint",
            // T15.0 (14) の 5 枚: (a) CPU の絞り / (b) 動かないトンネル /
            // (c) 名前解決の内訳 / (d) 受付待ち / (e) 利用者が待つ時間
            "cputag",
            "cpukpi",
            "cpubar",
            "cpudetail",
            "ch-cpu",
            "topthreads",
            "topthreadshint",
            "idleconns",
            "idleconnhint",
            "ch-dnswarm",
            "lg-dnskind",
            "st-dnskind",
            "dnskindhint",
            "lg-queue",
            "st-queue",
            "queuehint",
            "ch-rundelay",
            "rundelayhint",
            "waitp50",
            "waitp50tag",
            "waitdetail",
            "ch-wait",
        ] {
            assert!(html.contains(&format!("id=\"{}\"", id)), "{} が無い", id);
        }
        // 配列の配列を読む側 (キー名を戻す関数) と区間の分位点、`/status` を読む側 (T13.3)
        for f in [
            "function toSamples(",
            "function winQuantile(",
            "function mergeWindows(",
            "function dnsStats(",
            "function badHosts(",
            "function peak(",
            // 個票を読む側 (T13.4)
            "function errorRows(",
            "function connRows(",
            // プロファイルを読む側 (T14.3)
            "function toProfile(",
            "function stageRows(",
            "function longestStage(",
            "function roleRows(",
            "function lockRows(",
            "function drawStack(",
            // T15.0 (14) の 5 枚が読む側 (`scripts/check-dashboard.js` が名前で切り出す)
            "function toKernel(",
            "function cpuThrottle(",
            "function topThreads(",
            "function runDelay(",
            "function queueSpread(",
            "function idleTunnels(",
            "function dnsMissKinds(",
            "function waitKpi(",
        ] {
            assert!(html.contains(f), "{} が無い", f);
        }
        // ホスト別の表の列 (名前解決 / 接続、v4 / v6) と並びの選択肢 (T13.3 で 2 つ増えた)
        assert!(html.contains("名前解決 / 接続"), "{}", "内訳の列が無い");
        assert!(html.contains("v4 / v6"), "{}", "族の列が無い");
        for opt in ["\"p95\"", "\"errors\"", "\"dns\"", "\"slow\"", "\"bytes\""] {
            assert!(
                html.contains(&format!("<option value={}>", opt)),
                "並びの選択肢 {} が無い",
                opt
            );
        }
        // 「悪いホスト」は `?sort=` の 2 本を **30 秒に 1 回だけ** 取る (負荷を増やさない)
        assert!(
            html.contains("'/status?sort='+k"),
            "{}",
            "?sort= を取っていない"
        );
        assert!(
            html.contains("setInterval(pollBad,30000)"),
            "{}",
            "30 秒ごとになっていない"
        );
        // 個票は 5 秒ごとに `/errors?n=20` と `/connections` の 2 本 (T13.4)
        assert!(
            html.contains("fetchJson('/errors?n=20')"),
            "{}",
            "/errors を取っていない"
        );
        assert!(
            html.contains("fetchJson('/connections')"),
            "{}",
            "/connections を取っていない"
        );
        assert!(
            html.contains("setInterval(pollRecent,5000)"),
            "{}",
            "個票が 5 秒ごとになっていない"
        );
        // プロファイルは 30 秒ごとに `/profile?res=` を 1 本 (T14.3。重い口なので T28.25 で 5 → 30 秒)
        assert!(
            html.contains("fetchJson('/profile?res='+res)"),
            "{}",
            "/profile を取っていない"
        );
        assert!(
            html.contains("setInterval(pollProfile,30000)"),
            "{}",
            "プロファイルが 30 秒ごとになっていない"
        );
        // 起動のカード (T28.25): 30 秒ごとに `/events` を引いて「普通の再起動 / 無言の再起動」を出す
        assert!(
            html.contains("fetchJson('/events?n=200')")
                && html.contains("setInterval(pollEvents,30000)"),
            "{}",
            "/events を 30 秒ごとに取っていない"
        );
        for id in ["bootkpi", "bootdetail", "rejdetail"] {
            assert!(
                html.contains(&format!("id=\"{}\"", id)),
                "起動のカードの {} が無い",
                id
            );
        }
        // ヘッダーから個票へ行けること (T13.4)
        for link in [
            "/dns",
            "/log",
            "/hosts?limit=1000",
            "/errors",
            "/connections",
            "/profile",
        ] {
            assert!(
                html.contains(&format!("<a href=\"{}\" target=\"_blank\">", link)),
                "{} へのリンクが無い",
                link
            );
        }
        // 外部ライブラリは読み込まない (依存なしの 1 ページ)
        assert!(!html.contains("<script src="), "外部 JS を読み込んでいる");
        assert!(
            !html.contains("<link rel=\"stylesheet\""),
            "外部 CSS を読み込んでいる"
        );
    }

    /// 「調査」ページに T14.8 が描く図と、node の整形テストが抜き出す関数があること。
    ///
    /// 絵はブラウザが無いと確かめられないので、ここで見るのは「消えていないこと」と
    /// **大きさが 64 KiB 以下**なことだけ (読み方が実出力と合っているかは
    /// `scripts/check-dashboard.js` が `/snapshot` の実出力で見る)。
    #[test]
    fn the_inspect_page_has_what_phase_14_reads() {
        let html = super::INSPECT_HTML;
        assert!(
            html.len() <= 64 * 1024,
            "inspect.html が 64 KiB を超えた: {} B",
            html.len()
        );
        for id in [
            "ch-timeline", // (a) タイムライン
            "lg-timeline",
            "tllead",
            "slow", // (b) 遅い接続の表
            "lg-stages",
            "bursts",  // (c) 山の写真
            "clients", // (d) 接続元
            "ch-rtt",  // (e) RTT の散布
            "rtt",
            "win", // (f) 起動からの窓
            "winkv",
            "series-card", // T14.22 が入ったときだけ出る折れ線
            "lite",
        ] {
            assert!(html.contains(&format!("id=\"{}\"", id)), "{} が無い", id);
        }
        // node の整形テストが名前で抜き出す関数 (名前を変えるならあちらも直すこと)
        for f in [
            "function timeline(",
            "function eventMarks(",
            "function slowRows(",
            "function burstCards(",
            "function clientRows(",
            "function rttScatter(",
            "function sinceStart(",
            "function seriesLines(",
            "function toSamples(",
            "function winQuantile(",
        ] {
            assert!(html.contains(f), "{} が無い", f);
        }
        // (g) 先頭に `/snapshot` へのリンク。個票の口もヘッダーから開ける
        for link in [
            "/snapshot",
            "/recent?n=2000",
            "/bursts",
            "/clients?limit=1000",
        ] {
            assert!(
                html.contains(&format!("<a href=\"{}\" target=\"_blank\">", link)),
                "{} へのリンクが無い",
                link
            );
        }
        // 段階は T14.3 の 5 つ、閉じた理由は T14.4 の 8 種を色で持つ
        for stage in ["queue", "client_read", "dns", "connect", "first_relay"] {
            assert!(html.contains(stage), "段階 {} が無い", stage);
        }
        for reason in [
            "client_eof",
            "server_eof",
            "idle_timeout",
            "keepalive_timeout",
            "evicted",
            "limit",
            "shutdown",
        ] {
            assert!(html.contains(reason), "閉じた理由 {} が無い", reason);
        }
        // 外部ライブラリは読み込まない (依存なしの 1 ページ)
        assert!(!html.contains("<script src="), "外部 JS を読み込んでいる");
        assert!(
            !html.contains("<link rel=\"stylesheet\""),
            "外部 CSS を読み込んでいる"
        );
    }

    #[test]
    fn the_listing_hides_the_dashboard_in_lite_mode() {
        assert!(endpoint_list(false).contains("/dashboard"));
        assert!(!endpoint_list(true).contains("/dashboard"));
        for path in ["/status", "/metrics", "/proxy.pac", "/purge?url="] {
            assert!(endpoint_list(true).contains(path), "{}", path);
        }
    }
}
