//! 待ち受けソケットの用意: backlog の既定 (`min(1024, somaxconn)`) と、デュアルスタックの `bind`。
//!
//! 判断 (どのアドレスに立てるか・`bind` の失敗をどう扱うか・`somaxconn` の読み方) は
//! 文字列や値を受ける純粋な関数に寄せ、ソケットに触るのは [`bind_one`] と [`bind_all_with`] だけ。

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};

use crate::{log_debug, log_warn};

/// `std` の `TcpListener::bind` が使う backlog。**Rust はこれを変えられない**ので、
/// backlog を選ぶにはソケットを自分で作るしかない (T14.47)。
pub const STD_BACKLOG: u32 = 128;

/// backlog の既定 (`PROXY_LISTEN_BACKLOG=0`) の頭打ち。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // Linux でだけ使う (T36.31)
pub(crate) const DEFAULT_BACKLOG_CAP: u32 = 1024;

/// backlog の既定: **`min(1024, /proc/sys/net/core/somaxconn)`** (T14.47)。
///
/// `somaxconn` が読めなければ 1024 (カーネルが自分で `somaxconn` に切り詰めるので、
/// 大きめに渡しても溢れない)。Linux 以外は `std` のまま 128。
///
/// 128 では足りない理由: ブラウザがページを 1 枚開くと数十本の CONNECT が**同時に**来るのに、
/// accept ループは 1 本しかない (T4.3 で「並列にしても速くならない」と測って決めた。`docs/rejected.md`)。
/// 受け入れ待ち行列が溢れると SYN は**黙って捨てられ**、クライアントは 1 秒後に再送するので、
/// 利用者には 1 秒の待ちとして見える (T14.16 の手元の実測で max 1,011 ms、T14.12 の
/// `ListenOverflows` が直近 5 分で +122)。
pub fn default_backlog() -> u32 {
    #[cfg(target_os = "linux")]
    {
        backlog_from_somaxconn(
            std::fs::read_to_string("/proc/sys/net/core/somaxconn")
                .ok()
                .as_deref(),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        STD_BACKLOG
    }
}

/// `/proc/sys/net/core/somaxconn` の中身から backlog の既定を決める ([`default_backlog`] の判断の部)。
/// 読めない (`None`)・数でない・0 なら [`DEFAULT_BACKLOG_CAP`]、読めれば [`DEFAULT_BACKLOG_CAP`] で頭打ち。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // Linux でだけ使う (T36.31)
pub(super) fn backlog_from_somaxconn(somaxconn: Option<&str>) -> u32 {
    somaxconn
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_BACKLOG_CAP)
        .min(DEFAULT_BACKLOG_CAP)
}

/// 待ち受けソケットを 1 本作る。**Linux では backlog を選べる** (`socket` / `bind` /
/// `listen(fd, backlog)` を自分で呼ぶ。T14.47)。それ以外の OS は `std` の `bind`
/// (backlog は 128 固定) のままで、`backlog` は効かない。
fn bind_one(addr: SocketAddr, backlog: u32) -> io::Result<TcpListener> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::FromRawFd;

        match proxy_sys::sys::listen_socket(addr, backlog) {
            // SAFETY: listen まで済んだ記述子を 1 回だけ包む (以後は TcpListener が閉じる)。
            Ok(fd) => return Ok(unsafe { TcpListener::from_raw_fd(fd) }),
            // 定数の分からない arch では `std` に落ちる (backlog は 128 のまま)
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {}
            Err(e) => return Err(e),
        }
    }
    let _ = backlog;
    TcpListener::bind(addr)
}

/// 待ち受けを立てる候補 (`addrs` が空なら自動: IPv6 有効時はデュアルスタック (`[::]` + `0.0.0.0`)、
/// 無効時は `0.0.0.0` だけ)。
pub(super) fn listen_candidates(addrs: &[IpAddr], ipv6: bool) -> Vec<IpAddr> {
    if !addrs.is_empty() {
        return addrs.to_vec();
    }
    if ipv6 {
        vec![
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        ]
    } else {
        vec![IpAddr::V4(Ipv4Addr::UNSPECIFIED)]
    }
}

