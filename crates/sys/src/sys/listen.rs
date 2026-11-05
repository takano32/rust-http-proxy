//! backlog を選べる待ち受けソケット (`socket` → `SO_REUSEADDR` → `bind` → `listen`。T14.47)。
//! `sockaddr` の組み立て ([`SockAddr`]) は純粋で、族・ポートのバイト順の試験はそこで見る。

use std::io;
use std::os::fd::RawFd;

/// 待ち受けソケットを自分で作るための定数と束縛 (`socket` / `bind` / `listen`。T14.47)。
///
/// `std` の `TcpListener::bind` は **`listen(fd, 128)` 固定**なので、backlog を選ぶには
/// ソケットを自分で作るしかない。値は [`super::sockopt::consts`] と同じく **aarch64 / x86_64 だけ**で
/// 有効にする (`SOCK_CLOEXEC` と `SO_REUSEADDR` は arch によって値が違う)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod listensock {
    use std::ffi::{c_int, c_void};

    pub const AF_INET: c_int = 2;
    pub const AF_INET6: c_int = 10;
    pub const SOCK_STREAM: c_int = 1;
    /// `SOCK_CLOEXEC` は `O_CLOEXEC` と同じ値 (asm-generic)。
    pub const SOCK_CLOEXEC: c_int = 0o2000000;
    pub const SO_REUSEADDR: c_int = 2;

    /// `struct sockaddr_in` (16 B)。`sin_port` と `sin_addr` はネットワークバイト順。
    #[repr(C)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct SockAddrIn {
        pub sin_family: u16,
        pub sin_port: u16,
        pub sin_addr: [u8; 4],
        pub sin_zero: [u8; 8],
    }

    /// `struct sockaddr_in6` (28 B)。
    #[repr(C)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct SockAddrIn6 {
        pub sin6_family: u16,
        pub sin6_port: u16,
        pub sin6_flowinfo: u32,
        pub sin6_addr: [u8; 16],
        pub sin6_scope_id: u32,
    }

    unsafe extern "C" {
        pub fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
        pub fn bind(fd: c_int, addr: *const c_void, len: u32) -> c_int;
        pub fn listen(fd: c_int, backlog: c_int) -> c_int;
    }
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const _: () = assert!(size_of::<listensock::SockAddrIn>() == 16);
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const _: () = assert!(size_of::<listensock::SockAddrIn6>() == 28);

/// `bind(2)` に渡す `sockaddr` (族ごとの C の構造体)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum SockAddr {
    V4(listensock::SockAddrIn),
    V6(listensock::SockAddrIn6),
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
impl SockAddr {
    /// `SocketAddr` を C の `sockaddr_in` / `sockaddr_in6` に組む (ポートはネットワークバイト順)。
    pub(super) fn from_std(addr: std::net::SocketAddr) -> SockAddr {
        use listensock::*;

        let port = addr.port().to_be();
        match addr.ip() {
            std::net::IpAddr::V4(ip) => SockAddr::V4(SockAddrIn {
                sin_family: AF_INET as u16,
                sin_port: port,
                sin_addr: ip.octets(),
                sin_zero: [0; 8],
            }),
            std::net::IpAddr::V6(ip) => SockAddr::V6(SockAddrIn6 {
                sin6_family: AF_INET6 as u16,
                sin6_port: port,
                sin6_flowinfo: 0,
                sin6_addr: ip.octets(),
                // 待ち受けでは scope id を使わない (リンクローカルを明示するときだけ要る)
                sin6_scope_id: 0,
            }),
        }
    }

    /// `socket(2)` の `domain`。
    pub(super) fn domain(&self) -> std::ffi::c_int {
        match self {
            SockAddr::V4(_) => listensock::AF_INET,
            SockAddr::V6(_) => listensock::AF_INET6,
        }
    }

    /// `bind(2)` に渡す先頭の番地と長さ (族の構造体の大きさ)。
    fn as_raw(&self) -> (*const std::ffi::c_void, u32) {
        match self {
            SockAddr::V4(a) => (
                a as *const listensock::SockAddrIn as *const std::ffi::c_void,
                size_of::<listensock::SockAddrIn>() as u32,
            ),
            SockAddr::V6(a) => (
                a as *const listensock::SockAddrIn6 as *const std::ffi::c_void,
                size_of::<listensock::SockAddrIn6>() as u32,
            ),
        }
    }
}

