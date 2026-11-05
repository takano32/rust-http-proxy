//! `setsockopt(2)` / `getsockopt(2)` の包みと、それで当てるもの: 待ち受けから accept した接続へ
//! 引き継がせるオプション (`TCP_NODELAY` / `SO_RCVTIMEO` / `SO_SNDTIMEO`) と TCP keepalive (T14.52)。
//! 判断 (`Duration` → `timeval` の丸め、keepalive の 0 の切り上げ) は純粋な関数に出してある。

use std::ffi::{c_int, c_void};
use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

unsafe extern "C" {
    fn setsockopt(
        fd: c_int,
        level: c_int,
        name: c_int,
        value: *const c_void,
        len: u32, // socklen_t
    ) -> c_int;
    fn getsockopt(
        fd: c_int,
        level: c_int,
        name: c_int,
        value: *mut c_void,
        // socklen_t の入出力。渡した長さを、カーネルが書いた長さで上書きして返す
        len: *mut u32,
    ) -> c_int;
}

/// `setsockopt(2)` の定数 (aarch64 / x86_64 で同じ値。他の arch は値が違うので
/// [`inherit_socket_options`] ごと外し、呼び出し側は接続ごとの設定に落ちる)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub(super) mod consts {
    use std::ffi::c_int;

    pub const SOL_SOCKET: c_int = 1;
    pub const SO_RCVTIMEO: c_int = 20;
    pub const SO_SNDTIMEO: c_int = 21;
    pub const IPPROTO_TCP: c_int = 6;
    pub const TCP_NODELAY: c_int = 1;

    /// `struct timeval` (64 bit Linux)。
    #[repr(C)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct TimeVal {
        pub tv_sec: i64,
        pub tv_usec: i64,
    }
}

/// 待ち受けソケットに「accept した接続へ引き継がせたいオプション」をまとめて当てる。
///
/// Linux は `accept` のときに `sk_clone_lock` が `struct sock` ごと複製するので、
/// `TCP_NODELAY` / `SO_RCVTIMEO` / `SO_SNDTIMEO` は待ち受けから接続へそのまま引き継がれる。
/// 待ち受けに 1 回当てておけば接続ごとの `setsockopt` 3 回を払わなくて済む。
///
/// **`SO_RCVTIMEO` は `accept()` 自体にも効く** (`read` ごとに `EAGAIN` で戻る) ので、
/// 呼び出し側の accept ループは `WouldBlock` を「まだ来ていない」として読み飛ばすこと。
/// `read` が 0 (= 無期限) のときは `accept()` も無期限に待つ。
///
/// 読み (`SO_RCVTIMEO`) と書き (`SO_SNDTIMEO`) は別の値にできる (T28.0)。読みは 1 要求目の
/// 要求行を待つ長さ (`PROXY_HEADER_TIMEOUT_SECS` と `PROXY_TIMEOUT_SECS` の小さい方)、
/// 書きは `PROXY_TIMEOUT_SECS` のまま。
///
/// 失敗したら `Err` を返す。呼び出し側は**従来どおり接続ごとに設定する**こと
/// (カーネルが引き継がない環境でも動きが変わらないように)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn inherit_socket_options(
    listener_fd: RawFd,
    read: Duration,
    write: Duration,
) -> io::Result<()> {
    use consts::*;

    let one: c_int = 1;
    // Nagle を切る。応答ヘッダーと本文を別々に write すると delayed ACK と噛み合って
    // 1 要求あたり 40 ms 止まるため
    set(listener_fd, IPPROTO_TCP, TCP_NODELAY, &one)?;
    set(listener_fd, SOL_SOCKET, SO_RCVTIMEO, &timeval(read))?;
    set(listener_fd, SOL_SOCKET, SO_SNDTIMEO, &timeval(write))?;
    Ok(())
}

