//! 名前解決と組み合わせた接続の入口 (`connect` / `connect_with` / `connect_resolved`) と、
//! serve-stale の古い答えで繋げなかったときの繋ぎ直し (T21.3)、続けて時間切れになった宛先を
//! 待たずに断る門 (T36.8。表は [`super::negative`])。本体は [`super::happy_eyeballs`]。

use std::io;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use super::happy_eyeballs::connect_candidates;
use super::ipv6_enabled;
use super::negative::{Gate, NEGATIVE, skipped_error};
use crate::hostport::split_host_port_ref;
use crate::log_debug;

/// 名前解決して接続する。IPv6 無効時は A レコードだけ、有効時は Happy Eyeballs。全体の締め切りは `timeout`
/// (`Duration::ZERO` は無期限 = OS 既定の接続タイムアウトに任せる)。
pub fn connect(addr_str: &str, timeout: Duration) -> io::Result<TcpStream> {
    connect_with(addr_str, None, timeout)
}

/// [`connect`] の、**判定 (ACL) が引いた答えを渡せる版** (T12.7)。
///
/// `resolved` が同じホストの答えなら、ここでは名前解決をしない (要求 1 本で表を 1 回しか
/// 引かない。`PROXY_DNS_TTL_SECS=0` でも `getaddrinfo` は 1 回だけで、判定と接続が
/// 必ず同じ答えを使う)。`None` (IP リテラル、`PROXY_ALLOW_LOCAL=on` で判定を飛ばした、
/// 判定を経ない経路) のときは今までどおりここで解決する。
///
/// 同じ宛先が続けて時間切れになっていれば、繋ぎに行かずに [`super::is_skipped`] の
/// `TimedOut` で断る (T36.8。`PROXY_CONNECT_NEGATIVE_SECS`)。表が空なら原子の読み 1 回だけ。
/// ここは `#[inline]` にしない (呼び手の中に門ごと展開されると、呼び手の側に枠が 1 つ増えて
/// 確立 1 本あたり約 40 Ir 増えた。本体は下の `connect_with_inner` を中に畳む)。
pub fn connect_with(
    addr_str: &str,
    resolved: Option<&crate::dns::Resolved<'_>>,
    timeout: Duration,
) -> io::Result<TcpStream> {
    let gate = NEGATIVE.admit(addr_str, Instant::now);
    if let Gate::Skip(left) = gate {
        return Err(skipped_error(left));
    }
    let result = connect_with_inner(addr_str, resolved, timeout);
    let timed_out = matches!(&result, Err(e) if e.kind() == io::ErrorKind::TimedOut);
    NEGATIVE.settle(addr_str, gate, timed_out, Instant::now);
    result
}

/// [`connect_with`] の本体 (名前解決と、serve-stale の古い答えで繋げなかったときの繋ぎ直し)。
#[inline(always)]
fn connect_with_inner(
    addr_str: &str,
    resolved: Option<&crate::dns::Resolved<'_>>,
    timeout: Duration,
) -> io::Result<TcpStream> {
    let (host, port) = split_host_port_ref(addr_str);
    // 解決と一緒に「最後に勝った族」と「serve-stale の古い答えか」(T21.3) も受け取る
    // (表を引くのは 1 回だけ。T12.1)
    let (addrs, preferred, stale) = match resolved.zip(port).and_then(|(r, p)| {
        r.socket_addrs_for(host, p)
            .map(|a| (a, r.preferred(), r.is_stale()))
    }) {
        Some(triple) => triple,
        None => crate::dns::resolve_with_pref_marked(addr_str)?,
    };
    // 古い答えのときだけ、繋げなかったときに比べる元の番地を控える (T21.3。普通の回は
    // 確保もしない)
    let old: Option<Vec<IpAddr>> = stale.then(|| addrs.iter().map(|a| a.ip()).collect());
    let result = connect_addrs(host, addrs, preferred, timeout);
    match (old, port) {
        (Some(old), Some(port)) => match result {
            Err(e) if stale_retryable(&e) => retry_after_stale(host, port, &old, e, timeout),
            r => r,
        },
        _ => result,
    }
}

/// IPv6 を切っていれば IPv4 だけにして、候補列に繋ぐ ([`connect_with`] の本体)。
fn connect_addrs(
    host: &str,
    addrs: Vec<SocketAddr>,
    preferred: Option<bool>,
    timeout: Duration,
) -> io::Result<TcpStream> {
    let addrs = usable_family(addrs, ipv6_enabled())
        .map_err(|msg| io::Error::new(io::ErrorKind::NotFound, msg))?;
    connect_candidates(host, addrs, preferred, timeout)
}

