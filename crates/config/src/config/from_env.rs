//! 環境変数と `.env` とコマンドライン引数から [`Config`] を組む ([`Config::from_env`])。
//!
//! 読む順は鍵の群ごとの段に割ってあるが、**段の中も段の並びも `config.rs` に 1 本で書いてあった頃と
//! 同じ** (読めない値の WARN が出る順を変えないため)。純粋な読み方は [`super::parse`]、
//! 読めない値と知らない名前の覚え方は [`super::problems`]。

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use super::parse::{
    CountSpec, PeekSniSpec, is_http_url, lowercase_list, parse_count_or_auto, parse_peek_sni,
};
use super::{
    Config, EnvProblems, MAX_SNAPSHOT_DAYS, Slo, Sources, TcpKeepalive, connect_timeout_for,
    default_max_conns, default_max_threads, header_timeout_for, mark_cache_sources,
    parse_bind_list, parse_stagger_ms,
};
use crate::acl::{ClientAcl, PortSet};
use crate::cache::CacheConfig;
use crate::envfile;
use crate::records;

impl Config {
    pub fn from_env() -> Result<Self, String> {
        // **効いた値にだけ**出どころの印を付ける (`/config` の `source`。T14.15)。
        // 読めない書き方は既定に落ちるので、その行は `default` のままになる
        let mut src = Sources::default();
        // 読めない値と知らない名前 (T28.6)。読めない値は WARN を 1 行出して既定のまま
        let mut bad = EnvProblems::default();
        let mut cfg = Self::read_base(&mut src, &mut bad)?;
        cfg.read_timeouts(&mut src, &mut bad);
        cfg.read_pool_and_process(&mut src, &mut bad);
        cfg.read_listen_and_tls(&mut src, &mut bad);
        cfg.read_limits(&mut src, &mut bad);
        cfg.read_access(&mut src, &mut bad);
        cfg.read_statistics_and_ca(&mut src, &mut bad);
        cfg.read_dns_and_canary(&mut src, &mut bad);
        cfg.read_lists_and_log(&mut src, &mut bad);
        Ok(cfg.finish_with_cache(src, bad))
    }

    /// 既定の `Config` を作る土台: ポート・宛先の ACL・共通の締め切り・`lite`・待ち受けアドレス。
    fn read_base(src: &mut Sources, bad: &mut EnvProblems) -> Result<Self, String> {
        let port_str = envfile::var("SERVER_PORT").unwrap_or_else(|| "8080".to_string());
        let allow_hosts = envfile::var("PROXY_ALLOW_HOSTS");
        let deny_hosts = envfile::var("PROXY_DENY_HOSTS");
        // `0` は無期限。1 秒に切り上げないのは、切り上げると**無期限を表す手段が
        // 設定から無くなる**ため (`PROXY_TUNNEL_IDLE_SECS` と意味を揃えた。T10.6)
        let timeout_secs = bad
            .num::<u64>("PROXY_TIMEOUT_SECS")
            .inspect(|_| src.mark("PROXY_TIMEOUT_SECS"))
            .unwrap_or(30);

        let mut cfg = Self::new(
            &port_str,
            allow_hosts.as_deref(),
            deny_hosts.as_deref(),
            Duration::from_secs(timeout_secs),
        )?;
        if envfile::var("SERVER_PORT").is_some() {
            src.mark("SERVER_PORT");
        }
        if allow_hosts.is_some() {
            src.mark("PROXY_ALLOW_HOSTS");
        }
        if deny_hosts.is_some() {
            src.mark("PROXY_DENY_HOSTS");
        }
        // lite は「既定をまとめて off にする」だけなので、後続の環境変数が上書きできる
        cfg.lite = bad.profile_is_lite();
        if cfg.lite {
            cfg.stats_persist = false;
            src.mark("PROXY_PROFILE");
        }
        if let Some(bind) = envfile::var("PROXY_BIND") {
            cfg.bind_addrs = parse_bind_list(&bind)?;
            src.mark("PROXY_BIND");
        }
        Ok(cfg)
    }

