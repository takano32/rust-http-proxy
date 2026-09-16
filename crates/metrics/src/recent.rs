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
//! - [`RecentRing`]: 閉じた接続の個票 (`/recent`。T14.4)。接続の終了で 1 回。
//! - [`BurstRing`]: 同時接続数が上限の一定割合を越えた瞬間の写真 (`/bursts`。T14.6)。
//!   **撮るのは history スレッド**で、越えた接続を受けたスレッドは旗を立てるだけ。
//! - [`ClosedCounts`]: 閉じた理由・寿命・バイト・預けの分布 (`/history` の `closed`。T14.6)。
//!   足すのは `/recent` に 1 件書くのと同じ場所で、**既に鍵の内側**。
//!
//! [`Metrics::record_error`]: crate::metrics::Metrics::record_error

use std::collections::HashMap;
use std::sync::atomic::{
    AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
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

    /// 分布の添字 ([`CLOSE_REASON_NAMES`] の並び。T14.6)。
    ///
    /// `Error` は原因を落として 1 つにまとめる (原因別は `/status` の `errors_by_cause` と
    /// `/errors` にあるので、窓の列を 8 つ増やさない)。
    pub fn index(self) -> usize {
        match self {
            CloseReason::ClientEof => 0,
            CloseReason::ServerEof => 1,
            CloseReason::IdleTimeout => 2,
            CloseReason::KeepaliveTimeout => 3,
            CloseReason::Evicted => 4,
            CloseReason::Limit => 5,
            CloseReason::Shutdown => 6,
            CloseReason::Error(_) => 7,
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
/// 山の写真 (`/bursts`) を何枚覚えておくか (固定。T14.6)。
///
/// 1 枚 4 KiB 以下 (下のテストで見る) なので 50 枚で 200 KiB。デプロイ先の山は
/// 1 日に数回なので、50 枚あれば 1 週間以上さかのぼれる。
pub const MAX_BURSTS: usize = 50;

/// 1 枚に載せる接続元の数 (それ以外は `clients_other` に本数だけ)。
pub const MAX_SHOT_CLIENTS: usize = 16;

/// 1 枚に載せる宛先の数 (それ以外は `targets_other` に本数だけ)。
pub const MAX_SHOT_TARGETS: usize = 10;

/// [`ConnState`] の数と、その名前 (`ConnState as usize` が添字)。
pub const CONN_STATES: usize = 5;
pub const CONN_STATE_NAMES: [&str; CONN_STATES] =
    ["serving", "reading", "parked", "queued", "relaying"];

/// 山の写真 1 枚 (`/bursts`。T14.6)。
///
/// 撮るのは **history スレッド**で、中身は `/connections` の表から作る (鍵 1 回)。
/// 越えた接続を受けたスレッドは [`BurstRing::request`] で旗を立てるだけなので、
/// **accept の経路に鍵は 1 つも増えない**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BurstShot {
    /// 撮った時刻 (epoch 秒)
    pub at: u64,
    /// 何枚目か (捨てた分も含む通し番号)
    pub seq: u64,
    /// 撮った瞬間の同時接続数 (`/status` の `active_connections`)
    pub active: usize,
    /// 旗が立った瞬間の本数 (撮るまでに最大 1 周期あるので `active` とは違いうる)
    pub trigger_active: usize,
    /// そのときの上限と閾 (`max_conns` × `PROXY_BURST_PERCENT`)
    pub max_conns: usize,
    pub threshold: usize,
    /// 接続元ごとの本数 (多い順、上位 [`MAX_SHOT_CLIENTS`])
    pub clients: Vec<(String, u32)>,
    pub clients_distinct: usize,
    pub clients_other: u32,
    /// 宛先ごとの本数 (多い順、上位 [`MAX_SHOT_TARGETS`])
    pub targets: Vec<(String, u32)>,
    pub targets_distinct: usize,
    pub targets_other: u32,
    /// 状態別 ([`CONN_STATE_NAMES`] の並び) と種類別
    pub states: [u32; CONN_STATES],
    pub connects: u32,
    pub https: u32,
    /// そのときの累計 (T13.2 の追い出しと、上限で断った数)
    pub evicted_idle: u64,
    pub rejected_overload: u64,
    /// プロセスのスレッド数と記述子 (`/proc`。history スレッドが読む)
    pub threads: u64,
    pub fds: u64,
    pub max_fds: u64,
}

impl BurstShot {
    /// `/connections` の表から 1 枚作る (`ConnTable::snapshot` の鍵 1 回のあと)。
    #[allow(clippy::too_many_arguments)]
    pub fn take(
        rows: &[Arc<ConnSlot>],
        seq: u64,
        active: usize,
        trigger_active: usize,
        max_conns: usize,
        threshold: usize,
        evicted_idle: u64,
        rejected_overload: u64,
    ) -> BurstShot {
        let mut by_client: HashMap<&str, u32> = HashMap::new();
        let mut by_target: HashMap<String, u32> = HashMap::new();
        let mut states = [0u32; CONN_STATES];
        let (mut connects, mut https) = (0u32, 0u32);
        for slot in rows {
            *by_client.entry(slot.client.as_str()).or_insert(0) += 1;
            let target = slot.target.locked().clone();
            if !target.is_empty() {
                *by_target.entry(target).or_insert(0) += 1;
            }
            states[slot.state() as usize] += 1;
            if slot.is_connect() {
                connects += 1;
            } else {
                https += 1;
            }
        }
        let (clients, clients_other) = top_counts(
            by_client.into_iter().map(|(k, n)| (k.to_string(), n)),
            MAX_SHOT_CLIENTS,
        );
        let clients_distinct = clients.len() + usize::from(clients_other > 0);
        let (targets, targets_other) = top_counts(by_target.into_iter(), MAX_SHOT_TARGETS);
        let targets_distinct = targets.len() + usize::from(targets_other > 0);
        let (threads, fds, max_fds) = process_counts();
        BurstShot {
            at: crate::cache::now_epoch(),
            seq,
            active,
            trigger_active,
            max_conns,
            threshold,
            clients,
            clients_distinct,
            clients_other,
            targets,
            targets_distinct,
            targets_other,
            states,
            connects,
            https,
            evicted_idle,
            rejected_overload,
            threads,
            fds,
            max_fds,
        }
    }

    /// `/bursts` の 1 要素。
    pub fn to_json(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(2048);
        let _ = write!(
            out,
            "{{\"at\":{},\"seq\":{},\"active\":{},\"trigger_active\":{},\"max_conns\":{},\"threshold\":{},\"clients\":",
            self.at, self.seq, self.active, self.trigger_active, self.max_conns, self.threshold,
        );
        push_counts(&mut out, "client", &self.clients);
        let _ = write!(
            out,
            ",\"clients_distinct\":{},\"clients_other\":{},\"targets\":",
            self.clients_distinct, self.clients_other
        );
        push_counts(&mut out, "target", &self.targets);
        let _ = write!(
            out,
            ",\"targets_distinct\":{},\"targets_other\":{},\"states\":{{",
            self.targets_distinct, self.targets_other
        );
        for (i, name) in CONN_STATE_NAMES.iter().enumerate() {
            let _ = write!(
                out,
                "{}\"{}\":{}",
                if i == 0 { "" } else { "," },
                name,
                self.states[i]
            );
        }
        let _ = write!(
            out,
            "}},\"kinds\":{{\"connect\":{},\"http\":{}}},\"evicted_idle\":{},\"rejected_overload\":{},\"threads\":{},\"fds\":{},\"max_fds\":{}}}",
            self.connects,
            self.https,
            self.evicted_idle,
            self.rejected_overload,
            self.threads,
            self.fds,
            self.max_fds,
        );
        out
    }
}

/// 多い順に `max` 件まで取り、残りの本数を返す (同数は名前の順で安定させる)。
fn top_counts(items: impl Iterator<Item = (String, u32)>, max: usize) -> (Vec<(String, u32)>, u32) {
    let mut all: Vec<(String, u32)> = items.collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let other = all.iter().skip(max).map(|(_, n)| *n).sum();
    all.truncate(max);
    (all, other)
}

/// `[{"<key>":"…","conns":N},…]`。
fn push_counts(out: &mut String, key: &str, rows: &[(String, u32)]) {
    use std::fmt::Write as _;
    out.push('[');
    for (i, (name, n)) in rows.iter().enumerate() {
        let _ = write!(
            out,
            "{}{{\"{}\":\"{}\",\"conns\":{}}}",
            if i == 0 { "" } else { "," },
            key,
            crate::json::escape(name),
            n
        );
    }
    out.push(']');
}

/// プロセスのスレッド数 / 記述子 / その上限。**写真を撮るときだけ**読む (`/proc` 2 つ)。
fn process_counts() -> (u64, u64, u64) {
    let threads = crate::sysinfo::process_threads().unwrap_or(0);
    let (fds, max_fds) = crate::sysinfo::process_fds().unwrap_or((0, 0));
    (threads, fds, max_fds)
}

/// 山の写真のリングと、「撮ってくれ」の旗 (T14.6)。
///
/// **accept の経路がするのは、閾を越えたときに原子の読み 1 回** ([`BurstRing::request`])
/// だけ。撮るのは history スレッド ([`BurstRing::take_pending`] → [`BurstShot::take`] →
/// [`BurstRing::push`]) で、5 秒以内に 1 枚撮れる。
///
/// **同じ山では 1 枚だけ**: 撮ると旗を下ろし、同時接続数が閾の 80% を下回るまで
/// 次の 1 枚を撮らない ([`BurstRing::rearm_if_calm`])。
pub struct BurstRing {
    /// 次の山で 1 枚撮ってよいか
    armed: AtomicBool,
    /// history スレッドへの「撮ってくれ」= 旗が立った瞬間の本数 (0 = 頼まれていない)
    pending: AtomicUsize,
    /// 旗を立てた接続が見ていた閾と上限 (写真と、山が引いたかの判定に使う)
    threshold: AtomicUsize,
    max_conns: AtomicUsize,
    inner: Mutex<BurstBuf>,
}

#[derive(Default)]
struct BurstBuf {
    buf: Vec<BurstShot>,
    /// 次に書く位置 (`buf` が満杯になってからだけ意味を持つ)
    next: usize,
    /// 起動からの通算 (捨てた分も含む)
    total: u64,
}

impl Default for BurstRing {
    fn default() -> Self {
        BurstRing::new()
    }
}

impl BurstRing {
    pub fn new() -> BurstRing {
        BurstRing {
            armed: AtomicBool::new(true),
            pending: AtomicUsize::new(0),
            threshold: AtomicUsize::new(0),
            max_conns: AtomicUsize::new(0),
            inner: Mutex::new(BurstBuf::default()),
        }
    }

    /// 閾を**越えた瞬間**に accept の経路から呼ぶ (T14.6)。
    ///
    /// 呼び出し側は `open > cfg.burst_at` の**比較 1 回**で来るかどうかを決める。
    /// ここでも原子の読みが 1 回で、旗が下りていれば (= 同じ山の 2 本目以降) すぐ戻る。
    pub fn request(&self, active: usize, threshold: usize, max_conns: usize) {
        if !self.armed.load(Ordering::Relaxed) {
            return;
        }
        // 同じ瞬間に複数のスレッドが越えても、旗を取れるのは 1 つだけ
        if !self.armed.swap(false, Ordering::Relaxed) {
            return;
        }
        self.threshold.store(threshold, Ordering::Relaxed);
        self.max_conns.store(max_conns, Ordering::Relaxed);
        // 0 は「頼まれていない」の印なので 1 未満にはしない
        self.pending.store(active.max(1), Ordering::Relaxed);
    }

    /// 頼まれていれば旗を下ろし、立った瞬間の本数を返す (history スレッド)。
    pub fn take_pending(&self) -> Option<usize> {
        match self.pending.swap(0, Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    /// 山が引いたら次の 1 枚に備える (history スレッド)。
    ///
    /// 戻す境目を閾そのものではなく **80%** にしてあるのは、閾のすぐ上で出入りする
    /// 接続が 1 本あるだけで写真がリングを埋めてしまうため。
    pub fn rearm_if_calm(&self, active: usize) {
        let threshold = self.threshold.load(Ordering::Relaxed);
        if threshold == 0 || self.pending.load(Ordering::Relaxed) != 0 {
            return;
        }
        if active.saturating_mul(100) < threshold.saturating_mul(80) {
            self.armed.store(true, Ordering::Relaxed);
        }
    }

    /// 1 枚書く (満杯なら最も古いものを上書きする)。
    pub fn push(&self, shot: BurstShot) {
        let mut r = self.inner.locked();
        r.total += 1;
        if r.buf.len() < MAX_BURSTS {
            r.buf.push(shot);
            return;
        }
        let at = r.next;
        r.buf[at] = shot;
        r.next = (at + 1) % MAX_BURSTS;
    }

    /// 次に撮る 1 枚の通し番号 (1 始まり)。
    pub fn next_seq(&self) -> u64 {
        self.inner.locked().total + 1
    }

    /// 直近 `n` 枚を**新しい順**で返す。2 つ目は起動からの通算 (捨てた分も含む)。
    pub fn recent(&self, n: usize) -> (Vec<BurstShot>, u64) {
        let r = self.inner.locked();
        let len = r.buf.len();
        let n = n.min(len);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let start = if len < MAX_BURSTS { len } else { r.next };
            let at = (start + len - 1 - i) % len;
            out.push(r.buf[at].clone());
        }
        (out, r.total)
    }

    pub fn len(&self) -> usize {
        self.inner.locked().buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// いまの閾と上限 (`/bursts` に出す。旗が 1 度も立っていなければ 0)。
    pub fn threshold(&self) -> usize {
        self.threshold.load(Ordering::Relaxed)
    }

    pub fn max_conns(&self) -> usize {
        self.max_conns.load(Ordering::Relaxed)
    }

    /// 次の山で撮る用意ができているか / 撮ってくれと頼まれているか (`/bursts` に出す)。
    pub fn armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    pub fn pending(&self) -> bool {
        self.pending.load(Ordering::Relaxed) != 0
    }
}

/// 閉じた理由の数 ([`CLOSE_REASON_NAMES`] と同じ。T14.6)。
///
/// [`CloseReason`] は 8 種類で、`Error` だけは原因を連れている。分布では
/// **`error:*` を 1 つにまとめる** (原因別は `/status` の `errors_by_cause` と
/// `/errors` にあるので、ここで 8 + 8 = 15 列に増やさない)。
pub const CLOSE_REASONS: usize = 8;
pub const CLOSE_REASON_NAMES: [&str; CLOSE_REASONS] = [
    "client_eof",
    "server_eof",
    "idle_timeout",
    "keepalive_timeout",
    "evicted",
    "limit",
    "shutdown",
    "error",
];

/// 寿命の区間 (秒)。**12 段** (T14.6)。
///
/// `PROXY_KEEPALIVE_SECS` (15) と `PROXY_TUNNEL_IDLE_SECS` (300) を**区間の境目に
/// 置いてある**: 「その設定で切られた接続がどれだけ居るか」を読むのがこの分布の仕事なので、
/// 境目が設定とずれていると読めない。
pub const LIFE_BOUNDS_SECS: [u64; 12] = [1, 2, 5, 10, 15, 30, 60, 120, 300, 900, 3600, 21600];

/// 上り・下りバイトの区間。**12 段、1 KiB から 4 倍ずつ** (T14.6)。
///
/// 1 KiB (要求 1 本ぶんのヘッダー) から 1 GiB (10 段目) までを 4 倍刻みで覆う。
/// 最後の 1 段 (4 GiB) は「桁が違う 1 本」を上限なしの区間に落とさないための余白。
pub const BYTE_BOUNDS: [u64; 12] = [
    1 << 10,
    1 << 12,
    1 << 14,
    1 << 16,
    1 << 18,
    1 << 20,
    1 << 22,
    1 << 24,
    1 << 26,
    1 << 28,
    1 << 30,
    1 << 32,
];

/// 区間の数 (上限なしの 1 段を足す)。
pub const LIFE_BUCKETS: usize = LIFE_BOUNDS_SECS.len() + 1;
pub const BYTE_BUCKETS: usize = BYTE_BOUNDS.len() + 1;

/// 「その窓に閉じた接続」の分布 (T14.6)。**累計ではない。**
///
/// 足すのは [`crate::metrics::Metrics::record_closed`] = 接続の終了で 1 回だけで、
/// **既に鍵の内側**なので原子操作もシステムコールも増えない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClosedCounts {
    /// 閉じた接続の数
    pub closed: u64,
    /// 閉じた理由別 ([`CLOSE_REASON_NAMES`] の並び)
    pub reasons: [u64; CLOSE_REASONS],
    /// 寿命の区間 ([`LIFE_BOUNDS_SECS`] + 上限なし)
    pub life: [u64; LIFE_BUCKETS],
    /// 上り / 下りバイトの区間 ([`BYTE_BOUNDS`] + 上限なし)
    pub up: [u64; BYTE_BUCKETS],
    pub down: [u64; BYTE_BUCKETS],
    /// 合計 (平均を出すため)
    pub life_secs_sum: u64,
    pub parked_secs_sum: u64,
    pub parks: u64,
    pub up_bytes: u64,
    pub down_bytes: u64,
}

impl Default for ClosedCounts {
    fn default() -> Self {
        ClosedCounts {
            closed: 0,
            reasons: [0; CLOSE_REASONS],
            life: [0; LIFE_BUCKETS],
            up: [0; BYTE_BUCKETS],
            down: [0; BYTE_BUCKETS],
            life_secs_sum: 0,
            parked_secs_sum: 0,
            parks: 0,
            up_bytes: 0,
            down_bytes: 0,
        }
    }
}

/// `bounds` の何段目か (どれにも収まらなければ最後の「上限なし」)。
fn bucket_of(bounds: &[u64], v: u64) -> usize {
    bounds.iter().position(|&b| v <= b).unwrap_or(bounds.len())
}

impl ClosedCounts {
    /// 閉じた接続 1 本を足す (`/recent` に残す 1 件と同じもの)。
    pub fn observe(&mut self, e: &RecentEntry) {
        self.closed += 1;
        self.reasons[e.reason.index()] += 1;
        self.life[bucket_of(&LIFE_BOUNDS_SECS, e.secs)] += 1;
        self.up[bucket_of(&BYTE_BOUNDS, e.up)] += 1;
        self.down[bucket_of(&BYTE_BOUNDS, e.down)] += 1;
        self.life_secs_sum = self.life_secs_sum.saturating_add(e.secs);
        self.parked_secs_sum = self.parked_secs_sum.saturating_add(e.parked_secs);
        self.parks = self.parks.saturating_add(e.parks as u64);
        self.up_bytes = self.up_bytes.saturating_add(e.up);
        self.down_bytes = self.down_bytes.saturating_add(e.down);
    }

    /// 粗い解像度へ畳むときは足し合わせる (区間の値なので平均でも最後の値でもない)。
    pub fn merge(&mut self, o: &ClosedCounts) {
        self.closed += o.closed;
        for (a, b) in self.reasons.iter_mut().zip(o.reasons.iter()) {
            *a += *b;
        }
        for (a, b) in self.life.iter_mut().zip(o.life.iter()) {
            *a += *b;
        }
        for (a, b) in self.up.iter_mut().zip(o.up.iter()) {
            *a += *b;
        }
        for (a, b) in self.down.iter_mut().zip(o.down.iter()) {
            *a += *b;
        }
        self.life_secs_sum = self.life_secs_sum.saturating_add(o.life_secs_sum);
        self.parked_secs_sum = self.parked_secs_sum.saturating_add(o.parked_secs_sum);
        self.parks = self.parks.saturating_add(o.parks);
        self.up_bytes = self.up_bytes.saturating_add(o.up_bytes);
        self.down_bytes = self.down_bytes.saturating_add(o.down_bytes);
    }

    pub fn is_empty(&self) -> bool {
        self.closed == 0
    }

    /// 1 窓を配列 1 行として書く ([`CLOSED_KEYS`] の順。先頭は窓の始まりの時刻)。
    pub fn push_row(&self, out: &mut String, t: u64) {
        use std::fmt::Write as _;
        let _ = write!(out, "[{},{},", t, self.closed);
        push_u64_array(out, &self.reasons);
        out.push(',');
        push_u64_array(out, &self.life);
        out.push(',');
        push_u64_array(out, &self.up);
        out.push(',');
        push_u64_array(out, &self.down);
        let _ = write!(
            out,
            ",{},{},{},{},{}]",
            self.life_secs_sum, self.parked_secs_sum, self.parks, self.up_bytes, self.down_bytes
        );
    }
}

/// [`ClosedCounts::push_row`] が並べる列の名前 (`/history` の `closed.keys`)。
pub const CLOSED_KEYS: [&str; 11] = [
    "t",
    "closed",
    "reasons",
    "life",
    "up",
    "down",
    "life_secs_sum",
    "parked_secs_sum",
    "parks",
    "up_bytes",
    "down_bytes",
];

fn push_u64_array(out: &mut String, v: &[u64]) {
    use std::fmt::Write as _;
    out.push('[');
    for (i, n) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}", n);
    }
    out.push(']');
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

/// 山の写真と、閉じた接続の分布 (T14.6)。
#[cfg(test)]
mod burst_tests {
    use super::*;

    fn closed(reason: CloseReason, secs: u64, up: u64, down: u64) -> RecentEntry {
        RecentEntry {
            id: 1,
            at: 1_700_000_000,
            client: "198.51.100.7".to_string(),
            target: "example.net:443".to_string(),
            connect: true,
            secs,
            requests: 0,
            up,
            down,
            reason,
            status: 0,
            parked_secs: 3,
            parks: 2,
            stage_ms: [0; STAGES],
        }
    }

    /// **閾を越えた瞬間の 1 本だけ**が旗を立て、同じ山の 2 本目以降は立てない。
    #[test]
    fn only_the_connection_that_crosses_raises_the_flag() {
        let ring = BurstRing::new();
        assert!(ring.armed());
        assert!(!ring.pending());
        // 閾 4 (上限 8 × 50%) を越えるのは 5 本目
        ring.request(5, 4, 8);
        assert_eq!(ring.take_pending(), Some(5), "越えた本数が旗に乗る");
        assert!(!ring.armed(), "撮ると旗は下りる");
        // 6〜8 本目は何も頼まない
        for n in 6..=8 {
            ring.request(n, 4, 8);
        }
        assert_eq!(ring.take_pending(), None, "同じ山では 1 枚だけ");
        assert_eq!(ring.threshold(), 4);
        assert_eq!(ring.max_conns(), 8);
    }

    /// 閾の 80% を下回るまでは次の 1 枚を撮らない。
    #[test]
    fn the_next_shot_waits_until_the_spike_is_over() {
        let ring = BurstRing::new();
        ring.request(5, 4, 8);
        assert_eq!(ring.take_pending(), Some(5));
        // 閾の 80% (4 × 0.8 = 3.2 本) 以上のあいだは戻らない
        for active in [8, 5, 4] {
            ring.rearm_if_calm(active);
            assert!(!ring.armed(), "{} 本ではまだ戻らない", active);
        }
        ring.rearm_if_calm(3);
        assert!(ring.armed(), "3 本 (< 3.2) まで減ったら次の 1 枚に備える");
        ring.request(5, 4, 8);
        assert_eq!(ring.take_pending(), Some(5), "2 枚目が撮れる");
    }

    /// まだ撮っていない旗を、山が引いたからといって消さない。
    #[test]
    fn a_pending_shot_is_not_lost_when_the_spike_passes() {
        let ring = BurstRing::new();
        ring.request(5, 4, 8);
        ring.rearm_if_calm(0);
        assert!(!ring.armed(), "頼んだ 1 枚を撮るまでは戻らない");
        assert_eq!(ring.take_pending(), Some(5));
        ring.rearm_if_calm(0);
        assert!(ring.armed());
    }

    /// 写真は `/connections` の表から作る (接続元・宛先・状態・種類の内訳)。
    #[test]
    fn a_shot_summarises_the_connection_table() {
        let t = ConnTable::new();
        let now = Instant::now();
        for i in 0..5u64 {
            let slot = t.register(i, "198.51.100.7", now).unwrap();
            slot.begin_tunnel("mtalk.google.com:5228");
            slot.set_state(ConnState::Parked);
        }
        let other = t.register(9, "203.0.113.9", now).unwrap();
        other.set_first_target("example.net:80");
        other.set_state(ConnState::Reading);

        let shot = BurstShot::take(&t.snapshot(), 1, 6, 5, 8, 4, 2, 7);
        assert_eq!(shot.seq, 1);
        assert_eq!(shot.active, 6);
        assert_eq!(shot.trigger_active, 5);
        assert_eq!(shot.max_conns, 8);
        assert_eq!(shot.threshold, 4);
        assert_eq!(shot.clients.len(), 2);
        assert_eq!(shot.clients[0], ("198.51.100.7".to_string(), 5), "多い順");
        assert_eq!(shot.clients_distinct, 2);
        assert_eq!(shot.targets[0], ("mtalk.google.com:5228".to_string(), 5));
        assert_eq!(shot.targets_distinct, 2);
        assert_eq!(shot.states[ConnState::Parked as usize], 5);
        assert_eq!(shot.states[ConnState::Reading as usize], 1);
        assert_eq!((shot.connects, shot.https), (5, 1));
        assert_eq!((shot.evicted_idle, shot.rejected_overload), (2, 7));
        let json = shot.to_json();
        assert!(json.contains("\"active\":6"), "{}", json);
        assert!(
            json.contains(
                "\"states\":{\"serving\":0,\"reading\":1,\"parked\":5,\"queued\":0,\"relaying\":0}"
            ),
            "{}",
            json
        );
        assert!(
            json.contains("\"kinds\":{\"connect\":5,\"http\":1}"),
            "{}",
            json
        );
        assert!(
            json.contains("{\"client\":\"198.51.100.7\",\"conns\":5}"),
            "{}",
            json
        );
        assert!(
            json.contains("{\"target\":\"mtalk.google.com:5228\",\"conns\":5}"),
            "{}",
            json
        );
        assert!(json.contains("\"evicted_idle\":2"), "{}", json);
        println!("burst shot: typical {} B", json.len());
    }

    /// 1 枚は 4 KiB 以内 (50 枚で 200 KiB。`/bursts` の 256 KiB に収まる大きさ)。
    #[test]
    fn one_shot_fits_in_4_kib() {
        let t = ConnTable::new();
        let now = Instant::now();
        // 接続元も宛先も上限いっぱいの長さで、載せる数より多い種類を作る
        for i in 0..(MAX_SHOT_CLIENTS as u64 + MAX_SHOT_TARGETS as u64 + 20) {
            let slot = t
                .register(
                    i,
                    &format!("2001:0db8:0000:0000:0000:ff00:0042:{:04x}%enp0s31f6xx", i),
                    now,
                )
                .unwrap();
            // **通し番号は先頭に置く**: 宛先は 80 B で切られるので、末尾に置くと
            // 切られたあとが全部同じ名前になって「種類が 1 つ」になってしまう
            slot.begin_tunnel(&format!("{:03}{}example.net:65535", i, "sub.".repeat(20)));
        }
        let shot = BurstShot::take(
            &t.snapshot(),
            u64::MAX,
            usize::MAX,
            usize::MAX,
            4096,
            2048,
            u64::MAX,
            u64::MAX,
        );
        assert_eq!(shot.clients.len(), MAX_SHOT_CLIENTS);
        assert_eq!(shot.targets.len(), MAX_SHOT_TARGETS);
        assert!(
            shot.clients_other > 0 && shot.targets_other > 0,
            "残りは本数だけ"
        );
        let json = shot.to_json();
        assert!(json.len() <= 4096, "1 枚が {} B", json.len());
        println!("burst shot: worst {} B", json.len());
    }

    /// リングは 50 枚で頭打ち、新しい順に返る。
    #[test]
    fn the_burst_ring_keeps_the_newest_fifty() {
        let ring = BurstRing::new();
        let t = ConnTable::new();
        for i in 0..(MAX_BURSTS as u64 + 3) {
            assert_eq!(ring.next_seq(), i + 1);
            ring.push(BurstShot::take(&t.snapshot(), i + 1, 5, 5, 8, 4, 0, 0));
        }
        let (got, total) = ring.recent(MAX_BURSTS);
        assert_eq!(total, MAX_BURSTS as u64 + 3);
        assert_eq!(got.len(), MAX_BURSTS);
        assert_eq!(got[0].seq, MAX_BURSTS as u64 + 3, "新しい順");
        assert_eq!(got[MAX_BURSTS - 1].seq, 4, "最古は 50 枚前");
        assert_eq!(ring.recent(2).0.len(), 2);
    }

    /// 閉じた理由・寿命・バイト・預けの分布が 1 本ずつ積み上がること。
    #[test]
    fn the_distribution_counts_reasons_lifetimes_and_bytes() {
        let mut c = ClosedCounts::default();
        assert!(c.is_empty());
        // 寿命 12 秒 (10 < 12 <= 15 = `PROXY_KEEPALIVE_SECS` の段)、上り 2 KiB、下り 3 MiB
        c.observe(&closed(CloseReason::KeepaliveTimeout, 12, 2048, 3 << 20));
        c.observe(&closed(CloseReason::Error(ErrCause::Reset), 0, 0, 0));
        c.observe(&closed(
            CloseReason::Error(ErrCause::Dns),
            100_000,
            u64::MAX,
            1,
        ));
        assert_eq!(c.closed, 3);
        assert_eq!(c.reasons[CloseReason::KeepaliveTimeout.index()], 1);
        assert_eq!(c.reasons[7], 2, "`error:*` は 1 つにまとめる");
        assert_eq!(c.life[4], 1, "12 秒は 15 秒の段");
        assert_eq!(c.life[0], 1, "0 秒は 1 秒の段");
        assert_eq!(c.life[LIFE_BUCKETS - 1], 1, "100,000 秒は上限なしの段");
        assert_eq!(c.up[1], 1, "2 KiB は 4 KiB の段");
        assert_eq!(c.up[0], 1, "0 B は 1 KiB の段");
        assert_eq!(c.up[BYTE_BUCKETS - 1], 1, "20 桁は上限なしの段");
        assert_eq!(c.down[6], 1, "3 MiB は 4 MiB の段");
        assert_eq!(c.life_secs_sum, 12 + 100_000);
        assert_eq!(c.parked_secs_sum, 9);
        assert_eq!(c.parks, 6);
        assert_eq!(c.down_bytes, (3 << 20) + 1);
        // 畳むと足し合わせ
        let mut agg = ClosedCounts::default();
        agg.merge(&c);
        agg.merge(&c);
        assert_eq!(agg.closed, 6);
        assert_eq!(agg.reasons[7], 4);
        assert_eq!(agg.life[4], 2);
        assert_eq!(agg.parks, 12);

        let mut out = String::new();
        c.push_row(&mut out, 1_700_000_005);
        assert!(
            out.starts_with("[1700000005,3,[0,0,0,1,0,0,0,2],"),
            "{}",
            out
        );
        assert!(
            out.ends_with(",100012,9,6,18446744073709551615,3145729]"),
            "{}",
            out
        );
        // 列の数が `CLOSED_KEYS` と合っていること (入れ子の配列は 1 列)
        let mut depth = 0;
        let cols = 1 + out
            .chars()
            .filter(|ch| {
                match ch {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ => {}
                }
                *ch == ',' && depth == 1
            })
            .count();
        assert_eq!(cols, CLOSED_KEYS.len(), "{}", out);
    }

    /// 8 つの理由が分布の 8 列に 1 対 1 で並ぶこと (名前も同じ綴り)。
    #[test]
    fn every_reason_has_its_own_column() {
        let all = [
            CloseReason::ClientEof,
            CloseReason::ServerEof,
            CloseReason::IdleTimeout,
            CloseReason::KeepaliveTimeout,
            CloseReason::Evicted,
            CloseReason::Limit,
            CloseReason::Shutdown,
            CloseReason::Error(ErrCause::Refused),
        ];
        for (i, reason) in all.into_iter().enumerate() {
            assert_eq!(reason.index(), i, "{}", reason.text());
            // `error:*` 以外は `/recent` の綴りと同じ名前
            if i < 7 {
                assert_eq!(CLOSE_REASON_NAMES[i], reason.text());
            }
        }
        assert_eq!(CLOSE_REASON_NAMES[7], "error");
        // 区間は 12 段ずつで、単調に増える
        assert_eq!(LIFE_BOUNDS_SECS.len(), 12);
        assert_eq!(BYTE_BOUNDS.len(), 12);
        assert!(LIFE_BOUNDS_SECS.windows(2).all(|w| w[0] < w[1]));
        assert!(BYTE_BOUNDS.windows(2).all(|w| w[0] < w[1]));
        // 設定の値そのものが境目にある (T14.6 の目的)
        assert!(LIFE_BOUNDS_SECS.contains(&15), "PROXY_KEEPALIVE_SECS");
        assert!(LIFE_BOUNDS_SECS.contains(&300), "PROXY_TUNNEL_IDLE_SECS");
        assert_eq!(BYTE_BOUNDS[0], 1024);
        assert_eq!(BYTE_BOUNDS[10], 1 << 30);
    }
}
