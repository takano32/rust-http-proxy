//! エラーの個票とリング (`recent` の一部。T25.2 で `recent.rs` から移した)。
//!
//! [`ErrorEntry`]・[`EntryKind`]・[`EntryCause`] と [`ErrorRing`]、それに
//! 切り詰めの [`clip`] を置く。**`recent` の他の塊はここを呼ぶだけ** (葉)。
//!
//! 行を動かしただけで中身は変えていない。外から見える名前は `recent` の
//! `pub use` でそのまま (T25.2)。

use std::sync::Mutex;

use crate::metrics::{BadRequestReason, BlockCause, DnsCause, ErrCause};
use crate::sync::LockExt;

/// エラーの個票を何件覚えておくか (固定)。
///
/// デプロイ先のエラーは 58.6 時間で 99 件 = 1.7 件/時 なので、500 件あれば
/// 10 日以上さかのぼれる。1 件は下の切り詰めで 300 B 以内に収まる
/// (T20.3 で `repeats` と `last_at` を足す前は 256 B 以内)。
pub const MAX_ERRORS: usize = 500;

/// 1 件に収める宛先の長さ (バイト)。
///
/// 宛先は `host:port` なので実際は 30〜60 B に収まる。長い名前で 1 件が太ると
/// 500 件ぶんの応答が 256 KiB を越えうるのでここで切る (切ったら末尾に `…`)。
pub const MAX_TARGET: usize = 80;

/// 1 件に収める接続元の長さ (バイト)。IPv6 の文字列表現は最長 45 文字。
pub const MAX_CLIENT: usize = 45;

/// 同じ失敗の繰り返しを探す範囲 (リングの**新しい方から**この件数。T20.3)。
///
/// 2026-10-08 のデプロイ先では、IPv6 だけの宛先への同じ 502 が約 140 秒おきに続いて
/// 500 件が全部その 1 種類で埋まり、バーストの `timeout` 8 件が押し出されて読めなかった。
/// 全件を探すと鍵の中の仕事が 500 件ぶんになるので、**直近の 8 件だけ**を見る
/// (間に別の失敗が 7 件まで挟まっても同じ行に足せる。書くのはエラーの経路だけ)。
pub const MERGE_WINDOW: usize = 8;

/// 記録しておく ms の上限 (7 桁 = 約 2.7 時間)。1 件の長さを決めるために頭打ちにする。
pub(super) const MAX_MS: u64 = 9_999_999;

/// 記録しておく秒数の上限 (8 桁 = 約 3.2 年)。寿命と預かり秒を頭打ちにする。
pub(super) const MAX_SECS: u64 = 99_999_999;

/// 個票 1 件の原因。
///
/// 5xx を返したエラーは [`ErrCause`] (`/status` の `errors_by_cause` と同じ 8 つ)、
/// 403 で拒否したものは [`BlockCause`] (T14.2 (4))、読めずに断った要求は
/// [`BadRequestReason`] (T14.28)。**403 と 400 は集計の配列には乗らない**
/// (乗せると `.rrd` の標本が領域に収まらず、版を上げて統計を捨てることになる)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryCause {
    Error(ErrCause),
    /// 名前解決の失敗の内訳 (T25.0)。`/errors` の `cause` は `nxdomain` / `dns_temp` に
    /// なり (`dns` の内訳なので、`dns` で数えたい読む側は 2 つを足す)、
    /// 集計の `errors_by_cause` では 9・10 番目に同じ数が入る
    ErrorDns(DnsCause),
    Blocked(BlockCause),
    /// 読めずに 400 / 414 / 431 で断った要求 (T14.28)。`cause` は `bad_request:<reason>`
    BadRequest(BadRequestReason),
}

