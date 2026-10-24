//! リクエストの解析: 使うヘッダーの抽出と、要求先 (オリジン) の決定。

use std::io;

use crate::ascii;
use crate::log_trace;
use crate::log_warn;
use crate::net;
use crate::origin::Scheme;

/// リクエストヘッダーのうち転送・キャッシュ判断に使うもの。
#[derive(Default)]
pub struct RequestHeaders {
    pub host: Option<String>,
    /// `Host` が 2 本以上あった (T19.2)。RFC 9110 §7.2 は 400 を要求するので、
    /// これを立てて断る。**値は最初の 1 本**を残す (2 本目以降は確保しない)
    pub dup_host: bool,
    /// `Content-Length` の値 (重複が同値なら畳んだ 1 本。数でなければ `None`)
    pub content_length: Option<u64>,
    /// `Content-Length` の形が不正 (数字でない・`u64` に収まらない・重複で不一致)。
    ///
    /// 本文の切れ目が決まらないので、要求そのものを 400 で断る (T19.1)。
    /// **判定 (ここ) と転送 (生の行) が食い違わないようにする**ための持ち場で、
    /// 真なら [`RequestHeaders::content_length`] の値は使ってはいけない。
    pub content_length_invalid: bool,
    /// `Transfer-Encoding` の最後の指定が `chunked` で、`chunked` が 1 回だけか
    /// (このときだけ本文を chunked として読む。T27.6 で [`RequestHeaders::te_invalid`] と分けた)
    pub chunked: bool,
    /// `Transfer-Encoding` の行が 1 本でもあった (HTTP/1.0 の要求で立っていれば断る。T27.6)
    pub te_present: bool,
    /// `Transfer-Encoding` があるのに本文の切れ目が決まらない (T27.6): 最後の符号化が `chunked`
    /// でない (`xchunked`・`gzip`・`chunked, identity`) か、`chunked` が 2 回以上ある。
    /// RFC 9112 §6.3 は「要求の TE の最後が chunked でなければ 400 で断って閉じる」を求める。
    /// 今までは本文なしとしてヘッダーだけを転送し、本文はクライアントの次の要求として読まれた
    pub te_invalid: bool,
    pub authorization: bool,
    pub cache_control: String,
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<String>,
    pub if_range: Option<String>,
    pub range: Option<String>,
    pub accept_encoding: Option<String>,
    pub connection_close: bool,
    pub connection_keep_alive: bool,
    /// 折り返し (obs-fold) の継続行が混ざっていた (RFC 9112 §5.2)。
    /// 中継する側は 400 で断るか SP 1 個に置き換えるかを選べるが、置き換えると値が
    /// 黙って変わるので**断る**方を採る (T27.2)。断るかどうかは `http` 側が決める
    pub obs_fold: bool,
    /// ヘッダーの形が不正 (T27.5): 名前とコロンの間に空白がある (RFC 9112 §5.1 は 400 を求める)、
    /// または値に CR / LF / NUL がある (RFC 9110 §5.5。断るか SP に置き換える)。どちらも前段と
    /// 後段で読み方が割れる (`Content-Length : 5` を無視するオリジン、CR を改行と読むオリジン)。
    /// 要求は置き換えると値が黙って変わるので**断る**。断るのは `http` 側と CONNECT の入口
    pub bad_header: bool,
}