/// 自動の候補に `bind` が失敗したときの扱い ([`classify_bind_failure`] の答え)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BindFailure {
    /// `[::]` がデュアルスタックで v4 も受けているので `0.0.0.0` が使用中になった (黙って飛ばす)
    Covered,
    /// IPv6 の待ち受けが作れない環境 (debug を 1 行出して IPv4 に落ちる)
    NoIpv6,
    /// 自動の候補のそれ以外の失敗 (warn を 1 行出して、ほかの候補が立てばよしとする)
    Skipped,
    /// 明示されたアドレスの失敗 (そのまま誤りとして返す)
    Fatal,
}

/// 1 本の `bind` の失敗をどう扱うか。`auto` は候補を自動で選んだか、`bound` は既に立った本数。
pub(super) fn classify_bind_failure(
    auto: bool,
    ip: IpAddr,
    kind: io::ErrorKind,
    bound: usize,
) -> BindFailure {
    if !auto {
        BindFailure::Fatal
    } else if kind == io::ErrorKind::AddrInUse && bound > 0 {
        BindFailure::Covered
    } else if ip.is_ipv6() {
        BindFailure::NoIpv6
    } else {
        BindFailure::Skipped
    }
}

/// 待ち受けソケットを作る。`addrs` が空なら IPv6 有効時はデュアルスタック (`[::]` + `0.0.0.0`)、
/// 無効時は `0.0.0.0` だけ。`port` が 0 のときは最初に取れたポートを残りにも使う。
/// `backlog` は受け入れ待ち行列の長さ (`PROXY_LISTEN_BACKLOG`。0 で [`default_backlog`])。
pub fn bind_all(addrs: &[IpAddr], port: u16, backlog: u32) -> io::Result<Vec<TcpListener>> {
    bind_all_with(addrs, port, super::ipv6_enabled(), backlog)
}

pub(crate) fn bind_all_with(
    addrs: &[IpAddr],
    port: u16,
    ipv6: bool,
    backlog: u32,
) -> io::Result<Vec<TcpListener>> {
    let candidates = listen_candidates(addrs, ipv6);
    let auto = addrs.is_empty();
    let backlog = if backlog == 0 {
        default_backlog()
    } else {
        backlog
    };
    let mut out: Vec<TcpListener> = Vec::new();
    let mut port = port;
    let mut first_err = None;
    for ip in candidates {
        match bind_one(SocketAddr::new(ip, port), backlog) {
            Ok(l) => {
                if port == 0 {
                    port = l.local_addr().map(|a| a.port()).unwrap_or(0);
                }
                out.push(l);
            }
            Err(e) => match classify_bind_failure(auto, ip, e.kind(), out.len()) {
                BindFailure::Covered => {
                    log_debug!(None, "{} already covered by the dual-stack socket", ip);
                }
                BindFailure::NoIpv6 => {
                    log_debug!(
                        None,
                        "IPv6 listener unavailable ({}), falling back to IPv4",
                        e
                    );
                    first_err.get_or_insert(e);
                }
                BindFailure::Skipped => {
                    log_warn!(None, "failed to bind {}:{}: {}", ip, port, e);
                    first_err.get_or_insert(e);
                }
                BindFailure::Fatal => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!("bind {}:{}: {}", ip, port, e),
                    ));
                }
            },
        }
    }
    if out.is_empty() {
        return Err(first_err.unwrap_or_else(|| io::Error::other("no listener could be bound")));
    }
    Ok(out)
}

/// 待ち受けアドレスの表示用文字列 (`[::]:8080 (dual-stack)` など)。
pub fn describe_listener(l: &TcpListener) -> String {
    describe_local_addr(l.local_addr())
}

