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
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::metrics::{BlockCause, ErrCause};
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

/// 記録しておく秒数の上限 (8 桁 = 約 3.2 年)。寿命と預かり秒を頭打ちにする。
const MAX_SECS: u64 = 99_999_999;

/// 個票 1 件の原因。
///
/// 5xx を返したエラーは [`ErrCause`] (`/status` の `errors_by_cause` と同じ 8 つ)、
/// 403 で拒否したものは [`BlockCause`] (T14.2 (4))。**403 は集計の配列には乗らない**
/// (乗せると `.rrd` の標本が領域に収まらず、版を上げて統計を捨てることになる)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryCause {
    Error(ErrCause),
    Blocked(BlockCause),
}

impl EntryCause {
    /// `/errors` の `cause` に出す名前。
    pub fn name(self) -> &'static str {
        match self {
            EntryCause::Error(c) => c.name(),
            EntryCause::Blocked(c) => c.name(),
        }
    }
}

/// エラー 1 件の個票。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorEntry {
    /// いつ (epoch 秒)
    pub at: u64,
    /// CONNECT の確立で失敗したか (`false` = forward)
    pub connect: bool,
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
}

impl ErrorEntry {
    /// 長さを切り詰めて 1 件を作る (時刻は今)。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        connect: bool,
        target: &str,
        client: &str,
        status: u16,
        cause: EntryCause,
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
    /// 宛先。**書くのは 1 本につき 1 回だけ**: CONNECT はトンネルを開いたとき
    /// ([`ConnSlot::begin_tunnel`])、keep-alive の HTTP 接続は**最初の要求**のとき
    /// ([`ConnSlot::set_first_target`]。T14.2 (5))。要求ごとには触らない
    target: Mutex<String>,
    /// CONNECT トンネルか (`false` = keep-alive の HTTP)
    connect: AtomicBool,
    /// 運んだ合計バイト数 (トンネルが暇になるたびに書く)
    bytes: AtomicU64,
    /// 閉じた理由の符号 (0 = まだ決まっていない)。**先に書いた方が勝つ**
    /// (T13.2 の追い出しや監視スレッドの停止は、接続自身が理由を決める前に書くため)
    close: AtomicU16,
    /// 接続の終了で 1 回だけ書く値 (上り / 下り / 状態コード / 要求数 / 段階の ms)。
    /// 要求ごとの積み上げは接続を持っているスレッドの箱 ([`ConnTally`]) で行う
    up: AtomicU64,
    down: AtomicU64,
    status: AtomicU16,
    requests: AtomicU32,
    stage_ms: [AtomicU32; STAGES],
    /// 預けられた回数と、預かり所にいた合計 ms、いま預けられた時刻
    /// (接続を受けてからの ms。[`NOT_PARKED`] なら預けられていない)。
    /// **書くのは預ける / 引き上げる瞬間だけ**で、要求ごとには触らない
    parks: AtomicU32,
    parked_ms: AtomicU64,
    parked_at: AtomicU64,
}

