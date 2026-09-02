//! URL からの取得。プロキシ自身のオリジン接続 (HTTPS 含む) を使い、リダイレクトを追う。

use std::io::{self, BufReader, Read, Write};
use std::time::Duration;

use crate::body::{BodyReader, Framing};
use crate::http::{parse_origin, read_response_head};
use crate::{Upstream, origin};

/// 取得した一覧の上限 (これ以上は切る)。
const MAX_DOWNLOAD: usize = 64 * 1024 * 1024;

pub fn fetch(url: &str, upstream: &Upstream, timeout: Duration) -> io::Result<Vec<u8>> {
    let mut url = url.to_string();
    for _ in 0..6 {
        let o = parse_origin(&url, None)?;
        let server_addr = o.server_addr();
        let stream = origin::connect(
            o.scheme,
            &server_addr,
            &o.host(),
            timeout,
            upstream.tls.as_ref(),
        )?;
        let mut reader = BufReader::new(stream);
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: rust-http-proxy\r\nAccept: text/plain, */*\r\nConnection: close\r\n\r\n",
            o.path, o.host_port
        );
        reader.get_mut().write_all(req.as_bytes())?;
        reader.get_mut().flush()?;
        let (_, status, headers) = read_response_head(&mut reader)?;
        if (300..400).contains(&status)
            && let Some((_, loc)) = headers.iter().find(|(k, _)| k == "location")
        {
            url = if loc.contains("://") {
                loc.clone()
            } else {
                format!("{}://{}{}", o.scheme, server_addr, loc)
            };
            continue;
        }
        if status != 200 {
            return Err(io::Error::other(format!("HTTP {} from {}", status, url)));
        }
        let framing = Framing::of_response(status, false, &headers);
        let mut body = Vec::new();
        BodyReader::new(&mut reader, framing)
            .take(MAX_DOWNLOAD as u64)
            .read_to_end(&mut body)?;
        return Ok(body);
    }
    Err(io::Error::other("too many redirects"))
}
