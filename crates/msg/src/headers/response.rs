//! オリジンの応答の先頭 (状態行 + ヘッダー) を、クライアントへ返す形に整える。
//!
//! 熱い経路は [`write_response_head`] が生の先頭から `out` へ直接書く。`Location` の書き換えが要る
//! マッピング形式だけ [`sanitize_response_head`] で `String` の行に組む ([`ResponseHead`])。

use super::{ConnectionNames, FRAMING_HEADERS, clean_value, is_hop_by_hop, is_hop_by_hop_name};
use crate::ascii;

/// レスポンスの先頭 (ステータス行 + ヘッダー) から hop-by-hop と枠組みのヘッダーを除いたもの。
/// ステータス行は自分のバージョン (HTTP/1.1) に揃える。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseHead {
    pub status_line: String,
    /// `Name: value` (CRLF 無し)
    pub lines: Vec<String>,
}

impl ResponseHead {
    /// 追加のヘッダー行を足して、空行まで含めたバイト列にする。
    pub fn assemble<S: AsRef<str>>(&self, extra: &[S]) -> Vec<u8> {
        let mut out = String::with_capacity(256);
        out.push_str(&self.status_line);
        out.push_str("\r\n");
        for line in self.lines.iter() {
            out.push_str(line);
            out.push_str("\r\n");
        }
        for line in extra {
            out.push_str(line.as_ref());
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        out.into_bytes()
    }
}

/// 応答ヘッダー 1 行の名前が `names` のどれかと一致するか (大小は無視する)。
///
/// 落とすヘッダーの指名に使う。行に `:` が無ければ名前は空扱い (どれにも一致しない)。
pub fn line_named(line: &str, names: &[&str]) -> bool {
    if names.is_empty() {
        return false;
    }
    let name = ascii::split_once(line, b':')
        .map(|(k, _)| k.trim_ascii())
        .unwrap_or("");
    names.iter().any(|n| name.eq_ignore_ascii_case(n))
}

/// 固定枠に控えられる行の範囲の数。ふつうの応答はこの配列に収まる。
const INLINE_LINES: usize = 48;

/// 応答の先頭の、状態行より後ろの空でない行の範囲 (行末の空白は落とした形)。
///
/// 1 回の走査で範囲だけを控え、名前の判定と書き出しはその範囲に対して行う
/// (`split('\n')` の反復子を 2 回回していた。T.28.12)。範囲は `(始まり, 終わり)` の
/// バイト位置で、[`INLINE_LINES`] 本までは確保せずに持つ。
struct HeadLines {
    inline: [(u32, u32); INLINE_LINES],
    spill: Vec<(u32, u32)>,
    n: usize,
}

impl HeadLines {
    /// `text` の `first_end` (状態行の改行の位置) より後ろを走査する。
    fn scan(text: &str, first_end: usize) -> HeadLines {
        let bytes = text.as_bytes();
        let mut me = HeadLines {
            inline: [(0, 0); INLINE_LINES],
            spill: Vec::new(),
            n: 0,
        };
        let mut pos = first_end.saturating_add(1).min(bytes.len());
        while pos < bytes.len() {
            let end = bytes[pos..]
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |i| pos + i);
            let mut e = end;
            while e > pos && bytes[e - 1].is_ascii_whitespace() {
                e -= 1;
            }
            if e > pos {
                if me.n < INLINE_LINES {
                    me.inline[me.n] = (pos as u32, e as u32);
                } else {
                    me.spill.push((pos as u32, e as u32));
                }
                me.n += 1;
            }
            pos = end + 1;
        }
        me
    }

    /// `i` 番目の行 (`text` は `scan` に渡したもの)。
    fn at<'t>(&self, text: &'t str, i: usize) -> &'t str {
        let (s, e) = if i < INLINE_LINES {
            self.inline[i]
        } else {
            self.spill[i - INLINE_LINES]
        };
        &text[s as usize..e as usize]
    }

    /// 全部の行を順に。
    fn iter<'t>(&self, text: &'t str) -> impl Iterator<Item = &'t str> {
        (0..self.n).map(move |i| self.at(text, i))
    }
}