pub fn parse_request_headers(raw_headers: &[String], conn_id: usize) -> RequestHeaders {
    let mut h = RequestHeaders::default();
    // `Transfer-Encoding` は複数の行でも 1 つの並びとして読む (RFC 9110 §5.3)。
    // 最後の符号化が `chunked` か、`chunked` を何回見たか (T27.6)
    let mut te_last_chunked = false;
    let mut te_chunked_count = 0u32;
    for line in raw_headers {
        // 行の先頭が SP / HTAB なら直前の行の続き (obs-fold。RFC 9112 §5.2)。
        // 単独のヘッダーとして読むと継続行の値が黙って消える (オリジンには前の行だけが届く)
        // ので、ここでは旗を立てるだけにして呼び出し側に 400 を返させる
        if matches!(line.as_bytes().first(), Some(b' ' | b'\t')) {
            h.obs_fold = true;
            continue;
        }
        let Some((k, v)) = ascii::split_once(line, b':') else {
            continue;
        };
        // 名前の後ろの空白 (`Content-Length : 5`) と、値の CR / LF / NUL (T27.5)。行は行末の
        // CRLF (か LF) を付けたまま来るので、行末の CR / LF を外した残りの途中にあるものだけを見る
        if matches!(k.as_bytes().last(), Some(b' ' | b'\t'))
            || v.trim_end_matches(['\r', '\n'])
                .bytes()
                .any(|b| matches!(b, b'\r' | b'\n' | 0))
        {
            h.bad_header = true;
        }
        // 名前は小文字化した `String` を作らずに比べる (要求ごとにヘッダーの本数だけ確保していた)
        let name = k.trim_ascii();
        let v_trim = v.trim_ascii();
        log_trace!(Some(conn_id), "req header  {}: {}", name, v_trim);
        let is = |want: &str| name.eq_ignore_ascii_case(want);
        if is("host") {
            // 2 本目を見た時点で「重複」の印を立てる (要求の密輸と、どの Host を信じるかの
            // 食い違いを避ける)。RFC 9110 §7.2 は中継する側に 400 を求めている
            if h.host.is_some() {
                h.dup_host = true;
            } else {
                h.host = Some(v_trim.to_string());
            }
        } else if is("authorization") {
            h.authorization = true;
        } else if is("cache-control") || is("pragma") {
            if !h.cache_control.is_empty() {
                h.cache_control.push(',');
            }
            h.cache_control.push_str(&v_trim.to_ascii_lowercase());
        } else if is("if-none-match") {
            h.if_none_match = Some(v_trim.to_string());
        } else if is("if-modified-since") {
            h.if_modified_since = Some(v_trim.to_string());
        } else if is("if-range") {
            h.if_range = Some(v_trim.to_string());
        } else if is("range") {
            h.range = Some(v_trim.to_string());
        } else if is("accept-encoding") {
            h.accept_encoding = Some(v_trim.to_string());
        } else if is("connection") || is("proxy-connection") {
            for token in v_trim.split(',') {
                let t = token.trim_ascii();
                if t.eq_ignore_ascii_case("close") {
                    h.connection_close = true;
                } else if t.eq_ignore_ascii_case("keep-alive") {
                    h.connection_keep_alive = true;
                }
            }
        } else if is("content-length") {
            // RFC 9110 §8.6: `Content-Length = 1*DIGIT`。数字だけの 10 進で `u64` に収まること。
            // 重複は全部同じ値なら 1 本に畳んでよく、1 つでも違えば本文の切れ目が決まらないので
            // 要求を断る (T19.1)
            match (h.content_length, content_length_value(v_trim)) {
                (_, None) => {
                    log_warn!(Some(conn_id), "invalid Content-Length: {:?}", v_trim);
                    h.content_length_invalid = true;
                }
                (None, Some(n)) => h.content_length = Some(n),
                (Some(prev), Some(n)) if prev == n => {}
                (Some(prev), Some(n)) => {
                    log_warn!(
                        Some(conn_id),
                        "conflicting Content-Length: {} and {}",
                        prev,
                        n
                    );
                    h.content_length_invalid = true;
                }
            }
        } else if is("transfer-encoding") {
            // 最後の符号化が chunked なら本文は chunked (RFC 9112 §6.1)。並びの空の要素は読み飛ばす
            h.te_present = true;
            for t in v_trim
                .split(',')
                .map(str::trim_ascii)
                .filter(|t| !t.is_empty())
            {
                te_last_chunked = t.eq_ignore_ascii_case("chunked");
                te_chunked_count += u32::from(te_last_chunked);
            }
        }
    }
    if h.te_present {
        h.chunked = te_last_chunked && te_chunked_count == 1;
        h.te_invalid = !h.chunked;
        if h.te_invalid {
            log_warn!(
                Some(conn_id),
                "Transfer-Encoding does not end in a single chunked coding"
            );
        }
    }
    h
}