    /// 締め切りと間隔: CONNECT の確立・要求行とヘッダー・Happy Eyeballs の間隔・keep-alive。
    fn read_timeouts(&mut self, src: &mut Sources, bad: &mut EnvProblems) {
        // CONNECT のオリジン接続だけの締め切り (T15.6)。書かなければ `Config::new` が入れた
        // 既定 (3 秒と `PROXY_TIMEOUT_SECS` の小さい方。T28.43) のまま = 出どころも `default` のまま。
        // 書いてあればその値をそのまま使う (`0` と明示すれば無期限。T10.6)
        if let Some(secs) = bad.num::<u64>("PROXY_CONNECT_TIMEOUT_SECS") {
            self.connect_timeout =
                connect_timeout_for(self.timeout, Some(Duration::from_secs(secs)));
            src.mark("PROXY_CONNECT_TIMEOUT_SECS");
        }
        // 要求行とヘッダーの締め切り (T28.0)。書かなければ `Config::new` の既定のまま
        if let Some(secs) =
            envfile::var("PROXY_HEADER_TIMEOUT_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            self.header_timeout = header_timeout_for(self.timeout, Some(Duration::from_secs(secs)));
            src.mark("PROXY_HEADER_TIMEOUT_SECS");
        }
        // Happy Eyeballs の間隔 (T17.6)。範囲外は既定のまま (出どころも `default`) にして 1 行知らせる
        if let Some(v) = envfile::var("PROXY_HE_STAGGER_MS") {
            match parse_stagger_ms(&v) {
                Some(d) => {
                    self.he_stagger = d;
                    src.mark("PROXY_HE_STAGGER_MS");
                }
                None => {
                    crate::log_warn!(
                        None,
                        "PROXY_HE_STAGGER_MS={:?} is not a number in {}..={} ms; using the default {} ms",
                        v.trim(),
                        crate::net::STAGGER_RANGE_MS.start(),
                        crate::net::STAGGER_RANGE_MS.end(),
                        crate::net::DEFAULT_STAGGER.as_millis()
                    );
                    bad.note("PROXY_HE_STAGGER_MS", &v, "a number in 10..=2000 ms");
                }
            }
        }
        if let Some(secs) = bad.num::<u64>("PROXY_KEEPALIVE_SECS") {
            self.keepalive = Duration::from_secs(secs);
            src.mark("PROXY_KEEPALIVE_SECS");
        }
    }

    /// オリジンのプールと、プロセスの調整 (malloc・暇な接続の預け方)。
    fn read_pool_and_process(&mut self, src: &mut Sources, bad: &mut EnvProblems) {
        if let Some(n) = bad.num::<usize>("PROXY_ORIGIN_POOL") {
            self.pool_per_host = n;
            src.mark("PROXY_ORIGIN_POOL");
        }
        if let Some(n) = bad.num::<usize>("PROXY_ORIGIN_POOL_TOTAL") {
            self.pool_total = n;
            src.mark("PROXY_ORIGIN_POOL_TOTAL");
        }
        if let Some(n) = bad.num::<usize>("PROXY_MALLOC_ARENAS") {
            self.malloc_arenas = n;
            src.mark("PROXY_MALLOC_ARENAS");
        }
        if let Some(n) = bad.num::<usize>("PROXY_MALLOC_MMAP_THRESHOLD_KB") {
            self.malloc_mmap_threshold_kb = n;
            src.mark("PROXY_MALLOC_MMAP_THRESHOLD_KB");
        }
        if let Some(ms) = bad.num::<u64>("PROXY_PARK_GRACE_MS") {
            self.park_grace = Duration::from_millis(ms);
            src.mark("PROXY_PARK_GRACE_MS");
        }
        if let Some(on) = bad.flag("PROXY_PARK_IDLE") {
            self.park_idle = on;
            src.mark("PROXY_PARK_IDLE");
        }
    }

