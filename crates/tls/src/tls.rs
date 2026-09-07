//! システムの OpenSSL (`libssl` / `libcrypto`) を実行時に `dlopen` して使う TLS クライアント。
//!
//! クレートもビルド時のリンクも要らず、1 バイナリのまま。ライブラリが見つからない環境では
//! `TlsClient::load()` が `None` を返し、HTTPS のオリジンへの取得だけが無効になる。
//! 証明書の検証は既定で有効 (システムの CA ストア、または `PROXY_TLS_CA_FILE`)。
//! 同時に使う SSL オブジェクトはスレッドごとに別なので、OpenSSL 1.1 以降ではロック不要。

use std::ffi::{CStr, CString, c_char, c_int, c_long, c_ulong, c_void};
use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::ptr;

const RTLD_NOW: c_int = 2;
const RTLD_GLOBAL: c_int = 0x100;

const SSL_VERIFY_NONE: c_int = 0;
const SSL_VERIFY_PEER: c_int = 1;
const SSL_CTRL_SET_TLSEXT_HOSTNAME: c_int = 55;
const TLSEXT_NAMETYPE_HOST_NAME: c_long = 0;
const SSL_CTRL_SET_MIN_PROTO_VERSION: c_int = 123;
const TLS1_2_VERSION: c_long = 0x0303;
const SSL_ERROR_ZERO_RETURN: c_int = 6;
const SSL_ERROR_SYSCALL: c_int = 5;
const SSL_ERROR_SSL: c_int = 1;
const X509_V_OK: c_long = 0;

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

type FnMethod = unsafe extern "C" fn() -> *const c_void;
type FnCtxNew = unsafe extern "C" fn(*const c_void) -> *mut c_void;
type FnCtxFree = unsafe extern "C" fn(*mut c_void);
type FnCtxInt = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnCtxLoad = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type FnCtxVerify = unsafe extern "C" fn(*mut c_void, c_int, *const c_void);
type FnCtrl = unsafe extern "C" fn(*mut c_void, c_int, c_long, *mut c_void) -> c_long;
type FnSslNew = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnSslFree = unsafe extern "C" fn(*mut c_void);
type FnSslSetFd = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type FnSslSetHost = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type FnSslInt = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnSslRead = unsafe extern "C" fn(*mut c_void, *mut c_void, c_int) -> c_int;
type FnSslWrite = unsafe extern "C" fn(*mut c_void, *const c_void, c_int) -> c_int;
type FnGetError = unsafe extern "C" fn(*const c_void, c_int) -> c_int;
type FnVerifyResult = unsafe extern "C" fn(*const c_void) -> c_long;
type FnGetParam = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnParamIp = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type FnErrGet = unsafe extern "C" fn() -> c_ulong;
type FnErrString = unsafe extern "C" fn(c_ulong, *mut c_char, usize);
type FnCertErrString = unsafe extern "C" fn(c_long) -> *const c_char;
type FnVersion = unsafe extern "C" fn(c_int) -> *const c_char;

/// dlsym で解決した関数ポインタの束。
struct Api {
    tls_client_method: FnMethod,
    ctx_new: FnCtxNew,
    ctx_free: FnCtxFree,
    ctx_set_default_verify_paths: FnCtxInt,
    ctx_load_verify_locations: FnCtxLoad,
    ctx_set_verify: FnCtxVerify,
    ctx_ctrl: FnCtrl,
    ssl_new: FnSslNew,
    ssl_free: FnSslFree,
    ssl_set_fd: FnSslSetFd,
    ssl_ctrl: FnCtrl,
    ssl_set1_host: FnSslSetHost,
    ssl_connect: FnSslInt,
    ssl_shutdown: FnSslInt,
    ssl_read: FnSslRead,
    ssl_write: FnSslWrite,
    ssl_get_error: FnGetError,
    ssl_get_verify_result: FnVerifyResult,
    ssl_get0_param: FnGetParam,
    param_set1_ip_asc: FnParamIp,
    err_get_error: FnErrGet,
    err_error_string_n: FnErrString,
    x509_verify_cert_error_string: FnCertErrString,
    openssl_version: FnVersion,
}

fn open_lib(names: &[&str]) -> Option<*mut c_void> {
    for name in names {
        let c = CString::new(*name).ok()?;
        // SAFETY: 有効な C 文字列を渡す。ハンドルはプロセス終了まで保持する。
        let h = unsafe { dlopen(c.as_ptr(), RTLD_NOW | RTLD_GLOBAL) };
        if !h.is_null() {
            return Some(h);
        }
    }
    None
}

fn sym(handle: *mut c_void, name: &str) -> Option<*mut c_void> {
    let c = CString::new(name).ok()?;
    // SAFETY: 有効なハンドルと C 文字列。
    let p = unsafe { dlsym(handle, c.as_ptr()) };
    (!p.is_null()).then_some(p)
}

