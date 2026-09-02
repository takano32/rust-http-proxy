//! IPv4 / IPv6 デュアルスタックのためのネットワーク補助 (`PROXY_IPV6=off` で IPv4 のみ)。
//!
//! - 待ち受け: `[::]` と `0.0.0.0` の両方を試し、IPv6 が無い環境では IPv4 だけにフォールバック。
//!   IPv6 無効時は `0.0.0.0` のみ
//! - 接続: A / AAAA の両方を引き、IPv6 優先で 250 ms ずつずらして並行に試す (Happy Eyeballs,
//!   RFC 8305)。IPv6 無効時は A レコードだけ
//! - `[2001:db8::1]:8080` 形式のホスト・ポート解析と、v4-mapped アドレス (`::ffff:1.2.3.4`) の正規化

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crate::{log_debug, log_warn};

/// Happy Eyeballs で次の接続試行を始めるまでの間隔。
const STAGGER: Duration = Duration::from_millis(250);

/// IPv6 を使うか (既定 on)。起動時に設定から決める。
static IPV6_ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_ipv6_enabled(on: bool) {
    IPV6_ENABLED.store(on, Ordering::Relaxed);
}

pub fn ipv6_enabled() -> bool {
    IPV6_ENABLED.load(Ordering::Relaxed)
}

/// `host:port` / `[v6]:port` / `[v6]` / `host` / 素の `v6` を (ホスト, ポート) に分ける。
pub fn split_host_port(s: &str) -> (String, Option<u16>) {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let host = rest[..end].to_string();
            let port = rest[end + 1..]
                .strip_prefix(':')
                .and_then(|p| p.parse::<u16>().ok());
            return (host, port);
        }
        return (s.to_string(), None);
    }
    // ':' が 2 つ以上あれば括弧無しの IPv6 リテラル (ポート無し)
    if s.matches(':').count() >= 2 {
        return (s.to_string(), None);
    }
    match s.rsplit_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(p) => (host.to_string(), Some(p)),
            Err(_) => (s.to_string(), None),
        },
        None => (s.to_string(), None),
    }
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

pub fn canonical_addr(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(canonical_ip(addr.ip()), addr.port())
}

/// 待ち受けソケットを作る。`addrs` が空なら IPv6 有効時はデュアルスタック (`[::]` + `0.0.0.0`)、
/// 無効時は `0.0.0.0` だけ。`port` が 0 のときは最初に取れたポートを残りにも使う。
pub fn bind_all(addrs: &[IpAddr], port: u16) -> io::Result<Vec<TcpListener>> {
    bind_all_with(addrs, port, ipv6_enabled())
}

pub fn bind_all_with(addrs: &[IpAddr], port: u16, ipv6: bool) -> io::Result<Vec<TcpListener>> {
    let candidates: Vec<IpAddr> = if addrs.is_empty() {
        if ipv6 {
            vec![
                IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            ]
        } else {
            vec![IpAddr::V4(Ipv4Addr::UNSPECIFIED)]
        }
    } else {
        addrs.to_vec()
    };
    let auto = addrs.is_empty();
    let mut out: Vec<TcpListener> = Vec::new();
    let mut port = port;
    let mut first_err = None;
    for ip in candidates {
        match TcpListener::bind(SocketAddr::new(ip, port)) {
            Ok(l) => {
                if port == 0 {
                    port = l.local_addr().map(|a| a.port()).unwrap_or(0);
                }
                out.push(l);
            }
            // `[::]` がデュアルスタックで v4 も受けているときは `0.0.0.0` が使用中になる
            Err(e) if auto && e.kind() == io::ErrorKind::AddrInUse && !out.is_empty() => {
                log_debug!(None, "{} already covered by the dual-stack socket", ip);
            }
            Err(e) if auto && ip.is_ipv6() => {
                log_debug!(
                    None,
                    "IPv6 listener unavailable ({}), falling back to IPv4",
                    e
                );
                first_err.get_or_insert(e);
            }
            Err(e) if auto => {
                log_warn!(None, "failed to bind {}:{}: {}", ip, port, e);
                first_err.get_or_insert(e);
            }
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("bind {}:{}: {}", ip, port, e),
                ));
            }
        }
    }
    if out.is_empty() {
        return Err(first_err.unwrap_or_else(|| io::Error::other("no listener could be bound")));
    }
    Ok(out)
}

/// 待ち受けアドレスの表示用文字列 (`[::]:8080 (dual-stack)` など)。
pub fn describe_listener(l: &TcpListener) -> String {
    match l.local_addr() {
        Ok(a) if a.ip() == IpAddr::V6(Ipv6Addr::UNSPECIFIED) => format!("{} (IPv6 + IPv4)", a),
        Ok(a) => a.to_string(),
        Err(_) => "?".to_string(),
    }
}

