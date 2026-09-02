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

fn extract_host(host_or_addr: &str) -> String {
    crate::net::split_host_port(host_or_addr).0
}

pub(crate) fn match_pattern(pattern: &str, host: &str) -> bool {
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
mod tests {
    use super::*;

    #[test]
    fn test_ipv6_literal_host() {
        let acl = AclConfig::new(Some("2001:db8::1, example.com"), None);
        assert!(acl.is_allowed("[2001:db8::1]:443"));
        assert!(acl.is_allowed("[2001:db8::1]"));
        assert!(acl.is_allowed("example.com:80"));
        assert!(!acl.is_allowed("[2001:db8::2]:443"));
    }

    #[test]
    fn test_acl_default_allows_all() {
        let acl = AclConfig::new(None, None);
        assert!(acl.is_allowed("example.com"));
        assert!(acl.is_allowed("example.com:443"));
    }

    #[test]
    fn test_acl_deny() {
        let acl = AclConfig::new(None, Some("bad.com, *.blocked.org"));
        assert!(!acl.is_allowed("bad.com"));
        assert!(!acl.is_allowed("bad.com:80"));
        assert!(!acl.is_allowed("sub.blocked.org"));
        assert!(!acl.is_allowed("blocked.org"));
        assert!(acl.is_allowed("good.com"));
    }

    #[test]
    fn test_acl_allow() {
        let acl = AclConfig::new(Some("*.example.com, rust-lang.org"), None);
        assert!(acl.is_allowed("example.com"));
        assert!(acl.is_allowed("api.example.com"));
        assert!(acl.is_allowed("rust-lang.org:443"));
        assert!(!acl.is_allowed("other.com"));
    }
}