/// `Content-Length` の値として受け付ける形か (RFC 9110 §8.6 の `1*DIGIT`、`u64` に収まること)。
///
/// `-1`・`+5`・`5, 5`・空・`0x5`・21 桁 (u64 に収まらない) はどれも受け付けない。
/// 先頭の `0` は文法どおり許す (`0007` は 7)。**確保しない**ので要求ごとの費用は数字の判定 1 回。
fn content_length_value(v: &str) -> Option<u64> {
    // `str::parse::<u64>` は先頭の `+` を受け付けるので、数字だけであることを先に見る
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    v.parse::<u64>().ok()
}

/// ポート番号を確保せずに 10 進の文字列にする (`u16::to_string` は `String` を 1 本作る)。
fn port_digits(buf: &mut [u8; 5], mut port: u16) -> &str {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (port % 10) as u8;
        port /= 10;
        if port == 0 {
            break;
        }
    }
    // 数字しか書いていないので UTF-8 として必ず妥当
    std::str::from_utf8(&buf[i..]).unwrap_or("0")
}

/// 要求先 (オリジン)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub scheme: Scheme,
    /// ホスト (必要ならポート付き)、IPv6 リテラルは括弧付き
    pub host_port: String,
    pub path: String,
    /// `/https/host/path` 形式 (プロキシをオリジンとして叩く形) で頼まれたか。
    /// この場合、応答の Location も同じ形式に書き換えてクライアントをプロキシに留める
    pub mapped: bool,
}

impl Origin {
    // 以下 3 つは [`Origin::locate`] に委譲する。同じ正規化を 2 か所に書くと、
    // 片方だけ直したときに黙って食い違う (例: Location 先のキャッシュ無効化は
    // 「locate() 由来の接続先」と「server_addr() の結果」を突き合わせている)。

    pub fn server_addr(&self) -> String {
        self.locate().server_addr().to_string()
    }

    /// キャッシュキーとログに使う正規化 URL。
    pub fn url(&self) -> String {
        self.locate().into_url()
    }

    /// 接続プールのキー。
    pub fn pool_key(&self) -> String {
        self.locate().pool_key().to_string()
    }

    pub fn host(&self) -> String {
        net::split_host_port(&self.host_port).0
    }

    /// 正規化 URL を 1 本だけ組み立てる。接続プールのキーと接続先はその部分文字列として借りる。
    /// (要求ごとに `with_default_port` を 3 回、`format!` を 2 回やり直すのをやめる)
    pub fn locate(&self) -> Located {
        let addr_start = self.scheme.as_str().len() + 3; // "://"
        let mut url =
            String::with_capacity(addr_start + self.host_port.len() + 6 + self.path.len());
        url.push_str(self.scheme.as_str());
        url.push_str("://");
        let (host, port) = net::split_host_port_ref(&self.host_port);
        if host.contains(':') && !host.starts_with('[') {
            url.push('[');
            url.push_str(host);
            url.push(']');
        } else {
            url.push_str(host);
        }
        url.push(':');
        let mut digits = [0u8; 5];
        url.push_str(port_digits(
            &mut digits,
            port.unwrap_or_else(|| self.scheme.default_port()),
        ));
        let origin_end = url.len();
        url.push_str(&self.path);
        Located {
            url,
            origin_end,
            addr_start,
        }
    }
}

/// `scheme://host:port/path` を 1 本持ち、プールキーと接続先を部分文字列で返す。
pub struct Located {
    url: String,
    /// `scheme://host:port` が終わる位置
    origin_end: usize,
    /// `host:port` が始まる位置
    addr_start: usize,
}

