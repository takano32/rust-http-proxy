//! authority (`host[:port]`) の形の検査 (RFC 3986 §3.2・RFC 9112 §3.2。T32.12)。

use std::io;

use crate::ascii;

/// 絶対 URI の `//` の後ろで、authority (`host:port`) が終わる位置 (RFC 3986 §3.2: `/`・`?`・`#` の手前)。
///
/// `/` だけで切っていたので、`http://h?q` は `h?q` を名前として引きに行っていた (T27.8)。
pub(super) fn authority_end(rest: &str) -> usize {
    rest.find(['/', '?', '#']).unwrap_or(rest.len())
}

/// authority を確かめる。空ならホストが無い、`@` があれば userinfo (RFC 9110 §4.2.4。T27.8)。
///
/// userinfo は今までは `user:pw@host` を丸ごと名前として引きに行き 502 になっていた。
/// http(s) の URI では非推奨で、受け取った側は誤りとして扱うべきなので 400 にする
/// (呼び出し側は [`super::parse_origin`] の失敗を `bad_uri` で断る)。
///
/// T32.12 から形も見る (RFC 3986 §3.2.2・RFC 9112 §3.2): ホストは reg-name の字 (英数字と `-._~`、
/// `%XX`) か `[` IP リテラル `]`、ポートは 1〜5 桁の数字で 65535 以下。空のポート (`h:`) は既定の
/// ポートの意味で、返す authority からは `:` を落とす。括弧の無い IPv6 (`::1`) は今までどおり通す。
/// reg-name の sub-delims (`,`・`+`・`;` など) は RFC の文法では通るが、DNS の名前にはなりえないので断る。
/// 今までは `a b`・`a, b`・`a/b`・`h:99999` を名前解決まで行って 502 nxdomain (80 番宛ての origin
/// エラー) にし、`h:+22501` は Rust の `parse` が `+` を受けてオリジンへ `Host: h:+22501` を送っていた
pub(super) fn check_authority(host_port: &str) -> io::Result<&str> {
    // 速い道: ふつうの `host` / `host:port` (reg-name の字のあとに `:` と 1〜5 桁の数字) は 1 回の走査で
    // 決める。`[`・`%`・`@`・`:` 2 個以上・外れた形は下の遅い道で決め直す (結果は同じ。誤りの文言もそちら)
    let b = host_port.as_bytes();
    let host_end = b
        .iter()
        .position(|&c| !REG_NAME[c as usize])
        .unwrap_or(b.len());
    if host_end > 0 {
        if host_end == b.len() {
            return Ok(host_port);
        }
        if b[host_end] == b':' {
            let port = &b[host_end + 1..];
            if (1..=5).contains(&port.len()) {
                let mut n = 0u32;
                let mut digits = true;
                for &d in port {
                    digits &= d.is_ascii_digit();
                    n = n * 10 + u32::from(d.wrapping_sub(b'0'));
                }
                if digits && n <= 65535 {
                    return Ok(host_port);
                }
            }
        }
    }
    check_authority_slow(host_port)
}

/// reg-name の字のうち `%` を除いたもの (英数字と `-._~`)。[`check_authority`] の速い道の表 (T32.12)
const REG_NAME: [bool; 256] = {
    let mut t = [false; 256];
    let mut i = 0;
    while i < 256 {
        let b = i as u8;
        t[i] = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        i += 1;
    }
    t
};

/// [`check_authority`] の遅い道 (IP リテラル・`%XX`・空のポート・誤りの形)。
pub(super) fn check_authority_slow(host_port: &str) -> io::Result<&str> {
    let invalid = |what: &'static str| Err(io::Error::new(io::ErrorKind::InvalidInput, what));
    if host_port.is_empty() {
        return invalid("Missing host in request target");
    }
    if host_port.contains('@') {
        return invalid("userinfo in request target");
    }
    let b = host_port.as_bytes();
    let ipv6 = |s: &[u8]| {
        s.iter()
            .all(|c| c.is_ascii_hexdigit() || matches!(c, b':' | b'.'))
    };
    let (host_ok, port) = if b[0] == b'[' {
        match b.iter().position(|c| *c == b']') {
            Some(end) => {
                let inner = &b[1..end];
                let ok = !inner.is_empty() && inner.contains(&b':') && ipv6(inner);
                match &b[end + 1..] {
                    [] => (ok, None),
                    [b':', port @ ..] => (ok, Some(port)),
                    _ => (false, None),
                }
            }
            None => (false, None),
        }
    } else if ascii::count(host_port, b':') >= 2 {
        // 括弧の無い IPv6 リテラル (ポート無し)。`net::split_host_port_ref` と同じ読み方
        (ipv6(b), None)
    } else {
        let (host, port) = match b.iter().rposition(|c| *c == b':') {
            Some(i) => (&b[..i], Some(&b[i + 1..])),
            None => (b, None),
        };
        (!host.is_empty() && reg_name(host), port)
    };
    if !host_ok {
        return invalid("invalid host in request target");
    }
    match port {
        // 空のポートは既定のポート (RFC 3986 §3.2.3)。`:` を落として返す
        Some([]) => Ok(&host_port[..host_port.len() - 1]),
        Some(p)
            if p.len() > 5
                || !p.iter().all(u8::is_ascii_digit)
                || p.iter().fold(0u32, |n, d| n * 10 + u32::from(d - b'0')) > 65535 =>
        {
            invalid("invalid port in request target")
        }
        _ => Ok(host_port),
    }
}

/// reg-name (と IPv4) として受け付ける字か (英数字と `-._~`、`%` は 16 進 2 桁が続くこと)。T32.12
fn reg_name(host: &[u8]) -> bool {
    let mut i = 0;
    while i < host.len() {
        match host[i] {
            b'%' => {
                if !host
                    .get(i + 1..i + 3)
                    .is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit))
                {
                    return false;
                }
                i += 3;
                continue;
            }
            c if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~') => {}
            _ => return false,
        }
        i += 1;
    }
    true
}