impl EntryCause {
    /// `/errors` の `cause` に出す名前。
    pub fn name(self) -> &'static str {
        match self {
            EntryCause::Error(c) => c.name(),
            EntryCause::ErrorDns(c) => c.name(),
            EntryCause::Blocked(c) => c.name(),
            EntryCause::BadRequest(r) => r.cause_name(),
        }
    }

    /// ファイルに書くときの符号 (T14.9)。5xx は 0〜7、403 は 100〜103、
    /// 読めなかった要求 (400 / 414 / 431) は 200〜208 (T14.28 / T19.2 / T19.3 / T27.5)。
    pub fn code(self) -> u64 {
        match self {
            EntryCause::Error(c) => c as u64,
            // 名前解決の 2 つは 8 つの原因のすぐ後ろ (20・21)。**末尾に足す**ので
            // 0〜7 (5xx)・100〜103 (403)・200〜207 (400 番台) の符号は動かない
            EntryCause::ErrorDns(c) => 20 + c as u64,
            EntryCause::Blocked(c) => 100 + c as u64,
            EntryCause::BadRequest(r) => 200 + r as u64,
        }
    }

    /// 符号から戻す。知らない値は [`ErrCause::Other`]。
    pub fn from_code(v: u64) -> EntryCause {
        match v {
            0 => EntryCause::Error(ErrCause::Dns),
            1 => EntryCause::Error(ErrCause::Refused),
            2 => EntryCause::Error(ErrCause::Unreachable),
            3 => EntryCause::Error(ErrCause::Timeout),
            4 => EntryCause::Error(ErrCause::Reset),
            5 => EntryCause::Error(ErrCause::Tls),
            6 => EntryCause::Error(ErrCause::Loop),
            20 => EntryCause::ErrorDns(DnsCause::Nxdomain),
            21 => EntryCause::ErrorDns(DnsCause::DnsTemp),
            100 => EntryCause::Blocked(BlockCause::Acl),
            101 => EntryCause::Blocked(BlockCause::Blocklist),
            102 => EntryCause::Blocked(BlockCause::ConnectPort),
            103 => EntryCause::Blocked(BlockCause::Local),
            200..=208 => EntryCause::BadRequest(BadRequestReason::from_index((v - 200) as usize)),
            _ => EntryCause::Error(ErrCause::Other),
        }
    }
}

/// 個票 1 件の種類 (`/errors` の `kind`)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// 要求を中継する経路 (`GET http://…`)
    Forward,
    /// CONNECT トンネルの確立
    Connect,
    /// プロキシ自身の canary (利用者の要求ではない。T14.10)
    Canary,
}

impl EntryKind {
    /// `/errors` の `kind` に出す名前。
    pub fn name(self) -> &'static str {
        match self {
            EntryKind::Forward => "forward",
            EntryKind::Connect => "connect",
            EntryKind::Canary => "canary",
        }
    }

    /// 利用者の経路の旗 (`true` = CONNECT) から。
    pub fn from_connect(connect: bool) -> EntryKind {
        if connect {
            EntryKind::Connect
        } else {
            EntryKind::Forward
        }
    }

    /// ファイルに書くときの符号 (T14.9)。
    pub fn code(self) -> u64 {
        match self {
            EntryKind::Forward => 0,
            EntryKind::Connect => 1,
            EntryKind::Canary => 2,
        }
    }

    /// 符号から戻す。知らない値は [`EntryKind::Forward`]。
    pub fn from_code(v: u64) -> EntryKind {
        match v {
            1 => EntryKind::Connect,
            2 => EntryKind::Canary,
            _ => EntryKind::Forward,
        }
    }
}

/// エラー 1 件の個票。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorEntry {
    /// いつ (epoch 秒)
    pub at: u64,
    /// どの経路の 1 件か (forward / connect / canary)
    pub kind: EntryKind,
    /// 宛先 (`host:port`)
    pub target: String,
    /// 原因 (5xx は [`ErrCause`] = `/status` の `errors_by_cause` と同じ名前、
    /// 403 は [`BlockCause`] = `acl` / `blocklist` / `connect_port` / `local`)
    pub cause: EntryCause,
    /// 名前解決に費やした ms
    pub dns_ms: u64,
    /// 接続 (SYN → 確立) に費やした ms
    pub connect_ms: u64,
    /// クライアントへ返した状態コード
    pub status: u16,
    /// 接続元 IP
    pub client: String,
    /// 同じ失敗 (`kind`・`target`・`cause`・`status`・`client` が全部同じ) を何回
    /// この 1 行にまとめたか (T20.3。初めは 1)。**状態ファイルには書かない**ので、
    /// 再起動で 1 に戻る (通算は `/history` の `errors_by_cause` にある)
    pub repeats: u32,
    /// まとめた中で最後の 1 回の時刻 (epoch 秒。まとめていない行は `at` と同じ)。
    /// `dns_ms` / `connect_ms` もこの最後の 1 回の値
    pub last_at: u64,
}

