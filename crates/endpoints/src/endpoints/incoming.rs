//! 自分宛ての要求 1 本の読み方: **ポートだけ**の自分宛て判定 (T12.3)、メソッドの種類、
//! パスと問い合わせ。ソケットも時計も持たないので、そのまま単体試験で縛れる。
//! 門 (重い口・読み取り専用) と経路表は `super::handle` / `super::dispatch` の側。

use super::parse_query;

/// 要求のメソッドの種類。
///
/// **HEAD は GET として組み、本文だけ書かない** (RFC 9110 §9.3.2: GET と同じヘッダー。T.28.7)。
/// 外の生死監視やロードバランサは既定で HEAD を投げるものが多く、今までは全部 404 だった。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Method {
    Get,
    Head,
    /// `PURGE <url>` (要求ターゲットをそのまま URL として消す)
    Purge,
    /// GET / HEAD / PURGE 以外 (POST など) → 405 (T.28.7)
    Other,
}

impl Method {
    /// 名前の大小は区別しない (`purge` も `PURGE`)。
    pub(super) fn parse(method: &str) -> Method {
        if method.eq_ignore_ascii_case("PURGE") {
            Method::Purge
        } else if method.eq_ignore_ascii_case("HEAD") {
            Method::Head
        } else if method.eq_ignore_ascii_case("GET") {
            Method::Get
        } else {
            Method::Other
        }
    }

    /// GET として組むか (HEAD も GET と同じヘッダーで組む)。
    pub(super) fn is_get(self) -> bool {
        matches!(self, Method::Get | Method::Head)
    }

    pub(super) fn is_head(self) -> bool {
        self == Method::Head
    }

    pub(super) fn is_purge(self) -> bool {
        self == Method::Purge
    }
}

/// 自分宛てと決まった要求 1 本 ([`local_path`] がポートで判定済み)。
pub(super) struct Incoming<'a> {
    pub(super) method: Method,
    /// 問い合わせを外したパス (`/status`)。PURGE は要求ターゲット (URL) そのまま
    pub(super) path: &'a str,
    /// `?` の後ろ (無ければ `None`)
    pub(super) query: Option<&'a str>,
    /// 絶対形式か。この旗は「`/proxy.pac` が自分の名前をどちらから取るか」だけに使う
    /// (どちらの形式でも自分宛てかはポートで決めてある)
    pub(super) absolute_form: bool,
}

impl<'a> Incoming<'a> {
    /// 自分宛てなら `Some`。そうでなければ `None` (= 他所への転送で、ここでは何もしない)。
    /// PURGE は要求ターゲットを URL として受けるので、自分宛て判定を通さない。
    pub(super) fn parse(
        method: &str,
        target: &'a str,
        port: u16,
        host: Option<&str>,
    ) -> Option<Incoming<'a>> {
        let method = Method::parse(method);
        let local = if method.is_purge() {
            target
        } else {
            local_path(target, port, host)?
        };
        let absolute_form = !target.starts_with('/');
        let (path, query) = match local.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (local, None),
        };
        Some(Incoming {
            method,
            path,
            query,
            absolute_form,
        })
    }

    /// 書き換える口か (`/purge` / `PURGE` / `/blocklist?action=<空でない値>`。T14.18)。
    ///
    /// 呼ぶのは `PROXY_ENDPOINTS_READONLY=on` のときと HEAD のときだけ (`&&` の右に置いてある)
    /// なので、既定の GET では `/blocklist` の問い合わせを二度読むことはない。
    pub(super) fn is_write(&self) -> bool {
        is_write(self.method.is_purge(), self.path, self.query)
    }
}

/// 書き換える口か (`/purge` / `PURGE` / `/blocklist?action=<空でない値>`。T14.18)。
pub(super) fn is_write(is_purge: bool, path: &str, query: Option<&str>) -> bool {
    is_purge
        || path == "/purge"
        || (path == "/blocklist"
            && parse_query(query.unwrap_or(""))
                .iter()
                .any(|(k, v)| k == "action" && !v.is_empty()))
}