    /// 待ち受け (IPv6・backlog) と TLS の旗。
    fn read_listen_and_tls(&mut self, src: &mut Sources, bad: &mut EnvProblems) {
        if let Some(on) = bad.flag("PROXY_IPV6") {
            self.ipv6 = on;
            src.mark("PROXY_IPV6");
        }
        // 待ち受けの backlog (T14.47)。`0` と `auto` は既定 (= min(1024, somaxconn)) に戻す。
        // 大きすぎる値はカーネルが `somaxconn` で頭打ちにするので、ここでは切らない
        // (`/config` に書いたとおりの数が出て、実際に効いたかは `ss -ltn` の `Send-Q` で分かる)
        if let Some(v) = envfile::var("PROXY_LISTEN_BACKLOG") {
            match parse_count_or_auto::<u32>(&v) {
                CountSpec::Auto => {
                    self.listen_backlog = crate::net::default_backlog();
                    src.mark("PROXY_LISTEN_BACKLOG");
                }
                CountSpec::Number(n) => {
                    self.listen_backlog = if n == 0 {
                        crate::net::default_backlog()
                    } else {
                        n
                    };
                    src.mark("PROXY_LISTEN_BACKLOG");
                }
                CountSpec::Invalid => {
                    bad.reject("PROXY_LISTEN_BACKLOG", v.trim(), "a whole number or auto")
                }
                CountSpec::Empty => {}
            }
        }
        if let Some(on) = bad.flag("PROXY_TLS") {
            self.tls_enabled = on;
            src.mark("PROXY_TLS");
        }
        if let Some(on) = bad.flag("PROXY_TLS_VERIFY") {
            self.tls_verify = on;
            src.mark("PROXY_TLS_VERIFY");
        }
    }

    /// 同時接続・スレッド・山の写真・転送の速さ・トンネルの暇の上限。
    fn read_limits(&mut self, src: &mut Sources, bad: &mut EnvProblems) {
        // `auto` (既定) は記述子の上限から決める。数値ならその値、`0` は無制限。
        // 読めない書き方は既定 (auto) のままにする
        if let Some(v) = envfile::var("PROXY_MAX_CONNS") {
            match parse_count_or_auto::<usize>(&v) {
                CountSpec::Auto => {
                    self.max_conns = default_max_conns();
                    src.mark("PROXY_MAX_CONNS");
                }
                CountSpec::Number(n) => {
                    self.max_conns = n;
                    src.mark("PROXY_MAX_CONNS");
                }
                CountSpec::Invalid => {
                    bad.reject("PROXY_MAX_CONNS", v.trim(), "a whole number or auto")
                }
                CountSpec::Empty => {}
            }
        }
        // スレッドの上限は接続数の上限にも従うので、**PROXY_MAX_CONNS の後に**決める
        self.max_threads = default_max_threads(self.max_conns);
        if let Some(v) = envfile::var("PROXY_MAX_THREADS") {
            match parse_count_or_auto::<usize>(&v) {
                CountSpec::Number(n) => {
                    self.max_threads = n;
                    src.mark("PROXY_MAX_THREADS");
                }
                CountSpec::Invalid => {
                    bad.reject("PROXY_MAX_THREADS", v.trim(), "a whole number or auto")
                }
                // `auto` と読めない書き方は既定のまま
                CountSpec::Auto | CountSpec::Empty => {}
            }
        }
        // 山の写真の閾 (T14.6)。**`PROXY_MAX_CONNS` の後に**決める (割合を当てる相手が上限)。
        // `--lite` では個票を 1 つも記録しないので、写真も撮らない (T1.4 の方針)
        if let Some(n) = bad.num::<usize>("PROXY_BURST_PERCENT") {
            self.burst_percent = n.min(100);
            src.mark("PROXY_BURST_PERCENT");
        }
        self.refresh_burst_at();
        // 転送中の最小の速さ (T29.34)。`0` で切らない
        if let Some(bps) = bad.num::<u64>("PROXY_MIN_TRANSFER_BPS") {
            self.min_transfer.bps = bps;
            src.mark("PROXY_MIN_TRANSFER_BPS");
        }
        if let Some(secs) = bad.num::<u64>("PROXY_MIN_TRANSFER_WINDOW_SECS") {
            self.min_transfer.window = Duration::from_secs(secs);
            src.mark("PROXY_MIN_TRANSFER_WINDOW_SECS");
        }
        if let Some(secs) = bad.num::<u64>("PROXY_TUNNEL_IDLE_SECS") {
            self.tunnel_idle = Duration::from_secs(secs);
            src.mark("PROXY_TUNNEL_IDLE_SECS");
        }
    }