/// [`ConnSlot::parked_at`] の「預けられていない」印。
const NOT_PARKED: u64 = u64::MAX;

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
            close: AtomicU16::new(0),
            up: AtomicU64::new(0),
            down: AtomicU64::new(0),
            status: AtomicU16::new(0),
            requests: AtomicU32::new(0),
            stage_ms: [const { AtomicU32::new(0) }; STAGES],
            parks: AtomicU32::new(0),
            parked_ms: AtomicU64::new(0),
            parked_at: AtomicU64::new(NOT_PARKED),
        }
    }

    /// 状態を書く (原子 1 回。表の鍵は取らない)。
    pub fn set_state(&self, state: ConnState) {
        self.state.store(state as u8, Ordering::Relaxed);
    }

    pub fn state(&self) -> ConnState {
        ConnState::from_u8(self.state.load(Ordering::Relaxed))
    }

    /// keep-alive の HTTP 接続の**最初の要求の宛先**を書く (T14.2 (5))。
    ///
    /// T13.4 では「要求ごとに宛先が変わるので空のまま」にしていたが、`/connections` で
    /// 占有の内訳を読むとき **`http` の行だけ宛先が空** だと何に使われている接続か分からない。
    /// 要求ごとには書かない方針はそのままで、**接続の最初の 1 回だけ**書く
    /// (表の鍵は取らない。取るのはこの枠の `target` の鍵 1 つで、接続あたり 1 回)。
    /// 既に何か入っていれば触らない (CONNECT の宛先を上書きしない)。
    pub fn set_first_target(&self, target: &str) {
        let mut t = self.target.locked();
        if t.is_empty() {
            *t = clip(target, MAX_TARGET);
        }
    }

    /// CONNECT がつなげなかったときに宛先だけ書く (**エラーの経路だけ**。T14.4)。
    ///
    /// 502 で終わった CONNECT も `/recent` に「誰が・どこへ・何 ms で失敗したか」を
    /// 残すため。成功した経路は [`ConnSlot::begin_tunnel`] が同じ場所に書く。
    pub fn failed_tunnel(&self, target: &str) {
        *self.target.locked() = clip(target, MAX_TARGET);
        self.connect.store(true, Ordering::Relaxed);
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

    /// 閉じた理由を書く (**先に書いた方が勝つ**。T14.4)。
    ///
    /// T13.2 の追い出しや監視スレッドの停止は、接続自身が「なぜ終わったか」を決める前に
    /// 外から閉じる。そこで書いた理由の方が具体的なので、あとから来る既定の理由で
    /// 上書きしない (`set_first_target` と同じ方針)。
    pub fn set_close(&self, reason: CloseReason) {
        let _ = self
            .close
            .compare_exchange(0, reason.code(), Ordering::Relaxed, Ordering::Relaxed);
    }

    /// 接続の終わりに、個票に出す値をまとめて書く (**接続の終了で 1 回だけ**。T14.4)。
    ///
    /// 要求ごとの積み上げは接続を持っているスレッドの箱 ([`ConnTally`]) で行い、
    /// ここで初めて原子に移す。理由は [`ConnSlot::set_close`] と同じく先着優先。
    pub fn finish(&self, reason: CloseReason, tally: ConnTally, requests: u32) {
        self.set_close(reason);
        self.up.store(tally.up, Ordering::Relaxed);
        self.down.store(tally.down, Ordering::Relaxed);
        self.status.store(tally.status, Ordering::Relaxed);
        self.requests.store(requests, Ordering::Relaxed);
        for (cell, ms) in self.stage_ms.iter().zip(tally.stage_ms) {
            cell.store(ms.min(MAX_MS) as u32, Ordering::Relaxed);
        }
    }

    /// 預かり所に入った (原子 2 回。**預ける瞬間だけ**で、要求ごとには触らない)。
    pub fn on_park(&self) {
        self.parks.fetch_add(1, Ordering::Relaxed);
        self.parked_at.store(
            self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }

    /// 預かり所から出た (起こされた / 期限切れ / 追い出された)。預かっていた時間を足す。
    pub fn on_unpark(&self) {
        let at = self.parked_at.swap(NOT_PARKED, Ordering::Relaxed);
        if at == NOT_PARKED {
            return;
        }
        let now = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.parked_ms
            .fetch_add(now.saturating_sub(at), Ordering::Relaxed);
    }

    /// 閉じた接続の個票を 1 件作る (`ConnTable` から外すときに 1 回だけ)。
    ///
    /// **自分宛て (`/status` `/dashboard` …) だけで終わった接続は `None`**: 宛先を 1 つも
    /// 選ばず要求も 1 本も代理していない接続で、監視が 5 秒おきに引くとリングが
    /// それだけで埋まってしまう (2,000 件 = 2.8 時間ぶん)。数は `/status` にあるので、
    /// 個票としては残さない。
    pub fn closed_entry(&self, now: Instant) -> Option<RecentEntry> {
        let target = clip(&self.target.locked(), MAX_RECENT_TARGET);
        let requests = self.requests.load(Ordering::Relaxed);
        if target.is_empty() && requests == 0 {
            return None;
        }
        let age = now.saturating_duration_since(self.started);
        // 預けられたまま閉じた接続は、最後の 1 区間もここで足す (`on_unpark` を通らない)
        let mut parked_ms = self.parked_ms.load(Ordering::Relaxed);
        let at = self.parked_at.load(Ordering::Relaxed);
        if at != NOT_PARKED {
            parked_ms = parked_ms
                .saturating_add((age.as_millis().min(u64::MAX as u128) as u64).saturating_sub(at));
        }
        let mut stage_ms = [0u64; STAGES];
        for (out, cell) in stage_ms.iter_mut().zip(self.stage_ms.iter()) {
            *out = cell.load(Ordering::Relaxed) as u64;
        }
        Some(RecentEntry {
            id: self.id,
            // 開いた時刻は「いま − 寿命」で出す (接続を受けるときに時計を読まない)
            at: crate::cache::now_epoch().saturating_sub(age.as_secs()),
            client: self.client.clone(),
            target,
            connect: self.is_connect(),
            secs: age.as_secs().min(MAX_SECS),
            requests,
            up: self.up.load(Ordering::Relaxed),
            down: self.down.load(Ordering::Relaxed),
            reason: CloseReason::from_code(self.close.load(Ordering::Relaxed)),
            status: self.status.load(Ordering::Relaxed),
            parked_secs: (parked_ms / 1000).min(MAX_SECS),
            parks: self.parks.load(Ordering::Relaxed),
            stage_ms,
        })
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

    /// 接続を 1 本抹消し、その枠を返す (接続の終了で 1 回だけ)。
    ///
    /// 返した枠から閉じた接続の個票を作る (`/recent`。T14.4)。`--lite` と、
    /// 既に抹消済み (2 回目) では `None`。
    pub fn unregister(&self, id: u64) -> Option<Arc<ConnSlot>> {
        if !self.enabled() {
            return None;
        }
        self.map.locked().remove(&id)
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

/// 閉じた接続の個票を何件覚えておくか (固定。T14.4)。
///
/// デプロイ先は 72.7 時間で接続 3,100 本 = 43 本/時 なので、2,000 件あれば
/// **山のあった時間帯を丸ごと** さかのぼれる。1 件は下の切り詰めで 256 B 以内に収まる。
pub const MAX_RECENT: usize = 2000;

/// 閉じた接続の個票に収める宛先の長さ (バイト)。
///
/// `/errors` (500 件) より短いのは、2,000 件 × 1 件 256 B の枠に**寿命やバイトや段階の ms も
/// 一緒に**入れるため。`host:port` は実際には 20〜40 B なので、切れるのは異様に長い名前だけ。
pub const MAX_RECENT_TARGET: usize = 48;

/// 段階の ms の数 ([`STAGE_NAMES`] と同じ並び)。
pub const STAGES: usize = 6;

/// 段階の ms の名前 (`/recent` の `ms` に出す鍵)。
///
/// **いま埋まるのは先頭 3 つだけ** (`dns` / `connect` / `first_byte` = [`crate::metrics::Detail`] に
/// あるもの)。残り 3 つは T14.3 (`/profile`) が段階の時計を足したときに埋める欄で、
/// それまでは 0 のまま (0 の段階は JSON に出さない)。
pub const STAGE_NAMES: [&str; STAGES] = [
    "dns",
    "connect",
    "first_byte",
    "queue",
    "client_read",
    "first_relay",
];

/// [`STAGE_NAMES`] の添字。
pub const STAGE_DNS: usize = 0;
pub const STAGE_CONNECT: usize = 1;
pub const STAGE_FIRST_BYTE: usize = 2;

/// 接続が閉じた理由 (`/recent` の `reason`)。
///
/// 8 種類しかないのは、読む人が「次に何を見るか」を変えられる粒度で切ったため
/// ([`ErrCause`] と同じ方針)。`Error` だけは原因を連れて `error:refused` のように出す。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// クライアントが先に EOF を送った (ふつうの終わり方)
    ClientEof,
    /// 宛先が先に EOF を送った
    ServerEof,
    /// トンネルが無通信で打ち切られた (`PROXY_TUNNEL_IDLE_SECS`)
    IdleTimeout,
    /// keep-alive の接続が次の要求を待ちきれずに閉じた (`PROXY_KEEPALIVE_SECS`)
    KeepaliveTimeout,
    /// 同時接続の上限に当たった accept が席を作るために閉じた (T13.2)
    Evicted,
    /// 1 接続あたりの要求数の上限に達した (`Config::max_requests_per_conn`。T14.2)
    Limit,
    /// 上のどれでもない、プロキシ側の都合 (監視スレッドの停止、4xx で断った、内部エンドポイント)
    Shutdown,
    /// 入出力が失敗した (原因は [`ErrCause`] と同じ 8 つ)
    Error(ErrCause),
}

impl CloseReason {
    /// リングに書く前の符号 (0 = まだ決まっていない)。[`ConnSlot`] の原子に入れる。
    fn code(self) -> u16 {
        match self {
            CloseReason::ClientEof => 1,
            CloseReason::ServerEof => 2,
            CloseReason::IdleTimeout => 3,
            CloseReason::KeepaliveTimeout => 4,
            CloseReason::Evicted => 5,
            CloseReason::Limit => 6,
            CloseReason::Shutdown => 7,
            CloseReason::Error(c) => 8 + c as u16,
        }
    }

    /// 符号から戻す。知らない値と 0 (未設定) は [`CloseReason::Shutdown`]。
    fn from_code(v: u16) -> CloseReason {
        match v {
            1 => CloseReason::ClientEof,
            2 => CloseReason::ServerEof,
            3 => CloseReason::IdleTimeout,
            4 => CloseReason::KeepaliveTimeout,
            5 => CloseReason::Evicted,
            6 => CloseReason::Limit,
            8 => CloseReason::Error(ErrCause::Dns),
            9 => CloseReason::Error(ErrCause::Refused),
            10 => CloseReason::Error(ErrCause::Unreachable),
            11 => CloseReason::Error(ErrCause::Timeout),
            12 => CloseReason::Error(ErrCause::Reset),
            13 => CloseReason::Error(ErrCause::Tls),
            14 => CloseReason::Error(ErrCause::Loop),
            15 => CloseReason::Error(ErrCause::Other),
            _ => CloseReason::Shutdown,
        }
    }

    /// `/recent` の `reason` に出す綴り (`error:` は原因を連れる)。
    pub fn text(self) -> std::borrow::Cow<'static, str> {
        use std::borrow::Cow;
        match self {
            CloseReason::ClientEof => Cow::Borrowed("client_eof"),
            CloseReason::ServerEof => Cow::Borrowed("server_eof"),
            CloseReason::IdleTimeout => Cow::Borrowed("idle_timeout"),
            CloseReason::KeepaliveTimeout => Cow::Borrowed("keepalive_timeout"),
            CloseReason::Evicted => Cow::Borrowed("evicted"),
            CloseReason::Limit => Cow::Borrowed("limit"),
            CloseReason::Shutdown => Cow::Borrowed("shutdown"),
            CloseReason::Error(c) => Cow::Owned(format!("error:{}", c.name())),
        }
    }
}

/// 1 接続ぶんの「個票に足す値」(T14.4)。
///
/// **原子ではない。** 1 本の接続を同時に触るスレッドは 1 つだけなので、要求ごとの
/// 積み上げは [`std::cell::Cell`] に置いた普通の変数で行い、**接続の終了で 1 回だけ**
/// [`ConnSlot::finish`] で個票へ移す (要求ごとの原子操作を 1 つも増やさないため)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnTally {
    /// クライアントから受けたバイト (要求のヘッダーと本文、トンネルは上り)
    pub up: u64,
    /// クライアントへ返したバイト (応答、トンネルは下り)
    pub down: u64,
    /// 最後の応答の状態コード (http だけ)
    pub status: u16,
    /// 段階の ms (いちばん大きかった要求の値)。並びは [`STAGE_NAMES`]
    pub stage_ms: [u64; STAGES],
}

impl ConnTally {
    /// 1 要求ぶんを足す (段階の ms は**いちばん遅かった要求**を採る)。
    pub fn add_request(&mut self, status: u16, up: u64, down: u64, stage_ms: [u64; STAGES]) {
        self.up = self.up.saturating_add(up);
        self.down = self.down.saturating_add(down);
        self.status = status;
        for (slot, ms) in self.stage_ms.iter_mut().zip(stage_ms) {
            *slot = (*slot).max(ms);
        }
    }
}

/// 閉じた接続 1 本の個票 (`/recent`)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentEntry {
    /// 接続の通し番号 (ログの `conn#N`、`/connections` の `id` と同じ)
    pub id: u64,
    /// 開いた時刻 (epoch 秒)
    pub at: u64,
    /// 接続元 IP
    pub client: String,
    /// 宛先 (`host:port`。CONNECT はトンネルの相手、http は最初の要求の宛先)
    pub target: String,
    /// CONNECT トンネルか (`false` = keep-alive の HTTP)
    pub connect: bool,
    /// 寿命 (秒)
    pub secs: u64,
    /// 処理した要求の数 (http だけ)
    pub requests: u32,
    /// クライアントから受けたバイト / クライアントへ返したバイト
    pub up: u64,
    pub down: u64,
    /// 閉じた理由
    pub reason: CloseReason,
    /// 最後の応答の状態コード (http だけ。0 = 無し)
    pub status: u16,
    /// 預かり所にいた合計秒と、預けられた回数
    pub parked_secs: u64,
    pub parks: u32,
    /// 段階の ms ([`STAGE_NAMES`] の並び)
    pub stage_ms: [u64; STAGES],
}

