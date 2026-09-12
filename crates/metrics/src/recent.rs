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
//! - [`ConnTable`]: いまの接続の一覧 (`/connections`)。**表の鍵を取るのは
//!   接続の開始と終了の 2 回だけ**で、状態と転送バイトは [`ConnSlot`] の原子に書く。
//!
//! [`Metrics::record_error`]: crate::metrics::Metrics::record_error

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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

/// 接続 1 本の状態 (`/connections` の `state`)。
///
/// 更新するのは**要求ごとではない場所**だけ: 接続を受けたとき、トンネルを開いたとき、
/// 預けたとき、起こしてワーカーに渡すとき、ワーカーが取ったとき。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ConnState {
    /// ワーカースレッドが持っていて、要求を処理している
    Serving = 0,
    /// ワーカースレッドが持ったまま次の要求を待っている (預けられなかった接続)
    Reading = 1,
    /// 預かり所 (epoll) にいる。スレッドは握っていない
    Parked = 2,
    /// 起こされてワーカーの空きを待っている
    Queued = 3,
    /// CONNECT トンネルとして中継中
    Relaying = 4,
}

impl ConnState {
    pub fn name(self) -> &'static str {
        match self {
            ConnState::Serving => "serving",
            ConnState::Reading => "reading",
            ConnState::Parked => "parked",
            ConnState::Queued => "queued",
            ConnState::Relaying => "relaying",
        }
    }

    fn from_u8(v: u8) -> ConnState {
        match v {
            1 => ConnState::Reading,
            2 => ConnState::Parked,
            3 => ConnState::Queued,
            4 => ConnState::Relaying,
            _ => ConnState::Serving,
        }
    }
}

/// 接続 1 本の今の姿。[`ConnTable`] と接続自身が同じものを [`Arc`] で持つ。
///
/// **状態と転送バイトは原子で書く。** 表の `HashMap` の鍵を取るのは登録と抹消の
/// 2 回だけで、預ける / 戻す / 中継を始める のどれも表を止めない。バーストで 240 本が
/// 一斉に預け直す場面こそ `/connections` で見たい場面なので、**見るための仕掛けが
/// そこに鍵を 1 つ増やしてはいけない**。
pub struct ConnSlot {
    /// 接続の通し番号 (ログの `conn#N` と同じ)
    pub id: u64,
    /// 接続元 IP (接続ごとに 1 回だけ作った文字列の複製)
    pub client: String,
    /// 受けた時刻 (`age_secs` を出すため)
    pub started: Instant,
    state: AtomicU8,
    /// CONNECT の宛先。**書くのは 1 本につき 1 回だけ** (トンネルを開いたとき)。
    /// keep-alive の HTTP 接続は要求ごとに宛先が変わるので空のまま (要求ごとに触らない)
    target: Mutex<String>,
    /// CONNECT トンネルか (`false` = keep-alive の HTTP)
    connect: AtomicBool,
    /// 運んだ合計バイト数 (トンネルが暇になるたびに書く)
    bytes: AtomicU64,
}

impl ConnSlot {
    fn new(id: u64, client: &str, started: Instant) -> ConnSlot {
        ConnSlot {
            id,
            client: clip(client, MAX_CLIENT),
            started,
            state: AtomicU8::new(ConnState::Serving as u8),
            target: Mutex::new(String::new()),
            connect: AtomicBool::new(false),
            bytes: AtomicU64::new(0),
        }
    }

    /// 状態を書く (原子 1 回。表の鍵は取らない)。
    pub fn set_state(&self, state: ConnState) {
        self.state.store(state as u8, Ordering::Relaxed);
    }

    pub fn state(&self) -> ConnState {
        ConnState::from_u8(self.state.load(Ordering::Relaxed))
    }

    /// CONNECT トンネルになった (宛先が決まった)。**1 本につき 1 回だけ呼ぶ。**
    pub fn begin_tunnel(&self, target: &str) {
        *self.target.locked() = clip(target, MAX_TARGET);
        self.connect.store(true, Ordering::Relaxed);
        self.set_state(ConnState::Relaying);
    }

    /// 運んだ合計バイト数を書く (中継が止まるところで 1 回。バイトごとには書かない)。
    pub fn set_bytes(&self, bytes: u64) {
        self.bytes.store(bytes, Ordering::Relaxed);
    }

