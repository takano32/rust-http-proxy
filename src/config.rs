use std::env;
use std::net::SocketAddr;

use crate::acl::AclConfig;
use crate::auth::AuthConfig;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub auth: AuthConfig,
    pub acl: AclConfig,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let port_str = env::var("SERVER_PORT").unwrap_or_else(|_| "8080".to_string());
        let auth_val = env::var("PROXY_AUTH").ok();
        let allow_hosts = env::var("PROXY_ALLOW_HOSTS").ok();
        let deny_hosts = env::var("PROXY_DENY_HOSTS").ok();

        Self::new(
            &port_str,
            auth_val,
            allow_hosts.as_deref(),
            deny_hosts.as_deref(),
        )
    }

    pub fn new(
        port_str: &str,
        proxy_auth: Option<String>,
        allow_hosts: Option<&str>,
        deny_hosts: Option<&str>,
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_port() {
        let cfg = Config::new("9090", None, None, None).unwrap();
        assert_eq!(cfg.bind_addr.port(), 9090);
        assert!(!cfg.auth.is_enabled());
    }

    #[test]
    fn test_auth_config() {
        let cfg = Config::new("9090", Some("user:pass".to_string()), None, None).unwrap();
        assert!(cfg.auth.is_enabled());
    }

    #[test]
    fn test_invalid_port() {
        assert!(Config::new("invalid", None, None, None).is_err());
        assert!(Config::new("99999", None, None, None).is_err());
    }
}
