//! CONNECT の最初のバイトから SNI を読む (`PROXY_PEEK_SNI`。T14.38)。
//!
//! T14.7 の `literal_targets` (IP リテラル宛ての CONNECT) は「**本当はどこへ行って
//! いるか**」が分からない。CONNECT のあとクライアントが最初に送るのは TLS の
//! ClientHello で、その中の SNI (`server_name` 拡張) に宛先の名前がある。
//! `200 Connection Established` を書いたあと、**最初の中継の前に 1 回だけ
//! `recv(MSG_PEEK)`** すれば (バイトは消費しないので `splice` の経路は 1 命令も
//! 変わらない)、IP リテラル宛てでも名前が分かり、CONNECT のホストと SNI が違う
//! (domain fronting、または設定を間違えたクライアント) ことも数えられる。
//!
//! ここに置くのは**解析と設定の旗**だけで、`recv` を呼ぶのは
//! [`crate::tunnel`] の中継 (Linux) の入口 1 か所。壊れていれば `None` で終わり、
//! 中継はそのまま続く。

use std::sync::atomic::{AtomicU32, Ordering};

/// 覗く長さ (バイト)。ClientHello の大半はこれで足りる (足りなければ `None`)。
pub const PEEK_LEN: usize = 1024;

/// TLS の既定のポート。**ここ以外では覗かない** (TLS とは限らないため)。
pub const TLS_PORT: u16 = 443;

/// SNI として受け取る名前の最長 (バイト)。DNS の名前の上限と同じ。
const MAX_NAME: usize = 255;

/// 覗くポート (`PROXY_PEEK_SNI`)。`0` = 覗かない (`off`)。
///
/// 既定は [`TLS_PORT`] (= `on`)。`on:<port>` を指定すると **443 に加えて**その
/// ポートでも覗く (試験のオリジンを 443 に立てられないため。`cfg(test)` ではなく
/// 設定で持つのは、実バイナリでも同じ道を通すため)。起動時に 1 回書くだけ。
static PEEK_PORT: AtomicU32 = AtomicU32::new(TLS_PORT as u32);

/// 覗くポートを決める (起動時に 1 回だけ呼ぶ)。`None` で覗かない。
pub fn set_peek(port: Option<u16>) {
    PEEK_PORT.store(port.map_or(0, u32::from), Ordering::Relaxed);
}

/// このポートの CONNECT で覗くか (トンネル 1 本につき 1 回読む `Relaxed` 1 回)。
#[inline]
pub fn peek_on(port: u16) -> bool {
    let p = PEEK_PORT.load(Ordering::Relaxed);
    p != 0 && (port == TLS_PORT || u32::from(port) == p)
}

/// CONNECT の宛先 (`host:port`) と SNI が食い違うか (大小同一視、末尾の `.` は無視)。
///
/// IP リテラル宛て (T14.7 の `literal_targets`) は必ず食い違うので、
/// `/status` の `sni_mismatches` は「**宛先の名前が CONNECT から読めなかった本数**」
/// も含む (README の読み方に書いてある)。
pub fn differs(target: &str, sni: &str) -> bool {
    let (host, _) = crate::net::split_host_port_ref(target);
    let host = host.strip_suffix('.').unwrap_or(host);
    let sni = sni.strip_suffix('.').unwrap_or(sni);
    !host.eq_ignore_ascii_case(sni)
}

/// 先頭から順に読むだけの器 (足りなければ `None`)。
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn rest(&self) -> usize {
        self.0.len()
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u16(&mut self) -> Option<usize> {
        self.take(2).map(|b| (b[0] as usize) << 8 | b[1] as usize)
    }
}

/// ClientHello から SNI を取り出す (壊れていれば `None`)。
///
/// TLS record (type `0x16`) → handshake (type `0x01`) → extensions →
/// `server_name` (type `0x0000`) の `host_name` (type `0`)。
/// **覗いた 1,024 バイトに収まらなければ `None`** (途中で切れた長さは読めないため)。
pub fn parse_client_hello(buf: &[u8]) -> Option<&str> {
    let mut r = Reader(buf);
    // TLS record: type(1) + version(2) + length(2)
    if r.u8()? != 0x16 {
        return None;
    }
    r.skip(2)?;
    let len = r.u16()?;
    // 覗いた分で切れていても、読める所までは見る (足りなければこの先で `None`)
    let record = r.take(len.min(r.rest()))?;
    let mut h = Reader(record);
    // handshake: type(1) + length(3)。長さは record をまたぐことがあるので使わない
    if h.u8()? != 0x01 {
        return None;
    }
    h.skip(3)?;
    // client_version(2) + random(32)
    h.skip(34)?;
    let n = h.u8()? as usize;
    h.skip(n)?; // session_id
    let n = h.u16()?;
    h.skip(n)?; // cipher_suites
    let n = h.u8()? as usize;
    h.skip(n)?; // compression_methods
    let n = h.u16()?;
    // 拡張は**覗いた分まで**見る (`min`)。ClientHello が 1,024 バイトに収まらなくても、
    // `server_name` が前の方にあれば読める (後ろが切れているだけなら `take` で止まる)
    let mut ext = Reader(h.take(n.min(h.rest()))?);
    while ext.rest() >= 4 {
        let kind = ext.u16()?;
        let n = ext.u16()?;
        let data = ext.take(n)?;
        if kind != 0x0000 {
            continue;
        }
        // ServerNameList: length(2) + [ type(1) + length(2) + name ]...
        let mut list = Reader(data);
        let n = list.u16()?;
        // 名前そのものは**切れていたら受け取らない** (短い名前を作ってしまわないように)
        let mut list = Reader(list.take(n)?);
        while list.rest() >= 3 {
            let kind = list.u8()?;
            let n = list.u16()?;
            let name = list.take(n)?;
            if kind == 0 {
                return host_name(name);
            }
        }
        return None;
    }
    None
}

