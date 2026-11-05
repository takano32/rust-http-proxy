//! TLS で包んだ TCP ストリーム (`TlsStream`。読み書きはバイトごとに通る)。
//! T34.77 で `tls.rs` から移した (本体は変えていない)。

use super::api::*;
use std::ffi::{c_int, c_void};
use std::io::{self, Read, Write};
use std::net::TcpStream;

/// TLS で包んだ TCP ストリーム。
pub struct TlsStream {
    pub(super) api: &'static Api,
    pub(super) ssl: *mut c_void,
    pub(super) tcp: TcpStream,
}

// SAFETY: SSL オブジェクトは同時に 1 スレッドからしか使わない (ストリームの所有者だけが触る)。
unsafe impl Send for TlsStream {}

impl TlsStream {
    pub fn tcp(&self) -> &TcpStream {
        &self.tcp
    }

    pub(super) fn describe_error(&self, ret: c_int) -> String {
        // SAFETY: 直前の呼び出しの結果を問い合わせるだけ。
        let code = unsafe { (self.api.ssl_get_error)(self.ssl, ret) };
        match code {
            SSL_ERROR_SYSCALL => {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(0) {
                    "connection closed during TLS".to_string()
                } else {
                    e.to_string()
                }
            }
            SSL_ERROR_SSL => self.api.last_error(),
            other => format!("SSL error {}", other),
        }
    }

    fn map_error(&self, ret: c_int) -> io::Error {
        // SAFETY: 直前の呼び出しの結果を問い合わせるだけ。
        let code = unsafe { (self.api.ssl_get_error)(self.ssl, ret) };
        match code {
            SSL_ERROR_ZERO_RETURN => io::Error::new(io::ErrorKind::UnexpectedEof, "TLS closed"),
            SSL_ERROR_SYSCALL => {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(0) {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed during TLS")
                } else {
                    e
                }
            }
            SSL_ERROR_SSL => io::Error::other(self.api.last_error()),
            other => io::Error::other(format!("SSL error {}", other)),
        }
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min(c_int::MAX as usize) as c_int;
        // SAFETY: buf は有効で len 以下しか書かれない。
        let n = unsafe { (self.api.ssl_read)(self.ssl, buf.as_mut_ptr() as *mut c_void, len) };
        if n > 0 {
            return Ok(n as usize);
        }
        let err = self.map_error(n);
        if err.kind() == io::ErrorKind::UnexpectedEof {
            // 相手が close_notify を送ってきた = 正常な終端
            // SAFETY: 問い合わせのみ。
            let code = unsafe { (self.api.ssl_get_error)(self.ssl, n) };
            if code == SSL_ERROR_ZERO_RETURN {
                return Ok(0);
            }
        }
        Err(err)
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min(c_int::MAX as usize) as c_int;
        // SAFETY: buf は有効で len バイト読まれるだけ。
        let n = unsafe { (self.api.ssl_write)(self.ssl, buf.as_ptr() as *const c_void, len) };
        if n > 0 {
            Ok(n as usize)
        } else {
            Err(self.map_error(n))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for TlsStream {
    fn drop(&mut self) {
        // SAFETY: ssl はこの構造体が所有している。shutdown の失敗は無視してよい。
        unsafe {
            (self.api.ssl_shutdown)(self.ssl);
            (self.api.ssl_free)(self.ssl);
        }
    }
}
