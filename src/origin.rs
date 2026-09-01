//! オリジン (上流) への接続。平文 TCP と TLS を同じ型で扱う。

use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::net;
use crate::tls::{TlsClient, TlsStream};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 上流とのストリーム。
pub enum OriginStream {
    Plain(TcpStream),
    Tls(TlsStream),
}

impl OriginStream {
    /// 下位の TCP ソケット (タイムアウト設定・生存確認用)。
    pub fn tcp(&self) -> &TcpStream {
        match self {
            OriginStream::Plain(s) => s,
            OriginStream::Tls(s) => s.tcp(),
        }
    }

    pub fn set_timeouts(&self, timeout: Duration) -> io::Result<()> {
        self.tcp().set_read_timeout(Some(timeout))?;
        self.tcp().set_write_timeout(Some(timeout))
    }
}

impl Read for OriginStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            OriginStream::Plain(s) => s.read(buf),
            OriginStream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for OriginStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            OriginStream::Plain(s) => s.write(buf),
            OriginStream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            OriginStream::Plain(s) => s.flush(),
            OriginStream::Tls(s) => s.flush(),
        }
    }
}

/// 名前解決して接続し、HTTPS なら TLS ハンドシェイクまで行う。
pub fn connect(
    scheme: Scheme,
    server_addr: &str,
    host: &str,
    timeout: Duration,
    tls: Option<&TlsClient>,
) -> io::Result<OriginStream> {
    let tcp = net::connect(server_addr, timeout)?;
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;
    match scheme {
        Scheme::Http => Ok(OriginStream::Plain(tcp)),
        Scheme::Https => {
            let Some(tls) = tls else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "HTTPS origins need OpenSSL (libssl), which was not found",
                ));
            };
            Ok(OriginStream::Tls(tls.connect(tcp, host)?))
        }
    }
}