impl ErrorEntry {
    /// 長さを切り詰めて 1 件を作る (時刻は今)。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: EntryKind,
        target: &str,
        client: &str,
        status: u16,
        cause: EntryCause,
        dns_ms: u64,
        connect_ms: u64,
    ) -> ErrorEntry {
        let at = crate::cache::now_epoch();
        ErrorEntry {
            at,
            kind,
            target: clip(target, MAX_TARGET),
            cause,
            dns_ms: dns_ms.min(MAX_MS),
            connect_ms: connect_ms.min(MAX_MS),
            status,
            // 接続元は**この 1 関数**を通して記録の形に直す (T14.41)。
            // 既定 (`on`) は借りたまま返るので確保も比較も増えない
            client: clip(&crate::records::client_key(client), MAX_CLIENT),
            repeats: 1,
            last_at: at,
        }
    }

    /// 同じ失敗か (T20.3)。比べるのは `kind`・`target`・`cause`・`status`・`client` の 5 つで、
    /// 時刻と ms は見ない。安い比較 (数と列挙) を先に置く。
    fn same_failure(&self, other: &ErrorEntry) -> bool {
        self.status == other.status
            && self.kind == other.kind
            && self.cause == other.cause
            && self.target == other.target
            && self.client == other.client
    }

    /// `/errors` の 1 要素。**`repeats` と `last_at` は末尾** (T20.3。足すだけ)。
    pub fn to_json(&self) -> String {
        format!(
            "{{\"at\":{},\"kind\":\"{}\",\"target\":\"{}\",\"cause\":\"{}\",\"dns_ms\":{},\"connect_ms\":{},\"status\":{},\"client\":\"{}\",\"repeats\":{},\"last_at\":{}}}",
            self.at,
            self.kind.name(),
            crate::json::escape(&self.target),
            self.cause.name(),
            self.dns_ms,
            self.connect_ms,
            self.status,
            crate::json::escape(&self.client),
            self.repeats,
            self.last_at,
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

/// リングの「まだファイルに書いていない件」を**古い順**で取り出す (T14.9)。
///
/// 印を位置ではなく**通算の件数** (`total`) にしてあるのは、リングが古いものを
/// 上書きするため: 位置で覚えると 1 周したときに「どこまで書いたか」が分からなくなる。
/// `max` を越える分は**古い方から落とす** (1 周期の書き込みを 64 KiB に抑えるため。
/// 落とした件数を 2 つ目に返す。メモリのリングには全部残っている)。
pub(super) fn drain_unwritten<T: Clone>(
    buf: &[T],
    next: usize,
    cap: usize,
    total: u64,
    written: &mut u64,
    max: usize,
) -> (Vec<T>, u64) {
    let len = buf.len();
    let pending = total.saturating_sub(*written).min(len as u64) as usize;
    *written = total;
    if pending == 0 {
        return (Vec::new(), 0);
    }
    let take = pending.min(max);
    // 満杯になる前は先頭が最古、満杯になってからは次に書く位置が最古
    let start = if len < cap { 0 } else { next };
    let out = ((len - take)..len)
        .map(|i| buf[(start + i) % len].clone())
        .collect();
    (out, (pending - take) as u64)
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
    /// 起動からの通算 (捨てた分も、同じ行にまとめた分も含む)
    total: u64,
    /// **行を足した回数**の通算 (T20.3。同じ行にまとめた 1 件は数えない)。
    /// ファイルに書くのは行ができたときの 1 回なので、書いた所の印はこちらで数える
    /// (`total` で数えると、まとめた回数ぶん同じ行を書き直してしまう)
    rows: u64,
    /// **ファイルに書いた所までの `rows`** (T14.9。`rows` との差が「まだ書いていない件」)
    written: u64,
    /// 起動時に状態ファイルから読み戻した件数 (`/errors` の `"restored"`)
    restored: usize,
}

impl Ring {
    /// 新しい方から [`MERGE_WINDOW`] 件の中に同じ失敗があれば、その行に足す (T20.3)。
    ///
    /// 足したら `true` (呼んだ側は行を足さない)。行の `at` と並びは動かさず、
    /// `repeats` を 1 増やして `last_at` と `dns_ms` / `connect_ms` を最後の 1 回の値にする。
    fn merge(&mut self, entry: &ErrorEntry) -> bool {
        let len = self.buf.len();
        // `next` の 1 つ手前が最新 (満杯になる前は末尾が最新)。[`ErrorRing::recent`] と同じ
        let start = if len < MAX_ERRORS { len } else { self.next };
        for i in 0..len.min(MERGE_WINDOW) {
            let row = &mut self.buf[(start + len - 1 - i) % len];
            if row.same_failure(entry) {
                row.repeats = row.repeats.saturating_add(1);
                row.last_at = entry.at;
                row.dns_ms = entry.dns_ms;
                row.connect_ms = entry.connect_ms;
                return true;
            }
        }
        false
    }

    /// 1 行足す (満杯なら最も古いものを上書きする)。
    fn add(&mut self, entry: ErrorEntry) {
        self.rows += 1;
        if self.buf.len() < MAX_ERRORS {
            self.buf.push(entry);
            return;
        }
        let at = self.next;
        self.buf[at] = entry;
        self.next = (at + 1) % MAX_ERRORS;
    }
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
    ///
    /// `PROXY_RECORDS=off` なら**鍵も取らずに捨てる** (T14.41)。
    ///
    /// **同じ失敗の繰り返しは 1 行にまとめる** (T20.3): 新しい方から [`MERGE_WINDOW`] 件の中に
    /// `kind`・`target`・`cause`・`status`・`client` が全部同じ行があれば、行を足さずに
    /// その行の `repeats` を増やす。通算 (`total`) はどちらでも 1 増える。
    pub fn push(&self, entry: ErrorEntry) {
        if !crate::records::recording() {
            return;
        }
        let mut r = self.inner.locked();
        r.total += 1;
        if r.merge(&entry) {
            return;
        }
        r.add(entry);
    }

    /// 直近 `n` 件を**新しい順** (行ができた順) で返す。2 つ目は起動からの通算
    /// (捨てた分も、同じ行にまとめた分も含む)。
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

    /// まだファイルに書いていない件を古い順で取り出す (**history スレッドだけが呼ぶ**。T14.9)。
    pub fn take_unwritten(&self, max: usize) -> (Vec<ErrorEntry>, u64) {
        let mut r = self.inner.locked();
        let g = &mut *r;
        // 印は**行の通算** (`rows`)。まとめた 1 件は行を足していないので書かない (T20.3)
        drain_unwritten(&g.buf, g.next, MAX_ERRORS, g.rows, &mut g.written, max)
    }

    /// 置き場を [`MAX_ERRORS`] 件ぶん確保して触る (**起動時に 1 回だけ**。T17.8)。
    ///
    /// 既定のプロファイルの Linux だけが呼ぶ (呼べば「1 件も出なければ 1 バイトも確保しない」
    /// は外れる)。触るのは 1 件の固定部だけで、文字列は書くときに確保する。返すのは触ったバイト数。
    pub fn prefault(&self) -> usize {
        crate::prefault::vec(&mut self.inner.locked().buf, MAX_ERRORS)
    }

    /// 状態ファイルから読み戻す (**起動時に 1 回だけ**。T14.9)。
    ///
    /// 読み戻した件は**書き直さない** (印を行の通算に合わせる) 。件数は `/errors` の
    /// `"restored"` に出す。**読み戻すときはまとめない** (T20.3。ファイルの 1 行 = 1 行で、
    /// `repeats` は 1 から。`PROXY_RECORDS=off` なら今までどおり何も入れない)。
    pub fn restore(&self, entries: Vec<ErrorEntry>) {
        let n = entries.len().min(MAX_ERRORS);
        let mut r = self.inner.locked();
        if crate::records::recording() {
            for e in entries {
                r.total += 1;
                r.add(e);
            }
        }
        r.written = r.rows;
        r.restored = n;
    }

    /// 再起動前から引き継いだ件数 (`/errors` の `"restored"`)。
    pub fn restored(&self) -> usize {
        self.inner.locked().restored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recent::{
        CloseReason, ConnTable, ConnTally, STAGE_CLIENT_READ, STAGE_CONNECT, STAGE_DNS,
        STAGE_FIRST_RELAY, STAGE_QUEUE, STAGES,
    };
    use std::time::Instant;

    fn entry(n: u64) -> ErrorEntry {
        ErrorEntry::new(
            EntryKind::Connect,
            &format!("h{}.example.net:443", n),
            "127.0.0.1",
            502,
            EntryCause::Error(ErrCause::Refused),
            0,
            1,
        )
    }

    /// T14.3 が足した 3 つの段階 (`queue` / `client_read` / `first_relay`) が個票に出ること。
    ///
    /// `dns` と `connect` は必ず出て、残りは 0 でないときだけ出る (0 の段階で行を伸ばさない)。
    #[test]
    fn the_three_stages_from_the_profile_show_up_in_the_record() {
        let t = ConnTable::new();
        let slot = t.register(3, "198.51.100.7", Instant::now()).expect("枠");
        slot.begin_tunnel("a.example.net:443");
        let mut tally = ConnTally::default();
        let mut ms = [0u64; STAGES];
        ms[STAGE_DNS] = 6;
        ms[STAGE_CONNECT] = 9;
        ms[STAGE_QUEUE] = 159;
        ms[STAGE_CLIENT_READ] = 2;
        ms[STAGE_FIRST_RELAY] = 37;
        tally.add_request(0, 4, 4, ms, 0);
        slot.finish(CloseReason::ClientEof, tally, 0);
        let json = t
            .unregister(3)
            .expect("枠が返る")
            .closed_entry(Instant::now())
            .expect("宛先のある接続は残る")
            .to_json();
        for (name, v) in [
            ("dns", 6),
            ("connect", 9),
            ("queue", 159),
            ("client_read", 2),
            ("first_relay", 37),
        ] {
            assert!(
                json.contains(&format!("\"{}\":{}", name, v)),
                "{} が出ていない: {}",
                name,
                json
            );
        }
        // 0 の段階 (`first_byte`) は出さない
        assert!(!json.contains("\"first_byte\""), "{}", json);
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

    /// 1 件は 300 B 以内 (500 件で 150 KB。256 KiB の上限に対して余裕がある)。
    ///
    /// T20.3 で `repeats` (最大 10 桁) と `last_at` (10 桁) を末尾に足したので、
    /// 上限を 256 → 300 B に上げた (鍵 2 つで 22 B + 値 20 B)。
    #[test]
    fn one_entry_fits_in_300_bytes() {
        let long = "a".repeat(300);
        let e = ErrorEntry::new(
            EntryKind::Forward,
            &long,
            "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6xx",
            502,
            EntryCause::Error(ErrCause::Unreachable),
            u64::MAX,
            u64::MAX,
        );
        assert!(e.target.len() <= MAX_TARGET, "{}", e.target.len());
        assert!(e.client.len() <= MAX_CLIENT, "{}", e.client.len());
        let mut e = e;
        e.repeats = u32::MAX;
        let json = e.to_json();
        assert!(json.len() <= 300, "1 件が {} B", json.len());
        assert!(json.contains("\"kind\":\"forward\""));
        assert!(json.contains("\"cause\":\"unreachable\""));
    }

    /// 同じ失敗 1 件ぶん (IPv6 だけの宛先への 502 の形。T20.3)。
    fn same(connect_ms: u64) -> ErrorEntry {
        ErrorEntry::new(
            EntryKind::Connect,
            "v6only.example.net:443",
            "198.51.100.7",
            502,
            EntryCause::Error(ErrCause::Unreachable),
            3,
            connect_ms,
        )
    }

    /// 同じ失敗を 1,000 回書いても 1 行で、`repeats` と通算が 1,000 (T20.3)。
    ///
    /// ファイルへ書くのも行ができたときの 1 回だけ (まとめた回は書かない)。
    #[test]
    fn a_thousand_repeats_of_one_failure_stay_in_one_row() {
        let ring = ErrorRing::new();
        ring.push(same(1));
        // 1 行目はここで書かれる。以後の 999 回は行を足さないので、書くものが無い
        assert_eq!(ring.take_unwritten(usize::MAX).0.len(), 1);
        for i in 1..1000u64 {
            let mut e = same(1000 + i);
            e.at += i;
            ring.push(e);
        }
        let (got, total) = ring.recent(MAX_ERRORS);
        assert_eq!(got.len(), 1);
        assert_eq!(ring.len(), 1);
        assert_eq!(total, 1000);
        assert_eq!(got[0].repeats, 1000);
        // 行の時刻は最初の 1 回、`last_at` と ms は最後の 1 回
        assert_eq!(got[0].last_at, got[0].at + 999);
        assert_eq!(got[0].connect_ms, 1999);
        assert_eq!(got[0].dns_ms, 3);
        let (unwritten, dropped) = ring.take_unwritten(usize::MAX);
        assert!(unwritten.is_empty(), "まとめた回を書き直している");
        assert_eq!(dropped, 0);
        // JSON は足すだけ: 今までの 8 つの鍵の後ろに `repeats` と `last_at`
        let json = got[0].to_json();
        assert!(
            json.ends_with(&format!(
                ",\"status\":502,\"client\":\"198.51.100.7\",\"repeats\":1000,\"last_at\":{}}}",
                got[0].last_at
            )),
            "{}",
            json
        );
        println!(
            "T20.3 同じ失敗 1,000 回: {} 行 / total {} / repeats {} / {} B",
            got.len(),
            total,
            got[0].repeats,
            json.len()
        );
    }

    /// まとめていない行は `repeats` 1・`last_at` は `at` と同じ (T20.3)。
    #[test]
    fn a_single_failure_reports_one_repeat_and_its_own_time() {
        let e = entry(1);
        assert_eq!(e.repeats, 1);
        assert_eq!(e.last_at, e.at);
        let json = e.to_json();
        assert!(
            json.ends_with(&format!(",\"repeats\":1,\"last_at\":{}}}", e.at)),
            "{}",
            json
        );
    }

    /// 間に別の失敗が入っても、新しい方から 8 件の中なら同じ行に足す (T20.3)。
    #[test]
    fn another_failure_in_between_does_not_split_the_row() {
        let ring = ErrorRing::new();
        ring.push(same(1));
        ring.push(entry(1));
        ring.push(same(2));
        let (got, total) = ring.recent(10);
        assert_eq!(total, 3);
        assert_eq!(got.len(), 2);
        // 並びは行ができた順の新しい順のまま (足された行が前に出てこない)
        assert_eq!(got[0].target, "h1.example.net:443");
        assert_eq!(got[0].repeats, 1);
        assert_eq!(got[1].target, "v6only.example.net:443");
        assert_eq!(got[1].repeats, 2);
        assert_eq!(got[1].connect_ms, 2);

        // 間に 7 件 (= 同じ失敗は新しい方から 8 件目) まではまとまる
        let ring = ErrorRing::new();
        ring.push(same(1));
        for i in 0..(MERGE_WINDOW as u64 - 1) {
            ring.push(entry(i));
        }
        ring.push(same(2));
        let (got, total) = ring.recent(MAX_ERRORS);
        assert_eq!(total, MERGE_WINDOW as u64 + 1);
        assert_eq!(got.len(), MERGE_WINDOW);
        assert_eq!(got[MERGE_WINDOW - 1].repeats, 2);
    }

    /// 9 件前の同じ失敗は別の行 (鍵の中で比べるのは最大 8 件。T20.3)。
    #[test]
    fn the_same_failure_nine_rows_back_starts_a_new_row() {
        let ring = ErrorRing::new();
        ring.push(same(1));
        for i in 0..(MERGE_WINDOW as u64) {
            ring.push(entry(i));
        }
        ring.push(same(2));
        let (got, total) = ring.recent(MAX_ERRORS);
        assert_eq!(total, MERGE_WINDOW as u64 + 2);
        assert_eq!(got.len(), MERGE_WINDOW + 2);
        assert_eq!(got[0].target, "v6only.example.net:443");
        assert_eq!(got[0].repeats, 1);
        assert_eq!(got[MERGE_WINDOW + 1].target, "v6only.example.net:443");
        assert_eq!(got[MERGE_WINDOW + 1].repeats, 1);
        // 次の 1 回は新しい方の行に足す
        ring.push(same(3));
        let (got, _) = ring.recent(MAX_ERRORS);
        assert_eq!(got.len(), MERGE_WINDOW + 2);
        assert_eq!(got[0].repeats, 2);
        assert_eq!(got[MERGE_WINDOW + 1].repeats, 1);
    }

    /// 5 つの鍵 (`kind`・`target`・`cause`・`status`・`client`) のどれかが違えば別の行 (T20.3)。
    #[test]
    fn a_different_client_cause_kind_status_or_target_is_another_row() {
        let ring = ErrorRing::new();
        let base = same(1);
        ring.push(base.clone());
        let mut other_client = base.clone();
        other_client.client = "198.51.100.8".to_string();
        let mut other_cause = base.clone();
        other_cause.cause = EntryCause::Error(ErrCause::Timeout);
        let mut other_kind = base.clone();
        other_kind.kind = EntryKind::Forward;
        let mut other_status = base.clone();
        other_status.status = 504;
        let mut other_target = base.clone();
        other_target.target = "v6only.example.net:8443".to_string();
        for e in [
            other_client,
            other_cause,
            other_kind,
            other_status,
            other_target,
        ] {
            ring.push(e);
        }
        let (got, total) = ring.recent(MAX_ERRORS);
        assert_eq!(total, 6);
        assert_eq!(got.len(), 6);
        assert!(got.iter().all(|e| e.repeats == 1));
        // ms と時刻だけが違うものは同じ行
        let mut later = base.clone();
        later.at += 140;
        later.dns_ms = 9;
        ring.push(later);
        let (got, total) = ring.recent(MAX_ERRORS);
        assert_eq!(total, 7);
        assert_eq!(got.len(), 6);
        assert_eq!(got[5].repeats, 2);
        assert_eq!(got[5].dns_ms, 9);
    }

    /// 満杯で一巡したあとも、新しい方から 8 件を正しく見ること (T20.3)。
    #[test]
    fn merging_still_finds_the_newest_rows_after_the_ring_wraps() {
        let ring = ErrorRing::new();
        for i in 0..(MAX_ERRORS as u64 + 3) {
            ring.push(entry(i));
        }
        // 最新 (h502) と、新しい方から 8 件目 (h495) に足す。9 件目 (h494) は別の行になる
        let newest = MAX_ERRORS as u64 + 2;
        ring.push(entry(newest));
        ring.push(entry(newest - 7));
        let (got, total) = ring.recent(MAX_ERRORS);
        assert_eq!(total, MAX_ERRORS as u64 + 5);
        assert_eq!(got[0].repeats, 2);
        assert_eq!(got[7].repeats, 2);
        ring.push(entry(newest - 8));
        let (got, _) = ring.recent(MAX_ERRORS);
        assert_eq!(got[0].target, format!("h{}.example.net:443", newest - 8));
        assert_eq!(got[0].repeats, 1);
        assert_eq!(got[9].repeats, 1);
    }

    /// 読み戻した行はまとめない (ファイルの 1 行 = 1 行、`repeats` 1。T20.3)。
    ///
    /// 読み戻したあとの同じ失敗は、今までどおり新しい方の行に足す。書き直しもしない。
    #[test]
    fn restored_rows_are_not_merged_and_not_rewritten() {
        let ring = ErrorRing::new();
        ring.restore(vec![same(1), same(2), same(3)]);
        let (got, total) = ring.recent(MAX_ERRORS);
        assert_eq!(got.len(), 3);
        assert_eq!(total, 3);
        assert_eq!(ring.restored(), 3);
        assert!(got.iter().all(|e| e.repeats == 1));
        assert!(ring.take_unwritten(usize::MAX).0.is_empty());
        ring.push(same(4));
        let (got, total) = ring.recent(MAX_ERRORS);
        assert_eq!(got.len(), 3);
        assert_eq!(total, 4);
        assert_eq!(got[0].repeats, 2);
        assert!(ring.take_unwritten(usize::MAX).0.is_empty());
        // 新しい行ができたら、その 1 行だけ書く
        ring.push(entry(1));
        let (unwritten, _) = ring.take_unwritten(usize::MAX);
        assert_eq!(unwritten.len(), 1);
        assert_eq!(unwritten[0].target, "h1.example.net:443");
    }

    /// `io::Error` から閉じた理由を決める (T25.0)。DNS だけ内訳を持つ。
    #[test]
    fn errors_closing_a_connection_carry_the_dns_split() {
        use std::io::{Error, ErrorKind};
        let nx = Error::other("failed to lookup address information: Name or service not known");
        let temp = Error::other(
            "failed to lookup address information: Temporary failure in name resolution",
        );
        assert_eq!(
            CloseReason::error_from_io(&nx),
            CloseReason::ErrorDns(DnsCause::Nxdomain)
        );
        assert_eq!(
            CloseReason::error_from_io(&temp),
            CloseReason::ErrorDns(DnsCause::DnsTemp)
        );
        // DNS 以外は今までどおり 8 つの原因の綴り
        assert_eq!(
            CloseReason::error_from_io(&Error::new(ErrorKind::ConnectionRefused, "x")),
            CloseReason::Error(ErrCause::Refused)
        );
        // `/errors` の `cause` も同じ 2 つ (符号 20・21 で往復する)
        for (cause, name) in [
            (EntryCause::ErrorDns(DnsCause::Nxdomain), "nxdomain"),
            (EntryCause::ErrorDns(DnsCause::DnsTemp), "dns_temp"),
        ] {
            assert_eq!(cause.name(), name);
            assert_eq!(EntryCause::from_code(cause.code()), cause);
            let e = ErrorEntry::new(
                EntryKind::Connect,
                "imap.example.jp:993",
                "198.51.100.7",
                502,
                cause,
                2,
                0,
            );
            let json = e.to_json();
            assert!(
                json.contains(&format!("\"cause\":\"{}\"", name)),
                "{}",
                json
            );
            assert!(json.len() <= 256, "1 件が {} B", json.len());
        }
    }

    /// 403 の個票は `/errors` に `acl` / `blocklist` の名前で出る (T14.2 (4))。
    ///
    /// 集計 (`errors_by_cause`) には乗らないので、[`ErrCause`] とは別の型で持つ。
    #[test]
    fn blocked_entries_carry_their_own_cause_names() {
        for (cause, name) in [
            (BlockCause::Acl, "acl"),
            (BlockCause::Blocklist, "blocklist"),
            (BlockCause::ConnectPort, "connect_port"),
            (BlockCause::Local, "local"),
        ] {
            let e = ErrorEntry::new(
                EntryKind::Connect,
                "ads.example.net:443",
                "198.51.100.7",
                403,
                EntryCause::Blocked(cause),
                0,
                0,
            );
            let json = e.to_json();
            assert!(
                json.contains(&format!("\"cause\":\"{}\"", name)),
                "{}",
                json
            );
            assert!(json.contains("\"status\":403"), "{}", json);
            assert!(json.contains("\"kind\":\"connect\""), "{}", json);
            assert!(json.len() <= 256, "1 件が {} B", json.len());
        }
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