macro_rules! load {
    ($lib:expr, $name:literal, $ty:ty) => {{
        let p = sym($lib, $name)?;
        // SAFETY: OpenSSL の公開 API の実際の型と一致する関数ポインタ型へ変換する。
        unsafe { std::mem::transmute::<*mut c_void, $ty>(p) }
    }};
}

impl Api {
    fn load() -> Option<Self> {
        let crypto = open_lib(&["libcrypto.so.3", "libcrypto.so.1.1", "libcrypto.so"])?;
        let ssl = open_lib(&["libssl.so.3", "libssl.so.1.1", "libssl.so"])?;
        Some(Self {
            tls_client_method: load!(ssl, "TLS_client_method", FnMethod),
            ctx_new: load!(ssl, "SSL_CTX_new", FnCtxNew),
            ctx_free: load!(ssl, "SSL_CTX_free", FnCtxFree),
            ctx_set_default_verify_paths: load!(ssl, "SSL_CTX_set_default_verify_paths", FnCtxInt),
            ctx_load_verify_locations: load!(ssl, "SSL_CTX_load_verify_locations", FnCtxLoad),
            ctx_set_verify: load!(ssl, "SSL_CTX_set_verify", FnCtxVerify),
            ctx_ctrl: load!(ssl, "SSL_CTX_ctrl", FnCtrl),
            ssl_new: load!(ssl, "SSL_new", FnSslNew),
            ssl_free: load!(ssl, "SSL_free", FnSslFree),
            ssl_set_fd: load!(ssl, "SSL_set_fd", FnSslSetFd),
            ssl_ctrl: load!(ssl, "SSL_ctrl", FnCtrl),
            ssl_set1_host: load!(ssl, "SSL_set1_host", FnSslSetHost),
            ssl_connect: load!(ssl, "SSL_connect", FnSslInt),
            ssl_shutdown: load!(ssl, "SSL_shutdown", FnSslInt),
            ssl_read: load!(ssl, "SSL_read", FnSslRead),
            ssl_write: load!(ssl, "SSL_write", FnSslWrite),
            ssl_get_error: load!(ssl, "SSL_get_error", FnGetError),
            ssl_get_verify_result: load!(ssl, "SSL_get_verify_result", FnVerifyResult),
            ssl_get0_param: load!(ssl, "SSL_get0_param", FnGetParam),
            param_set1_ip_asc: load!(crypto, "X509_VERIFY_PARAM_set1_ip_asc", FnParamIp),
            err_get_error: load!(crypto, "ERR_get_error", FnErrGet),
            err_error_string_n: load!(crypto, "ERR_error_string_n", FnErrString),
            x509_verify_cert_error_string: load!(
                crypto,
                "X509_verify_cert_error_string",
                FnCertErrString
            ),
            openssl_version: load!(crypto, "OpenSSL_version", FnVersion),
        })
    }

    fn last_error(&self) -> String {
        // SAFETY: エラーキューを読むだけ。
        let code = unsafe { (self.err_get_error)() };
        if code == 0 {
            return "unknown TLS error".to_string();
        }
        let mut buf = [0 as c_char; 256];
        // SAFETY: 十分な長さのバッファを渡す。
        unsafe { (self.err_error_string_n)(code, buf.as_mut_ptr(), buf.len()) };
        // SAFETY: NUL 終端された文字列が書かれている。
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }
}

/// TLS クライアント (共有の SSL_CTX)。
pub struct TlsClient {
    api: &'static Api,
    ctx: *mut c_void,
    verify: bool,
}

// SAFETY: SSL_CTX は OpenSSL 1.1 以降スレッド安全に共有できる。
unsafe impl Send for TlsClient {}
unsafe impl Sync for TlsClient {}

impl TlsClient {
    /// システムの OpenSSL を読み込んでクライアントを作る。無ければ `None`。
    pub fn load(verify: bool, ca_file: Option<&Path>) -> Result<Option<Self>, String> {
        let Some(api) = Api::load() else {
            return Ok(None);
        };
        let api: &'static Api = Box::leak(Box::new(api));
        // SAFETY: 以下はすべて OpenSSL の公開 API を正しい引数で呼ぶだけ。
        unsafe {
            let ctx = (api.ctx_new)((api.tls_client_method)());
            if ctx.is_null() {
                return Err(format!("SSL_CTX_new failed: {}", api.last_error()));
            }
            (api.ctx_ctrl)(
                ctx,
                SSL_CTRL_SET_MIN_PROTO_VERSION,
                TLS1_2_VERSION,
                ptr::null_mut(),
            );
            if verify {
                (api.ctx_set_verify)(ctx, SSL_VERIFY_PEER, ptr::null());
                let ok = match ca_file {
                    Some(path) => {
                        let c = CString::new(path.to_string_lossy().as_bytes())
                            .map_err(|_| "invalid CA file path".to_string())?;
                        (api.ctx_load_verify_locations)(ctx, c.as_ptr(), ptr::null())
                    }
                    None => (api.ctx_set_default_verify_paths)(ctx),
                };
                if ok != 1 {
                    let msg = api.last_error();
                    (api.ctx_free)(ctx);
                    return Err(format!("loading CA certificates failed: {}", msg));
                }
            } else {
                (api.ctx_set_verify)(ctx, SSL_VERIFY_NONE, ptr::null());
            }
            Ok(Some(Self { api, ctx, verify }))
        }
    }

