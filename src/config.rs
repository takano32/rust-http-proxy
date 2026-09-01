use std::net::IpAddr;
use std::time::Duration;

use crate::acl::AclConfig;
use crate::cache::CacheConfig;
use crate::envfile;

#[derive(Debug, Clone)]
pub struct Config {
    /// 待ち受けポート
    pub port: u16,
    /// 待ち受けアドレス。空ならデュアルスタック (`[::]` + `0.0.0.0`) を自動で試す
    pub bind_addrs: Vec<IpAddr>,
    pub acl: AclConfig,
    pub timeout: Duration,
    /// クライアント接続を keep-alive で待つアイドル時間。0 なら 1 接続 1 要求
    pub keepalive: Duration,
    /// オリジンへのアイドル接続をホストごとに何本まで保持するか。0 で再利用しない
    pub pool_per_host: usize,
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
        Ok(cfg.with_cache(CacheConfig::from_env()))
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
            acl,
            timeout,
            keepalive: Duration::from_secs(15),
            pool_per_host: 8,
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
        assert!(cfg.bind_addrs.is_empty(), "dual-stack auto by default");
        assert_eq!(cfg.timeout, Duration::from_secs(10));
        assert_eq!(cfg.keepalive, Duration::from_secs(15));
        assert_eq!(cfg.pool_per_host, 8);
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
