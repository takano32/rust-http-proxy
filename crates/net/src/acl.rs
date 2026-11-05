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

/// `pattern` (大小を問わない。`*.` で始まればその名前と下位の名前) が `host` に当たるか。
///
/// 比べ方は「`pattern` を小文字にしたものと `host` をバイトで比べる」(`host` は呼ぶ側が小文字に
/// してから渡す)。T.34.66 で、照合のたびに作っていた `String` 2 つ (`to_ascii_lowercase` と
/// `format!(".{suffix}")`) を無くした: パターンは借用のまま 1 バイトずつ小文字にして比べ、`*.` は
/// 末尾が一致することと、その手前が `.` であることで見る。
#[inline]
pub fn match_pattern(pattern: &str, host: &str) -> bool {
    let (pattern, host) = (pattern.as_bytes(), host.as_bytes());
    if eq_lowered(pattern, host) {
        return true;
    }
    // `*` と `.` は小文字にしても変わらないので、そのまま外してよい
    if let Some(suffix) = pattern.strip_prefix(b"*.") {
        if eq_lowered(suffix, host) {
            return true;
        }
        // `host` が `.{suffix}` で終わるか
        if host.len() > suffix.len() {
            let cut = host.len() - suffix.len();
            return host[cut - 1] == b'.' && eq_lowered(suffix, &host[cut..]);
        }
    }
    false
}

/// `pattern` を ASCII の小文字にしたものが `host` とバイトで等しいか (確保しない)。
#[inline]
fn eq_lowered(pattern: &[u8], host: &[u8]) -> bool {
    pattern.len() == host.len()
        && pattern
            .iter()
            .zip(host)
            .all(|(p, h)| p.to_ascii_lowercase() == *h)
}

#[cfg(test)]
mod tests;