impl Located {
    /// キャッシュキーとログに使う正規化 URL。
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 接続プールのキー (`scheme://host:port`)。
    pub fn pool_key(&self) -> &str {
        &self.url[..self.origin_end]
    }

    /// 接続先 (`host:port`)。
    pub fn server_addr(&self) -> &str {
        &self.url[self.addr_start..self.origin_end]
    }

    /// 組み立てた URL をそのまま受け取る (複製しない)。
    pub fn into_url(self) -> String {
        self.url
    }
}

/// 要求行の target と Host ヘッダーからオリジンを決める。
/// 受け付ける形: `http://h/p`、`https://h/p` (絶対形式)、`/https/h/p`、`/http/h/p` (マッピング)、
/// `/p` (Host ヘッダー宛て)、`h` (ホストのみ)。
/// 絶対 URI の `//` の後ろで、authority (`host:port`) が終わる位置 (RFC 3986 §3.2: `/`・`?`・`#` の手前)。
///
/// `/` だけで切っていたので、`http://h?q` は `h?q` を名前として引きに行っていた (T27.8)。
fn authority_end(rest: &str) -> usize {
    rest.find(['/', '?', '#']).unwrap_or(rest.len())
}

/// authority を確かめる。空ならホストが無い、`@` があれば userinfo (RFC 9110 §4.2.4。T27.8)。
///
/// userinfo は今までは `user:pw@host` を丸ごと名前として引きに行き 502 になっていた。
/// http(s) の URI では非推奨で、受け取った側は誤りとして扱うべきなので 400 にする
/// (呼び出し側は [`parse_origin`] の失敗を `bad_uri` で断る)
fn check_authority(host_port: &str) -> io::Result<()> {
    if host_port.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Missing host in request target",
        ));
    }
    if host_port.contains('@') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "userinfo in request target",
        ));
    }
    Ok(())
}

/// 要求の対象から `#fragment` を落とす (RFC 9112 §3.2 の要求の対象に fragment は入らない。T27.8)。
/// オリジンへは fragment の手前までを送る (今までは `/a#frag` をそのまま送っていた)
fn without_fragment(path: &str) -> &str {
    match path.find('#') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => path,
    }
}

pub fn parse_origin(target: &str, host_header: Option<&str>) -> io::Result<Origin> {
    let split = |scheme: Scheme, rest: &str, mapped: bool| -> io::Result<Origin> {
        let end = authority_end(rest);
        let host_port = &rest[..end];
        check_authority(host_port)?;
        // `http://h?q` の path は `/?q` (RFC 9112 §3.2.2: path が空なら `/` を補う)
        let path = match &rest[end..] {
            "" => "/".to_string(),
            p if p.starts_with('/') => without_fragment(p).to_string(),
            p => match without_fragment(p) {
                "/" => "/".to_string(),
                q => format!("/{}", q),
            },
        };
        Ok(Origin {
            scheme,
            host_port: host_port.to_string(),
            path,
            mapped,
        })
    };
    if let Some(rest) = target.strip_prefix("http://") {
        return split(Scheme::Http, rest, false);
    }
    if let Some(rest) = target.strip_prefix("https://") {
        return split(Scheme::Https, rest, false);
    }
    if let Some(rest) = target.strip_prefix("/https/") {
        return split(Scheme::Https, rest, true);
    }
    if let Some(rest) = target.strip_prefix("/http/") {
        return split(Scheme::Http, rest, true);
    }
    if target.starts_with('/') {
        return match host_header {
            Some(h) if !h.is_empty() => Ok(Origin {
                scheme: Scheme::Http,
                host_port: h.to_string(),
                path: without_fragment(target).to_string(),
                mapped: false,
            }),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Missing host in HTTP request",
            )),
        };
    }
    Ok(Origin {
        scheme: Scheme::Http,
        host_port: target.to_string(),
        path: "/".to_string(),
        mapped: false,
    })
}

