//! `dlopen` / `dlsym` で解決する OpenSSL の関数ポインタの束 (`Api`) と、使う定数・型。
//! T.34.77 で `tls.rs` から移した (本体は変えていない)。

use std::ffi::{CStr, CString, c_char, c_int, c_long, c_ulong, c_void};

pub(super) const RTLD_NOW: c_int = 2;
pub(super) const RTLD_GLOBAL: c_int = 0x100;

pub(super) const SSL_VERIFY_NONE: c_int = 0;
pub(super) const SSL_VERIFY_PEER: c_int = 1;
pub(super) const SSL_CTRL_SET_TLSEXT_HOSTNAME: c_int = 55;
pub(super) const TLSEXT_NAMETYPE_HOST_NAME: c_long = 0;
pub(super) const SSL_CTRL_SET_MIN_PROTO_VERSION: c_int = 123;
pub(super) const TLS1_2_VERSION: c_long = 0x0303;
pub(super) const SSL_ERROR_ZERO_RETURN: c_int = 6;
pub(super) const SSL_ERROR_SYSCALL: c_int = 5;
pub(super) const SSL_ERROR_SSL: c_int = 1;
pub(super) const X509_V_OK: c_long = 0;

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

pub(super) type FnMethod = unsafe extern "C" fn() -> *const c_void;
pub(super) type FnCtxNew = unsafe extern "C" fn(*const c_void) -> *mut c_void;
pub(super) type FnCtxFree = unsafe extern "C" fn(*mut c_void);
pub(super) type FnCtxInt = unsafe extern "C" fn(*mut c_void) -> c_int;
pub(super) type FnCtxLoad =
    unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
pub(super) type FnCtxVerify = unsafe extern "C" fn(*mut c_void, c_int, *const c_void);
pub(super) type FnCtrl = unsafe extern "C" fn(*mut c_void, c_int, c_long, *mut c_void) -> c_long;
pub(super) type FnSslNew = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
pub(super) type FnSslFree = unsafe extern "C" fn(*mut c_void);
pub(super) type FnSslSetFd = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
pub(super) type FnSslSetHost = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
pub(super) type FnSslInt = unsafe extern "C" fn(*mut c_void) -> c_int;
pub(super) type FnSslRead = unsafe extern "C" fn(*mut c_void, *mut c_void, c_int) -> c_int;
pub(super) type FnSslWrite = unsafe extern "C" fn(*mut c_void, *const c_void, c_int) -> c_int;
pub(super) type FnGetError = unsafe extern "C" fn(*const c_void, c_int) -> c_int;
pub(super) type FnVerifyResult = unsafe extern "C" fn(*const c_void) -> c_long;
pub(super) type FnGetParam = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
pub(super) type FnParamIp = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
pub(super) type FnErrGet = unsafe extern "C" fn() -> c_ulong;
pub(super) type FnErrString = unsafe extern "C" fn(c_ulong, *mut c_char, usize);
pub(super) type FnCertErrString = unsafe extern "C" fn(c_long) -> *const c_char;
pub(super) type FnVersion = unsafe extern "C" fn(c_int) -> *const c_char;

/// dlsym で解決した関数ポインタの束。
pub(super) struct Api {
    pub(super) tls_client_method: FnMethod,
    pub(super) ctx_new: FnCtxNew,
    pub(super) ctx_free: FnCtxFree,
    pub(super) ctx_set_default_verify_paths: FnCtxInt,
    pub(super) ctx_load_verify_locations: FnCtxLoad,
    pub(super) ctx_set_verify: FnCtxVerify,
    pub(super) ctx_ctrl: FnCtrl,
    pub(super) ssl_new: FnSslNew,
    pub(super) ssl_free: FnSslFree,
    pub(super) ssl_set_fd: FnSslSetFd,
    pub(super) ssl_ctrl: FnCtrl,
    pub(super) ssl_set1_host: FnSslSetHost,
    pub(super) ssl_connect: FnSslInt,
    pub(super) ssl_shutdown: FnSslInt,
    pub(super) ssl_read: FnSslRead,
    pub(super) ssl_write: FnSslWrite,
    pub(super) ssl_get_error: FnGetError,
    pub(super) ssl_get_verify_result: FnVerifyResult,
    pub(super) ssl_get0_param: FnGetParam,
    pub(super) param_set1_ip_asc: FnParamIp,
    pub(super) err_get_error: FnErrGet,
    pub(super) err_error_string_n: FnErrString,
    pub(super) x509_verify_cert_error_string: FnCertErrString,
    pub(super) openssl_version: FnVersion,
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
    pub(super) fn load() -> Option<Self> {
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

    pub(super) fn last_error(&self) -> String {
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

pub(super) fn version_of(api: &Api) -> String {
    // SAFETY: 静的な文字列を返す API。
    unsafe { CStr::from_ptr((api.openssl_version)(0)) }
        .to_string_lossy()
        .into_owned()
}
