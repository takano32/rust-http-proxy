//! オリジンとのやり取りの部品: 接続の取得 (プールか新規)、要求の先頭の組み立て、
//! 要求本文の転送、失敗の見分け (接続を掴むまでの内訳は T.34.21 で
//! `metrics::Detail::of_connect` へ移した)。

use std::io::{self, BufReader, Read, Write};
use std::time::Duration;

use super::buffers::{CopyBuf, ORIGIN_READ_BUF};
use crate::Upstream;
use crate::body::{self, BodyReader, Framing};
use crate::dns;
use crate::headers;
use crate::log_debug;
use crate::origin::{self, OriginStream};
use crate::request::Origin;
use proxy_base::minrate::Side;

/// オリジンとの接続 (バッファ付き)。
pub(super) type OriginConn = BufReader<OriginStream>;

/// クライアントから受けたヘッダーをそのまま使って、オリジンへの要求の先頭を組み立てる。
/// [`request_head`] と同じ形だが、中間の `Vec<String>` を作らない。
pub(super) fn build_request_head(
    method: &str,
    origin: &Origin,
    raw_headers: &[String],
    client_ip: Option<&str>,
    conditional: &[String],
) -> Vec<u8> {
    let mut head = Vec::with_capacity(256 + raw_headers.iter().map(|h| h.len()).sum::<usize>());
    head.extend_from_slice(method.as_bytes());
    head.extend_from_slice(b" ");
    head.extend_from_slice(origin.path.as_bytes());
    head.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    head.extend_from_slice(origin.host_port.as_bytes());
    head.extend_from_slice(b"\r\n");
    headers::write_request_headers(&mut head, raw_headers, client_ip);
    for line in conditional {
        head.extend_from_slice(line.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    head
}

/// オリジンへの要求の先頭 (要求行 + Host + 転送するヘッダー + 条件付きヘッダー + 空行)。
/// Host はオリジンのものに差し替える (マッピング形式ではプロキシ宛ての Host が来る)。
/// 裏の再検証 (`refresh`) のように、転送するヘッダーを自前で作る経路で使う。
pub(super) fn request_head(
    method: &str,
    origin: &Origin,
    forwarded: &[String],
    conditional: &[String],
) -> Vec<u8> {
    let mut head = format!("{} {} HTTP/1.1\r\n", method, origin.path).into_bytes();
    head.extend_from_slice(format!("Host: {}\r\n", origin.host_port).as_bytes());
    for h in forwarded {
        head.extend_from_slice(h.as_bytes());
    }
    for line in conditional {
        head.extend_from_slice(line.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    head
}

/// プールにあれば再利用し、無ければ接続する (HTTPS なら TLS まで)。戻り値の bool は再利用したか。
/// `timeout` は接続と最初の応答を待つ時間 (stale があるときは短くする)。
///
/// プールから取り出すときは**どの要求でも**生存確認 (`recv(MSG_PEEK|MSG_DONTWAIT)` 1 回) をする
/// (T.29.2)。T26.0 で GET / HEAD (やり直せる要求) だけ省いていたが、省くと「前の応答を読み切った
/// あとにオリジンが遅れて送ったバイト」を次の要求の応答として読んでしまう (手元で: `GET /a` の
/// `CL: 2` の 0.3 秒後に届いた `HTTP/1.1 200 … X-Injected: yes … EVIL` が次の `GET /b` の応答になり、
/// その次は `HIT`)。覗けば読み残しがある接続は捨てる。覗いてから読むまでの窓は塞げない (README)。
pub(super) fn acquire_origin(
    upstream: &Upstream,
    timeout: Duration,
    connect_timeout: Duration,
    conn_id: usize,
    origin: &Origin,
    pool_key: &str,
    resolved: Option<&dns::Resolved<'_>>,
) -> io::Result<(OriginConn, bool)> {
    if let Some(server) = upstream.pool.get(pool_key, timeout) {
        log_debug!(Some(conn_id), "reusing pooled connection to {}", pool_key);
        return Ok((server, true));
    }
    log_debug!(Some(conn_id), "connecting to origin {}", pool_key);
    // TCP の確立だけは短い締め切り (`PROXY_CONNECT_TIMEOUT_SECS`)。`timeout` の方が短ければ
    // (stale を持っていて待たない回) そちらに合わせる。0 (無期限) は必ず負ける (T.28.8)
    let stream = origin::connect_within(
        origin.scheme,
        &origin.server_addr(),
        &origin.host(),
        proxy_base::timeout::shorter(connect_timeout, timeout),
        timeout,
        upstream.tls.as_ref(),
        resolved,
    )?;
    Ok((BufReader::with_capacity(ORIGIN_READ_BUF, stream), false))
}

/// クライアントのリクエスト本文をオリジンへ同じ枠組みで転送する。戻り値は本文のバイト数。
///
/// `total` には送れたぶんを積む (途中で失敗しても、そこまでのバイト数が残る。T.29.7)。
/// 読む 1 回ごとに見張り (`rate`) で待ちを測る (本文を送ってこない相手を切る。T.29.34)。
/// 呼ぶのは `fetch` の 1 か所だけで、本文の無い要求では分岐 1 つで返るので、呼び出しにせず埋め込む
/// (別の CGU に落ちると要求ごとに呼び出しが 1 回増える。T.34.8)
#[inline]
pub(super) fn forward_request_body(
    reader: &mut crate::clientio::ClientReader<'_>,
    server: &mut OriginStream,
    framing: Framing,
    total: &mut u64,
    rate: &proxy_base::minrate::MinRate,
    conn_id: usize,
) -> io::Result<u64> {
    match framing {
        Framing::None | Framing::Close => Ok(0),
        Framing::Length(_) => {
            // io::copy はスタックの 8 KiB で読むため、512 KiB の本文で recvfrom が 66 回出る。
            // chunked 側と同じ 64 KiB の使い回しバッファで読むと 8 回で済む
            let mut body = BodyReader::new(reader, framing);
            let mut buf = CopyBuf::take();
            let mut guard = rate.guard(Side::RequestBody, conn_id);
            loop {
                let n = guard
                    .time(|| body.read(&mut buf))
                    .map_err(client_body_error)?;
                if n == 0 {
                    break;
                }
                server.write_all(&buf[..n])?;
                *total += n as u64;
            }
            Ok(*total)
        }
        Framing::Chunked => {
            let mut body = BodyReader::new(reader, framing);
            let mut buf = CopyBuf::take();
            let mut guard = rate.guard(Side::RequestBody, conn_id);
            loop {
                let n = guard
                    .time(|| body.read(&mut buf))
                    .map_err(client_body_error)?;
                if n == 0 {
                    break;
                }
                body::write_chunk(server, &buf[..n])?;
                *total += n as u64;
            }
            body::write_last_chunk(server)?;
            Ok(*total)
        }
    }
}

/// クライアントの本文を読む側で起きた誤り (T27.8)。オリジンとの読み書きの失敗 (502) と分けて、
/// chunk の大きさが読めない (`0x5`・桁あふれ) などの**クライアントの誤り**を 400 で返すための印。
#[derive(Debug)]
struct ClientBodyError(io::Error);

impl std::fmt::Display for ClientBodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "client request body: {}", self.0)
    }
}

impl std::error::Error for ClientBodyError {}

pub(super) fn client_body_error(e: io::Error) -> io::Error {
    io::Error::new(e.kind(), ClientBodyError(e))
}

/// `e` がクライアントの本文を読む側の誤りか ([`client_body_error`] で包んだもの)。
pub(super) fn is_client_body_error(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|i| i.is::<ClientBodyError>())
}

