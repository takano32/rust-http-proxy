//! システムの OpenSSL (`libssl` / `libcrypto`) を実行時に `dlopen` して使う TLS クライアント。
//!
//! クレートもビルド時のリンクも要らず、1 バイナリのまま。ライブラリが見つからない環境では
//! `TlsClient::load()` が `None` を返し、HTTPS のオリジンへの取得だけが無効になる。
//! 証明書の検証は既定で有効 (システムの CA ストア、または `PROXY_TLS_CA_FILE`)。
//! 同時に使う SSL オブジェクトはスレッドごとに別なので、OpenSSL 1.1 以降ではロック不要。

use std::ffi::{CStr, CString, c_void};
use std::io;
use std::net::{IpAddr, TcpStream};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::ptr;

// 責務ごとに子のファイルへ分けてある (T34.77)。公開の名前はここで出し直す (`proxy_tls::tls::…` のまま)。
// - `api`: OpenSSL の関数ポインタの束 (dlopen / dlsym)
// - `stream`: TLS で包んだ TCP ストリーム
mod api;
mod stream;
use api::*;
pub use stream::TlsStream;

/// 読み込んだ OpenSSL と共有の SSL_CTX。
struct Loaded {
    api: &'static Api,
    ctx: *mut c_void,
}

impl Drop for Loaded {
    fn drop(&mut self) {
        // SAFETY: ctx はこの構造体が所有している。
        unsafe { (self.api.ctx_free)(self.ctx) }
    }
}

/// [`TlsClient::deferred`] が最初の利用で読み込んだ結果を知らせる先 (起動のログを書くため。T32.5)。
///
/// `Ok(Some(版))` は読めた、`Ok(None)` は libssl が見つからない、`Err` は CA の読み込みなどの失敗。
/// **1 プロセスで 1 回だけ**、最初に https:// のオリジンへ取りに行ったスレッドの上で呼ばれる。
pub type OnLoad = fn(&Result<Option<String>, String>);

/// TLS クライアント (共有の SSL_CTX)。
///
/// [`TlsClient::load`] はその場で OpenSSL を読み込む。[`TlsClient::deferred`] は**最初の
/// [`TlsClient::connect`] まで読み込まない** (T32.5)。起動の callgrind で主スレッドの 70% が
/// `SSL_CTX_set_default_verify_paths` (CA の束の PEM の解析) で、dlopen と合わせて RSS 約 5.5 MB・
/// 起動 20 ms を、https:// のオリジンを取りに行かない (CONNECT だけの) 置き場でも払っていたため。
/// 検証の設定は読み込みの時期に関わらず同じ。
pub struct TlsClient {
    verify: bool,
    ca_file: Option<std::path::PathBuf>,
    on_load: Option<OnLoad>,
    /// 読み込んだ結果。`Ok(None)` は libssl が無い
    state: std::sync::OnceLock<Result<Option<Loaded>, String>>,
}

// SAFETY: SSL_CTX は OpenSSL 1.1 以降スレッド安全に共有できる。
unsafe impl Send for TlsClient {}
unsafe impl Sync for TlsClient {}

impl TlsClient {
    /// システムの OpenSSL を読み込んでクライアントを作る。無ければ `None`。
    pub fn load(verify: bool, ca_file: Option<&Path>) -> Result<Option<Self>, String> {
        let me = Self::deferred(verify, ca_file, None);
        match me.loaded() {
            Ok(Some(_)) => Ok(Some(me)),
            Ok(None) => Ok(None),
            Err(e) => Err(e.clone()),
        }
    }

    /// 最初の [`TlsClient::connect`] で OpenSSL を読み込むクライアントを作る (T32.5)。
    ///
    /// 読み込みの結果は `on_load` に 1 回だけ渡る。libssl が無ければ、その後の
    /// [`TlsClient::connect`] は [`load`](TlsClient::load) が `None` を返したときの呼び出し側と
    /// 同じ `Unsupported` の誤りを返す。
    pub fn deferred(verify: bool, ca_file: Option<&Path>, on_load: Option<OnLoad>) -> Self {
        Self {
            verify,
            ca_file: ca_file.map(Path::to_path_buf),
            on_load,
            state: std::sync::OnceLock::new(),
        }
    }