    /// 誰を・どこへ通すか: CONNECT のポート・ローカル宛て・読み取り専用・接続元・追跡・SNI・記録・keepalive。
    fn read_access(&mut self, src: &mut Sources, bad: &mut EnvProblems) {
        if let Some(v) = envfile::var("PROXY_CONNECT_PORTS") {
            self.connect_ports = PortSet::parse(&v);
            src.mark("PROXY_CONNECT_PORTS");
        }
        if let Some(allow) = bad.allow_local("PROXY_ALLOW_LOCAL") {
            self.allow_local = allow;
            src.mark("PROXY_ALLOW_LOCAL");
        }
        if let Some(on) = bad.flag("PROXY_ENDPOINTS_READONLY") {
            self.endpoints_readonly = on;
            src.mark("PROXY_ENDPOINTS_READONLY");
        }
        if let Some(v) = envfile::var("PROXY_ALLOW_CLIENTS") {
            self.allow_clients = ClientAcl::parse(&v);
            src.mark("PROXY_ALLOW_CLIENTS");
        }
        if let Some(n) = bad.num::<usize>("PROXY_MAX_CONNS_PER_CLIENT") {
            self.max_conns_per_client = n;
            src.mark("PROXY_MAX_CONNS_PER_CLIENT");
        }
        // 追跡する接続元 (T14.27)。空 (と読めない書き方) は「追跡しない」= 既定のまま。
        // 覚えるのは `net::canonical_ip` を通した形なので、`::ffff:1.2.3.4` と書いても
        // `1.2.3.4` から来た接続に当たる
        if let Some(v) = EnvProblems::present("PROXY_TRACE_CLIENT") {
            match v.parse::<IpAddr>() {
                Ok(ip) => {
                    self.trace_client = Some(crate::net::canonical_ip(ip));
                    src.mark("PROXY_TRACE_CLIENT");
                }
                Err(_) => bad.reject("PROXY_TRACE_CLIENT", &v, "an IP address"),
            }
        }
        // CONNECT の最初のバイトから SNI を覗くか (T14.38)。`off` / `on` / `on:<port>` で、
        // 読めない書き方は既定 (`on` = 443 だけ) のまま
        if let Some(v) = envfile::var("PROXY_PEEK_SNI") {
            match parse_peek_sni(&v) {
                PeekSniSpec::Off => {
                    self.peek_sni = None;
                    src.mark("PROXY_PEEK_SNI");
                }
                PeekSniSpec::Port(p) => {
                    self.peek_sni = Some(p);
                    src.mark("PROXY_PEEK_SNI");
                }
                PeekSniSpec::Invalid => bad.reject(
                    "PROXY_PEEK_SNI",
                    &v.trim().to_ascii_lowercase(),
                    "off, on or on:<port>",
                ),
                PeekSniSpec::Default => {}
            }
        }
        // 記録の一括 off とハッシュ化 (T14.41)。読めない書き方は既定 (`on`) のまま
        if let Some(v) = EnvProblems::present("PROXY_RECORDS") {
            match records::parse(&v) {
                Some(m) => {
                    self.records = m;
                    src.mark("PROXY_RECORDS");
                }
                None => bad.reject("PROXY_RECORDS", &v, "on, off or hashed"),
            }
        }
        // 消えたクライアントの検知 (T14.52)。`off` / `on` / `on:<idle>:<intvl>:<cnt>` で、
        // 読めない書き方は既定 (`on` = 60:10:3) のまま
        if let Some(v) = EnvProblems::present("PROXY_TCP_KEEPALIVE") {
            match TcpKeepalive::parse(&v) {
                Some(k) => {
                    self.tcp_keepalive = k;
                    src.mark("PROXY_TCP_KEEPALIVE");
                }
                None => bad.reject(
                    "PROXY_TCP_KEEPALIVE",
                    &v,
                    "off, on or on:<idle>:<intvl>:<count>",
                ),
            }
        }
    }