/// 要求ターゲットを自分宛てのパスに直す。**どちらの形式もポートだけで判定する**:
/// 絶対形式は authority の、オリジン形式は `Host` ヘッダーのポート (無ければ 80) が
/// 自分の待ち受けポートと同じときだけ自分宛て。それ以外 (他所への転送) は `None`。
///
/// オリジン形式を無条件に自分宛てにしていた頃は、知らないパスが `Host` 宛ての転送に落ちて
/// **`Host` が自分自身ならループした** (1 要求で `max_conns` 本。T12.3 の前提 4)。
/// `Host` が無い HTTP/1.0 のオリジン形式は自分宛てにしない (今までどおり 400)。
pub(super) fn local_path<'a>(target: &'a str, port: u16, host: Option<&str>) -> Option<&'a str> {
    if target.starts_with('/') {
        return (authority_port(host?)? == port).then_some(target);
    }
    // スキームは大小を区別しない (`Http://` も。T.28.33)
    let rest = target
        .get(..7)
        .filter(|s| s.eq_ignore_ascii_case("http://"))
        .map(|_| &target[7..])?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    (authority_port(authority)? == port).then_some(path)
}

/// `host[:port]` のポート (無ければ 80)。`[::1]` のようなブラケット付きも扱う。
fn authority_port(authority: &str) -> Option<u16> {
    if authority.is_empty() {
        return None;
    }
    // ポートの無いブラケット付き (`[::1]`) も 80。最後の `:` で割ると `1]` を読んで `None` になり、
    // 80 で待ち受けていても転送に落ちていた (T.34.156)
    if authority.starts_with('[') && authority.ends_with(']') {
        return Some(80);
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => p.parse::<u16>().ok(),
        _ => Some(80),
    }
}

#[cfg(test)]
mod local_path_tests {
    use super::{Incoming, Method, authority_port, is_write, local_path};

    #[test]
    fn origin_form_is_local_only_when_the_host_port_is_ours() {
        assert_eq!(
            local_path("/status", 8080, Some("127.0.0.1:8080")),
            Some("/status")
        );
        assert_eq!(
            local_path("/purge?all=1", 8080, Some("[::1]:8080")),
            Some("/purge?all=1")
        );
        assert_eq!(local_path("/", 80, Some("proxy.example.net")), Some("/"));
        // ポートの無いブラケット付きの IPv6 の `Host` は 80 で待ち受けていれば自分宛て (T.34.156)
        assert_eq!(local_path("/status", 80, Some("[::1]")), Some("/status"));
        assert_eq!(local_path("/status", 8080, Some("[::1]")), None);
        // ポートが違えば今までどおり転送する (透過プロキシの使い方を壊さない)
        assert_eq!(local_path("/status", 8080, Some("example.com")), None);
        assert_eq!(local_path("/status", 8080, Some("example.com:80")), None);
        assert_eq!(local_path("/x", 8080, Some("127.0.0.1:9999")), None);
        // Host の無い HTTP/1.0 のオリジン形式は今までどおり (転送に落ちて 400)
        assert_eq!(local_path("/status", 8080, None), None);
        assert_eq!(local_path("/status", 8080, Some("")), None);
        assert_eq!(local_path("/status", 8080, Some("host:nope")), None);
    }

    #[test]
    fn absolute_form_is_local_only_on_our_port() {
        assert_eq!(
            local_path("http://tokyo.example.net:60624/status", 60624, None),
            Some("/status")
        );
        assert_eq!(local_path("http://[::1]:60624", 60624, None), Some("/"));
        assert_eq!(local_path("http://[::1]/status", 80, None), Some("/status"));
        assert_eq!(local_path("http://[::1]/status", 60624, None), None);
        assert_eq!(local_path("http://example.com/status", 60624, None), None);
        assert_eq!(
            local_path("http://example.com/status", 80, None),
            Some("/status")
        );
        assert_eq!(local_path("http://example.com:8080/x", 60624, None), None);
        assert_eq!(local_path("https://example.com:60624/x", 60624, None), None);
        // スキームは大小を区別しない (T.28.33)
        for t in ["HTTP://h:60624/status", "Http://h:60624/status"] {
            assert_eq!(local_path(t, 60624, None), Some("/status"), "{}", t);
        }
        assert_eq!(local_path("HTTPS://h:60624/status", 60624, None), None);
    }

    /// `host[:port]` のポートの読み方 (無ければ 80、ブラケット付きの IPv6、読めないポート)。
    #[test]
    fn the_authority_port_defaults_to_80_and_reads_bracketed_ipv6() {
        assert_eq!(authority_port("example.com"), Some(80));
        assert_eq!(authority_port("example.com:8080"), Some(8080));
        assert_eq!(authority_port("[::1]:8080"), Some(8080));
        assert_eq!(authority_port("[2001:db8::1]:80"), Some(80));
        // ポートの無いブラケット付きの IPv6 も 80 (T.34.156。前は最後の `:` で割って `1]` を読み、`None` だった)
        assert_eq!(authority_port("[::1]"), Some(80));
        assert_eq!(authority_port("[2001:db8::1]"), Some(80));
        // 読めないポートと空の authority は「自分宛てではない」
        assert_eq!(authority_port("host:nope"), None);
        assert_eq!(authority_port("host:70000"), None);
        assert_eq!(authority_port(""), None);
    }