    pub fn is_connect(&self) -> bool {
        self.connect.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// `/connections` の 1 要素。
    pub fn to_json(&self, now: Instant) -> String {
        let connect = self.is_connect();
        format!(
            "{{\"id\":{},\"client\":\"{}\",\"target\":\"{}\",\"kind\":\"{}\",\"state\":\"{}\",\"age_secs\":{},\"bytes\":{},\"fds\":{}}}",
            self.id,
            crate::json::escape(&self.client),
            crate::json::escape(&self.target.locked()),
            if connect { "connect" } else { "http" },
            self.state().name(),
            now.saturating_duration_since(self.started).as_secs(),
            self.bytes(),
            // トンネルはクライアントとオリジンの 2 本、keep-alive はクライアントの 1 本
            if connect { 2 } else { 1 },
        )
    }
}

/// いま開いている接続の表 (`/connections`)。
///
/// **登録と抹消は接続の開始と終了で 1 回ずつだけ** (要求ごとには触らない)。
/// `--lite` では [`ConnTable::set_enabled`] で切り、登録もしない (空の一覧を返す。T1.4 の方針)。
pub struct ConnTable {
    on: AtomicBool,
    map: Mutex<HashMap<u64, Arc<ConnSlot>>>,
}

impl Default for ConnTable {
    fn default() -> Self {
        ConnTable::new()
    }
}

impl ConnTable {
    pub fn new() -> ConnTable {
        ConnTable {
            on: AtomicBool::new(true),
            map: Mutex::new(HashMap::new()),
        }
    }

    /// 記録するかどうか (`--lite` では `false`)。起動時に 1 回だけ呼ぶ。
    pub fn set_enabled(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
        if !on {
            self.map.locked().clear();
        }
    }

    pub fn enabled(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// 接続を 1 本登録する (接続の開始で 1 回だけ)。`--lite` なら `None`。
    pub fn register(&self, id: u64, client: &str, started: Instant) -> Option<Arc<ConnSlot>> {
        if !self.enabled() {
            return None;
        }
        let slot = Arc::new(ConnSlot::new(id, client, started));
        self.map.locked().insert(id, Arc::clone(&slot));
        Some(slot)
    }

    /// 接続を 1 本抹消する (接続の終了で 1 回だけ)。
    pub fn unregister(&self, id: u64) {
        if !self.enabled() {
            return;
        }
        self.map.locked().remove(&id);
    }

    /// 今の一覧を**古い順** (通し番号の小さい順) で返す。
    pub fn snapshot(&self) -> Vec<Arc<ConnSlot>> {
        let mut v: Vec<Arc<ConnSlot>> = self.map.locked().values().map(Arc::clone).collect();
        v.sort_by_key(|s| s.id);
        v
    }

    pub fn len(&self) -> usize {
        self.map.locked().len()
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

#[cfg(test)]
mod conn_tests {
    use super::*;

    #[test]
    fn the_table_registers_and_unregisters_once() {
        let t = ConnTable::new();
        assert!(t.is_empty());
        let now = Instant::now();
        let a = t.register(1, "127.0.0.1", now).expect("登録できる");
        let _b = t.register(2, "::1", now).expect("登録できる");
        assert_eq!(t.len(), 2);
        // 古い順 (通し番号の順)
        let snap = t.snapshot();
        assert_eq!(snap[0].id, 1);
        assert_eq!(snap[1].id, 2);
        assert_eq!(snap[0].state(), ConnState::Serving);
        assert!(!snap[0].is_connect());

        // 状態と宛先は表の鍵を取らずに書ける
        a.begin_tunnel("example.com:443");
        a.set_bytes(4096);
        assert_eq!(t.snapshot()[0].state(), ConnState::Relaying);
        assert!(t.snapshot()[0].is_connect());
        let json = t.snapshot()[0].to_json(Instant::now());
        assert!(json.contains("\"kind\":\"connect\""), "{}", json);
        assert!(json.contains("\"state\":\"relaying\""), "{}", json);
        assert!(json.contains("\"target\":\"example.com:443\""), "{}", json);
        assert!(json.contains("\"bytes\":4096"), "{}", json);
        assert!(json.contains("\"fds\":2"), "{}", json);

        a.set_state(ConnState::Parked);
        assert_eq!(t.snapshot()[0].state(), ConnState::Parked);
        t.unregister(1);
        assert_eq!(t.len(), 1);
        t.unregister(1);
        assert_eq!(t.len(), 1, "2 回抹消しても増減しない");
    }

    /// `--lite` では登録しない (空の一覧)。
    #[test]
    fn lite_registers_nothing() {
        let t = ConnTable::new();
        t.set_enabled(false);
        assert!(t.register(1, "127.0.0.1", Instant::now()).is_none());
        assert!(t.is_empty());
        assert!(t.snapshot().is_empty());
        assert!(!t.enabled());
    }

    /// keep-alive の HTTP 接続は記述子 1 本・宛先なし。
    #[test]
    fn a_keepalive_connection_shows_one_descriptor() {
        let t = ConnTable::new();
        let slot = t.register(7, "10.0.0.1", Instant::now()).unwrap();
        slot.set_state(ConnState::Reading);
        let json = slot.to_json(Instant::now());
        assert!(json.contains("\"kind\":\"http\""), "{}", json);
        assert!(json.contains("\"state\":\"reading\""), "{}", json);
        assert!(json.contains("\"target\":\"\""), "{}", json);
        assert!(json.contains("\"fds\":1"), "{}", json);
        assert!(json.contains("\"id\":7"), "{}", json);
    }
}
