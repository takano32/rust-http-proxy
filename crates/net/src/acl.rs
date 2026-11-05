//! 宛て先のホストの許可・拒否 (`PROXY_ALLOW_HOSTS` / `PROXY_DENY_HOSTS`) とその照合。
//!
//! 責務ごとに子のファイルへ分けてある (T.34.65)。公開の名前はここで出し直す (`proxy_net::acl::…` のまま)。
//! - `ports`: 許可するポートの集合 (`PortSet`)
//! - `clients`: 接続元 IP の許可リスト (`ClientAcl`)
//! - `local`: ローカル宛て・私設アドレス宛ての判定と `resolve_target`、自己ベンチの相手役

mod clients;
mod local;
mod ports;
pub use clients::ClientAcl;
#[cfg(test)]
use local::is_local_target;
pub use local::{AllowLocal, is_local_ip, resolve_target, set_self_bench_ports};
pub use ports::PortSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AclConfig {
    pub allow_hosts: Vec<String>,
    pub deny_hosts: Vec<String>,
}

impl AclConfig {
    pub fn new(allow: Option<&str>, deny: Option<&str>) -> Self {
        let parse_list = |opt: Option<&str>| -> Vec<String> {
            match opt {
                Some(s) => s
                    .split(',')
                    .map(|item| item.trim().to_ascii_lowercase())
                    .filter(|item| !item.is_empty())
                    .collect(),
                None => Vec::new(),
            }
        };

        Self {
            allow_hosts: parse_list(allow),
            deny_hosts: parse_list(deny),
        }
    }

    pub fn is_allowed(&self, host_or_addr: &str) -> bool {
        // 許可も拒否も設定されていなければ何もしない (既定の経路で文字列を作らない)
        if self.allow_hosts.is_empty() && self.deny_hosts.is_empty() {
            return true;
        }
        let host = extract_host(host_or_addr).to_ascii_lowercase();

        // 1. Check deny list first
        for pattern in &self.deny_hosts {
            if match_pattern(pattern, &host) {
                return false;
            }
        }

        // 2. If allow list is configured, host must match at least one pattern
        if !self.allow_hosts.is_empty() {
            return self.allow_hosts.iter().any(|p| match_pattern(p, &host));
        }

        true
    }
}

fn extract_host(host_or_addr: &str) -> &str {
    crate::net::split_host_port_ref(host_or_addr).0
}

pub fn match_pattern(pattern: &str, host: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    if pattern == host {
        return true;
    }

    if let Some(suffix) = pattern.strip_prefix("*.")
        && (host == suffix || host.ends_with(&format!(".{}", suffix)))
    {
        return true;
    }

    false
}

#[cfg(test)]
mod tests;
