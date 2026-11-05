//! アドレス列の**純粋な**並べ替え: v4-mapped IPv6 の正規化と、RFC 8305 §4 の交互配置。
//! 入出力 (ソケット) を持たないので、そのまま単体試験で突き合わせられる。

use std::net::{IpAddr, SocketAddr};

/// v4-mapped IPv6 (`::ffff:1.2.3.4`) を IPv4 に戻す。
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

#[inline]
pub fn canonical_addr(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(canonical_ip(addr.ip()), addr.port())
}

/// 名前解決の結果を IPv6 優先で交互に並べる (RFC 8305 §4)。
#[cfg(test)]
pub(crate) fn interleave(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    interleave_from(addrs, true)
}

/// [`interleave`] の、**先頭に置く族だけ**を選べる版 (RFC 8305 §8)。交互に並べること自体も、
/// 同じ族の中の順も変えない。入れ替えるのは「どちらを先に置くか」だけ。
pub(crate) fn interleave_from(addrs: Vec<SocketAddr>, v6_first: bool) -> Vec<SocketAddr> {
    let (v6, v4): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|a| a.is_ipv6());
    let mut out = Vec::with_capacity(v6.len() + v4.len());
    let (mut a, mut b) = if v6_first {
        (v6.into_iter(), v4.into_iter())
    } else {
        (v4.into_iter(), v6.into_iter())
    };
    loop {
        match (a.next(), b.next()) {
            (None, None) => break,
            (x, y) => {
                out.extend(x);
                out.extend(y);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_mapped_addresses() {
        let mapped: IpAddr = "::ffff:192.0.2.1".parse().unwrap();
        assert_eq!(canonical_ip(mapped), "192.0.2.1".parse::<IpAddr>().unwrap());
        let real: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(canonical_ip(real), real);
        let sa: SocketAddr = "[::ffff:10.0.0.1]:5".parse().unwrap();
        assert_eq!(canonical_addr(sa).to_string(), "10.0.0.1:5");
    }

    #[test]
    fn interleaves_v6_first() {
        let v4a: SocketAddr = "192.0.2.1:80".parse().unwrap();
        let v4b: SocketAddr = "192.0.2.2:80".parse().unwrap();
        let v6a: SocketAddr = "[2001:db8::1]:80".parse().unwrap();
        assert_eq!(interleave(vec![v4a, v4b, v6a]), vec![v6a, v4a, v4b]);
        assert_eq!(interleave(vec![v4a]), vec![v4a]);
        // 先頭に置く族を入れ替えても、同じ族の中の順は変わらない (T12.1)
        assert_eq!(
            interleave_from(vec![v4a, v4b, v6a], false),
            vec![v4a, v6a, v4b]
        );
        assert_eq!(interleave_from(vec![v6a], false), vec![v6a]);
    }

    /// 片方の族しか無ければ順はそのまま、空なら空 (交互配置の端の形)。
    #[test]
    fn interleave_keeps_a_single_family_and_the_empty_list_as_they_are() {
        let v6a: SocketAddr = "[2001:db8::1]:80".parse().unwrap();
        let v6b: SocketAddr = "[2001:db8::2]:80".parse().unwrap();
        assert_eq!(interleave(vec![v6a, v6b]), vec![v6a, v6b]);
        assert_eq!(interleave_from(vec![v6a, v6b], false), vec![v6a, v6b]);
        assert_eq!(interleave(Vec::new()), Vec::<SocketAddr>::new());
    }
}