impl RecentEntry {
    /// 並べ替えに使う転送量の合計。
    pub fn bytes(&self) -> u64 {
        self.up.saturating_add(self.down)
    }

    /// 並べ替えに使う確立の ms (`?sort=slow`)。
    pub fn connect_ms(&self) -> u64 {
        self.stage_ms[STAGE_CONNECT]
    }

    /// `/recent` の 1 要素。
    ///
    /// `ms` の中身だけ可変で、**`dns` と `connect` は 0 でも必ず出す**
    /// (`?sort=slow` が読む値なので、無いのと 0 を区別させない)。残りの 4 つは
    /// 0 なら出さない — T14.3 が段階の時計を足すまで 0 のままなので、それまでは
    /// 1 バイトも増えない。
    pub fn to_json(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(256);
        let _ = write!(
            out,
            "{{\"id\":{},\"at\":{},\"client\":\"{}\",\"target\":\"{}\",\"kind\":\"{}\",\"secs\":{},\"reqs\":{},\"up\":{},\"down\":{},\"reason\":\"{}\",\"status\":{},\"parked_secs\":{},\"parks\":{},\"ms\":{{",
            self.id,
            self.at,
            crate::json::escape(&self.client),
            crate::json::escape(&self.target),
            if self.connect { "connect" } else { "http" },
            self.secs,
            self.requests,
            self.up,
            self.down,
            self.reason.text(),
            self.status,
            self.parked_secs,
            self.parks,
        );
        for (i, (name, ms)) in STAGE_NAMES.iter().zip(self.stage_ms).enumerate() {
            // `dns` と `connect` は必ず、残りは 0 でないときだけ
            if i > STAGE_CONNECT && ms == 0 {
                continue;
            }
            let _ = write!(out, "{}\"{}\":{}", if i == 0 { "" } else { "," }, name, ms);
        }
        out.push_str("}}");
        out
    }
}

