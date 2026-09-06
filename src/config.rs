use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::acl::{AclConfig, PortSet};
use crate::cache::CacheConfig;
use crate::envfile;

#[derive(Debug, Clone)]
pub struct Config {
    /// 待ち受けポート
    pub port: u16,
    /// 待ち受けアドレス。空ならデュアルスタック (`[::]` + `0.0.0.0`) を自動で試す
    pub bind_addrs: Vec<IpAddr>,
    /// IPv6 を使うか (待ち受けと AAAA での接続)。既定 on
    pub ipv6: bool,
    pub acl: AclConfig,
    pub timeout: Duration,
    /// クライアント接続を keep-alive で待つアイドル時間。0 なら 1 接続 1 要求
    pub keepalive: Duration,
    /// オリジンへのアイドル接続をホストごとに何本まで保持するか。0 で再利用しない
    pub pool_per_host: usize,
    /// アイドル接続の全体上限 (`PROXY_ORIGIN_POOL_TOTAL`)。ホスト数 × per_host の歯止め
    pub pool_total: usize,
    /// HTTPS のオリジンへ取得に行くか (システムの OpenSSL を使う)
    pub tls_enabled: bool,
    /// オリジンの証明書を検証するか
    pub tls_verify: bool,
    /// 追加の CA 証明書ファイル (PEM)。無ければシステムの CA ストア
    pub tls_ca_file: Option<PathBuf>,
    /// 名前解決の結果を保持する時間 (`PROXY_DNS_TTL_SECS`、0 で無効)
    pub dns_ttl: Duration,
    /// `/proxy.pac` で DIRECT にするホストの一覧 (`PROXY_PAC_DIRECT`、`*.example.com` 可)
    pub pac_direct: Vec<String>,
    /// ブロックリストのファイル (`PROXY_BLOCKLIST_FILE`、hosts 形式 / 1 行 1 ドメイン)
    pub blocklist_file: Option<PathBuf>,
    /// ブロックリストの URL (`PROXY_BLOCKLIST_URL`)
    pub blocklist_url: Option<String>,
    /// URL を取り直す間隔 (`PROXY_BLOCKLIST_REFRESH_SECS`)
    pub blocklist_refresh: Duration,
    /// ブロックリストの対象外にするホスト (`PROXY_BLOCKLIST_EXEMPT`、`*.example.com` 可)
    pub blocklist_exempt: Vec<String>,
    /// 統計と履歴を `$HOME/.rust-http-proxy.rrd` に残す (`PROXY_STATS_PERSIST`、既定 on)
    pub stats_persist: bool,
    /// 最速の素通しプロファイル (`PROXY_PROFILE=lite` / `--lite`)。
    /// キャッシュ・統計の永続化・ブロックリストを止め、ログを warn にする
    pub lite: bool,
    /// CONNECT を許すあて先ポート (`PROXY_CONNECT_PORTS`、既定は制限なし)
    pub connect_ports: PortSet,
    /// ループバック・リンクローカル宛てのオリジンを許すか (`PROXY_ALLOW_LOCAL`、既定 off)。
    /// 既定ではクラウドのメタデータ (`169.254.169.254`) 経由の SSRF を 403 で止める
    pub allow_local: bool,
    /// CONNECT トンネルのアイドル打ち切り時間 (`PROXY_TUNNEL_IDLE_SECS`、既定 300 秒、`0` で無期限)
    pub tunnel_idle: Duration,
    /// 同時に受ける接続数の上限 (`PROXY_MAX_CONNS`、既定 4096、`0` で無制限)。
    /// 超えた接続には 503 を返して閉じる (スレッドは起こさない)
    pub max_conns: usize,
    pub cache: CacheConfig,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let port_str = envfile::var("SERVER_PORT").unwrap_or_else(|| "8080".to_string());
        let allow_hosts = envfile::var("PROXY_ALLOW_HOSTS");
        let deny_hosts = envfile::var("PROXY_DENY_HOSTS");
        let timeout_secs = envfile::var("PROXY_TIMEOUT_SECS")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);

        let mut cfg = Self::new(
            &port_str,
            allow_hosts.as_deref(),
            deny_hosts.as_deref(),
            Duration::from_secs(timeout_secs),
        )?;
        // lite は「既定をまとめて off にする」だけなので、後続の環境変数が上書きできる
        cfg.lite =
            envfile::var("PROXY_PROFILE").is_some_and(|v| v.trim().eq_ignore_ascii_case("lite"));
        if cfg.lite {
            cfg.stats_persist = false;
        }
        if let Some(bind) = envfile::var("PROXY_BIND") {
            cfg.bind_addrs = parse_bind_list(&bind)?;
        }
        if let Some(secs) =
            envfile::var("PROXY_KEEPALIVE_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.keepalive = Duration::from_secs(secs);
        }
        if let Some(n) =
            envfile::var("PROXY_ORIGIN_POOL").and_then(|s| s.trim().parse::<usize>().ok())
        {
            cfg.pool_per_host = n;
        }
        if let Some(n) =
            envfile::var("PROXY_ORIGIN_POOL_TOTAL").and_then(|s| s.trim().parse::<usize>().ok())
        {
            cfg.pool_total = n;
        }
        let off = |v: String| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        };
        if let Some(v) = envfile::var("PROXY_IPV6") {
            cfg.ipv6 = !off(v);
        }
        if let Some(v) = envfile::var("PROXY_TLS") {
            cfg.tls_enabled = !off(v);
        }
        if let Some(v) = envfile::var("PROXY_TLS_VERIFY") {
            cfg.tls_verify = !off(v);
        }
        if let Some(n) =
            envfile::var("PROXY_MAX_CONNS").and_then(|s| s.trim().parse::<usize>().ok())
        {
            cfg.max_conns = n;
        }
        if let Some(secs) =
            envfile::var("PROXY_TUNNEL_IDLE_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.tunnel_idle = Duration::from_secs(secs);
        }
        if let Some(v) = envfile::var("PROXY_CONNECT_PORTS") {
            cfg.connect_ports = PortSet::parse(&v);
        }
        if let Some(v) = envfile::var("PROXY_ALLOW_LOCAL") {
            cfg.allow_local = !off(v);
        }
        if let Some(v) = envfile::var("PROXY_STATS_PERSIST") {
            cfg.stats_persist = !off(v);
        }
        if let Some(path) = envfile::var("PROXY_TLS_CA_FILE").filter(|p| !p.trim().is_empty()) {
            cfg.tls_ca_file = Some(PathBuf::from(path.trim()));
        }
        if let Some(secs) =
            envfile::var("PROXY_DNS_TTL_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.dns_ttl = Duration::from_secs(secs);
        }
        let list = |v: String| -> Vec<String> {
            v.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        };
        if let Some(v) = envfile::var("PROXY_PAC_DIRECT") {
            cfg.pac_direct = list(v);
        }
        cfg.blocklist_file = envfile::var("PROXY_BLOCKLIST_FILE")
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        cfg.blocklist_url = envfile::var("PROXY_BLOCKLIST_URL")
            .map(|u| u.trim().to_string())
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"));
        if let Some(secs) =
            envfile::var("PROXY_BLOCKLIST_REFRESH_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.blocklist_refresh = Duration::from_secs(secs.max(60));
        }
        if let Some(v) = envfile::var("PROXY_BLOCKLIST_EXEMPT") {
            cfg.blocklist_exempt = list(v);
        }
        let mut cache = CacheConfig::from_env();
        if cfg.lite {
            // lite ではブロックリストの取得もキャッシュもしない (明示指定があればそちらが勝つ)
            cfg.blocklist_file = None;
            cfg.blocklist_url = None;
            if envfile::var("PROXY_CACHE_ENABLED").is_none() {
                cache.enabled = false;
            }
        }
        Ok(cfg.with_cache(cache))
    }

    /// キャッシュ設定を差し替える。
    pub fn with_cache(mut self, cache: CacheConfig) -> Self {
        self.cache = cache;
        self
    }

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
        Ok(Self {
            port,
            bind_addrs: Vec::new(),
            ipv6: true,
            acl,
            timeout,
            keepalive: Duration::from_secs(15),
            pool_per_host: 64,
            pool_total: 256,
            tls_enabled: true,
            tls_verify: true,
            tls_ca_file: None,
            dns_ttl: Duration::from_secs(60),
            pac_direct: Vec::new(),
            blocklist_file: None,
            blocklist_url: None,
            blocklist_refresh: Duration::from_secs(86400),
            blocklist_exempt: Vec::new(),
            stats_persist: true,
            lite: false,
            connect_ports: PortSet::default(),
            allow_local: false,
            tunnel_idle: Duration::from_secs(300),
            max_conns: 4096,
            cache: CacheConfig::default(),
        })
    }
}

