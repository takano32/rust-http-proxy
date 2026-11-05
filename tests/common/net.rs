//! 生のソケットの FFI (送信元を固定した接続と、拒まれるポートの持ち分)。

#[cfg(not(target_os = "linux"))]
use std::net::TcpListener;
#[cfg(target_os = "linux")]
use std::net::TcpStream;

/// 送信元アドレスを固定して `127.0.0.1:port` へ繋ぐ (`127.0.0.2` のような「別の接続元」を
/// 作るため。T14.13)。
///
/// `std` には bind してから connect する口が無いので、`crates/sys` と同じ作法で
/// `socket` / `bind` / `connect` を直に宣言する (外部クレートは足さない)。ループバックは
/// `127.0.0.0/8` が丸ごと自分のアドレスなので、`127.0.0.2` は root でなくても bind できる。
#[cfg(target_os = "linux")]
pub fn connect_from(src: [u8; 4], port: u16) -> TcpStream {
    use std::os::fd::FromRawFd;

    #[repr(C)]
    struct SockAddrIn {
        family: u16,
        port: u16,
        addr: [u8; 4],
        zero: [u8; 8],
    }
    unsafe extern "C" {
        fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
        fn bind(fd: i32, addr: *const SockAddrIn, len: u32) -> i32;
        fn connect(fd: i32, addr: *const SockAddrIn, len: u32) -> i32;
        fn close(fd: i32) -> i32;
    }
    const AF_INET: u16 = 2;
    const SOCK_STREAM: i32 = 1;

    let len = std::mem::size_of::<SockAddrIn>() as u32;
    let fd = unsafe { socket(AF_INET as i32, SOCK_STREAM, 0) };
    assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
    let from = SockAddrIn {
        family: AF_INET,
        port: 0,
        addr: src,
        zero: [0; 8],
    };
    let to = SockAddrIn {
        family: AF_INET,
        // ポートだけはネットワークバイト順
        port: port.to_be(),
        addr: [127, 0, 0, 1],
        zero: [0; 8],
    };
    let ok = unsafe { bind(fd, &from, len) == 0 && connect(fd, &to, len) == 0 };
    if !ok {
        let e = std::io::Error::last_os_error();
        unsafe { close(fd) };
        panic!("could not connect from {:?}: {}", src, e);
    }
    unsafe { TcpStream::from_raw_fd(fd) }
}

/// 誰も待っていない (`connect` が必ず `ECONNREFUSED` になる) ポートの持ち分 (T20.2)。
///
/// 「束縛してすぐ手放す」だと、使うまでの間にカーネルが同じ番号をほかの待ち受けに配る
/// ことがある (テストバイナリを並べて回すと 336 回に 1 回)。ここでは**束縛したまま
/// `listen` しない**ソケットを持ち続ける: 番号はこの値が落ちるまで誰にも配られず、
/// 待ち受けていないので接続は拒まれる。`std` には `listen` しない束縛の口が無いので、
/// 下の `connect_from` と同じ作法で直に宣言する (外部クレートは足さない)。
///
/// 初めは `tests/proxy_test.rs` の中にあった。T20.4 の結合テストも使うのでここへ移した。
pub struct RefusedPort {
    port: u16,
    /// 番号を押さえているソケット (落とすと番号が空く)
    #[cfg(target_os = "linux")]
    _held: std::os::fd::OwnedFd,
}

impl RefusedPort {
    #[cfg(target_os = "linux")]
    pub fn reserve() -> RefusedPort {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        #[repr(C)]
        struct SockAddrIn {
            family: u16,
            port: u16,
            addr: [u8; 4],
            zero: [u8; 8],
        }
        unsafe extern "C" {
            fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
            fn bind(fd: i32, addr: *const SockAddrIn, len: u32) -> i32;
            fn getsockname(fd: i32, addr: *mut SockAddrIn, len: *mut u32) -> i32;
        }
        const AF_INET: u16 = 2;
        const SOCK_STREAM: i32 = 1;

        let fd = unsafe { socket(AF_INET as i32, SOCK_STREAM, 0) };
        assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
        // ここから先は失敗しても `OwnedFd` が閉じる
        let held = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut addr = SockAddrIn {
            family: AF_INET,
            // 0 = 空いている番号をカーネルに選ばせる
            port: 0,
            addr: [127, 0, 0, 1],
            zero: [0; 8],
        };
        let mut len = std::mem::size_of::<SockAddrIn>() as u32;
        let ok = unsafe {
            bind(held.as_raw_fd(), &addr, len) == 0
                && getsockname(held.as_raw_fd(), &mut addr, &mut len) == 0
        };
        assert!(ok, "bind: {}", std::io::Error::last_os_error());
        RefusedPort {
            // ポートだけはネットワークバイト順
            port: u16::from_be(addr.port),
            _held: held,
        }
    }

    /// ほかの OS は今までどおり「束縛してすぐ手放す」(番号を押さえない)。
    #[cfg(not(target_os = "linux"))]
    pub fn reserve() -> RefusedPort {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        RefusedPort { port }
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}