/// 閉じた接続の固定長リング (`/recent`。T14.4)。
///
/// 書くのは**接続の終了で 1 回**だけ ([`ConnTable::unregister`] と同じ場所)。
/// `/connections` が「いま」しか見せないのに対し、ここは「起きたこと」を残す口で、
/// バーストのとき誰が何を開いたか・遅かった 1 本がどの段階で遅かったかを後から読む。
pub struct RecentRing {
    inner: Mutex<RecentBuf>,
}

#[derive(Default)]
struct RecentBuf {
    buf: Vec<RecentEntry>,
    /// 次に書く位置 (`buf` が満杯になってからだけ意味を持つ)
    next: usize,
    /// 起動からの通算 (捨てた分も含む)
    total: u64,
}

impl Default for RecentRing {
    fn default() -> Self {
        RecentRing::new()
    }
}

impl RecentRing {
    pub fn new() -> RecentRing {
        RecentRing {
            inner: Mutex::new(RecentBuf::default()),
        }
    }

    /// 1 件書く (満杯なら最も古いものを上書きする)。**接続の終了で 1 回だけ。**
    pub fn push(&self, entry: RecentEntry) {
        let mut r = self.inner.locked();
        r.total += 1;
        if r.buf.len() < MAX_RECENT {
            r.buf.push(entry);
            return;
        }
        let at = r.next;
        r.buf[at] = entry;
        r.next = (at + 1) % MAX_RECENT;
    }

