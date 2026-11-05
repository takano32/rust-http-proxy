//! エラーの個票とリング (`recent` の一部。T25.2 で `recent.rs` から移した)。
//!
//! [`ErrorEntry`]・[`EntryKind`]・[`EntryCause`] と [`ErrorRing`]。**`recent` の他の塊は
//! ここを呼ぶだけ** (葉)。切り詰めの `clip` は `recent/clip.rs`、リングの添字の式は
//! `recent/ringbuf.rs` に出した (T34.2。同じ式を 5 本のリングで書いていたのを 1 か所に)。
//!
//! 行を動かしただけで中身は変えていない。外から見える名前は `recent` の
//! `pub use` でそのまま (T25.2)。

use std::sync::Mutex;

use super::clip::clip;
use super::ringbuf::FixedRing;
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
pub(crate) const MERGE_WINDOW: usize = 8;

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
    /// 読めずに 400 / 408 / 414 / 431 で断った要求 (T14.28。408 は T29.10)。`cause` は `bad_request:<reason>`
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
    /// 読めなかった要求 (400 / 408 / 414 / 431) は 200〜209 (T14.28 / T19.2 / T19.3 / T27.5 / T29.10)。
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
            200..=209 => EntryCause::BadRequest(BadRequestReason::from_index((v - 200) as usize)),
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
        ErrorEntry::at(
            crate::cache::now_epoch(),
            kind,
            target,
            client,
            status,
            cause,
            dns_ms,
            connect_ms,
        )
    }

    /// [`ErrorEntry::new`] の本体 (T34.48)。時刻 `at` (epoch 秒) を受ける。
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub(crate) fn at(
        at: u64,
        kind: EntryKind,
        target: &str,
        client: &str,
        status: u16,
        cause: EntryCause,
        dns_ms: u64,
        connect_ms: u64,
    ) -> ErrorEntry {
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

/// 直近のエラーの固定長リング。**書くのはエラーの経路だけ。**
///
/// 置き場は使った分だけ伸び、[`MAX_ERRORS`] 件で頭打ち (そこからは古いものを上書き)。
/// エラーが 1 件も出ないプロセスでは 1 バイトも確保しない。
pub struct ErrorRing {
    inner: Mutex<Ring>,
}

#[derive(Default)]
struct Ring {
    /// 置き場と通算 (捨てた分も、同じ行にまとめた分も含む)・ファイルに書いた所 (T14.9。**`rows` で
    /// 数える**: `rows` との差が「まだ書いていない件」)・読み戻した件数 (`/errors` の `"restored"`)
    ring: FixedRing<ErrorEntry, MAX_ERRORS>,
    /// **行を足した回数**の通算 (T20.3。同じ行にまとめた 1 件は数えない)。
    /// ファイルに書くのは行ができたときの 1 回なので、書いた所の印はこちらで数える
    /// (`total` で数えると、まとめた回数ぶん同じ行を書き直してしまう)
    rows: u64,
}

impl Ring {
    /// 新しい方から [`MERGE_WINDOW`] 件の中に同じ失敗があれば、その行に足す (T20.3)。
    ///
    /// 足したら `true` (呼んだ側は行を足さない)。行の `at` と並びは動かさず、
    /// `repeats` を 1 増やして `last_at` と `dns_ms` / `connect_ms` を最後の 1 回の値にする。
    fn merge(&mut self, entry: &ErrorEntry) -> bool {
        let len = self.ring.len();
        // `next` の 1 つ手前が最新 (満杯になる前は末尾が最新)。[`ErrorRing::recent`] と同じ
        for i in 0..len.min(MERGE_WINDOW) {
            let row = self.ring.newest_mut(i);
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
        self.ring.write(entry);
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
        r.ring.count();
        if r.merge(&entry) {
            return;
        }
        r.add(entry);
    }

    /// 直近 `n` 件を**新しい順** (行ができた順) で返す。2 つ目は起動からの通算
    /// (捨てた分も、同じ行にまとめた分も含む)。
    pub fn recent(&self, n: usize) -> (Vec<ErrorEntry>, u64) {
        let r = self.inner.locked();
        let n = n.min(r.ring.len());
        let mut out = Vec::with_capacity(n);
        // `next` の 1 つ手前が最新 (満杯になる前は `next == 0` なので末尾が最新)
        for e in r.ring.newest().take(n) {
            out.push(e.clone());
        }
        (out, r.ring.total())
    }

    /// 覚えている件数。
    pub fn len(&self) -> usize {
        self.inner.locked().ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// まだファイルに書いていない件を古い順で取り出す (**history スレッドだけが呼ぶ**。T14.9)。
    pub fn take_unwritten(&self, max: usize) -> (Vec<ErrorEntry>, u64) {
        let mut r = self.inner.locked();
        // 印は**行の通算** (`rows`)。まとめた 1 件は行を足していないので書かない (T20.3)
        let mark = r.rows;
        r.ring.take_unwritten(mark, max)
    }

    /// 置き場を [`MAX_ERRORS`] 件ぶん確保して触る (**起動時に 1 回だけ**。T17.8)。
    ///
    /// 既定のプロファイルの Linux だけが呼ぶ (呼べば「1 件も出なければ 1 バイトも確保しない」
    /// は外れる)。触るのは 1 件の固定部だけで、文字列は書くときに確保する。返すのは触ったバイト数。
    pub fn prefault(&self) -> usize {
        self.inner.locked().ring.prefault()
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
                r.ring.count();
                r.add(e);
            }
        }
        let mark = r.rows;
        r.ring.finish_restore(mark, n);
    }

    /// 再起動前から引き継いだ件数 (`/errors` の `"restored"`)。
    pub fn restored(&self) -> usize {
        self.inner.locked().ring.restored()
    }
}

/// エラーの個票とリングの単体試験 (T34.2 で別のファイルに移した)。
#[cfg(test)]
#[path = "errors_tests.rs"]
mod tests;
