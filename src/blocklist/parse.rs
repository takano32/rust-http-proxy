//! ブロックリストの解析 (hosts 形式 / 1 行 1 ドメイン)。

use std::collections::HashSet;

pub fn parse(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let Some(first) = tokens.next() else {
            continue;
        };
        // "0.0.0.0 host" / "127.0.0.1 host" / "::1 host" は後ろがホスト、それ以外は先頭がホスト
        let hosts: Vec<&str> = if first.parse::<std::net::IpAddr>().is_ok() {
            tokens.collect()
        } else {
            vec![first]
        };
        for h in hosts {
            let h = h.trim_end_matches('.').to_ascii_lowercase();
            if is_local_name(&h) || !looks_like_host(&h) {
                continue;
            }
            out.insert(h);
        }
    }
    out
}

fn is_local_name(h: &str) -> bool {
    matches!(
        h,
        "localhost"
            | "localhost.localdomain"
            | "local"
            | "broadcasthost"
            | "ip6-localhost"
            | "ip6-loopback"
            | "ip6-localnet"
            | "ip6-mcastprefix"
            | "ip6-allnodes"
            | "ip6-allrouters"
            | "ip6-allhosts"
            | "0.0.0.0"
    )
}

fn looks_like_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.contains('.')
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
}
