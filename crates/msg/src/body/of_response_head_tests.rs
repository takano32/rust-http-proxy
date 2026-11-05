use super::*;

/// 検査つきの版 (T19.5) が、壊れた `Content-Length` を 1 つも通さないこと。
#[test]
fn checked_rejects_every_broken_content_length() {
    let none = Framing::None;
    let len = |n| Ok(Framing::Length(n));
    // 正常な形は今までどおり (重複が同値なら 1 本に畳む)
    for (head, want) in [
        (&b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n"[..], len(5)),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n",
            len(5),
        ),
        (b"HTTP/1.1 200 OK\r\nContent-Length: 0007\r\n\r\n", len(7)),
        (b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", len(0)),
        (b"HTTP/1.1 200 OK\r\n\r\n", Ok(Framing::Close)),
        // chunked が来ていれば CL は見ない (RFC 9112 §6.3)
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n",
            Ok(Framing::Chunked),
        ),
        // 最後が chunked でない TE は CL を捨てて閉じるまで (T.28.1)。CL が壊れていても見ない
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nContent-Length: 5\r\n\r\n",
            Ok(Framing::Close),
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: xchunked\r\n\r\n",
            Ok(Framing::Close),
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked, gzip\r\nContent-Length: 5\r\n\r\n",
            Ok(Framing::Close),
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: cheese\r\nContent-Length: 5\r\n\r\n",
            Ok(Framing::Close),
        ),
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nContent-Length: abc\r\n\r\n",
            Ok(Framing::Close),
        ),
        // 複数の行は 1 つの並び: 最後の行が chunked なら chunked
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n",
            Ok(Framing::Chunked),
        ),
        // 空の値の TE は無いのと同じ (今までどおり CL で区切る)
        (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: \r\nContent-Length: 5\r\n\r\n",
            len(5),
        ),
    ] {
        assert_eq!(
            Framing::of_response_head_checked(200, false, head),
            want,
            "{:?}",
            String::from_utf8_lossy(head)
        );
    }
    // 壊れた形はすべて弾く (重複不一致・数字でない・負・桁あふれ・カンマ)
    for head in [
        &b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n"[..],
        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nContent-Length: 5\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: +5\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 5, 5\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: abc\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 0x5\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: \r\n\r\n",
        // 20 桁でも u64 に収まらなければ弾く / 21 桁も弾く
        b"HTTP/1.1 200 OK\r\nContent-Length: 99999999999999999999\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 999999999999999999999\r\n\r\n",
    ] {
        assert!(
            Framing::of_response_head_checked(200, false, head).is_err(),
            "{:?} を通した",
            String::from_utf8_lossy(head)
        );
    }
    // 本文を持たない応答でも CL は検査する (HEAD はオリジンの CL をそのまま転送する)。
    // 値が正常なら枠組みは今までどおり `None`
    for status in [101u16, 204, 304] {
        assert_eq!(
            Framing::of_response_head_checked(
                status,
                false,
                b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"
            ),
            Ok(none),
            "status={}",
            status
        );
        assert!(
            Framing::of_response_head_checked(
                status,
                false,
                b"HTTP/1.1 204 No Content\r\nContent-Length: -1\r\n\r\n"
            )
            .is_err(),
            "status={} で壊れた CL を通した",
            status
        );
    }
    assert_eq!(
        Framing::of_response_head_checked(
            200,
            true,
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n"
        ),
        Ok(none),
        "HEAD"
    );
    assert!(
        Framing::of_response_head_checked(
            200,
            true,
            b"HTTP/1.1 200 OK\r\nContent-Length: abc\r\n\r\n"
        )
        .is_err(),
        "HEAD で壊れた CL を通した"
    );
    // 本文を持たない応答は TE があっても `None` (CL の検査も今までどおり)
    assert_eq!(
        Framing::of_response_head_checked(
            200,
            true,
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nContent-Length: 5\r\n\r\n"
        ),
        Ok(none),
        "HEAD + TE"
    );
    // 見分けの口: CL だけ・TE なしの Close では偽、最後が chunked でない TE では真
    assert!(!Framing::response_has_transfer_encoding(
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n"
    ));
    assert!(!Framing::response_has_transfer_encoding(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: \r\n\r\n"
    ));
    assert!(Framing::response_has_transfer_encoding(
        b"HTTP/1.1 200 OK\r\ntransfer-encoding: cheese\r\n\r\n"
    ));
}

/// T.29.4: 1 回の走査で `Age` の値も拾う (枠組みと `Connection` の判定は今までと同じ)。
#[test]
fn scan_picks_up_the_age() {
    let head = b"HTTP/1.1 200 OK\r\nAGE: 500\r\nContent-Length: 2\r\nConnection: close\r\n\r\n";
    let scan = Framing::scan_response_head(200, false, head);
    assert_eq!(scan.framing, Ok(Framing::Length(2)));
    assert!(scan.close);
    assert_eq!(scan.age, Some("500"));
    let head = b"HTTP/1.1 200 OK\r\nX-Age: 1\r\nAges: 2\r\n\r\n";
    assert_eq!(Framing::scan_response_head(200, false, head).age, None);
}

/// T.28.35: クライアントへ伝える TE の行は、複数の行を 1 つの並びに畳み、空の要素を読み飛ばす。
#[test]
fn transfer_encoding_line_joins_the_list() {
    let line = |h: &[u8]| Framing::response_transfer_encoding_line(h);
    assert_eq!(
        line(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nContent-Length: 5\r\n\r\n").as_deref(),
        Some("Transfer-Encoding: gzip")
    );
    assert_eq!(
        line(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked , gzip\r\nX-A: 1\r\nTransfer-Encoding: ,br,\r\n\r\n")
            .as_deref(),
        Some("Transfer-Encoding: chunked, gzip, br")
    );
    assert_eq!(
        line(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: x\0y\r\n\r\n").as_deref(),
        Some("Transfer-Encoding: x y")
    );
    assert_eq!(
        line(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: , \r\n\r\n"),
        None
    );
    assert_eq!(line(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n"), None);
}

/// 生の先頭から決める版が、組を作ってから決める版と同じ結果になること。
#[test]
fn matches_the_pairs_version() {
    let heads: Vec<&[u8]> = vec![
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked, gzip\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: nope\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
        b"HTTP/1.1 200 OK\r\n\r\n",
        // T.28.1: 最後が chunked でない TE と CL
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nContent-Length: 5\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: gzip\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n",
    ];
    for head in heads {
        let pairs = crate::headers::response_pairs(head);
        for status in [200u16, 204, 304, 101, 500] {
            for head_only in [false, true] {
                assert_eq!(
                    Framing::of_response_head(status, head_only, head),
                    Framing::of_response(status, head_only, &pairs),
                    "head={:?} status={} head_only={}",
                    String::from_utf8_lossy(head),
                    status,
                    head_only
                );
            }
        }
    }
}
