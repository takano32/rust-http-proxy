use std::net::SocketAddr;

const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "upgrade",
    "proxy-connection",
];

pub fn is_hop_by_hop(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP_HEADERS.contains(&lower.as_str())
}

pub fn sanitize_and_inject_headers(
    headers: &[String],
    client_addr: Option<SocketAddr>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut custom_hop_by_hop = Vec::new();
    let mut x_forwarded_for: Option<String> = None;
    let mut has_via = false;

    // First pass: check Connection header for custom hop-by-hop header names
    for line in headers {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("connection") {
                for item in v.split(',') {
                    custom_hop_by_hop.push(item.trim().to_ascii_lowercase());
                }
            }
        }
    }

    // Second pass: filter and collect
    for line in headers {
        if let Some((k, v)) = line.split_once(':') {
            let k_trim = k.trim();
            let k_lower = k_trim.to_ascii_lowercase();

            if is_hop_by_hop(&k_lower) || custom_hop_by_hop.contains(&k_lower) {
                continue;
            }

            if k_lower == "x-forwarded-for" {
                x_forwarded_for = Some(v.trim().to_string());
                continue;
            }

            if k_lower == "via" {
                has_via = true;
                let new_via = format!("{}: {}, 1.1 sorahost-http-proxy\r\n", k_trim, v.trim());
                out.push(new_via);
                continue;
            }

            out.push(line.clone());
        }
    }

    // Add X-Forwarded-For
    if let Some(addr) = client_addr {
        let ip_str = addr.ip().to_string();
        let xff_val = match x_forwarded_for {
            Some(existing) => format!("{}, {}", existing, ip_str),
            None => ip_str,
        };
        out.push(format!("X-Forwarded-For: {}\r\n", xff_val));
    } else if let Some(existing) = x_forwarded_for {
        out.push(format!("X-Forwarded-For: {}\r\n", existing));
    }

    // Add Via if not already updated
    if !has_via {
        out.push("Via: 1.1 sorahost-http-proxy\r\n".to_string());
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hop_by_hop_removal() {
        let raw = vec![
            "Host: example.com\r\n".to_string(),
            "Proxy-Connection: keep-alive\r\n".to_string(),
            "Keep-Alive: timeout=5\r\n".to_string(),
            "Connection: close, X-Foo\r\n".to_string(),
            "X-Foo: bar\r\n".to_string(),
            "User-Agent: curl/7.88.1\r\n".to_string(),
        ];
        let addr = "192.168.1.100:54321".parse().unwrap();
        let cleaned = sanitize_and_inject_headers(&raw, Some(addr));

        assert!(cleaned.iter().any(|h| h.starts_with("Host: example.com")));
        assert!(cleaned.iter().any(|h| h.starts_with("User-Agent: curl/7.88.1")));
        assert!(cleaned.iter().any(|h| h.starts_with("X-Forwarded-For: 192.168.1.100")));
        assert!(cleaned.iter().any(|h| h.starts_with("Via: 1.1 sorahost-http-proxy")));

        assert!(!cleaned.iter().any(|h| h.to_ascii_lowercase().starts_with("proxy-connection")));
        assert!(!cleaned.iter().any(|h| h.to_ascii_lowercase().starts_with("keep-alive")));
        assert!(!cleaned.iter().any(|h| h.to_ascii_lowercase().starts_with("x-foo")));
    }
}