    /// 統計の永続化・自己ベンチ・雪像・SLO と、追加の CA ファイル。
    fn read_statistics_and_ca(&mut self, src: &mut Sources, bad: &mut EnvProblems) {
        if let Some(on) = bad.flag("PROXY_STATS_PERSIST") {
            self.stats_persist = on;
            src.mark("PROXY_STATS_PERSIST");
        }
        // 起動直後の自己ベンチ (T14.43)。**`--lite` でも明示されればそちらが勝つ**
        // (測るためだけの旗で、既定は off)
        if let Some(on) = bad.flag("PROXY_SELF_BENCH") {
            self.self_bench = on;
            src.mark("PROXY_SELF_BENCH");
        }
        if let Some(n) = bad.num::<usize>("PROXY_SNAPSHOT_DAYS") {
            self.snapshot_days = n.min(MAX_SNAPSHOT_DAYS);
            src.mark("PROXY_SNAPSHOT_DAYS");
        }
        if let Some(v) = envfile::var("PROXY_SLO").filter(|v| !v.trim().is_empty()) {
            let (slo, skipped) = Slo::parse_skipping(&v);
            bad.slo_items(&skipped);
            self.slo = slo;
            src.mark("PROXY_SLO");
        }
        if let Some(path) = envfile::var("PROXY_TLS_CA_FILE").filter(|p| !p.trim().is_empty()) {
            self.tls_ca_file = Some(PathBuf::from(path.trim()));
            src.mark("PROXY_TLS_CA_FILE");
        }
    }

    /// 名前解決の表と canary、`/profile` の標本の間隔。
    fn read_dns_and_canary(&mut self, src: &mut Sources, bad: &mut EnvProblems) {
        if let Some(secs) = bad.num::<u64>("PROXY_DNS_TTL_SECS") {
            self.dns_ttl = Duration::from_secs(secs);
            src.mark("PROXY_DNS_TTL_SECS");
        }
        if let Some(secs) = bad.num::<u64>("PROXY_DNS_NEGATIVE_SECS") {
            self.dns_negative = Duration::from_secs(secs);
            src.mark("PROXY_DNS_NEGATIVE_SECS");
        }
        if let Some(secs) = bad.num::<u64>("PROXY_DNS_WARM_SECS") {
            self.dns_warm = Duration::from_secs(secs);
            src.mark("PROXY_DNS_WARM_SECS");
        }
        if let Some(secs) = bad.num::<u64>("PROXY_DNS_STALE_SECS") {
            self.dns_stale = Duration::from_secs(secs);
            src.mark("PROXY_DNS_STALE_SECS");
        }
        if let Some(secs) = bad.num::<u64>("PROXY_DNS_TIMEOUT_SECS") {
            self.dns_timeout = Duration::from_secs(secs);
            src.mark("PROXY_DNS_TIMEOUT_SECS");
        }
        if let Some(v) = envfile::var("PROXY_CANARY") {
            self.canary = v.trim().to_ascii_lowercase();
            src.mark("PROXY_CANARY");
        }
        if let Some(secs) = bad.num::<u64>("PROXY_CANARY_SECS") {
            self.canary_secs = Duration::from_secs(secs.max(1));
            src.mark("PROXY_CANARY_SECS");
        }
        if let Some(on) = bad.flag("PROXY_CANARY_IPV6") {
            self.canary_ipv6 = on;
            src.mark("PROXY_CANARY_IPV6");
        }
        if let Some(ms) = bad.num::<u64>("PROXY_PROFILE_SAMPLE_MS") {
            self.profile_sample_ms = ms;
            src.mark("PROXY_PROFILE_SAMPLE_MS");
        }
    }