/// 要求先のホスト (`host[:port]`) だけを**借用で**取り出す ([`parse_origin`] と同じ規則)。
///
/// ACL / ブロックリストの判定は `host_port` しか見ないので、そのためだけに
/// [`parse_origin`] を呼ぶと要求ごとに `String` を 2 本 (`host_port` と `path`) 作ることになる。
/// 返す文字列は必ず `target` か `host_header` の部分文字列なので確保が要らない。
pub fn target_host<'a>(target: &'a str, host_header: Option<&'a str>) -> io::Result<&'a str> {
    let host_of = |rest: &'a str| -> io::Result<&'a str> {
        let host_port = &rest[..authority_end(rest)];
        check_authority(host_port)?;
        Ok(host_port)
    };
    for prefix in ["http://", "https://", "/https/", "/http/"] {
        if let Some(rest) = target.strip_prefix(prefix) {
            return host_of(rest);
        }
    }
    if target.starts_with('/') {
        return match host_header {
            Some(h) if !h.is_empty() => Ok(h),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Missing host in HTTP request",
            )),
        };
    }
    // スキームも `/` も無い形 (`h` や `h:443`) は、丸ごとがホストになる
    Ok(target)
}

/// 要求ターゲットのスキームだけを**確保せずに**取り出す ([`parse_origin`] と同じ規則)。
///
/// ポートを書かない要求 (`http://example.com/`) の宛先を `pool_key` と同じ `host:port` に
/// そろえるために使う (T14.48)。既定のポートはスキームで決まる ([`Scheme::default_port`]) ので、
/// そのためだけに [`parse_origin`] を呼んで `String` を 2 本作らずに済ませる。
pub fn target_scheme(target: &str) -> Scheme {
    if target.starts_with("https://") || target.starts_with("/https/") {
        Scheme::Https
    } else {
        // `http://` も `/http/` も、オリジン形式 (`/p`) もホストだけの形 (`h`) も http
        Scheme::Http
    }
}

/// マッピング形式のクライアント向けに、絶対 URL の Location / Content-Location を `/https/h/p` 形式へ。
pub fn map_locations(lines: &mut [String]) {
    for line in lines.iter_mut() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let lower = name.trim().to_ascii_lowercase();
        if lower != "location" && lower != "content-location" {
            continue;
        }
        let v = value.trim();
        let mapped = if let Some(rest) = v.strip_prefix("https://") {
            Some(format!("/https/{}", rest))
        } else {
            v.strip_prefix("http://")
                .map(|rest| format!("/http/{}", rest))
        };
        if let Some(m) = mapped {
            *line = format!("{}: {}", name.trim(), m);
        }
    }
}

#[cfg(test)]
mod content_length_tests {
    use super::*;

    fn parse(lines: &[&str]) -> RequestHeaders {
        let raw: Vec<String> = lines.iter().map(|l| format!("{}\r\n", l)).collect();
        parse_request_headers(&raw, 0)
    }

    /// 受け付ける形 (数字だけ・u64 に収まる・重複は同値なら畳む)。
    #[test]
    fn accepts_plain_decimal_and_collapses_equal_duplicates() {
        for (value, want) in [
            ("0", 0u64),
            ("5", 5),
            ("0007", 7),
            ("18446744073709551615", u64::MAX),
        ] {
            let h = parse(&[&format!("Content-Length: {}", value)]);
            assert!(!h.content_length_invalid, "value={:?}", value);
            assert_eq!(h.content_length, Some(want), "value={:?}", value);
        }
        let h = parse(&["Content-Length: 5", "Content-Length: 5"]);
        assert!(!h.content_length_invalid);
        assert_eq!(h.content_length, Some(5));
        // 大小の違う綴りでも同じ値なら同じ (名前も値も大小は問わない)
        let h = parse(&["content-length: 5", "CONTENT-LENGTH: 05"]);
        assert!(!h.content_length_invalid);
        assert_eq!(h.content_length, Some(5));
    }