/// 再利用した接続が死んでいた (要求が届いていない) と判断してよいエラーか。
/// タイムアウトは「届いたが処理が遅い」場合があるので含めない (含めると二重送信になる)。
pub(super) fn is_stale_conn_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            // 状態行が壊れている = 前のやり取りの読み残しを読んだ
            | io::ErrorKind::InvalidData
    )
}

pub(super) fn is_eof_like(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::Scheme;

    fn origin() -> Origin {
        Origin {
            scheme: Scheme::Http,
            host_port: "h.example:8080".to_string(),
            path: "/p?q=1".to_string(),
            mapped: false,
        }
    }

    /// 要求の先頭: 要求行は `HTTP/1.1`、`Host` はオリジンのもの、転送するヘッダーの後ろに条件付きの行。
    /// `Vec<String>` を経由する版 ([`request_head`]) と同じ骨格。
    #[test]
    fn request_head_is_built_from_the_origin_and_the_forwarded_lines() {
        let raw = vec![
            "Host: proxy.example\r\n".to_string(),
            "A: b\r\n".to_string(),
        ];
        let cond = vec!["If-None-Match: \"e\"".to_string()];
        let head = build_request_head("GET", &origin(), &raw, Some("203.0.113.7"), &cond);
        let text = String::from_utf8(head).unwrap();
        assert!(
            text.starts_with("GET /p?q=1 HTTP/1.1\r\nHost: h.example:8080\r\nA: b\r\n"),
            "{}",
            text
        );
        assert!(
            text.contains("X-Forwarded-For: 203.0.113.7\r\n"),
            "{}",
            text
        );
        assert!(text.ends_with("If-None-Match: \"e\"\r\n\r\n"), "{}", text);
        assert!(
            !text.contains("proxy.example"),
            "Host は差し替える: {}",
            text
        );
        let plain = request_head("GET", &origin(), &["A: b\r\n".to_string()], &cond);
        assert_eq!(
            String::from_utf8(plain).unwrap(),
            "GET /p?q=1 HTTP/1.1\r\nHost: h.example:8080\r\nA: b\r\nIf-None-Match: \"e\"\r\n\r\n"
        );
    }

    /// クライアントの本文の誤りの印は種類を保ち、ほかの誤りと見分けられる。
    #[test]
    fn client_body_errors_keep_their_kind() {
        let e = client_body_error(io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"));
        assert!(is_client_body_error(&e));
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert_eq!(e.to_string(), "client request body: bad chunk size");
        assert!(!is_client_body_error(&io::Error::other("plain")));
        // 死んだ接続の見分け: タイムアウトは含めない (二重送信になる)
        assert!(is_stale_conn_error(&io::Error::from(
            io::ErrorKind::UnexpectedEof
        )));
        assert!(is_stale_conn_error(&io::Error::from(
            io::ErrorKind::InvalidData
        )));
        assert!(!is_stale_conn_error(&io::Error::from(
            io::ErrorKind::TimedOut
        )));
        assert!(is_eof_like(&io::Error::from(io::ErrorKind::TimedOut)));
        assert!(!is_eof_like(&io::Error::from(io::ErrorKind::InvalidData)));
    }
}
