//! プロセスのメモリだけに持つ「個票」(T13.4)。
//!
//! `/status` は集計、`/history` は時系列で、どちらも「**誰が・いつ・なぜ**」を答えられない。
//! デプロイ後 58.6 時間の分析では、約 2 秒かかって失敗した名前解決の相手も、バーストで
//! 218 本を占めていた接続の中身も、エラー 99 件の相手と時刻も読めなかった (T13.0)。
//!
//! ここに置くのは**固定長のリング**で、`.rrd` には書かない (再起動で消えてよい個票)。
//! 書く場所は 2 つだけ:
//!
//! - [`ErrorRing`]: エラーを 1 件返したときだけ ([`Metrics::record_error`] 経由)。
//!   **成功の熱い経路は 1 命令も通らない。**
//! - 接続の一覧は [`crate::metrics::Metrics`] の側にあり、登録と抹消は接続の開始と
//!   終了で 1 回ずつ (T13.4 (2) で足す)。
//!
//! [`Metrics::record_error`]: crate::metrics::Metrics::record_error

use std::sync::Mutex;

use crate::metrics::ErrCause;
use crate::sync::LockExt;

/// エラーの個票を何件覚えておくか (固定)。
///
/// デプロイ先のエラーは 58.6 時間で 99 件 = 1.7 件/時 なので、500 件あれば
/// 10 日以上さかのぼれる。1 件は下の切り詰めで 256 B 以内に収まる。
pub const MAX_ERRORS: usize = 500;

/// 1 件に収める宛先の長さ (バイト)。
///
/// 宛先は `host:port` なので実際は 30〜60 B に収まる。長い名前で 1 件が太ると
/// 500 件ぶんの応答が 256 KiB を越えうるのでここで切る (切ったら末尾に `…`)。
pub const MAX_TARGET: usize = 80;

/// 1 件に収める接続元の長さ (バイト)。IPv6 の文字列表現は最長 45 文字。
pub const MAX_CLIENT: usize = 45;

/// 記録しておく ms の上限 (7 桁 = 約 2.7 時間)。1 件の長さを決めるために頭打ちにする。
const MAX_MS: u64 = 9_999_999;

/// エラー 1 件の個票。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorEntry {
    /// いつ (epoch 秒)
    pub at: u64,
    /// CONNECT の確立で失敗したか (`false` = forward)
    pub connect: bool,
    /// 宛先 (`host:port`)
    pub target: String,
    /// 原因 ([`ErrCause`]。`/status` の `errors_by_cause` と同じ名前で出す)
    pub cause: ErrCause,
    /// 名前解決に費やした ms
    pub dns_ms: u64,
    /// 接続 (SYN → 確立) に費やした ms
    pub connect_ms: u64,
    /// クライアントへ返した状態コード
    pub status: u16,
    /// 接続元 IP
    pub client: String,
}

impl ErrorEntry {
    /// 長さを切り詰めて 1 件を作る (時刻は今)。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        connect: bool,
        target: &str,
        client: &str,
        status: u16,
        cause: ErrCause,
        dns_ms: u64,
        connect_ms: u64,
    ) -> ErrorEntry {
        ErrorEntry {
            at: crate::cache::now_epoch(),
            connect,
            target: clip(target, MAX_TARGET),
            cause,
            dns_ms: dns_ms.min(MAX_MS),
            connect_ms: connect_ms.min(MAX_MS),
            status,
            client: clip(client, MAX_CLIENT),
        }
    }

    /// `/errors` の 1 要素。
    pub fn to_json(&self) -> String {
        format!(
            "{{\"at\":{},\"kind\":\"{}\",\"target\":\"{}\",\"cause\":\"{}\",\"dns_ms\":{},\"connect_ms\":{},\"status\":{},\"client\":\"{}\"}}",
            self.at,
            if self.connect { "connect" } else { "forward" },
            crate::json::escape(&self.target),
            self.cause.name(),
            self.dns_ms,
            self.connect_ms,
            self.status,
            crate::json::escape(&self.client),
        )
    }
}

/// 文字列を `max` バイト以内に切る (文字の途中で切らない。切ったら `…` を付ける)。
pub fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // `…` は UTF-8 で 3 バイトなので、その手前までに収める
    let room = max.saturating_sub(3);
    let mut end = room;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(max);
    out.push_str(&s[..end]);
    out.push('…');
    out
}

