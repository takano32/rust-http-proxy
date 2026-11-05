//! 接続元 IP の許可リスト (`PROXY_ALLOW_CLIENTS`)。T.34.65 で `acl.rs` から移した (本体は変えていない)。

use std::net::IpAddr;

/// 接続元 IP の許可リスト (`PROXY_ALLOW_CLIENTS`)。空なら全許可 (既定)。
///
/// 宛先の [`AclConfig`](super::AclConfig) とは**別物**で、こちらは accept した相手の IP を見る
/// ([`is_local_ip`](super::is_local_ip) / `PROXY_ALLOW_LOCAL` は宛先の話なので無関係)。
/// **認証ではない** — 経路を絞るだけで、同じアドレスから来られれば誰でも通る。
///
/// 持ち方は「前置長の付いたアドレス」の配列 (`1.2.3.4` は `/32`、`2001:db8::` は `/128`)。
/// 想定は多くても数十件なので線形に照合する (16 件で 1 us 未満)。
/// v4-mapped IPv6 (`::ffff:1.2.3.4`) は [`crate::net::canonical_ip`] で IPv4 に直してから
/// 照合するので、デュアルスタックで待ち受けていても `1.2.3.4` の 1 行で書ける。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientAcl {
    v4: Vec<(u32, u8)>,
    v6: Vec<(u128, u8)>,
}

impl ClientAcl {
    /// `1.2.3.4,10.0.0.0/8,2001:db8::/32` を読む。空文字なら全許可。
    /// **書式が違う項目は無視する** ([`PortSet::parse`](super::PortSet::parse) と同じ作法。1 項目の書き損じで
    /// 全部が空 = 全許可 に化けると、絞ったつもりで絞れていないことになるため)。
    pub fn parse(spec: &str) -> ClientAcl {
        let mut acl = ClientAcl::default();
        for item in spec.split(',').map(str::trim).filter(|i| !i.is_empty()) {
            let (addr, len) = match item.split_once('/') {
                Some((a, b)) => match b.trim().parse::<u8>() {
                    Ok(n) => (a.trim(), Some(n)),
                    Err(_) => continue,
                },
                None => (item, None),
            };
            let Ok(ip) = addr
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
            else {
                continue;
            };
            match crate::net::canonical_ip(ip) {
                // `::ffff:1.2.3.0/120` のような書き方も IPv4 の数え方に直す
                IpAddr::V4(v4) => {
                    let bits = match len {
                        Some(n) if ip.is_ipv6() => match n.checked_sub(96) {
                            Some(n) => n,
                            None => continue,
                        },
                        Some(n) => n,
                        None => 32,
                    };
                    if bits <= 32 {
                        acl.v4.push((u32::from(v4), bits));
                    }
                }
                IpAddr::V6(v6) => {
                    let bits = len.unwrap_or(128);
                    if bits <= 128 {
                        acl.v6.push((u128::from(v6), bits));
                    }
                }
            }
        }
        acl
    }

    /// 1 項目も無いか (= 全許可。**既定の経路はこの分岐 1 回だけ**)。
    pub fn is_empty(&self) -> bool {
        self.v4.is_empty() && self.v6.is_empty()
    }

    /// この接続元を受けてよいか。空なら常に true。
    pub fn allows(&self, ip: IpAddr) -> bool {
        if self.is_empty() {
            return true;
        }
        // 前置長 0 (`0.0.0.0/0`) は `>>` が桁あふれるので先に拾う
        match crate::net::canonical_ip(ip) {
            IpAddr::V4(v4) => {
                let a = u32::from(v4);
                (self.v4.iter()).any(|&(net, bits)| bits == 0 || (a ^ net) >> (32 - bits) == 0)
            }
            IpAddr::V6(v6) => {
                let a = u128::from(v6);
                (self.v6.iter()).any(|&(net, bits)| bits == 0 || (a ^ net) >> (128 - bits) == 0)
            }
        }
    }
}

/// 起動ログに「実際に読めた項目」を出すため (書き損じた項目は消えているので気づける)。
impl std::fmt::Display for ClientAcl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let v4 = (self.v4.iter())
            .map(|&(net, bits)| (IpAddr::V4(std::net::Ipv4Addr::from(net)), bits, 32u8));
        let v6 = (self.v6.iter())
            .map(|&(net, bits)| (IpAddr::V6(std::net::Ipv6Addr::from(net)), bits, 128u8));
        for (i, (ip, bits, full)) in v4.chain(v6).enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            if bits == full {
                write!(f, "{}", ip)?;
            } else {
                write!(f, "{}/{}", ip, bits)?;
            }
        }
        Ok(())
    }
}