    /// メソッドの種類は名前の大小を問わず、HEAD は GET として組む (T.28.7)。
    #[test]
    fn methods_are_classified_case_insensitively() {
        assert_eq!(Method::parse("GET"), Method::Get);
        assert_eq!(Method::parse("get"), Method::Get);
        assert_eq!(Method::parse("HEAD"), Method::Head);
        assert_eq!(Method::parse("Head"), Method::Head);
        assert_eq!(Method::parse("PURGE"), Method::Purge);
        assert_eq!(Method::parse("purge"), Method::Purge);
        assert_eq!(Method::parse("POST"), Method::Other);
        assert_eq!(Method::parse(""), Method::Other);
        assert!(Method::Get.is_get() && Method::Head.is_get());
        assert!(!Method::Purge.is_get() && !Method::Other.is_get());
        assert!(Method::Head.is_head() && !Method::Get.is_head());
        assert!(Method::Purge.is_purge() && !Method::Get.is_purge());
    }

    /// 自分宛ての要求はパスと問い合わせに割れ、PURGE は要求ターゲットをそのまま持つ。
    #[test]
    fn an_incoming_request_splits_the_path_and_the_query() {
        let r = Incoming::parse("GET", "/recent?n=5&client=10.0.0.1", 8080, Some("h:8080"))
            .expect("自分宛て");
        assert_eq!(r.method, Method::Get);
        assert_eq!(r.path, "/recent");
        assert_eq!(r.query, Some("n=5&client=10.0.0.1"));
        assert!(!r.absolute_form);
        // 絶対形式はパスだけが残り、旗が立つ (`/proxy.pac` が自分の名前を authority から取る)
        let r = Incoming::parse("HEAD", "http://proxy.local:8080/status", 8080, None)
            .expect("自分宛て");
        assert_eq!(r.method, Method::Head);
        assert_eq!((r.path, r.query), ("/status", None));
        assert!(r.absolute_form);
        // 問い合わせの無いパス
        let r = Incoming::parse("GET", "/", 8080, Some("h:8080")).expect("自分宛て");
        assert_eq!((r.path, r.query), ("/", None));
        // PURGE は自分宛て判定を通さず、URL をそのまま持つ (問い合わせも割れる)
        let r = Incoming::parse("PURGE", "http://example.com/x?y=1", 8080, None).expect("PURGE");
        assert_eq!(r.method, Method::Purge);
        assert_eq!((r.path, r.query), ("http://example.com/x", Some("y=1")));
        assert!(r.absolute_form);
        // 他所宛ては `None` (転送する)
        assert!(Incoming::parse("GET", "http://example.com/", 8080, None).is_none());
        assert!(Incoming::parse("POST", "/status", 8080, Some("example.com:80")).is_none());
    }

    /// 書き換える口は `/purge`・`PURGE`・空でない `action=` の `/blocklist` だけ (T14.18)。
    #[test]
    fn only_purge_and_blocklist_actions_are_writes() {
        assert!(is_write(true, "http://example.com/x", None));
        assert!(is_write(false, "/purge", Some("url=http://a/")));
        assert!(is_write(false, "/purge", None));
        assert!(is_write(
            false,
            "/blocklist",
            Some("host=a.test&action=block")
        ));
        // 空の `action=` と、`action` の無い `/blocklist` は読む口
        assert!(!is_write(false, "/blocklist", Some("host=a.test&action=")));
        assert!(!is_write(false, "/blocklist", Some("host=a.test")));
        assert!(!is_write(false, "/blocklist", None));
        assert!(!is_write(false, "/status", Some("action=block")));
        // `Incoming` 越しも同じ
        let r =
            Incoming::parse("GET", "/blocklist?action=clear&host=a.test", 1, Some("h:1")).unwrap();
        assert!(r.is_write());
        let r = Incoming::parse("GET", "/blocklist?host=a.test", 1, Some("h:1")).unwrap();
        assert!(!r.is_write());
    }
}