/// `Duration` を `SO_RCVTIMEO` / `SO_SNDTIMEO` の `timeval` に直す。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub(super) fn timeval(timeout: Duration) -> consts::TimeVal {
    let mut tv = consts::TimeVal {
        tv_sec: timeout.as_secs().min(i64::MAX as u64) as i64,
        tv_usec: timeout.subsec_micros() as i64,
    };
    // Linux では `timeval {0, 0}` が「タイムアウト無し」。`PROXY_TIMEOUT_SECS=0` は
    // まさにそれを意味するので (T10.6)、0 はそのまま渡して**継承させる**。
    // 以前はここで断って接続ごとの設定に落としていたが、落ちた先の
    // `set_write_timeout(Some(ZERO))` を std が `InvalidInput` で拒むので、
    // その設定では全接続が失敗していた。
    // 0 でない指定が丸めで「無期限」に化けるのは困るので、そちらは 1 us に上げる
    // (std と同じ丸め方)
    if !timeout.is_zero() && tv.tv_sec == 0 && tv.tv_usec == 0 {
        tv.tv_usec = 1;
    }
    tv
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn inherit_socket_options(
    _listener_fd: RawFd,
    _read: Duration,
    _write: Duration,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "socket option constants are only known for aarch64 and x86_64",
    ))
}

/// `setsockopt` を 1 つ当てる。`value` は C 側の型と同じレイアウトを持つ値。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub(super) fn set<T>(fd: RawFd, level: c_int, name: c_int, value: &T) -> io::Result<()> {
    // SAFETY: value は呼び出しの間だけ有効なら良く (カーネルが値を複製する)、
    // 長さもその型の大きさをそのまま渡している。
    let r = unsafe {
        setsockopt(
            fd,
            level,
            name,
            value as *const T as *const c_void,
            size_of::<T>() as u32,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `getsockopt` で `len` バイトまでの値を 1 つ読む ([`set`] の裏返し)。書けた長さを返す。
///
/// `value` は C 側の型と同じレイアウトの書込先。カーネルは `min(len, sizeof)` バイトだけ書き、
/// 書いた長さを返すので、新しい欄を持たない古いカーネルでも溢れない。
pub(super) fn get<T>(fd: RawFd, level: c_int, name: c_int, value: &mut T) -> io::Result<u32> {
    let mut len = size_of::<T>() as u32;
    // SAFETY: `T` 1 つぶんの領域とその長さを対で渡している。カーネルは
    // min(len, sizeof) バイトだけ書き、書いた長さを len に返す。
    let r = unsafe { getsockopt(fd, level, name, value as *mut T as *mut c_void, &mut len) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(len)
}

/// TCP keepalive の定数 (T14.52)。
///
/// `SO_KEEPALIVE` は `SOL_SOCKET` の 9 で、[`consts`] の値と同じく **arch によって
/// 違う** (asm-generic は 9、mips や alpha は別) ので同じ条件で囲む。
/// `TCP_KEEPIDLE` / `TCP_KEEPINTVL` / `TCP_KEEPCNT` は linux/tcp.h の 4 / 5 / 6 で
/// arch に依らない (`TCP_INFO` と同じ)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod keepalive {
    use std::ffi::c_int;

    pub const SO_KEEPALIVE: c_int = 9;
    pub const TCP_KEEPIDLE: c_int = 4;
    pub const TCP_KEEPINTVL: c_int = 5;
    pub const TCP_KEEPCNT: c_int = 6;
}

/// 1 本のソケットに当たっている TCP keepalive (T14.52。読み戻し用)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keepalive {
    /// `SO_KEEPALIVE` が立っているか
    pub on: bool,
    /// 無通信がこれだけ続いたら最初の探りを送る (秒。`TCP_KEEPIDLE`)
    pub idle_secs: u32,
    /// 探りと探りの間隔 (秒。`TCP_KEEPINTVL`)
    pub intvl_secs: u32,
    /// 返事の無い探りを何回まで送るか (`TCP_KEEPCNT`)
    pub count: u32,
}

/// `TCP_KEEPIDLE` / `TCP_KEEPINTVL` / `TCP_KEEPCNT` に渡す `c_int`。
/// 0 は「カーネルの既定のまま」になってしまうので、最低 1 秒 / 1 回に上げる。
pub(super) fn keepalive_value(v: u32) -> c_int {
    v.max(1).min(c_int::MAX as u32) as c_int
}

/// 消えたクライアントを見つけるための TCP keepalive を 1 本に当てる (T14.52)。
///
/// **`setsockopt` を 4 回**呼ぶ (接続あたりの固定費はこの 4 回だけ。当てないときは
/// 呼び出し側が分岐 1 回で飛ばす)。`TCP_NODELAY` などと違って**待ち受けから継承させて
/// いない**のは、`PROXY_TCP_KEEPALIVE` を `.env` で変えたときに次の接続から効くように
/// するため (待ち受けに当てると当て直しの機会が無い)。
///
/// 当てたあと相手が消えると、カーネルは `idle_secs + intvl_secs * count` 秒ほどで
/// そのソケットの保留エラーを `ETIMEDOUT` にする。以後の `read` / `write` / `splice` は
/// その errno で返るので、呼び出し側は「相手が消えた」と記録できる。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn set_keepalive(fd: RawFd, idle_secs: u32, intvl_secs: u32, count: u32) -> io::Result<()> {
    use keepalive::*;

    let on: c_int = 1;
    set(fd, consts::SOL_SOCKET, SO_KEEPALIVE, &on)?;
    let idle = keepalive_value(idle_secs);
    let intvl = keepalive_value(intvl_secs);
    let cnt = keepalive_value(count);
    set(fd, super::SOL_TCP, TCP_KEEPIDLE, &idle)?;
    set(fd, super::SOL_TCP, TCP_KEEPINTVL, &intvl)?;
    set(fd, super::SOL_TCP, TCP_KEEPCNT, &cnt)?;
    Ok(())
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn set_keepalive(_fd: RawFd, _idle: u32, _intvl: u32, _count: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SO_KEEPALIVE is only known for aarch64 and x86_64",
    ))
}