    /// 断る形 (数字でない・負・記号つき・カンマ・空・u64 に収まらない桁数・重複で不一致)。
    #[test]
    fn rejects_everything_that_is_not_one_plain_decimal() {
        for value in [
            "-1",
            "+5",
            "5, 5",
            "",
            "0x5",
            "5.",
            "1e3",
            "٥",                    // 非 ASCII の数字
            "99999999999999999999", // 21 桁
            "18446744073709551616", // u64::MAX + 1
        ] {
            let h = parse(&[&format!("Content-Length: {}", value)]);
            assert!(h.content_length_invalid, "value={:?} を通した", value);
        }
        // 前後の OWS は値の側で落とすので `5 ` / ` 5` は 5 として通る (今までどおり)
        let h = parse(&["Content-Length:  5 "]);
        assert!(!h.content_length_invalid);
        assert_eq!(h.content_length, Some(5));
        // 空の値 (`Content-Length:`) も断る
        let h = parse(&["Content-Length:"]);
        assert!(h.content_length_invalid);

        // 重複で不一致
        let h = parse(&["Content-Length: 5", "Content-Length: 6"]);
        assert!(h.content_length_invalid);
        // 1 本でも壊れていれば断る (後ろに正しい値があっても通さない)
        let h = parse(&["Content-Length: x", "Content-Length: 6"]);
        assert!(h.content_length_invalid);
        let h = parse(&["Content-Length: 6", "Content-Length: x"]);
        assert!(h.content_length_invalid);
    }

    /// CL の無い要求・他のヘッダーだけの要求は今までどおり (誤検出しない)。
    /// T27.6: `Transfer-Encoding` は最後がちょうど 1 回の `chunked` のときだけ chunked。ほかは `te_invalid`。
    #[test]
    fn transfer_encoding_must_end_in_one_chunked() {
        for lines in [
            &["Transfer-Encoding: chunked\r\n"][..],
            &["Transfer-Encoding: Chunked\r\n"],
            &["Transfer-Encoding: gzip, chunked\r\n"],
            &[
                "Transfer-Encoding: gzip\r\n",
                "Transfer-Encoding: chunked\r\n",
            ],
            &["Transfer-Encoding: chunked,\r\n"],
        ] {
            let h = parse(lines);
            assert!(h.te_present && h.chunked && !h.te_invalid, "{:?}", lines);
        }
        for lines in [
            &["Transfer-Encoding: xchunked\r\n"][..],
            &["Transfer-Encoding: gzip\r\n"],
            &["Transfer-Encoding: chunked, identity\r\n"],
            &["Transfer-Encoding: chunked, chunked\r\n"],
            &[
                "Transfer-Encoding: chunked\r\n",
                "Transfer-Encoding: chunked\r\n",
            ],
            &["Transfer-Encoding: chunked;x=1\r\n"],
            &["Transfer-Encoding: \r\n"],
        ] {
            let h = parse(lines);
            assert!(h.te_present && !h.chunked && h.te_invalid, "{:?}", lines);
        }
        let h = parse(&["Content-Length: 5\r\n"]);
        assert!(!h.te_present && !h.chunked && !h.te_invalid);
    }

    /// T27.5: 名前とコロンの間の空白と、値の途中の CR / LF / NUL に印が立つ。行末の CRLF と値の HTAB には立たない。
    #[test]
    fn flags_whitespace_before_colon_and_cr_lf_nul_in_values() {
        for line in [
            "Content-Length : 5\r\n",
            "X-A\t: 1\r\n",
            "X-A: a\rTransfer-Encoding: chunked\r\n",
            "X-A: a\0b\r\n",
            "X-A: a\nb\r\n",
        ] {
            assert!(parse(&[line]).bad_header, "line={:?} を通した", line);
        }
        for line in ["X-A: a\tb\r\n", "X-A: 1\n", "X-A:1", "X-A:  spaced  \r\n"] {
            assert!(!parse(&[line]).bad_header, "line={:?} を断った", line);
        }
    }