    /// 条件に合うものを**新しい順**で返す。2 つ目は起動からの通算 (捨てた分も含む)。
    ///
    /// 絞りはここ (鍵の内側) で済ませる。`since` は「開いた時刻がこれ以降」、
    /// `client` は接続元の完全一致 (空なら絞らない)。
    pub fn select(&self, since: u64, client: &str) -> (Vec<RecentEntry>, u64) {
        let r = self.inner.locked();
        let len = r.buf.len();
        let mut out = Vec::with_capacity(len.min(MAX_RECENT));
        for i in 0..len {
            let start = if len < MAX_RECENT { len } else { r.next };
            let at = (start + len - 1 - i) % len;
            let e = &r.buf[at];
            if e.at < since || (!client.is_empty() && e.client != client) {
                continue;
            }
            out.push(e.clone());
        }
        (out, r.total)
    }

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
            EntryCause::Error(ErrCause::Refused),
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
            EntryCause::Error(ErrCause::Unreachable),
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
                true,
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

    /// 閉じた接続の個票が 1 件ずつ残ること (`/recent`。T14.4)。
    #[test]
    fn closing_a_connection_leaves_one_entry() {
        let t = ConnTable::new();
        let ring = RecentRing::new();
        let slot = t.register(7, "198.51.100.7", Instant::now()).unwrap();
        slot.begin_tunnel("mtalk.google.com:5228");
        slot.on_park();
        slot.on_unpark();
        slot.finish(
            CloseReason::ClientEof,
            ConnTally {
                up: 4096,
                down: 65536,
                status: 0,
                stage_ms: [3, 9, 0, 0, 0, 0],
            },
            0,
        );
        let got = t.unregister(7).expect("枠が返る");
        ring.push(
            got.closed_entry(Instant::now())
                .expect("宛先のある接続は残る"),
        );
        assert!(t.is_empty());
        assert!(t.unregister(7).is_none(), "2 回目は何も返らない");

        let (rows, total) = ring.select(0, "");
        assert_eq!(total, 1);
        assert_eq!(rows.len(), 1);
        let e = &rows[0];
        assert_eq!(e.id, 7);
        assert_eq!(e.client, "198.51.100.7");
        assert_eq!(e.target, "mtalk.google.com:5228");
        assert!(e.connect);
        assert_eq!(e.up, 4096);
        assert_eq!(e.down, 65536);
        assert_eq!(e.bytes(), 4096 + 65536);
        assert_eq!(e.reason, CloseReason::ClientEof);
        assert_eq!(e.connect_ms(), 9);
        assert_eq!(e.parks, 1);
        let json = e.to_json();
        assert!(json.contains("\"reason\":\"client_eof\""), "{}", json);
        assert!(json.contains("\"kind\":\"connect\""), "{}", json);
        assert!(
            json.contains("\"ms\":{\"dns\":3,\"connect\":9}"),
            "{}",
            json
        );
        // T14.3 が埋める段階はまだ 0 なので 1 バイトも出さない
        assert!(!json.contains("queue"), "{}", json);
        assert!(json.len() <= 256, "ありふれた 1 件が {} B", json.len());
        println!("closed entry: typical {} B\n  {}", json.len(), json);
    }