    /// 読み込み済みか (`--check` や試験が、遅らせたことを確かめるため)。
    #[cfg(test)]
    pub(crate) fn is_loaded(&self) -> bool {
        self.state.get().is_some()
    }

    fn loaded(&self) -> &Result<Option<Loaded>, String> {
        self.state.get_or_init(|| {
            let r = Self::open(self.verify, self.ca_file.as_deref());
            if let Some(f) = self.on_load {
                let seen = match &r {
                    Ok(Some(l)) => Ok(Some(version_of(l.api))),
                    Ok(None) => Ok(None),
                    Err(e) => Err(e.clone()),
                };
                f(&seen);
            }
            r
        })
    }

    fn open(verify: bool, ca_file: Option<&Path>) -> Result<Option<Loaded>, String> {
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
            let loaded = Loaded { api, ctx };
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
                    return Err(format!(
                        "loading CA certificates failed: {}",
                        api.last_error()
                    ));
                }
            } else {
                (api.ctx_set_verify)(ctx, SSL_VERIFY_NONE, ptr::null());
            }
            Ok(Some(loaded))
        }
    }

    /// 読み込んだ OpenSSL のバージョン文字列 (読み込んでいなければここで読み込む)。
    /// 読めなければ空。
    pub fn version(&self) -> String {
        match self.loaded() {
            Ok(Some(l)) => version_of(l.api),
            _ => String::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn verifies(&self) -> bool {
        self.verify
    }

    /// 接続済みの TCP ストリーム上で TLS ハンドシェイクを行う。`host` は SNI と証明書検証に使う。
    pub fn connect(&self, tcp: TcpStream, host: &str) -> io::Result<TlsStream> {
        let (api, ctx) = match self.loaded() {
            Ok(Some(l)) => (l.api, l.ctx),
            Ok(None) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "HTTPS origins need OpenSSL (libssl), which was not found",
                ));
            }
            Err(e) => return Err(io::Error::other(format!("TLS setup failed: {}", e))),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let c_host = CString::new(host)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid host name"))?;
        let is_ip = host.parse::<IpAddr>().is_ok();
        // SAFETY: 以下はすべて OpenSSL の公開 API を正しい引数で呼ぶだけ。ssl は失敗時に必ず解放する。
        unsafe {
            let ssl = (api.ssl_new)(ctx);
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

    /// 遅らせたクライアントは作っただけでは読まず、最初の利用で 1 回だけ読んで知らせる (T32.5)。
    #[test]
    fn deferred_client_loads_once_on_first_use() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEEN: AtomicUsize = AtomicUsize::new(0);
        fn on_load(_: &Result<Option<String>, String>) {
            SEEN.fetch_add(1, Ordering::SeqCst);
        }
        let tls = TlsClient::deferred(true, None, Some(on_load));
        assert!(!tls.is_loaded());
        assert_eq!(SEEN.load(Ordering::SeqCst), 0);
        let v1 = tls.version();
        let v2 = tls.version();
        assert!(tls.is_loaded());
        assert_eq!(v1, v2);
        assert_eq!(SEEN.load(Ordering::SeqCst), 1);
    }

    /// 遅らせたクライアントで CA の読み込みに失敗したら、接続は誤りになる (検証を切って通さない)。
    #[test]
    fn deferred_client_with_a_bad_ca_file_refuses_to_connect() {
        let tls = TlsClient::deferred(true, Some(Path::new("/nonexistent/ca.pem")), None);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let err = tls
            .connect(tcp, "localhost")
            .err()
            .expect("must not connect");
        if TlsClient::load(true, None).ok().flatten().is_none() {
            assert_eq!(err.kind(), io::ErrorKind::Unsupported, "{}", err);
        } else {
            assert!(err.to_string().contains("CA"), "{}", err);
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