/// `ipv6` が偽なら IPv4 の候補だけに絞る。残らなければ `NotFound` に添える文言を返す
/// (IPv6 を切っているせいで残らなかったときは、戻し方 (`PROXY_IPV6=on`) を添える)。
pub(super) fn usable_family(
    addrs: Vec<SocketAddr>,
    ipv6: bool,
) -> Result<Vec<SocketAddr>, &'static str> {
    let addrs: Vec<SocketAddr> = if ipv6 {
        addrs
    } else {
        addrs.into_iter().filter(|a| a.is_ipv4()).collect()
    };
    if addrs.is_empty() {
        return Err(if ipv6 {
            "Could not resolve host"
        } else {
            "host has no IPv4 address (IPv6 is disabled; set PROXY_IPV6=on)"
        });
    }
    Ok(addrs)
}

/// 古い答え (serve-stale) で繋げなかったとき、引き直す価値のある失敗か (T21.3)。
///
/// `refused` / `unreachable` / `timeout` (`ErrCause::from_io` と同じ分け方) は「その番地にもう
/// 相手が居ない」ことがありうる。`reset` や TLS などは繋がったあとの話なので引き直さない。
pub(super) fn stale_retryable(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::AddrNotAvailable
            | io::ErrorKind::TimedOut
    )
}

/// 古い答えで繋げなかったので、1 回だけ引き直して、**番地が変わっていれば** 1 回だけ繋ぎ直す (T21.3)。
/// 番地が同じ・引けなかったときは元のエラー `first` を返す。繋ぎ直しの締め切りは元と同じ `timeout`。
fn retry_after_stale(
    host: &str,
    port: u16,
    old: &[IpAddr],
    first: io::Error,
    timeout: Duration,
) -> io::Result<TcpStream> {
    let Some((ips, preferred)) = crate::dns::relookup_after_stale(host, port, old) else {
        return Err(first);
    };
    log_debug!(
        None,
        "stale DNS answer for {} failed ({}); retrying with the new answer",
        host,
        first
    );
    let addrs = ips.iter().map(|&ip| SocketAddr::new(ip, port)).collect();
    connect_addrs(host, addrs, preferred, timeout)
}

/// 解決済みのアドレス列に Happy Eyeballs で接続する (`host` はホストごとの記憶の鍵。空なら覚えない)。
pub fn connect_resolved(
    host: &str,
    addrs: Vec<SocketAddr>,
    timeout: Duration,
) -> io::Result<TcpStream> {
    // 候補が 1 つなら並べ替えも記憶も要らない (熱い経路。ここでは鍵を取らない)
    let preferred = (addrs.len() > 1)
        .then(|| crate::dns::preferred_family(host))
        .flatten();
    connect_candidates(host, addrs, preferred, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IPv6 を切ると A だけが残り、残らなければ戻し方を添えた文言。有効なら候補はそのまま。
    #[test]
    fn disabling_ipv6_keeps_only_a_records() {
        let v4: SocketAddr = "192.0.2.1:443".parse().unwrap();
        let v6: SocketAddr = "[2001:db8::1]:443".parse().unwrap();
        assert_eq!(usable_family(vec![v6, v4], true), Ok(vec![v6, v4]));
        assert_eq!(usable_family(vec![v6, v4], false), Ok(vec![v4]));
        assert_eq!(
            usable_family(vec![v6], false),
            Err("host has no IPv4 address (IPv6 is disabled; set PROXY_IPV6=on)")
        );
        assert_eq!(
            usable_family(Vec::new(), true),
            Err("Could not resolve host")
        );
    }

    /// 繋げなかった理由のうち、引き直す価値があるのは「その番地にもう相手が居ない」もの (T21.3)。
    #[test]
    fn only_unreachable_kinds_are_worth_a_relookup() {
        for kind in [
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::NetworkUnreachable,
            io::ErrorKind::HostUnreachable,
            io::ErrorKind::AddrNotAvailable,
            io::ErrorKind::TimedOut,
        ] {
            assert!(stale_retryable(&io::Error::from(kind)), "{:?}", kind);
        }
        for kind in [
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::Other,
            io::ErrorKind::InvalidInput,
        ] {
            assert!(!stale_retryable(&io::Error::from(kind)), "{:?}", kind);
        }
    }
}