    /// 閉じた理由は**先に書いた方が勝つ** (T13.2 の追い出しが中継の終わり方に負けない)。
    #[test]
    fn the_first_close_reason_wins() {
        let t = ConnTable::new();
        let slot = t.register(1, "127.0.0.1", Instant::now()).unwrap();
        slot.begin_tunnel("example.net:443");
        slot.set_close(CloseReason::Evicted);
        slot.finish(CloseReason::ClientEof, ConnTally::default(), 0);
        let e = slot
            .closed_entry(Instant::now())
            .expect("宛先のある接続は残る");
        assert_eq!(e.reason, CloseReason::Evicted);
        assert_eq!(e.reason.text(), "evicted");
        // 何も書かなければ「プロキシ側の都合」
        let other = t.register(2, "127.0.0.1", Instant::now()).unwrap();
        other.begin_tunnel("example.net:443");
        assert_eq!(
            other.closed_entry(Instant::now()).unwrap().reason,
            CloseReason::Shutdown
        );
        // 宛先を 1 つも選ばなかった接続 (自分宛ての `/status` など) は残さない
        let local = t.register(3, "127.0.0.1", Instant::now()).unwrap();
        assert!(local.closed_entry(Instant::now()).is_none());
    }

    /// `error:<原因>` の綴りと、8 つの理由が往復できること。
    #[test]
    fn every_close_reason_survives_the_round_trip() {
        let all = [
            (CloseReason::ClientEof, "client_eof"),
            (CloseReason::ServerEof, "server_eof"),
            (CloseReason::IdleTimeout, "idle_timeout"),
            (CloseReason::KeepaliveTimeout, "keepalive_timeout"),
            (CloseReason::Evicted, "evicted"),
            (CloseReason::Limit, "limit"),
            (CloseReason::Shutdown, "shutdown"),
            (CloseReason::Error(ErrCause::Refused), "error:refused"),
            (CloseReason::Error(ErrCause::Dns), "error:dns"),
            (CloseReason::Error(ErrCause::Other), "error:other"),
        ];
        for (reason, name) in all {
            assert_eq!(reason.text(), name);
            assert_eq!(CloseReason::from_code(reason.code()), reason, "{}", name);
        }
    }