    #[test]
    fn leaves_requests_without_content_length_alone() {
        let h = parse(&["Host: example.com", "Accept: */*"]);
        assert!(!h.content_length_invalid);
        assert_eq!(h.content_length, None);
        // `Transfer-Encoding: chunked` 単独もここでは断らない (400 は http 側の判定)
        let h = parse(&["Transfer-Encoding: chunked"]);
        assert!(!h.content_length_invalid);
        assert!(h.chunked);
        assert_eq!(h.content_length, None);
    }
}

#[cfg(test)]
mod target_host_tests {
    use super::*;

    /// [`parse_origin`] の `host_port` と 1 文字も違わないこと (乖離すると ACL がすり抜ける)。
    #[test]
    fn matches_parse_origin() {
        let cases = [
            ("http://example.com/a/b", None),
            ("http://example.com:8080/", None),
            ("https://[::1]:8443/x", None),
            ("/https/example.com/a", None),
            ("/http/example.com:81/a", None),
            ("/only/path", Some("host.example:80")),
            ("/", Some("h")),
            ("example.com:443", None),
            ("example.com", None),
            ("http://example.com?q=1", None),
            ("http://example.com#frag", None),
            ("http://user:pw@example.com/", None),
        ];
        for (target, host) in cases {
            let want = parse_origin(target, host).map(|o| o.host_port);
            let got = target_host(target, host).map(|h| h.to_string());
            match (want, got) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "target={:?} host={:?}", target, host),
                (Err(_), Err(_)) => {}
                (a, b) => panic!("target={:?} host={:?}: {:?} vs {:?}", target, host, a, b),
            }
        }
        // どちらも「ホストが無い」で失敗すること
        for (target, host) in [("http:///a", None), ("/p", None), ("/p", Some(""))] {
            assert!(parse_origin(target, host).is_err());
            assert!(target_host(target, host).is_err());
        }
    }

    /// T27.8: authority は `/`・`?`・`#` の手前まで、userinfo は断る、fragment はオリジンへ送らない。
    #[test]
    fn authority_ends_at_slash_query_or_fragment_and_userinfo_is_refused() {
        let o = |t: &str| {
            let o = parse_origin(t, Some("h.example")).unwrap();
            (o.host_port, o.path)
        };
        assert_eq!(o("http://h?q=1"), ("h".into(), "/?q=1".into()));
        assert_eq!(o("http://h/a?q#frag"), ("h".into(), "/a?q".into()));
        assert_eq!(o("http://h#frag"), ("h".into(), "/".into()));
        assert_eq!(o("http://h?q#f"), ("h".into(), "/?q".into()));
        assert_eq!(o("/a#frag"), ("h.example".into(), "/a".into()));
        assert_eq!(o("/https/h:8443/x#y"), ("h:8443".into(), "/x".into()));
        for t in ["http://user:pw@h/", "http://user@h", "/https/u@h/x"] {
            assert!(parse_origin(t, None).is_err(), "{}", t);
            assert!(target_host(t, None).is_err(), "{}", t);
        }
    }

    /// [`target_scheme`] が [`parse_origin`] の `scheme` と食い違わないこと (T14.48)。
    ///
    /// 食い違うと、個票の宛先に補う既定のポートが `pool_key` のものとずれる。
    #[test]
    fn the_scheme_matches_parse_origin() {
        let cases = [
            ("http://example.com/a/b", None),
            ("https://example.com/a/b", None),
            ("https://[::1]:8443/x", None),
            ("/https/example.com/a", None),
            ("/http/example.com:81/a", None),
            ("/only/path", Some("host.example")),
            ("example.com:443", None),
            ("example.com", None),
        ];
        for (target, host) in cases {
            let want = parse_origin(target, host).unwrap().scheme;
            assert_eq!(want, target_scheme(target), "target={:?}", target);
        }
        assert_eq!(target_scheme("https://h/x").default_port(), 443);
        assert_eq!(target_scheme("h").default_port(), 80);
    }
}
