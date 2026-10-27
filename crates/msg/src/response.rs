//! オリジンからの応答の先頭 (ステータス行とヘッダー部) を読む。
//!
//! 中継の本体からも、ブロックリストの取得からも使うので、指標やキャッシュに
//! 依存しないここに置く。

use std::io::{self, BufRead};

use crate::log_trace;

/// 応答の先頭 (状態行 + ヘッダー行。1xx は 1 本ずつ) の合計の上限 (T.29.1)。
///
/// 要求の先頭の上限 (`proxy-server` の `MAX_HEADER_BYTES`) と同じ 128 KiB。今までは無制限で、
/// 空行を送らずにヘッダーを流し続けるオリジン (フォワードプロキシでは宛先をクライアントが選べる)
/// に 300 MB 送られると RSS が 18 → 907 MB になった。越えたら [`HeadTooLarge`] の誤りを返し、
/// 呼ぶ側は 502 にする。
pub const MAX_RESPONSE_HEAD: usize = 128 * 1024;

/// 応答の先頭が [`MAX_RESPONSE_HEAD`] を越えた印 (`io::Error` の中身。T.29.1)。
///
/// 種類は `Other` にしてある (`InvalidData` だと「再利用した接続の読み残し」としてやり直しに回り、
/// 同じ悪いオリジンをもう 1 回叩くことになる)。
#[derive(Debug)]
pub struct HeadTooLarge;

impl std::fmt::Display for HeadTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "origin response head exceeds {} bytes",
            MAX_RESPONSE_HEAD
        )
    }
}

impl std::error::Error for HeadTooLarge {}

/// `e` が [`HeadTooLarge`] か。
pub fn is_head_too_large(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|i| i.is::<HeadTooLarge>())
}

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
/// `100 Continue` も中継しない (`Expect: 100-continue` には中継の本体が本文を読む前に自分で
/// 100 を返し、`Expect` の行はオリジンへ転送しない (T.28.3)。オリジンが勝手に返した 100 を
/// 出すと、クライアントには 100 が 2 回届く)。
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
///
/// 行ごとの `String` (確保 2 回 + UTF-8 の検証 + 写し) を経由せず、`head` へ直接積む。
/// 読んだバイト列は今までと 1 バイトも変わらない (T.28.12)。
///
/// **合計 [`MAX_RESPONSE_HEAD`] までしか読まない** (T.29.1)。手元のバッファ (`fill_buf`) を
/// 上限で切った窓の中で行を探し、窓を読み終えてから 1 回だけ `consume` する。行ごとの
/// `read_until` を `take` で包む形より命令が少ない (包むと先頭 1 つで 70〜90 Ir 増えた)。
fn read_one_response_head<R: BufRead>(reader: &mut R) -> io::Result<(Vec<u8>, u16)> {
    let mut head = Vec::with_capacity(1024);
    let mut status: Option<u16> = None;
    // いま読んでいる行の始まり (`head` の中の位置。行がバッファをまたぐと次の窓へ続く)
    let mut line_start = 0usize;
    loop {
        let avail = match reader.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if avail.is_empty() {
            break;
        }
        let room = MAX_RESPONSE_HEAD - head.len();
        if room == 0 {
            return Err(io::Error::other(HeadTooLarge));
        }
        let window = &avail[..avail.len().min(room)];
        let mut rest = window;
        let mut finished = None;
        while !rest.is_empty() {
            // `&[u8]` の `read_until` は std の memchr で探す (失敗しない)
            rest.read_until(b'\n', &mut head)?;
            if head.last() != Some(&b'\n') {
                // 行の途中で窓が尽きた: 続きは次の `fill_buf` から
                break;
            }
            if status.is_none() {
                match parse_status_line(&head) {
                    Ok(s) => status = Some(s),
                    Err(e) => {
                        finished = Some(Err(e));
                        break;
                    }
                }
            } else if head[line_start..].trim_ascii().is_empty() {
                finished = Some(Ok(()));
                break;
            }
            line_start = head.len();
        }
        let used = window.len() - rest.len();
        reader.consume(used);
        match finished {
            Some(Ok(())) => break,
            Some(Err(e)) => return Err(e),
            None => {}
        }
    }
    match status {
        Some(s) => Ok((head, s)),
        None if head.is_empty() => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "origin closed before sending a status line",
        )),
        // 改行の無い状態行のまま閉じた: そこまでを状態行として読む (今までどおり)
        None => parse_status_line(&head).map(|s| (head, s)),
    }
}

