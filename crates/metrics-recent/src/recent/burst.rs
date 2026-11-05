//! 山の写真 (`/bursts`。T14.6) — 1 枚 ([`BurstShot`]) と、そのリングと「撮ってくれ」の旗
//! ([`BurstRing`])。`recent` の一部 (T.34.2 で `recent/mod.rs` から移した)。
//!
//! **accept の経路がするのは閾を越えたときの原子の読み 1 回** ([`BurstRing::request`]) だけで、
//! 撮るのは history スレッド。写真の中身は `/connections` の表 ([`ConnSlot`](super::ConnSlot) の
//! 並び) から作る — 表を数える所 ([`ShotTally`]) は時計も `/proc` も触らないので、単体試験は
//! そこへ直に枠を渡して書ける。
//!
//! 行を動かしただけで中身は変えていない。外から見える名前は `recent` の `pub use` でそのまま。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::ConnSlot;
use super::errors::drain_unwritten;
use crate::sync::LockExt;

/// 山の写真 (`/bursts`) を何枚覚えておくか (固定。T14.6)。
///
/// 1 枚 4 KiB 以下 (下のテストで見る) なので 50 枚で 200 KiB。デプロイ先の山は
/// 1 日に数回なので、50 枚あれば 1 週間以上さかのぼれる。
pub const MAX_BURSTS: usize = 50;

/// 1 枚に載せる接続元の数 (それ以外は `clients_other` に本数だけ)。
pub const MAX_SHOT_CLIENTS: usize = 16;

/// 1 枚に載せる宛先の数 (それ以外は `targets_other` に本数だけ)。
pub const MAX_SHOT_TARGETS: usize = 10;

/// [`ConnState`](super::ConnState) の数と、その名前 (`ConnState as usize` が添字)。
pub const CONN_STATES: usize = 5;
pub(crate) const CONN_STATE_NAMES: [&str; CONN_STATES] =
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

/// `/connections` の表を数えたもの (写真 1 枚の「表から読める部分」。T.34.2 で [`BurstShot::take`]
/// から割った)。
///
/// 時計も `/proc` も触らないので、単体試験は枠の並びを渡すだけで中身を確かめられる。
/// 写真そのものは [`BurstShot::take`] がこれに時刻とプロセスの数を足して作る。
struct ShotTally {
    clients: Vec<(String, u32)>,
    clients_distinct: usize,
    clients_other: u32,
    targets: Vec<(String, u32)>,
    targets_distinct: usize,
    targets_other: u32,
    states: [u32; CONN_STATES],
    connects: u32,
    https: u32,
}

impl ShotTally {
    /// 接続元・宛先・状態・種類を数える (宛先が空の枠 = まだ宛先を選んでいない接続は宛先に数えない)。
    fn of(rows: &[Arc<ConnSlot>]) -> ShotTally {
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
        ShotTally {
            clients,
            clients_distinct,
            clients_other,
            targets,
            targets_distinct,
            targets_other,
            states,
            connects,
            https,
        }
    }
}

impl BurstShot {
    /// `/connections` の表から 1 枚作る (`ConnTable::snapshot` の鍵 1 回のあと)。
    ///
    /// 表を数えるのは [`ShotTally::of`]、ここで足すのは時刻と `/proc` の 2 つ (写真を撮るときだけ読む)。
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
        let t = ShotTally::of(rows);
        let (threads, fds, max_fds) = process_counts();
        BurstShot {
            at: crate::cache::now_epoch(),
            seq,
            active,
            trigger_active,
            max_conns,
            threshold,
            clients: t.clients,
            clients_distinct: t.clients_distinct,
            clients_other: t.clients_other,
            targets: t.targets,
            targets_distinct: t.targets_distinct,
            targets_other: t.targets_other,
            states: t.states,
            connects: t.connects,
            https: t.https,
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
    /// **ファイルに書いた所までの通算** (T14.9)
    written: u64,
    /// 起動時に状態ファイルから読み戻した枚数 (`/bursts` の `"restored"`)
    restored: usize,
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
    ///
    /// `PROXY_RECORDS=off` なら**鍵も取らずに捨てる** (T14.41)。写真の中身は
    /// `/connections` の表から作るので、`off` ではそもそも接続元が 1 つも入らない
    pub fn push(&self, shot: BurstShot) {
        if !crate::records::recording() {
            return;
        }
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

    /// まだファイルに書いていない枚数を古い順で取り出す (**history スレッドだけが呼ぶ**。T14.9)。
    pub fn take_unwritten(&self, max: usize) -> (Vec<BurstShot>, u64) {
        let mut r = self.inner.locked();
        let g = &mut *r;
        drain_unwritten(&g.buf, g.next, MAX_BURSTS, g.total, &mut g.written, max)
    }

    /// 置き場を [`MAX_BURSTS`] 枚ぶん確保して触る (**起動時に 1 回だけ**。T17.8)。
    ///
    /// 触るのは 1 枚の固定部だけで、写真の中の一覧は撮るときに確保する。返すのは触ったバイト数。
    pub fn prefault(&self) -> usize {
        crate::prefault::vec(&mut self.inner.locked().buf, MAX_BURSTS)
    }

    /// 状態ファイルから読み戻す (**起動時に 1 回だけ**。T14.9)。
    ///
    /// 通し番号 (`seq`) は `total` から作るので、読み戻すと再起動前の続きから振られる。
    pub fn restore(&self, shots: Vec<BurstShot>) {
        let n = shots.len().min(MAX_BURSTS);
        for shot in shots {
            self.push(shot);
        }
        let mut r = self.inner.locked();
        r.written = r.total;
        r.restored = n;
    }

    /// 再起動前から引き継いだ枚数 (`/bursts` の `"restored"`)。
    pub fn restored(&self) -> usize {
        self.inner.locked().restored
    }
}

/// 山の写真の単体試験 (T14.6。T.34.2 で別のファイルに移した)。
#[cfg(test)]
#[path = "burst_tests.rs"]
mod tests;