    /// `since` と `client` で絞れ、新しい順に返ること。
    #[test]
    fn the_closed_ring_filters_and_overwrites_the_oldest() {
        let ring = RecentRing::new();
        for i in 0..(MAX_RECENT as u64 + 5) {
            ring.push(RecentEntry {
                id: i,
                at: 1_000_000 + i,
                client: if i % 2 == 0 { "10.0.0.1" } else { "10.0.0.2" }.to_string(),
                target: "example.net:443".to_string(),
                connect: true,
                secs: i,
                requests: 0,
                up: i,
                down: i * 2,
                reason: CloseReason::ClientEof,
                status: 0,
                parked_secs: 0,
                parks: 0,
                stage_ms: [0, i, 0, 0, 0, 0],
            });
        }
        let (all, total) = ring.select(0, "");
        assert_eq!(total, MAX_RECENT as u64 + 5);
        assert_eq!(all.len(), MAX_RECENT, "リングは 2,000 件で頭打ち");
        assert_eq!(all[0].id, MAX_RECENT as u64 + 4, "新しい順");
        assert_eq!(all[MAX_RECENT - 1].id, 5, "最古は 2,000 件前");
        // 接続元で絞る
        let (mine, _) = ring.select(0, "10.0.0.2");
        assert_eq!(mine.len(), MAX_RECENT / 2);
        assert!(mine.iter().all(|e| e.client == "10.0.0.2"));
        // 時刻で絞る (それ以降に開いたものだけ)
        let (fresh, _) = ring.select(1_000_000 + MAX_RECENT as u64, "");
        assert_eq!(fresh.len(), 5);
        assert!(ring.select(9_999_999, "").0.is_empty());
    }

    /// 桁を振り切った 1 件でも 448 B に収まること (**リングの大きさの上限**)。
    ///
    /// 1 件の目安は 256 B (T13.4 の `/errors` と同じ) で、**ありふれた 1 件は上のテストの
    /// とおり 225 B**。ここで見るのは「起こりえない桁 (転送 20 桁、段階の ms が 6 つとも
    /// 7 桁) を並べてもリングが 2,000 × 448 B = 875 KiB を越えない」ことだけで、
    /// `/recent?n=2000` の応答は 256 KiB のバイト数打ち切りに当たるのが設計どおり
    /// (`crates/endpoints` のテストで見る)。
    #[test]
    fn the_worst_closed_entry_stays_small() {
        let slot = ConnSlot::new(
            u64::MAX,
            "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6xx",
            Instant::now(),
        );
        slot.begin_tunnel(&"sub.".repeat(40));
        slot.on_park();
        slot.finish(
            CloseReason::KeepaliveTimeout,
            ConnTally {
                up: u64::MAX,
                down: u64::MAX,
                status: 599,
                stage_ms: [u64::MAX; STAGES],
            },
            u32::MAX,
        );
        let e = slot
            .closed_entry(Instant::now())
            .expect("宛先のある接続は残る");
        assert!(e.target.len() <= MAX_RECENT_TARGET, "{}", e.target.len());
        assert!(e.client.len() <= MAX_CLIENT, "{}", e.client.len());
        let json = e.to_json();
        assert!(json.len() <= 448, "最悪の 1 件が {} B", json.len());
        println!("closed entry: worst {} B", json.len());
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
