use std::env;
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let port_str = env::var("SERVER_PORT").unwrap_or_else(|_| "8080".to_string());
        Self::from_port_str(&port_str)
    }

    pub fn from_port_str(port_str: &str) -> Result<Self, String> {
        let port: u16 = port_str
            .parse()
            .map_err(|e| format!("Invalid SERVER_PORT '{}': {}", port_str, e))?;
        let bind_addr: SocketAddr = format!("0.0.0.0:{}", port)
            .parse()
            .map_err(|e| format!("Invalid bind address: {}", e))?;
        Ok(Self { bind_addr })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_port() {
        let cfg = Config::from_port_str("9090").unwrap();
        assert_eq!(cfg.bind_addr.port(), 9090);
    }

    #[test]
    fn test_invalid_port() {
        assert!(Config::from_port_str("invalid").is_err());
        assert!(Config::from_port_str("99999").is_err());
    }
}
