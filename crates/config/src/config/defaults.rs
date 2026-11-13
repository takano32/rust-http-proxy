//! `Config` の既定値 ([`Config::new`]) と、欄を直に書き換えたあとの辻褄合わせ。

use std::time::Duration;

use super::{
    Config, DEFAULT_BURST_PERCENT, DEFAULT_MAX_REQUESTS_PER_CONN, DEFAULT_PEEK_SNI_PORT,
    DEFAULT_SLO, DEFAULT_SNAPSHOT_DAYS, DEFAULT_TCP_KEEPALIVE, EnvProblems, Sources,
    burst_threshold, connect_timeout_for, default_max_conns, default_max_threads,
    header_timeout_for,
};
use crate::acl::{AclConfig, AllowLocal, ClientAcl, PortSet};
use crate::cache::CacheConfig;

impl Config {
    pub fn new(
        port_str: &str,
        allow_hosts: Option<&str>,
        deny_hosts: Option<&str>,
        timeout: Duration,
    ) -> Result<Self, String> {
        let port: u16 = port_str
            .parse()
            .map_err(|e| format!("Invalid SERVER_PORT '{}': {}", port_str, e))?;
        let acl = AclConfig::new(allow_hosts, deny_hosts);
        let max_conns = default_max_conns();
        Ok(Self {
            port,
            bind_addrs: Vec::new(),
            ipv6: true,
            listen_backlog: crate::net::default_backlog(),
            acl,
            timeout,
            min_transfer: proxy_base::minrate::MinRate::DEFAULT,
            // 未設定のときの実効値は `connect_timeout_for` が決める (既定 3 秒。T28.43。
            // `timeout` が 3 秒より短ければそちらに合わせる。T15.6)。ここで入れておくと
            // `Config::new` を呼ぶ既存の所 (テストと `/config` の組み立て) が自動で追随する
            connect_timeout: connect_timeout_for(timeout, None),
            connect_negative: crate::net::DEFAULT_CONNECT_NEGATIVE,
            // 未設定のときの実効値は `header_timeout_for` (既定 10 秒と `timeout` の小さい方。T28.0)
            header_timeout: header_timeout_for(timeout, None),
            he_stagger: crate::net::DEFAULT_STAGGER,
            keepalive: Duration::from_secs(15),
            pool_per_host: 64,
            pool_total: 256,
            park_idle: true,
            park_grace: Duration::from_millis(3),
            malloc_arenas: 8,
            malloc_mmap_threshold_kb: 256,
            tls_enabled: true,
            tls_verify: true,
            tls_ca_file: None,
            dns_ttl: Duration::from_secs(60),
            dns_negative: crate::dns::NEGATIVE,
            dns_warm: crate::dns::WARM_DEFAULT,
            dns_stale: crate::dns::SERVE_STALE,
            dns_timeout: crate::dns::TIMEOUT_DEFAULT,
            // 既定は `auto` = 直近 1 時間で最も使われた CONNECT の宛先を 60 秒に 1 回
            // (値の意味は `proxy-metrics` の `canary`。この層は文字列を運ぶだけ)
            canary: "auto".to_string(),
            canary_secs: Duration::from_secs(60),
            canary_ipv6: true,
            pac_direct: Vec::new(),
            blocklist_file: None,
            blocklist_url: None,
            blocklist_refresh: Duration::from_secs(86400),
            blocklist_exempt: Vec::new(),
            stats_persist: true,
            snapshot_days: DEFAULT_SNAPSHOT_DAYS,
            slo: DEFAULT_SLO,
            lite: false,
            // 既定 1,000 ms (`proxy_metrics::profile::DEFAULT_SAMPLE_MS` と同じ値。
            // この層は計測クレートに依存しないので数値で持つ)
            profile_sample_ms: 1000,
            self_bench: false,
            connect_ports: PortSet::default(),
            allow_local: AllowLocal::Off,
            tunnel_idle: Duration::from_secs(300),
            max_conns,
            max_requests_per_conn: DEFAULT_MAX_REQUESTS_PER_CONN,
            max_threads: default_max_threads(max_conns),
            burst_percent: DEFAULT_BURST_PERCENT,
            burst_at: burst_threshold(max_conns, DEFAULT_BURST_PERCENT),
            cache: CacheConfig::default(),
            endpoints_readonly: false,
            allow_clients: ClientAcl::default(),
            max_conns_per_client: 0,
            trace_client: None,
            peek_sni: Some(DEFAULT_PEEK_SNI_PORT),
            records: crate::records::Mode::On,
            tcp_keepalive: Some(DEFAULT_TCP_KEEPALIVE),
            sources: Sources::default(),
            problems: EnvProblems::default(),
        })
    }

    /// キャッシュ設定を差し替える。
    pub(crate) fn with_cache(mut self, cache: CacheConfig) -> Self {
        self.cache = cache;
        self
    }

    /// `max_conns` か `burst_percent` を手で書き換えたあとに山の写真の閾を計算し直す (T14.6)。
    ///
    /// `from_env` は最後にこれと同じことをしている。**構造体の欄を直に書き換える場所**
    /// (テストと、設定を組み立てる道具) はここを呼ぶこと。
    pub fn refresh_burst_at(&mut self) {
        // `--lite` は個票を 1 つも記録しない (`/connections` の表も空) ので写真も撮らない
        self.burst_at = if self.lite {
            usize::MAX
        } else {
            burst_threshold(self.max_conns, self.burst_percent)
        };
    }
}