/// 状態行の形を確かめる。再利用した接続に前のやり取りの読み残しがあると、それを応答として
/// 中継してしまうので、ここで弾いて再試行に落とす。
fn parse_status_line(status_line: &[u8]) -> io::Result<u16> {
    let status = std::str::from_utf8(status_line)
        .ok()
        .filter(|s| s.starts_with("HTTP/1."))
        .and_then(|s| s.split_ascii_whitespace().nth(1))
        .filter(|s| s.len() == 3)
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|s| (100..600).contains(s));
    status.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "origin sent a malformed status line: {:?}",
                String::from_utf8_lossy(status_line).trim_ascii()
            ),
        )
    })
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

    /// T.29.1 で読み方を「バッファの窓の中で行を探す」に変えたので、行がバッファの境目を
    /// またいでも (容量 1〜40 バイト) 読む先頭と残りが同じことを見る。
    #[test]
    fn head_is_the_same_across_buffer_boundaries() {
        use std::io::Read;
        let raw = b"HTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\n\
HTTP/1.1 200 OK\r\nX-A: 1\r\nX-Long: 0123456789012345678901234567890123456789\r\n  \r\nBODY";
        for cap in 1..=40 {
            let mut reader = io::BufReader::with_capacity(cap, &raw[..]);
            let mut seen = Vec::new();
            let (head, status) = read_head_forwarding(&mut reader, |h, s| {
                seen.push((s, h.to_vec()));
                Ok(())
            })
            .unwrap();
            assert_eq!(status, 200, "cap {}", cap);
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].1, b"HTTP/1.1 103 Early Hints\r\nLink: </a>\r\n\r\n");
            assert_eq!(
                head,
                b"HTTP/1.1 200 OK\r\nX-A: 1\r\nX-Long: 0123456789012345678901234567890123456789\r\n  \r\n",
                "cap {}",
                cap
            );
            let mut rest = Vec::new();
            reader.read_to_end(&mut rest).unwrap();
            assert_eq!(rest, b"BODY", "cap {}", cap);
        }
        // 空行の無いまま閉じる・改行の無い状態行で閉じる・何も来ない・壊れた状態行
        let (head, status) = read_head(&mut io::BufReader::with_capacity(
            3,
            &b"HTTP/1.1 204 No\r\nX: 1\r\n"[..],
        ))
        .unwrap();
        assert_eq!(
            (head.as_slice(), status),
            (&b"HTTP/1.1 204 No\r\nX: 1\r\n"[..], 204)
        );
        let (head, status) = read_head(&mut &b"HTTP/1.1 204 No"[..]).unwrap();
        assert_eq!((head.as_slice(), status), (&b"HTTP/1.1 204 No"[..], 204));
        assert_eq!(
            read_head(&mut &b""[..]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let mut r: &[u8] = b"hello\r\nHTTP/1.1 200 OK\r\n\r\n";
        assert_eq!(
            read_head(&mut r).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(r, b"HTTP/1.1 200 OK\r\n\r\n", "壊れた状態行の 1 行だけ読む");
    }

    /// T.29.1: 先頭が 128 KiB を越えたら、そこまでしか読まずに [`HeadTooLarge`] で止まる。
    /// 64 KiB の正当な先頭は通る。
    #[test]
    fn response_head_is_bounded() {
        use std::io::Read;
        // 空行の来ないヘッダー
        let mut raw = b"HTTP/1.1 200 OK\r\n".to_vec();
        while raw.len() < 4 * MAX_RESPONSE_HEAD {
            raw.extend_from_slice(b"X-Pad: 0123456789abcdef0123456789abcdef\r\n");
        }
        let mut reader = io::BufReader::new(&raw[..]);
        let err = read_head(&mut reader).unwrap_err();
        assert!(is_head_too_large(&err), "{:?}", err);
        assert_eq!(err.kind(), io::ErrorKind::Other);
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert!(raw.len() - rest.len() <= MAX_RESPONSE_HEAD + 8 * 1024);
        // 改行の無い 1 行 (状態行もヘッダー行も)
        let raw = vec![b'H'; 2 * MAX_RESPONSE_HEAD];
        assert!(is_head_too_large(
            &read_head(&mut io::BufReader::new(&raw[..])).unwrap_err()
        ));
        let mut raw = b"HTTP/1.1 200 OK\r\nX-A: ".to_vec();
        raw.extend(std::iter::repeat_n(b'a', 2 * MAX_RESPONSE_HEAD));
        assert!(is_head_too_large(
            &read_head(&mut io::BufReader::new(&raw[..])).unwrap_err()
        ));
        // 64 KiB のヘッダーは通る
        let mut raw = b"HTTP/1.1 200 OK\r\nX-A: ".to_vec();
        raw.extend(std::iter::repeat_n(b'a', 64 * 1024));
        raw.extend_from_slice(b"\r\nContent-Length: 2\r\n\r\nok");
        let (head, status) = read_head(&mut io::BufReader::new(&raw[..])).unwrap();
        assert_eq!(status, 200);
        assert_eq!(head.len(), raw.len() - 2);
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
