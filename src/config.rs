use std::env;
use std::net::SocketAddr;

use crate::auth::AuthConfig;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub auth: AuthConfig,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let port_str = env::var("SERVER_PORT").unwrap_or_else(|_| "8080".to_string());
        let auth_val = env::var("PROXY_AUTH").ok();
        Self::new(&port_str, auth_val)
    }

    pub fn new(port_str: &str, proxy_auth: Option<String>) -> Result<Self, String> {
        let port: u16 = port_str
            .parse()
            .map_err(|e| format!("Invalid SERVER_PORT '{}': {}", port_str, e))?;
        let bind_addr: SocketAddr = format!("0.0.0.0:{}", port)
            .parse()
            .map_err(|e| format!("Invalid bind address: {}", e))?;
        let auth = AuthConfig::new(proxy_auth);
        Ok(Self { bind_addr, auth })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_port() {
        let cfg = Config::new("9090", None).unwrap();
        assert_eq!(cfg.bind_addr.port(), 9090);
        assert!(!cfg.auth.is_enabled());
    }

    #[test]
    fn test_auth_config() {
        let cfg = Config::new("9090", Some("user:pass".to_string())).unwrap();
        assert!(cfg.auth.is_enabled());
    }

    #[test]
    fn test_invalid_port() {
        assert!(Config::new("invalid", None).is_err());
        assert!(Config::new("99999", None).is_err());
    }
}
