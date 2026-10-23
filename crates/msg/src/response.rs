//! オリジンからの応答の先頭 (ステータス行とヘッダー部) を読む。
//!
//! 中継の本体からも、ブロックリストの取得からも使うので、指標やキャッシュに
//! 依存しないここに置く。

use std::io::{self, BufRead};

use crate::log_trace;

/// (生バイト列, ステータスコード, 小文字化したヘッダー名と値の組)
pub type ResponseHead = (Vec<u8>, u16, Vec<(String, String)>);

/// ステータス行とヘッダー部を読み切り、**生バイト列とステータスだけ**返す。
///
/// 小文字化した名前と値の複製 (`Vec<(String, String)>`) は作らない。素通しの経路では
/// 枠組みと `Connection: close` しか見ないのに、要求ごとにヘッダーの本数だけ確保していた。
/// 組が要るところ (キャッシュの判定) は [`crate::headers::response_pairs`] を呼ぶ。
///
/// 1xx の中間応答は中継せずに読み飛ばす。中継する経路は [`read_head_forwarding`] を使う。
pub fn read_head<R: BufRead>(reader: &mut R) -> io::Result<(Vec<u8>, u16)> {
    read_head_forwarding(reader, |_, _| Ok(()))
}

/// [`read_head`] の、1xx の中間応答を 1 本ずつ `on_interim` に渡す版。
///
/// RFC 9110 §15.2 は、自分が起こした要求でない限り 1xx を中継することを求めている
/// (中継の本体はこれに当たる)。**101 Switching Protocols は除く** — これは接続の
/// 切り替えの応答で、プロトコルを切り替えない中継では最終応答として扱う。
///
/// `100 Continue` も中継しない (`Expect: 100-continue` をオリジンまで素通ししていて、
/// 要求の本文は待たずに送っているので、これをクライアントへ出す意味がない)。
/// 1xx は何本でもよく、`on_interim` は届いた順に呼ばれ、その後で最終応答を返す。
pub fn read_head_forwarding<R: BufRead, F>(
    reader: &mut R,
    mut on_interim: F,
) -> io::Result<(Vec<u8>, u16)>
where
    F: FnMut(&[u8], u16) -> io::Result<()>,
{
    loop {
        let (head, status) = read_one_response_head(reader)?;
        if (100..200).contains(&status) && status != 101 {
            if status == 100 {
                log_trace!(None, "skipping interim {} response from the origin", status);
            } else {
                log_trace!(None, "interim {} response from the origin", status);
                on_interim(&head, status)?;
            }
            continue;
        }
        return Ok((head, status));
    }
}

/// [`read_head`] に、小文字化したヘッダーの組を足したもの。
pub fn read_response_head<R: BufRead>(reader: &mut R) -> io::Result<ResponseHead> {
    let (head, status) = read_head(reader)?;
    let headers = crate::headers::response_pairs(&head);
    Ok((head, status, headers))
}

/// 応答を 1 つだけ読む (1xx の読み飛ばしは呼び出し側)。
fn read_one_response_head<R: BufRead>(reader: &mut R) -> io::Result<(Vec<u8>, u16)> {
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
                status_line.trim_ascii()
            ),
        ));
    };

    // 行の読み取りバッファは 1 本を使い回す (ヘッダーの数だけ String を作らない)
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        head.extend_from_slice(line.as_bytes());
        if line.trim_ascii().is_empty() {
            break;
        }
    }

    Ok((head, status))
}

#[cfg(test)]
mod interim_tests {
    use super::*;
    use std::io::Cursor;

    /// 1xx は届いた順に `on_interim` へ渡り、100 と 101 は渡らない。
    #[test]
    fn interim_responses_are_passed_in_order() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\n\
HTTP/1.1 103 Early Hints\r\nLink: </a.css>\r\n\r\n\
HTTP/1.1 102 Processing\r\n\r\n\
HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
            .to_vec();
        let mut reader = Cursor::new(raw);
        let mut seen: Vec<(u16, String)> = Vec::new();
        let (head, status) = read_head_forwarding(&mut reader, |h, s| {
            seen.push((s, String::from_utf8_lossy(h).into_owned()));
            Ok(())
        })
        .unwrap();
        assert_eq!(status, 200);
        assert!(head.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert_eq!(seen.len(), 2, "100 は渡らない: {:?}", seen);
        assert_eq!(seen[0].0, 103);
        assert!(seen[0].1.contains("Link: </a.css>"));
        assert_eq!(seen[1].0, 102);
        // 本文は読み捨てない (呼び出し側が続きを読む)
        let mut rest = String::new();
        use std::io::Read;
        reader.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "ok");
    }

    /// 101 は中間応答として渡さず、最終応答として返す (切り替えの応答なので中継しない)。
    #[test]
    fn switching_protocols_is_returned_as_the_final_response() {
        let raw = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n".to_vec();
        let mut reader = Cursor::new(raw);
        let mut seen = 0usize;
        let (_, status) = read_head_forwarding(&mut reader, |_, _| {
            seen += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(status, 101);
        assert_eq!(seen, 0);
    }

    /// `on_interim` が失敗したら、その場で読むのをやめて同じ失敗を返す。
    #[test]
    fn a_failing_interim_callback_stops_the_read() {
        let raw = b"HTTP/1.1 103 Early Hints\r\n\r\nHTTP/1.1 200 OK\r\n\r\n".to_vec();
        let mut reader = Cursor::new(raw);
        let err = read_head_forwarding(&mut reader, |_, _| {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "client gone"))
        })
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }
}
