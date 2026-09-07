//! オリジンからの応答の先頭 (ステータス行とヘッダー部) を読む。
//!
//! 中継の本体からも、ブロックリストの取得からも使うので、指標やキャッシュに
//! 依存しないここに置く。

use std::io::{self, BufRead};

use crate::log_trace;

/// (生バイト列, ステータスコード, 小文字化したヘッダー名と値の組)
pub type ResponseHead = (Vec<u8>, u16, Vec<(String, String)>);

/// ステータス行とヘッダー部を読み切り、[`ResponseHead`] を返す。
pub fn read_response_head<R: BufRead>(reader: &mut R) -> io::Result<ResponseHead> {
    loop {
        let (head, status, headers) = read_one_response_head(reader)?;
        // 1xx は中間応答なので読み飛ばして本物の応答を待つ (101 Switching Protocols は除く)。
        // `Expect: 100-continue` はオリジンまで素通しているので、これが無いと 100 Continue を
        // 最終応答として中継してしまう
        if (100..200).contains(&status) && status != 101 {
            log_trace!(None, "skipping interim {} response from the origin", status);
            continue;
        }
        return Ok((head, status, headers));
    }
}

/// 応答を 1 つだけ読む (1xx の読み飛ばしは呼び出し側)。
fn read_one_response_head<R: BufRead>(reader: &mut R) -> io::Result<ResponseHead> {
    let mut head = Vec::with_capacity(1024);
    let mut status_line = String::new();
    if reader.read_line(&mut status_line)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "origin closed before sending a status line",
        ));
    }
    head.extend_from_slice(status_line.as_bytes());

    // 状態行の形を確かめる。再利用した接続に前のやり取りの読み残しがあると、それを応答として
    // 中継してしまうので、ここで弾いて再試行に落とす
    let status = status_line
        .split_whitespace()
        .nth(1)
        .filter(|s| s.len() == 3)
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|s| (100..600).contains(s));
    let Some(status) = status.filter(|_| status_line.starts_with("HTTP/1.")) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "origin sent a malformed status line: {:?}",
                status_line.trim()
            ),
        ));
    };

    let mut headers = Vec::new();
    // 行の読み取りバッファは 1 本を使い回す (ヘッダーの数だけ String を作らない)
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        head.extend_from_slice(line.as_bytes());
        if line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    Ok((head, status, headers))
}
