//! 判定 (ACL) と接続が同じ答えを使うための入れ物 (`Resolved`) と、`resolve*` の入口。
//! 本体の表引きは `lookup` にある。T.34.54 で `dns.rs` から移した (本体は変えていない)。

use super::resolve_host_marked;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

/// 判定 (ACL) が引いた答えを、そのまま接続まで運ぶための入れ物 (T12.7)。
///
/// ローカル宛ての判定 (`proxy_net::acl::resolve_target`) と、その直後の接続
/// (`proxy_net::net::connect_with`) は**同じ答えを使う**。名前解決が 1 要求 1 回で
/// 済むだけでなく、`PROXY_DNS_TTL_SECS=0` (キャッシュ無効) でも判定と接続が
/// 別の答えを引かない (DNS rebinding で判定をすり抜けられない)。
pub struct Resolved<'a> {
    /// 解決したホスト名。**別のホスト宛ての接続には使わない**ための鍵で、
    /// 突き合わせは大小無視 (要求ごとに小文字の複製を作らない)
    host: &'a str,
    addrs: Vec<IpAddr>,
    /// このホストで最後に勝った族 (T12.1)。同じ鍵取りで一緒に受け取っておく
    preferred: Option<bool>,
    /// serve-stale で返した期限切れの答えか (T21.3)。立っていれば、接続側は繋げなかったとき
    /// 1 回だけ引き直して繋ぎ直す ([`relookup_after_stale`])
    stale: bool,
}

impl<'a> Resolved<'a> {
    pub fn new(host: &'a str, addrs: Vec<IpAddr>, preferred: Option<bool>) -> Resolved<'a> {
        Resolved {
            host,
            addrs,
            preferred,
            stale: false,
        }
    }

    /// serve-stale で返した答えだという印を付ける (T21.3)。
    pub fn with_stale(mut self, stale: bool) -> Resolved<'a> {
        self.stale = stale;
        self
    }

    /// serve-stale で返した期限切れの答えか (T21.3)。
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    pub fn addrs(&self) -> &[IpAddr] {
        &self.addrs
    }

    /// ホスト名の借用を手放してアドレス列だけを取り出す (別のスコープへ運ぶとき)。
    pub fn into_addrs(self) -> Vec<IpAddr> {
        self.addrs
    }

    pub fn preferred(&self) -> Option<bool> {
        self.preferred
    }

    /// `host` の答えなら、ポートを付けた候補列を返す (別のホストなら `None` = 引き直す)。
    pub fn socket_addrs_for(&self, host: &str, port: u16) -> Option<Vec<SocketAddr>> {
        self.host
            .eq_ignore_ascii_case(host)
            .then(|| with_port(&self.addrs, port))
    }
}

/// `addr_str` (`host:port`) を解決する。キャッシュがあれば OS に問い合わせない。
pub fn resolve(addr_str: &str) -> io::Result<Vec<SocketAddr>> {
    resolve_with_pref(addr_str).map(|(addrs, _)| addrs)
}

/// [`resolve`] に「最後に勝った族」の記憶を添えて返す (T12.1)。**表を引くのは 1 回だけ**
/// にするためで、接続側が別に [`preferred_family`] を呼ぶと鍵を 2 回取ることになる。
pub fn resolve_with_pref(addr_str: &str) -> io::Result<(Vec<SocketAddr>, Option<bool>)> {
    resolve_with_pref_marked(addr_str).map(|(addrs, pref, _)| (addrs, pref))
}

/// [`resolve_with_pref`] に「serve-stale で返した期限切れの答えか」の印を添えて返す (T21.3)。
pub fn resolve_with_pref_marked(
    addr_str: &str,
) -> io::Result<(Vec<SocketAddr>, Option<bool>, bool)> {
    let (host, port) = crate::hostport::split_host_port_ref(addr_str);
    let Some(port) = port else {
        return addr_str
            .to_socket_addrs()
            .map(|i| (i.collect(), None, false));
    };
    // IP リテラルは表も確保も要らない (熱い経路: ベンチも `127.0.0.1:port` で来る)
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok((vec![SocketAddr::new(ip, port)], None, false));
    }
    let (addrs, pref, stale) = resolve_host_marked(host, port)?;
    Ok((with_port(&addrs, port), pref, stale))
}

/// ホスト名を解決して**アドレスだけ**を返す (T12.7)。`port` は `getaddrinfo` に渡す
/// サービス番号で、キャッシュの鍵はホスト名だけ (返す側でポートを付ける)。
///
/// IP リテラルは表を使わない (解決するものが無い)。`PROXY_DNS_TTL_SECS=0` の
/// ときも表を使わないが、**OS に投げた回数はミスとして数える**
/// (`dns.hits + dns.misses` が「解決した回数」を表すため。T12.7)。
pub fn resolve_host(host: &str, port: u16) -> io::Result<(Vec<IpAddr>, Option<bool>)> {
    resolve_host_marked(host, port).map(|(addrs, pref, _)| (addrs, pref))
}

pub(super) fn with_port(addrs: &[IpAddr], port: u16) -> Vec<SocketAddr> {
    addrs.iter().map(|&ip| SocketAddr::new(ip, port)).collect()
}