/// 名前として受け取ってよいバイト列か (DNS の名前は ASCII。IDN は punycode)。
///
/// 制御文字や引用符が個票や `/status` に混ざらないよう、ここで弾く。
fn host_name(raw: &[u8]) -> Option<&str> {
    if raw.is_empty() || raw.len() > MAX_NAME {
        return None;
    }
    if !raw
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        return None;
    }
    std::str::from_utf8(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手書きの ClientHello (`extensions` は渡された中身をそのまま入れる)。
    fn client_hello(extensions: &[u8]) -> Vec<u8> {
        let mut body = vec![0x03, 0x03]; // client_version
        body.extend_from_slice(&[0x11; 32]); // random
        body.push(0); // session_id なし
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // cipher_suites 1 つ
        body.extend_from_slice(&[0x01, 0x00]); // compression_methods
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(extensions);

        let mut hs = vec![0x01];
        hs.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        hs.extend_from_slice(&body);

        let mut rec = vec![0x16, 0x03, 0x01];
        rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        rec.extend_from_slice(&hs);
        rec
    }

    /// `server_name` 拡張 1 つ。
    fn server_name(name: &str) -> Vec<u8> {
        let mut entry = vec![0x00];
        entry.extend_from_slice(&(name.len() as u16).to_be_bytes());
        entry.extend_from_slice(name.as_bytes());
        let mut list = (entry.len() as u16).to_be_bytes().to_vec();
        list.extend_from_slice(&entry);
        let mut ext = vec![0x00, 0x00];
        ext.extend_from_slice(&(list.len() as u16).to_be_bytes());
        ext.extend_from_slice(&list);
        ext
    }

    #[test]
    fn reads_the_name_from_a_client_hello() {
        let hello = client_hello(&server_name("example.test"));
        assert_eq!(parse_client_hello(&hello), Some("example.test"));
    }

    #[test]
    fn reads_the_name_after_another_extension() {
        // 先に別の拡張 (supported_versions) を置いても読める
        let mut ext = vec![0x00, 0x2b, 0x00, 0x03, 0x02, 0x03, 0x04];
        ext.extend_from_slice(&server_name("a.example.test"));
        let hello = client_hello(&ext);
        assert_eq!(parse_client_hello(&hello), Some("a.example.test"));
    }

    #[test]
    fn a_broken_record_is_none() {
        let hello = client_hello(&server_name("example.test"));
        // record の種類が handshake ではない
        let mut other = hello.clone();
        other[0] = 0x17;
        assert_eq!(parse_client_hello(&other), None);
        // handshake の種類が ClientHello ではない
        let mut other = hello.clone();
        other[5] = 0x02;
        assert_eq!(parse_client_hello(&other), None);
        // 途中で切れている (覗いた 1,024 バイトに収まらなかったのと同じ形)
        assert_eq!(parse_client_hello(&hello[..hello.len() - 4]), None);
        assert_eq!(parse_client_hello(&[]), None);
        assert_eq!(parse_client_hello(b"GET / HTTP/1.1\r\n"), None);
        // 名前に制御文字が混ざっていたら受け取らない
        let bad = client_hello(&server_name("ex\u{1}ample"));
        assert_eq!(parse_client_hello(&bad), None);
    }

    /// 1,024 バイトに収まらない ClientHello でも、`server_name` が前の方にあれば読める。
    #[test]
    fn a_truncated_tail_still_yields_an_early_name() {
        let mut ext = server_name("early.test");
        // 後ろに大きな拡張 (key_share のつもり) を置き、覗いた 1,024 バイトで切る
        ext.extend_from_slice(&[0x00, 0x33]);
        ext.extend_from_slice(&(2000u16).to_be_bytes());
        ext.extend_from_slice(&vec![0x42; 2000]);
        let hello = client_hello(&ext);
        assert!(hello.len() > PEEK_LEN);
        assert_eq!(parse_client_hello(&hello[..PEEK_LEN]), Some("early.test"));
    }

    #[test]
    fn a_client_hello_without_sni_is_none() {
        assert_eq!(parse_client_hello(&client_hello(&[])), None);
        // server_name 以外の拡張だけ
        let hello = client_hello(&[0x00, 0x2b, 0x00, 0x03, 0x02, 0x03, 0x04]);
        assert_eq!(parse_client_hello(&hello), None);
        // server_name はあるが host_name (type 0) ではない
        let hello = client_hello(&[0x00, 0x00, 0x00, 0x06, 0x00, 0x04, 0x01, 0x00, 0x01, 0x41]);
        assert_eq!(parse_client_hello(&hello), None);
    }

    #[test]
    fn only_443_and_the_configured_port_are_peeked() {
        set_peek(Some(TLS_PORT));
        assert!(peek_on(443));
        assert!(!peek_on(9443));
        set_peek(Some(9443));
        assert!(peek_on(443), "443 は常に覗く");
        assert!(peek_on(9443));
        set_peek(None);
        assert!(!peek_on(443));
        set_peek(Some(TLS_PORT));
    }

    #[test]
    fn the_host_of_the_connect_is_compared_without_case() {
        assert!(!differs("example.test:443", "EXAMPLE.test"));
        assert!(!differs("example.test:443", "example.test."));
        assert!(differs("93.184.216.34:443", "example.test"));
        assert!(differs("other.test:443", "example.test"));
    }
}
