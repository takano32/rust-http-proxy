use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use crate::acl::AclConfig;
use crate::cache::CacheConfig;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub acl: AclConfig,
    pub timeout: Duration,
    pub cache: CacheConfig,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let port_str = env::var("SERVER_PORT").unwrap_or_else(|_| "8080".to_string());
        let allow_hosts = env::var("PROXY_ALLOW_HOSTS").ok();
        let deny_hosts = env::var("PROXY_DENY_HOSTS").ok();
        let timeout_secs = env::var("PROXY_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);

        Self::new(
            &port_str,
            allow_hosts.as_deref(),
            deny_hosts.as_deref(),
            Duration::from_secs(timeout_secs),
        )
        .map(|cfg| cfg.with_cache(CacheConfig::from_env()))
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
        let bind_addr: SocketAddr = format!("0.0.0.0:{}", port)
            .parse()
            .map_err(|e| format!("Invalid bind address: {}", e))?;
        let acl = AclConfig::new(allow_hosts, deny_hosts);
        Ok(Self {
            bind_addr,
            acl,
            timeout,
            cache: CacheConfig::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_port() {
        let cfg = Config::new("9090", None, None, Duration::from_secs(10)).unwrap();
        assert_eq!(cfg.bind_addr.port(), 9090);
        assert_eq!(cfg.timeout, Duration::from_secs(10));
    }

    #[test]
    fn test_cache_defaults() {
        let cfg = Config::new("8080", None, None, Duration::from_secs(30)).unwrap();
        assert_eq!(cfg.cache.mem_capacity, 200 * 1024 * 1024);
        assert_eq!(cfg.cache.disk_capacity, 2048 * 1024 * 1024);
        assert!(cfg.cache.enabled);
    }

    #[test]
    fn test_invalid_port() {
        assert!(Config::new("invalid", None, None, Duration::from_secs(30)).is_err());
        assert!(Config::new("99999", None, None, Duration::from_secs(30)).is_err());
    }
}