/// [`describe_listener`] の判断の部 (`local_addr()` の答えを受ける)。
pub(super) fn describe_local_addr(addr: io::Result<SocketAddr>) -> String {
    match addr {
        Ok(a) if a.ip() == IpAddr::V6(Ipv6Addr::UNSPECIFIED) => format!("{} (IPv6 + IPv4)", a),
        Ok(a) => a.to_string(),
        Err(_) => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `somaxconn` の読み方: 読めない・数でない・0 は 1,024、小さければその値、大きければ頭打ち (T14.47)。
    #[test]
    fn the_default_backlog_follows_somaxconn_up_to_the_cap() {
        assert_eq!(backlog_from_somaxconn(None), DEFAULT_BACKLOG_CAP);
        assert_eq!(backlog_from_somaxconn(Some("junk\n")), DEFAULT_BACKLOG_CAP);
        assert_eq!(backlog_from_somaxconn(Some("0\n")), DEFAULT_BACKLOG_CAP);
        assert_eq!(backlog_from_somaxconn(Some("128\n")), 128);
        assert_eq!(backlog_from_somaxconn(Some(" 4096 ")), DEFAULT_BACKLOG_CAP);
        assert_eq!(backlog_from_somaxconn(Some("1024")), 1024);
    }

    /// 自動なら IPv6 の有無で `[::]` + `0.0.0.0` か `0.0.0.0` だけ、明示されていればそのまま。
    #[test]
    fn listen_candidates_are_dual_stack_by_default() {
        assert_eq!(
            listen_candidates(&[], true),
            vec![
                IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            ]
        );
        assert_eq!(
            listen_candidates(&[], false),
            vec![IpAddr::V4(Ipv4Addr::UNSPECIFIED)]
        );
        let explicit = [IpAddr::V4(Ipv4Addr::LOCALHOST)];
        assert_eq!(listen_candidates(&explicit, true), explicit.to_vec());
        assert_eq!(listen_candidates(&explicit, false), explicit.to_vec());
    }

    /// `bind` の失敗の扱い: 明示は致命、自動の `0.0.0.0` が使用中で既に 1 本立っていれば
    /// デュアルスタックに覆われている、IPv6 は無い環境、それ以外は飛ばす。
    #[test]
    fn bind_failures_are_classified_by_what_was_asked() {
        let v6 = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
        let v4 = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        assert_eq!(
            classify_bind_failure(false, v4, io::ErrorKind::AddrInUse, 1),
            BindFailure::Fatal
        );
        assert_eq!(
            classify_bind_failure(true, v4, io::ErrorKind::AddrInUse, 1),
            BindFailure::Covered
        );
        // 1 本も立っていないのに使用中なら、覆われているのではなく本当に使われている
        assert_eq!(
            classify_bind_failure(true, v4, io::ErrorKind::AddrInUse, 0),
            BindFailure::Skipped
        );
        assert_eq!(
            classify_bind_failure(true, v6, io::ErrorKind::AddrNotAvailable, 0),
            BindFailure::NoIpv6
        );
        assert_eq!(
            classify_bind_failure(true, v6, io::ErrorKind::AddrInUse, 1),
            BindFailure::Covered
        );
        assert_eq!(
            classify_bind_failure(true, v4, io::ErrorKind::PermissionDenied, 0),
            BindFailure::Skipped
        );
    }

    #[test]
    fn describes_the_dual_stack_listener() {
        assert_eq!(
            describe_local_addr(Ok("[::]:8080".parse().unwrap())),
            "[::]:8080 (IPv6 + IPv4)"
        );
        assert_eq!(
            describe_local_addr(Ok("127.0.0.1:8080".parse().unwrap())),
            "127.0.0.1:8080"
        );
        assert_eq!(
            describe_local_addr(Ok("[::1]:8080".parse().unwrap())),
            "[::1]:8080"
        );
        assert_eq!(describe_local_addr(Err(io::Error::other("x"))), "?");
    }
}
