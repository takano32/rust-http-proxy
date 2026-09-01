use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use crate::acl::AclConfig;
use crate::auth::AuthConfig;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub auth: AuthConfig,
    pub acl: AclConfig,
    pub timeout: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let port_str = env::var("SERVER_PORT").unwrap_or_else(|_| "8080".to_string());
        let auth_val = env::var("PROXY_AUTH").ok();
        let allow_hosts = env::var("PROXY_ALLOW_HOSTS").ok();
        let deny_hosts = env::var("PROXY_DENY_HOSTS").ok();
        let timeout_secs = env::var("PROXY_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);

        Self::new(
            &port_str,
            auth_val,
            allow_hosts.as_deref(),
            deny_hosts.as_deref(),
            Duration::from_secs(timeout_secs),
        )
    }

    pub fn new(
        port_str: &str,
        proxy_auth: Option<String>,
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
        let auth = AuthConfig::new(proxy_auth);
        let acl = AclConfig::new(allow_hosts, deny_hosts);
        Ok(Self {
            bind_addr,
            auth,
            acl,
            timeout,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_port() {
        let cfg = Config::new("9090", None, None, None, Duration::from_secs(10)).unwrap();
        assert_eq!(cfg.bind_addr.port(), 9090);
        assert_eq!(cfg.timeout, Duration::from_secs(10));
        assert!(!cfg.auth.is_enabled());
    }

    #[test]
    fn test_auth_config() {
        let cfg = Config::new("9090", Some("user:pass".to_string()), None, None, Duration::from_secs(30)).unwrap();
        assert!(cfg.auth.is_enabled());
    }

    #[test]
    fn test_invalid_port() {
        assert!(Config::new("invalid", None, None, None, Duration::from_secs(30)).is_err());
        assert!(Config::new("99999", None, None, None, Duration::from_secs(30)).is_err());
    }
}