    /// 読み込んだ OpenSSL のバージョン文字列。
    pub fn version(&self) -> String {
        // SAFETY: 静的な文字列を返す API。
        unsafe { CStr::from_ptr((self.api.openssl_version)(0)) }
            .to_string_lossy()
            .into_owned()
    }

    pub fn verifies(&self) -> bool {
        self.verify
    }

    /// 接続済みの TCP ストリーム上で TLS ハンドシェイクを行う。`host` は SNI と証明書検証に使う。
    pub fn connect(&self, tcp: TcpStream, host: &str) -> io::Result<TlsStream> {
        let api = self.api;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let c_host = CString::new(host)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid host name"))?;
        let is_ip = host.parse::<IpAddr>().is_ok();
        // SAFETY: 以下はすべて OpenSSL の公開 API を正しい引数で呼ぶだけ。ssl は失敗時に必ず解放する。
        unsafe {
            let ssl = (api.ssl_new)(self.ctx);
            if ssl.is_null() {
                return Err(io::Error::other(format!(
                    "SSL_new failed: {}",
                    api.last_error()
                )));
            }
            let stream = TlsStream { api, ssl, tcp };
            if (api.ssl_set_fd)(ssl, stream.tcp.as_raw_fd()) != 1 {
                return Err(io::Error::other(format!(
                    "SSL_set_fd failed: {}",
                    api.last_error()
                )));
            }
            if !is_ip {
                (api.ssl_ctrl)(
                    ssl,
                    SSL_CTRL_SET_TLSEXT_HOSTNAME,
                    TLSEXT_NAMETYPE_HOST_NAME,
                    c_host.as_ptr() as *mut c_void,
                );
            }
            if self.verify {
                let ok = if is_ip {
                    (api.param_set1_ip_asc)((api.ssl_get0_param)(ssl), c_host.as_ptr())
                } else {
                    (api.ssl_set1_host)(ssl, c_host.as_ptr())
                };
                if ok != 1 {
                    return Err(io::Error::other("failed to set the expected host name"));
                }
            }
            let r = (api.ssl_connect)(ssl);
            if r != 1 {
                let verify = (api.ssl_get_verify_result)(ssl);
                let detail = if verify != X509_V_OK {
                    CStr::from_ptr((api.x509_verify_cert_error_string)(verify))
                        .to_string_lossy()
                        .into_owned()
                } else {
                    stream.describe_error(r)
                };
                return Err(io::Error::other(format!(
                    "TLS handshake with {} failed: {}",
                    host, detail
                )));
            }
            Ok(stream)
        }
    }
}

impl Drop for TlsClient {
    fn drop(&mut self) {
        // SAFETY: ctx はこの構造体が所有している。
        unsafe { (self.api.ctx_free)(self.ctx) }
    }
}

/// TLS で包んだ TCP ストリーム。
pub struct TlsStream {
    api: &'static Api,
    ssl: *mut c_void,
    tcp: TcpStream,
}

// SAFETY: SSL オブジェクトは同時に 1 スレッドからしか使わない (ストリームの所有者だけが触る)。
unsafe impl Send for TlsStream {}

impl TlsStream {
    pub fn tcp(&self) -> &TcpStream {
        &self.tcp
    }

    fn describe_error(&self, ret: c_int) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_system_openssl_when_present() {
        match TlsClient::load(true, None) {
            Ok(Some(tls)) => {
                assert!(tls.version().contains("OpenSSL"), "{}", tls.version());
                assert!(tls.verifies());
            }
            Ok(None) => eprintln!("skipping: libssl not found"),
            Err(e) => panic!("{}", e),
        }
    }

    #[test]
    fn missing_ca_file_is_reported() {
        match TlsClient::load(true, Some(Path::new("/nonexistent/ca.pem"))) {
            Ok(None) => eprintln!("skipping: libssl not found"),
            Ok(Some(_)) => panic!("should fail to load a missing CA file"),
            Err(e) => assert!(e.contains("CA"), "{}", e),
        }
    }
}