/// backlog を選んで待ち受けソケットを 1 本作る (`socket` → `SO_REUSEADDR` → `bind` → `listen`。T14.47)。
///
/// 返すのは **`listen` まで済んだ生の記述子**で、呼び出し側が `TcpListener::from_raw_fd` で
/// 包む (包んだ時点で閉じる責任も移る)。途中で失敗したらここで閉じてから `Err` を返す。
///
/// `std` の `TcpListener::bind` との違いは **backlog だけ**。`SO_REUSEADDR` は `std` と同じく
/// `bind` の前に立て、**`IPV6_V6ONLY` は触らない** (カーネルの既定 =
/// `/proc/sys/net/ipv6/bindv6only` に任せる。`[::]` が IPv4 も受ける今の姿を変えないため)。
/// `backlog` はカーネルが `/proc/sys/net/core/somaxconn` で頭打ちにするので、
/// 大きめの値を渡しても溢れない。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn listen_socket(addr: std::net::SocketAddr, backlog: u32) -> io::Result<RawFd> {
    use listensock::*;
    use std::ffi::c_int;

    let sockaddr = SockAddr::from_std(addr);
    // SAFETY: 引数は定数だけ。失敗は -1 で返る。
    let fd = unsafe { socket(sockaddr.domain(), SOCK_STREAM | SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // ここから先の失敗は、開けた記述子を閉じてから返す (漏らさない)
    let failed = |fd: RawFd| -> io::Error {
        let e = io::Error::last_os_error();
        // SAFETY: 自分で開いた fd を 1 回だけ閉じる。
        unsafe { super::close(fd) };
        e
    };
    // `std` の `TcpListener::bind` と同じく bind の前に `SO_REUSEADDR` を立てる
    // (TIME_WAIT の残っているポートにも待ち受けられるように)
    let one: c_int = 1;
    if super::sockopt::set(fd, super::sockopt::consts::SOL_SOCKET, SO_REUSEADDR, &one).is_err() {
        return Err(failed(fd));
    }
    let (ptr, len) = sockaddr.as_raw();
    // SAFETY: その族の `sockaddr` とその大きさを対で渡している。
    if unsafe { bind(fd, ptr, len) } < 0 {
        return Err(failed(fd));
    }
    // SAFETY: 自分で開いた記述子を listen するだけ。カーネルが somaxconn で頭打ちにする。
    if unsafe { listen(fd, backlog.min(c_int::MAX as u32) as c_int) } < 0 {
        return Err(failed(fd));
    }
    Ok(fd)
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn listen_socket(_addr: std::net::SocketAddr, _backlog: u32) -> io::Result<RawFd> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "socket constants are only known for aarch64 and x86_64",
    ))
}

#[cfg(all(test, any(target_arch = "aarch64", target_arch = "x86_64")))]
mod tests {
    use super::listensock::*;
    use super::*;

    /// `sockaddr_in` / `sockaddr_in6` の組み立て: 族の番号、ポートはネットワークバイト順、番地は octets。
    #[test]
    fn builds_the_sockaddr_of_each_family_with_the_port_in_network_order() {
        let v4 = SockAddr::from_std("127.0.0.1:8080".parse().unwrap());
        assert_eq!(
            v4,
            SockAddr::V4(SockAddrIn {
                sin_family: AF_INET as u16,
                sin_port: 8080u16.to_be(),
                sin_addr: [127, 0, 0, 1],
                sin_zero: [0; 8],
            })
        );
        assert_eq!(v4.domain(), AF_INET);
        assert_eq!(v4.as_raw().1, 16);
        let v6 = SockAddr::from_std("[::1]:443".parse().unwrap());
        let mut loopback = [0u8; 16];
        loopback[15] = 1;
        assert_eq!(
            v6,
            SockAddr::V6(SockAddrIn6 {
                sin6_family: AF_INET6 as u16,
                sin6_port: 443u16.to_be(),
                sin6_flowinfo: 0,
                sin6_addr: loopback,
                sin6_scope_id: 0,
            })
        );
        assert_eq!(v6.domain(), AF_INET6);
        assert_eq!(v6.as_raw().1, 28);
        // 8080 = 0x1f90 はネットワークバイト順で 0x1f 0x90 の並び
        let SockAddr::V4(a) = v4 else { unreachable!() };
        assert_eq!(a.sin_port.to_ne_bytes(), [0x1f, 0x90]);
    }
}