/// `PROXY_BIND` のカンマ区切りアドレス (`::`, `0.0.0.0`, `127.0.0.1`, `[::1]`)。空なら自動。
pub fn parse_bind_list(s: &str) -> Result<Vec<IpAddr>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            item.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .map_err(|e| format!("Invalid PROXY_BIND entry '{}': {}", item, e))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_port() {
        let cfg = Config::new("9090", None, None, Duration::from_secs(10)).unwrap();
        assert_eq!(cfg.port, 9090);
        assert!(cfg.bind_addrs.is_empty());
        assert!(cfg.ipv6, "IPv6 on by default");
        assert_eq!(cfg.timeout, Duration::from_secs(10));
        assert_eq!(cfg.keepalive, Duration::from_secs(15));
        assert_eq!(cfg.pool_per_host, 64);
        assert_eq!(cfg.pool_total, 256);
        assert_eq!(cfg.max_conns, 4096);
        assert_eq!(cfg.tunnel_idle, Duration::from_secs(300));
        assert!(cfg.connect_ports.is_empty() && !cfg.allow_local);
    }

    #[test]
    fn test_bind_list() {
        let list = parse_bind_list(" ::, 0.0.0.0 ,[::1]").unwrap();
        assert_eq!(list.len(), 3);
        assert!(list[0].is_ipv6() && list[1].is_ipv4() && list[2].is_loopback());
        assert!(parse_bind_list("nope").is_err());
        assert!(parse_bind_list("").unwrap().is_empty());
    }

    #[test]
    fn test_cache_defaults() {
        let cfg = Config::new("8080", None, None, Duration::from_secs(30)).unwrap();
        assert!(cfg.cache.mem_limit.is_auto());
        assert!(cfg.cache.disk_limit.is_auto());
        assert_eq!(cfg.cache.mem_limit.target_percent(), Some(100));
        assert!(cfg.cache.enabled && cfg.cache.reserve);
    }

    #[test]
    fn test_invalid_port() {
        assert!(Config::new("invalid", None, None, Duration::from_secs(30)).is_err());
        assert!(Config::new("99999", None, None, Duration::from_secs(30)).is_err());
    }
}