/// いま当たっている keepalive を読み戻す (`getsockopt` 4 回)。
///
/// 使うのは**テストと診断だけ**で、接続の経路からは呼ばない。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn keepalive_of(fd: RawFd) -> io::Result<Keepalive> {
    use keepalive::*;

    Ok(Keepalive {
        on: get_int(fd, consts::SOL_SOCKET, SO_KEEPALIVE)? != 0,
        idle_secs: get_int(fd, super::SOL_TCP, TCP_KEEPIDLE)?.max(0) as u32,
        intvl_secs: get_int(fd, super::SOL_TCP, TCP_KEEPINTVL)?.max(0) as u32,
        count: get_int(fd, super::SOL_TCP, TCP_KEEPCNT)?.max(0) as u32,
    })
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn keepalive_of(_fd: RawFd) -> io::Result<Keepalive> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SO_KEEPALIVE is only known for aarch64 and x86_64",
    ))
}

/// `getsockopt` で `c_int` を 1 つ読む。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn get_int(fd: RawFd, level: c_int, name: c_int) -> io::Result<c_int> {
    let mut v: c_int = 0;
    get(fd, level, name, &mut v)?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `timeval` の丸め: 0 は「無期限」のまま、0 でない 1 us 未満は 1 us に上げる (std と同じ)。
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn timeval_keeps_zero_and_rounds_tiny_timeouts_up_to_one_microsecond() {
        let tv = |sec: i64, usec: i64| consts::TimeVal {
            tv_sec: sec,
            tv_usec: usec,
        };
        assert_eq!(timeval(Duration::ZERO), tv(0, 0), "0 = 無期限 (T10.6)");
        assert_eq!(timeval(Duration::from_nanos(1)), tv(0, 1), "1 us に上げる");
        assert_eq!(timeval(Duration::from_micros(1)), tv(0, 1));
        assert_eq!(timeval(Duration::from_millis(1500)), tv(1, 500_000));
        assert_eq!(timeval(Duration::from_secs(30)), tv(30, 0));
        assert_eq!(timeval(Duration::MAX).tv_sec, i64::MAX, "溢れない");
    }

    /// keepalive の 0 は `EINVAL` になるので 1 に上げ、大きすぎる値は `c_int` で頭打ち (T14.52)。
    #[test]
    fn keepalive_values_are_at_least_one_and_fit_an_int() {
        assert_eq!(keepalive_value(0), 1);
        assert_eq!(keepalive_value(1), 1);
        assert_eq!(keepalive_value(60), 60);
        assert_eq!(keepalive_value(u32::MAX), c_int::MAX);
    }
}