/// 応答の先頭を `Vec<String>` を経由せずに `out` へ書く。
///
/// [`sanitize_response_head`] + [`ResponseHead::assemble`] と同じ結果を、ヘッダー 1 本ごとの
/// String を作らずに得る。`Location` の書き換えが要るとき (マッピング形式) は使えないので、
/// そのときは従来どおり [`sanitize_response_head`] を使う。
/// `status_line` を渡すとステータス行を差し替える (206 / 416 用)。
/// `drop_names` に挙げた名前のヘッダーは落とす (416 の `Content-Type` 用。空なら何もしない)。
pub fn write_response_head<S: AsRef<str>>(
    out: &mut Vec<u8>,
    head: &[u8],
    status_line: Option<&str>,
    drop_names: &[&str],
    extra: &[S],
) {
    // 不正な UTF-8 のときだけ写しを作る
    let lossy;
    let text: &str = match std::str::from_utf8(head) {
        Ok(s) => s,
        Err(_) => {
            lossy = String::from_utf8_lossy(head);
            &lossy
        }
    };
    let first_end = text
        .as_bytes()
        .iter()
        .position(|b| *b == b'\n')
        .unwrap_or(text.len());
    write_status_line(out, text[..first_end].trim_ascii(), status_line);
    let lines = HeadLines::scan(text, first_end);

    // `Connection:` で指名された名前 (借用のまま。溢れたら元の行を読み直す)
    let custom = ConnectionNames::collect(lines.iter(text));
    let named_in_connection = |name: &str| -> bool { custom.contains(name, || lines.iter(text)) };

    // 折り返し (obs-fold) を畳む先。直前の行を残したときだけ、値の終わりを覚える (T19.7)
    let mut fold_at: Option<usize> = None;
    for line in lines.iter(text) {
        // RFC 9112 §5.2: 中継する側は obs-fold を SP 1 個に置き換えてよい (値を捨てない)
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(pos) = fold_at {
                let cont = line.trim_ascii();
                if !cont.is_empty() {
                    let mut piece = Vec::with_capacity(cont.len() + 1);
                    piece.push(b' ');
                    piece.extend_from_slice(cont.as_bytes());
                    out.splice(pos..pos, piece);
                }
            }
            continue;
        }
        let Some(colon) = line.as_bytes().iter().position(|b| *b == b':') else {
            fold_at = None;
            continue;
        };
        let (k, v) = (&line[..colon], &line[colon + 1..]);
        let name = k.trim_ascii();
        // 名前が token でない行 (`X A: b`・`: v`) は落とす (RFC 9110 §5.1。T.32.15 (2))。今まではオリジンの
        // 行をそのまま渡していた (要求の側は T.29.8 から 400)。どのみち落とす名前では字を見ないよう最後に置く
        if is_hop_by_hop_name(name)
            || named_in_connection(name)
            || drop_names.iter().any(|d| name.eq_ignore_ascii_case(d))
            || FRAMING_HEADERS.iter().any(|f| name.eq_ignore_ascii_case(f))
            || !ascii::is_token(name)
        {
            fold_at = None;
            continue;
        }
        let value = v.trim_ascii();
        // 元の行がちょうど `Name: value` の形 (名前の後ろに空白なし・コロンの後ろが SP 1 個・
        // 値に CR / NUL なし) なら、その行をそのまま 1 回で写す (結果のバイト列は同じ)
        let plain = name.len() == colon
            && v.as_bytes().first() == Some(&b' ')
            && value.len() == v.len() - 1
            && !value.bytes().any(|b| b == b'\r' || b == 0);
        if plain {
            out.extend_from_slice(line.as_bytes());
        } else {
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(clean_value(value).as_bytes());
        }
        fold_at = Some(out.len());
        out.extend_from_slice(b"\r\n");
    }
    for line in extra {
        out.extend_from_slice(line.as_ref().as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
}

/// 状態行を書く。`status_line` があればそれ、無ければオリジンの状態行 (`first`) の版だけを
/// `HTTP/1.1` に揃える (版の後ろが無ければ `200 OK`)。CRLF まで書く。
fn write_status_line(out: &mut Vec<u8>, first: &str, status_line: Option<&str>) {
    match status_line {
        Some(sl) => out.extend_from_slice(sl.as_bytes()),
        None => {
            let rest = ascii::split_once(first, b' ')
                .map(|x| x.1)
                .unwrap_or("200 OK")
                .trim_ascii();
            out.extend_from_slice(b"HTTP/1.1 ");
            out.extend_from_slice(rest.as_bytes());
        }
    }
    out.extend_from_slice(b"\r\n");
}

/// 応答の先頭を `String` の行に組む (`Location` を書き換えるマッピング形式のため)。
///
/// 規則は [`write_response_head`] と同じ。`write_response_tests` が 1 バイトも違わないことを
/// 突き合わせる相手 (手本) でもあるので、`Connection:` の指名の読み方もこちらは組を作る形のまま。
pub fn sanitize_response_head(head: &[u8]) -> ResponseHead {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split('\n').map(|l| l.trim_ascii_end());
    let status_line = lines.next().unwrap_or("").trim_ascii();
    let mut parts = status_line.splitn(2, ' ');
    let _version = parts.next();
    let rest = parts.next().unwrap_or("200 OK").trim_ascii();
    let mut out = ResponseHead {
        status_line: format!("HTTP/1.1 {}", rest),
        lines: Vec::new(),
    };
    let raw: Vec<&str> = lines.filter(|l| !l.is_empty()).collect();
    let mut custom_hop: Vec<String> = Vec::new();
    for line in &raw {
        if let Some((k, v)) = ascii::split_once(line, b':')
            && k.trim_ascii().eq_ignore_ascii_case("connection")
        {
            custom_hop.extend(v.split(',').map(|t| t.trim_ascii().to_ascii_lowercase()));
        }
    }
    // 折り返し (obs-fold) の行を、直前の「残した」行に畳むかどうか
    let mut fold_target: Option<usize> = None;
    for line in raw {
        // RFC 9112 §5.2: 中継する側は obs-fold を SP 1 個に置き換えてよい (値を捨てない)。
        // 直前の行を落としていた (hop-by-hop など) ときは、その続きも一緒に落とす
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(i) = fold_target {
                let cont = line.trim_ascii();
                if !cont.is_empty() {
                    out.lines[i].push(' ');
                    out.lines[i].push_str(cont);
                }
            }
            continue;
        }
        let Some((k, v)) = ascii::split_once(line, b':') else {
            fold_target = None;
            continue;
        };
        // 名前が token でない行は落とす ([`write_response_head`] と同じ。T.32.15 (2))
        if !ascii::is_token(k.trim_ascii()) {
            fold_target = None;
            continue;
        }
        let lower = k.trim_ascii().to_ascii_lowercase();
        if is_hop_by_hop(&lower)
            || custom_hop.contains(&lower)
            || FRAMING_HEADERS.contains(&lower.as_str())
        {
            fold_target = None;
            continue;
        }
        out.lines.push(format!(
            "{}: {}",
            k.trim_ascii(),
            clean_value(v.trim_ascii())
        ));
        fold_target = Some(out.lines.len() - 1);
    }
    out
}