/// 直近のエラーの固定長リング。**書くのはエラーの経路だけ。**
///
/// 置き場は使った分だけ伸び、[`MAX_ERRORS`] 件で頭打ち (そこからは古いものを上書き)。
/// エラーが 1 件も出ないプロセスでは 1 バイトも確保しない。
pub struct ErrorRing {
    inner: Mutex<Ring>,
}

#[derive(Default)]
struct Ring {
    buf: Vec<ErrorEntry>,
    /// 次に書く位置 (`buf` が満杯になってからだけ意味を持つ)
    next: usize,
    /// 起動からの通算 (捨てた分も含む)
    total: u64,
}

impl Default for ErrorRing {
    fn default() -> Self {
        ErrorRing::new()
    }
}

impl ErrorRing {
    pub fn new() -> ErrorRing {
        ErrorRing {
            inner: Mutex::new(Ring::default()),
        }
    }

    /// 1 件書く (満杯なら最も古いものを上書きする)。
    pub fn push(&self, entry: ErrorEntry) {
        let mut r = self.inner.locked();
        r.total += 1;
        if r.buf.len() < MAX_ERRORS {
            r.buf.push(entry);
            return;
        }
        let at = r.next;
        r.buf[at] = entry;
        r.next = (at + 1) % MAX_ERRORS;
    }

    /// 直近 `n` 件を**新しい順**で返す。2 つ目は起動からの通算 (捨てた分も含む)。
    pub fn recent(&self, n: usize) -> (Vec<ErrorEntry>, u64) {
        let r = self.inner.locked();
        let len = r.buf.len();
        let n = n.min(len);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            // `next` の 1 つ手前が最新 (満杯になる前は `next == 0` なので末尾が最新)
            let start = if len < MAX_ERRORS { len } else { r.next };
            let at = (start + len - 1 - i) % len;
            out.push(r.buf[at].clone());
        }
        (out, r.total)
    }

    /// 覚えている件数。
    pub fn len(&self) -> usize {
        self.inner.locked().buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: u64) -> ErrorEntry {
        ErrorEntry::new(
            true,
            &format!("h{}.example.net:443", n),
            "127.0.0.1",
            502,
            ErrCause::Refused,
            0,
            1,
        )
    }

    #[test]
    fn the_ring_keeps_the_newest_entries_first() {
        let ring = ErrorRing::new();
        assert!(ring.is_empty());
        for i in 0..3 {
            ring.push(entry(i));
        }
        let (got, total) = ring.recent(10);
        assert_eq!(total, 3);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].target, "h2.example.net:443");
        assert_eq!(got[2].target, "h0.example.net:443");
        assert_eq!(ring.recent(2).0.len(), 2);
    }

    #[test]
    fn the_ring_overwrites_the_oldest_when_full() {
        let ring = ErrorRing::new();
        for i in 0..(MAX_ERRORS as u64 + 7) {
            ring.push(entry(i));
        }
        let (got, total) = ring.recent(MAX_ERRORS);
        assert_eq!(total, MAX_ERRORS as u64 + 7);
        assert_eq!(got.len(), MAX_ERRORS);
        // 最新は最後に書いたもの、最古は 500 件前
        assert_eq!(
            got[0].target,
            format!("h{}.example.net:443", MAX_ERRORS as u64 + 6)
        );
        assert_eq!(got[MAX_ERRORS - 1].target, "h7.example.net:443");
    }

    /// 1 件は 256 B 以内 (500 件で 256 KiB の上限に対して 2 倍の余裕がある)。
    #[test]
    fn one_entry_fits_in_256_bytes() {
        let long = "a".repeat(300);
        let e = ErrorEntry::new(
            false,
            &long,
            "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6xx",
            502,
            ErrCause::Unreachable,
            u64::MAX,
            u64::MAX,
        );
        assert!(e.target.len() <= MAX_TARGET, "{}", e.target.len());
        assert!(e.client.len() <= MAX_CLIENT, "{}", e.client.len());
        let json = e.to_json();
        assert!(json.len() <= 256, "1 件が {} B", json.len());
        assert!(json.contains("\"kind\":\"forward\""));
        assert!(json.contains("\"cause\":\"unreachable\""));
    }

    #[test]
    fn clip_does_not_split_a_character() {
        assert_eq!(clip("abc", 8), "abc");
        assert_eq!(clip("abcdefghij", 8), "abcde…");
        // 3 バイト文字の途中で切らない
        assert_eq!(clip("日本語です", 8), "日…");
        assert!(clip("日本語です", 8).len() <= 8);
    }
}
