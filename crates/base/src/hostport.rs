//! `host:port` の分解と組み立て (ASCII だけを見る)。
//!
//! 名前解決 (`proxy-net-dns`)・接続 (`proxy-net-conn`)・判定 (`proxy-net`) の 3 つとも
//! 使うので、いちばん下の層に置いてある。`crate::net` からは今までの綴りのまま
//! (`net::split_host_port_ref`) 呼べるように再輸出している。

/// `host:port` / `[v6]:port` / `[v6]` / `host` / 素の `v6` を (ホスト, ポート) に分ける。
/// 文字列を作らない版 ([`split_host_port`] は所有権が要るときに使う)。
#[inline]
pub fn split_host_port_ref(s: &str) -> (&str, Option<u16>) {
    // 要求ごとに何度も通るので、区切りの探索も空白の除去も ASCII だけで済ませる
    let s = s.trim_ascii();
    if let Some(rest) = s.strip_prefix('[') {
        if let Some(end) = rest.as_bytes().iter().position(|b| *b == b']') {
            let host = &rest[..end];
            let port = rest[end + 1..]
                .strip_prefix(':')
                .and_then(|p| p.parse::<u16>().ok());
            return (host, port);
        }
        return (s, None);
    }
    // ':' が 2 つ以上あれば括弧無しの IPv6 リテラル (ポート無し)
    if crate::ascii::count(s, b':') >= 2 {
        return (s, None);
    }
    match crate::ascii::rsplit_once(s, b':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(p) => (host, Some(p)),
            Err(_) => (s, None),
        },
        None => (s, None),
    }
}

/// [`split_host_port_ref`] のホストを複製して返す版。
#[inline]
pub fn split_host_port(s: &str) -> (String, Option<u16>) {
    let (host, port) = split_host_port_ref(s);
    (host.to_string(), port)
}

/// ホストとポートを `host:port` に組み立てる (IPv6 リテラルは括弧で囲む)。
pub fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

/// ポートが無ければ `default` を補った `host:port` を返す。
pub fn with_default_port(s: &str, default: u16) -> String {
    let (host, port) = split_host_port(s);
    join_host_port(&host, port.unwrap_or(default))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_host_and_port_forms() {
        assert_eq!(
            split_host_port("example.com:8080"),
            ("example.com".into(), Some(8080))
        );
        assert_eq!(split_host_port("example.com"), ("example.com".into(), None));
        assert_eq!(
            split_host_port("[2001:db8::1]:443"),
            ("2001:db8::1".into(), Some(443))
        );
        assert_eq!(
            split_host_port("[2001:db8::1]"),
            ("2001:db8::1".into(), None)
        );
        assert_eq!(split_host_port("2001:db8::1"), ("2001:db8::1".into(), None));
        assert_eq!(
            split_host_port("host:notaport"),
            ("host:notaport".into(), None)
        );
        assert_eq!(with_default_port("example.com", 80), "example.com:80");
        assert_eq!(with_default_port("[::1]", 80), "[::1]:80");
        assert_eq!(with_default_port("::1", 443), "[::1]:443");
        assert_eq!(with_default_port("[::1]:8080", 80), "[::1]:8080");
        assert_eq!(join_host_port("1.2.3.4", 1), "1.2.3.4:1");
    }
}