/// 名前解決の結果を IPv6 優先で交互に並べる (RFC 8305 §4)。
pub fn interleave(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let (v6, v4): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|a| a.is_ipv6());
    let mut out = Vec::with_capacity(v6.len() + v4.len());
    let (mut a, mut b) = (v6.into_iter(), v4.into_iter());
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

/// 名前解決して接続する。IPv6 無効時は A レコードだけ、有効時は Happy Eyeballs。全体の締め切りは `timeout`。
pub fn connect(addr_str: &str, timeout: Duration) -> io::Result<TcpStream> {
    let resolved: Vec<SocketAddr> = crate::dns::resolve(addr_str)?;
    let ipv6 = ipv6_enabled();
    let addrs: Vec<SocketAddr> = if ipv6 {
        interleave(resolved)
    } else {
        resolved.into_iter().filter(|a| a.is_ipv4()).collect()
    };
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            if ipv6 {
                "Could not resolve host"
            } else {
                "host has no IPv4 address (IPv6 is disabled; set PROXY_IPV6=on)"
            },
        ));
    }
    connect_resolved(addrs, timeout)
}

/// 並べ替え済みのアドレス列に Happy Eyeballs で接続する。
pub fn connect_resolved(addrs: Vec<SocketAddr>, timeout: Duration) -> io::Result<TcpStream> {
    if addrs.len() == 1 {
        return TcpStream::connect_timeout(&addrs[0], timeout);
    }

    let deadline = Instant::now() + timeout;
    let (tx, rx) = mpsc::channel::<io::Result<TcpStream>>();
    let mut launched = 0usize;
    let mut pending = 0usize;
    let mut next_launch = Instant::now();
    let mut last_err: Option<io::Error> = None;

    loop {
        let now = Instant::now();
        if launched < addrs.len() && (pending == 0 || now >= next_launch) {
            let addr = addrs[launched];
            let tx = tx.clone();
            let remaining = deadline
                .saturating_duration_since(now)
                .max(Duration::from_millis(1));
            thread::spawn(move || {
                let _ = tx.send(TcpStream::connect_timeout(&addr, remaining));
            });
            launched += 1;
            pending += 1;
            next_launch = now + STAGGER;
        }
        let wait = if launched < addrs.len() {
            next_launch.saturating_duration_since(now)
        } else {
            deadline.saturating_duration_since(now)
        };
        match rx.recv_timeout(wait) {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => {
                pending -= 1;
                last_err = Some(e);
                if pending == 0 && launched == addrs.len() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Err(last_err.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::TimedOut, "connection attempts timed out")
    }))
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
    }

    #[test]
    fn binds_dual_stack_or_falls_back() {
        assert!(ipv6_enabled(), "on by default");
        let v4 = bind_all_with(&[], 0, false).expect("v4-only listener");
        assert_eq!(v4.len(), 1);
        assert!(v4[0].local_addr().unwrap().ip().is_ipv4());
        let listeners = bind_all(&[], 0).expect("at least one listener");
        assert!(!listeners.is_empty() && listeners.len() <= 2);
        let port = listeners[0].local_addr().unwrap().port();
        assert!(
            listeners
                .iter()
                .all(|l| l.local_addr().unwrap().port() == port)
        );
        // 明示指定なら指定どおり
        let explicit = bind_all(&[IpAddr::V4(Ipv4Addr::LOCALHOST)], 0).unwrap();
        assert_eq!(explicit.len(), 1);
        assert!(explicit[0].local_addr().unwrap().ip().is_loopback());
    }

    #[test]
    fn happy_eyeballs_skips_unreachable_first_candidate() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // 到達不能 (TEST-NET-1) → ループバックの順で試させる
        let addrs = vec![
            SocketAddr::new("192.0.2.1".parse().unwrap(), 9),
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
        ];
        let started = Instant::now();
        let stream = connect_addrs(addrs, Duration::from_secs(5)).expect("loopback should win");
        assert_eq!(stream.peer_addr().unwrap().port(), port);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "did not wait for the dead address"
        );
    }

    #[test]
    fn connect_reports_error_when_all_fail() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let err = connect(&format!("127.0.0.1:{}", port), Duration::from_secs(2)).unwrap_err();
        assert!(matches!(
            err.kind(),
            io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut
        ));
    }

    /// テスト用: 解決済みのアドレス列に対して Happy Eyeballs を走らせる。
    fn connect_addrs(addrs: Vec<SocketAddr>, timeout: Duration) -> io::Result<TcpStream> {
        // `connect` は文字列を解決するので、ここでは同じアルゴリズムを直接使う
        super::connect_resolved(addrs, timeout)
    }
}