    /// PAC の直接接続・ブロックリストの出所と、ログの高さの印。
    fn read_lists_and_log(&mut self, src: &mut Sources, bad: &mut EnvProblems) {
        if let Some(v) = envfile::var("PROXY_PAC_DIRECT") {
            self.pac_direct = lowercase_list(&v);
            src.mark("PROXY_PAC_DIRECT");
        }
        self.blocklist_file = envfile::var("PROXY_BLOCKLIST_FILE")
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .inspect(|_| src.mark("PROXY_BLOCKLIST_FILE"));
        self.blocklist_url = EnvProblems::present("PROXY_BLOCKLIST_URL")
            .filter(|u| {
                let ok = is_http_url(u);
                if !ok {
                    bad.reject("PROXY_BLOCKLIST_URL", u, "an http:// or https:// URL");
                }
                ok
            })
            .inspect(|_| src.mark("PROXY_BLOCKLIST_URL"));
        if let Some(secs) = bad.num::<u64>("PROXY_BLOCKLIST_REFRESH_SECS") {
            self.blocklist_refresh = Duration::from_secs(secs.max(60));
            src.mark("PROXY_BLOCKLIST_REFRESH_SECS");
        }
        if let Some(v) = envfile::var("PROXY_BLOCKLIST_EXEMPT") {
            self.blocklist_exempt = lowercase_list(&v);
            src.mark("PROXY_BLOCKLIST_EXEMPT");
        }
        // ログの高さは `Config` に持たないが (`crate::log` の全体の状態)、
        // `/config` には効いている値と出どころを並べる
        // 読めない高さの知らせは `log::init_from_env` が起動時に 1 行出しているので、ここは覚えるだけ
        if let Some(v) = EnvProblems::present("PROXY_LOG_LEVEL") {
            if crate::log::Level::parse(&v).is_some() {
                src.mark("PROXY_LOG_LEVEL");
            } else {
                bad.note("PROXY_LOG_LEVEL", &v, "error, warn, info, debug or trace");
            }
        }
    }

    /// キャッシュの設定を読んで `lite` の決まりを当て、出どころと問題を仕上げる。
    fn finish_with_cache(mut self, mut src: Sources, mut bad: EnvProblems) -> Self {
        let mut cache = CacheConfig::from_env();
        if self.lite {
            // lite ではブロックリストの取得もキャッシュもしない。**ブロックリストは明示指定 (`PROXY_BLOCKLIST_FILE` /
            // `_URL`) より lite が勝つ** (読まない)。キャッシュだけは `PROXY_CACHE_ENABLED` の明示指定が勝つ (T28.48)
            self.blocklist_file = None;
            self.blocklist_url = None;
            if envfile::var("PROXY_CACHE_ENABLED").is_none() {
                cache.enabled = false;
            }
        }
        mark_cache_sources(&mut src, &mut bad);
        self.sources = src;
        let mut cfg = self.with_cache(cache);
        // 知らない名前は、知っている名前 (`/config` に並ぶ全部) と比べる
        let known: Vec<&'static str> = cfg.settings().iter().map(|s| s.key).collect();
        bad.find_unknown(&known);
        cfg.problems = bad;
        cfg
    }
}
