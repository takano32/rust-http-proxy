//! いま開いている接続の表 ([`ConnTable`]。`/connections`) と、空回りしている接続の
//! [`SpinningConn`]。`recent` の一部 (T25.2 で `recent.rs` から `conns.rs` へ移し、T.34.2 で
//! 枠 ([`ConnSlot`]) と分けた)。
//!
//! **表の鍵を取るのは接続の開始と終了の 2 回だけ** ([`ConnTable::register`] /
//! [`ConnTable::unregister`]) で、`PROXY_MAX_CONNS_PER_CLIENT` を設定したときだけ accept の判定で
//! 1 回増える (T14.13)。history スレッドの周期が回す [`ConnTable::update_rates`] /
//! [`ConnTable::spinning`] も同じ鍵。接続元ごとの本数の帳簿は [`Conns`] の 3 つの表で持つ。
//!
//! 行を動かしただけで中身は変えていない。外から見える名前は `recent` の `pub use` でそのまま。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::ConnSlot;
use crate::sync::LockExt;

/// いま開いている接続の表 (`/connections`)。
///
/// **登録と抹消は接続の開始と終了で 1 回ずつだけ** (要求ごとには触らない)。
/// `--lite` では [`ConnTable::set_enabled`] で切り、登録もしない (空の一覧を返す。T1.4 の方針)。
///
/// `PROXY_MAX_CONNS_PER_CLIENT` が設定されている間だけ、**同じ鍵の内側で**接続元ごとの
/// 本数も持つ ([`ConnTable::at_client_limit`]。T14.13)。accept のたびに 240 本を数え直さずに
/// 済ませるためで、上限が `0` (既定) のときはこの表を作らない。
pub struct ConnTable {
    on: AtomicBool,
    /// 全接続の転送速度の和 (`/status` の `rate_bps_total`。T14.39)。
    /// 書くのは history スレッドの周期だけで、`/status` は原子を 1 回読む
    rate_total: AtomicU64,
    /// 接続元ごとの本数を数えているか (`PROXY_MAX_CONNS_PER_CLIENT` が設定されている間だけ)。
    /// **`--lite` でも数える**: 枠 ([`ConnSlot`]) を作らないまま本数だけ ±1 できるので、
    /// 記録を全部止めたプロファイルでも公平さの上限は効く (T14.13)
    counting: AtomicBool,
    inner: Mutex<Conns>,
}

/// [`ConnTable`] が 1 つの鍵で守るもの。
#[derive(Default)]
struct Conns {
    /// 接続 id → 枠 (`/connections`)
    slots: HashMap<u64, Arc<ConnSlot>>,
    /// 接続元 → いま生きている本数 (T14.13。数えているときだけ中身がある)
    per_client: HashMap<Arc<str>, u32>,
    /// 接続 id → 接続元 (抹消のときに鍵を引くため。`--lite` で枠が無くても引ける)
    client_of: HashMap<u64, Arc<str>>,
}

impl Conns {
    /// この接続元の本数を 1 増やす (**呼び出し側が鍵を持っている**)。
    fn add_client(&mut self, id: u64, client: &str) {
        let key: Arc<str> = Arc::from(client);
        *self.per_client.entry(Arc::clone(&key)).or_insert(0) += 1;
        self.client_of.insert(id, key);
    }

    /// この接続の接続元の本数を 1 減らす (数えていない接続なら何もしない)。
    fn drop_client(&mut self, id: u64) {
        let Some(key) = self.client_of.remove(&id) else {
            return;
        };
        if let Some(n) = self.per_client.get_mut(&key) {
            *n -= 1;
            if *n == 0 {
                // 0 本になった接続元は落とす (見知らぬ接続元で表が伸び続けないように)
                self.per_client.remove(&key);
            }
        }
    }

    /// いま生きている接続から数え直す (数え始めるとき)。
    ///
    /// `--lite` は枠を持たないので 0 から数え始める (= 切り替えより前から居る接続は
    /// 入らない。閉じるときも引かないので数が狂うことはない)。
    fn recount_clients(&mut self) {
        let live: Vec<(u64, Arc<str>)> = self
            .slots
            .iter()
            .map(|(id, slot)| (*id, Arc::from(slot.client.as_str())))
            .collect();
        self.forget_clients();
        for (id, key) in live {
            *self.per_client.entry(Arc::clone(&key)).or_insert(0) += 1;
            self.client_of.insert(id, key);
        }
    }

    fn forget_clients(&mut self) {
        self.per_client.clear();
        self.client_of.clear();
    }
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
            rate_total: AtomicU64::new(0),
            counting: AtomicBool::new(false),
            inner: Mutex::new(Conns::default()),
        }
    }

    /// 記録するかどうか (`--lite` では `false`)。起動時に 1 回だけ呼ぶ。
    pub fn set_enabled(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
        if !on {
            // 接続元ごとの本数は `--lite` でも持てるので、捨てるのは枠だけ
            self.inner.locked().slots.clear();
            self.rate_total.store(0, Ordering::Relaxed);
        }
    }

    pub fn enabled(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// 接続元ごとの本数を数えているか (`PROXY_MAX_CONNS_PER_CLIENT` が設定されている間。T14.13)。
    pub fn counting_clients(&self) -> bool {
        self.counting.load(Ordering::Relaxed)
    }

    /// この接続元の生きている接続が `limit` 本以上か (`PROXY_MAX_CONNS_PER_CLIENT`。T14.13)。
    ///
    /// **accept ごとに 1 回**、上限が設定されているときだけ呼ぶ。取るのは表の鍵 1 回
    /// (登録・抹消と同じ鍵) で、数えるのは `HashMap` を 1 回引くだけ。初めて呼ばれたときに
    /// 「数える」へ切り替え、そのとき生きている接続から数え直す。
    pub fn at_client_limit(&self, client: &str, limit: usize) -> bool {
        // 表の鍵は `register` と同じ**記録の形** (`PROXY_RECORDS=hashed` ならハッシュ)
        // なので、引くときも同じ 1 関数を通す。判定そのものは呼ぶ側が渡した**生の IP**
        // から始まっていて、同じ接続元は必ず同じ鍵になる = 数え方は 3 つの値で同じ (T14.41)
        let key = crate::records::client_key(client);
        let mut g = self.inner.locked();
        if !self.counting.load(Ordering::Relaxed) {
            g.recount_clients();
            self.counting.store(true, Ordering::Relaxed);
        }
        g.per_client
            .get(key.as_ref())
            .is_some_and(|&n| n as usize >= limit)
    }

    /// 数えるのをやめて表を捨てる (上限を `0` に戻したとき。T14.13)。
    pub fn stop_counting_clients(&self) {
        let mut g = self.inner.locked();
        self.counting.store(false, Ordering::Relaxed);
        g.forget_clients();
    }

    /// この接続元のいまの本数 (数えていなければ `0`)。
    pub fn client_conns(&self, client: &str) -> u32 {
        let key = crate::records::client_key(client);
        self.inner
            .locked()
            .per_client
            .get(key.as_ref())
            .copied()
            .unwrap_or(0)
    }

    /// 接続を 1 本登録する (接続の開始で 1 回だけ)。`--lite` なら `None`。
    ///
    /// **`PROXY_RECORDS=off` は `--lite` と同じ扱い** (枠を作らない = `/connections` も
    /// `/recent` も `/bursts` も空)。ただし接続元ごとの本数は `--lite` と同じく数え続ける
    /// ので、`PROXY_MAX_CONNS_PER_CLIENT` の公平さの上限は `off` でも効く (T14.41)。
    /// **旗を見るのはここ 1 回**で、抹消 ([`ConnTable::unregister`]) は見ない —
    /// 途中で `on` → `off` に変えたときに、既に登録してある枠が表に残ってしまうため
    pub fn register(&self, id: u64, client: &str, started: Instant) -> Option<Arc<ConnSlot>> {
        let on = self.enabled() && crate::records::recording();
        // `--lite` で上限も使っていないときは鍵を取らない (今までどおり費用 0)
        if !on && !self.counting_clients() {
            return None;
        }
        // 接続元は**この 1 回**だけ記録の形に直し、枠と本数の表の両方に同じ値を使う
        // (`client` は生のまま残す — 下の自己ベンチの判定が IP そのものを見るため)
        let key = crate::records::client_key(client);
        let mut g = self.inner.locked();
        if self.counting.load(Ordering::Relaxed) {
            g.add_client(id, &key);
        }
        if !on {
            return None;
        }
        // 起動時の自己ベンチ (T14.43) が自分で打った接続は枠を作らない (`--lite` と同じ扱い)。
        // 作ると 2,000 本の CONNECT が `/recent` の 4,096 件を半分埋め、その個票が
        // 状態ファイルに残ってしまう。**接続元ごとの上限 (すぐ上の `add_client`) には
        // ちゃんと数える** (プロキシから見れば本物の負荷なので)。
        // 費用は自己ベンチが回っていないときの原子の読み 1 回
        if crate::selfbench::is_client(client) {
            return None;
        }
        let slot = Arc::new(ConnSlot::new(id, &key, started));
        g.slots.insert(id, Arc::clone(&slot));
        Some(slot)
    }

    /// 接続を 1 本抹消し、その枠を返す (接続の終了で 1 回だけ)。
    ///
    /// 返した枠から閉じた接続の個票を作る (`/recent`。T14.4)。`--lite` と、
    /// 既に抹消済み (2 回目) では `None`。
    pub fn unregister(&self, id: u64) -> Option<Arc<ConnSlot>> {
        let on = self.enabled();
        if !on && !self.counting_clients() {
            return None;
        }
        let mut g = self.inner.locked();
        if self.counting.load(Ordering::Relaxed) {
            g.drop_client(id);
        }
        if !on {
            return None;
        }
        g.slots.remove(&id)
    }

    /// 今の一覧を**古い順** (通し番号の小さい順) で返す。
    pub fn snapshot(&self) -> Vec<Arc<ConnSlot>> {
        let mut v: Vec<Arc<ConnSlot>> =
            self.inner.locked().slots.values().map(Arc::clone).collect();
        v.sort_by_key(|s| s.id);
        v
    }

    /// 全 slot の `bytes` を控え直して、直近 `ms` の転送速度を書く (T14.39)。
    ///
    /// **呼ぶのは history スレッドの周期 (本番は 5 秒) の 1 行だけ**で、接続の経路
    /// (accept / 中継 / close) には 1 命令も増えない。費用は 240 本で原子の読み書き
    /// 240 回ぶんと表の鍵 1 回 (数 us)。`snapshot` ではなくここで鍵を取って回すのは、
    /// 240 本ぶんの [`Arc`] の複製と並べ替えの方が控える仕事より重いため。
    /// 戻り値と `/status` の `rate_bps_total` は**全接続の和**。
    pub fn update_rates(&self, ms: u64) -> u64 {
        let mut total: u64 = 0;
        {
            let g = self.inner.locked();
            for slot in g.slots.values() {
                total = total.saturating_add(slot.sweep_rate(ms));
            }
        }
        self.rate_total.store(total, Ordering::Relaxed);
        total
    }

    /// 全接続の転送速度の和 (`/status` の `rate_bps_total`)。原子の読み 1 回。
    pub fn rate_bps_total(&self) -> u64 {
        self.rate_total.load(Ordering::Relaxed)
    }

    /// **直近の周期に `min_delta` 回以上空回りした接続**を、多い順に `max` 本まで
    /// (と、閾を越えた接続の総数)。T15.0 (6)。
    ///
    /// **呼ぶのは history スレッドの周期だけ** ([`crate::anomaly`] の判定) で、接続の
    /// 経路には 1 命令も増えない。読むのは [`ConnTable::update_rates`] が同じ周期に
    /// 書いた値なので、`update_rates` の**あと**に呼ぶこと。
    /// 平常時は 1 本も越えないので、確保もほとんど起きない。
    ///
    /// **鍵の内側では文字列を作らない**: 表の鍵は accept の経路が取り合うもので、
    /// 事故の最中 (越えた接続が何十本もある) こそ長く握ってはいけない。内側で
    /// 拾うのは枠の参照 (原子の足し算 1 回) だけにして、宛先の複製は上位 `max` 本に
    /// 絞ってから行う (呼ぶ側は `max = 1`)。
    pub fn spinning(&self, min_delta: u64, max: usize) -> (Vec<SpinningConn>, usize) {
        let now = Instant::now();
        let mut over: Vec<(u64, u64, Arc<ConnSlot>)> = Vec::new();
        {
            let g = self.inner.locked();
            for slot in g.slots.values() {
                let spins_delta = slot.spins_delta();
                if spins_delta < min_delta {
                    continue;
                }
                over.push((spins_delta, slot.id, Arc::clone(slot)));
            }
        }
        let total = over.len();
        // 多い順 (同点は通し番号の小さい方 = 古い方が先)
        over.sort_by_key(|&(spins_delta, id, _)| (std::cmp::Reverse(spins_delta), id));
        over.truncate(max);
        let found = over
            .into_iter()
            .map(|(spins_delta, id, slot)| SpinningConn {
                id,
                // 鍵を放してから複製する (取るのは枠ごとの鍵で、表の鍵ではない)
                target: slot.target.locked().clone(),
                age_secs: now.saturating_duration_since(slot.started).as_secs(),
                half_closed: slot.half_closed_side(),
                spins_delta,
            })
            .collect();
        (found, total)
    }

    pub fn len(&self) -> usize {
        self.inner.locked().slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 「いま空回りしている」接続 1 本 ([`ConnTable::spinning`] が返す。T15.0 (6))。
///
/// 異常の説明 1 件は [`crate::events::MAX_TEXT`] (128 B) で切られるので、持つのは
/// **1 行に書けるものだけ** (どの接続か・どこ宛てか・いつからか・半閉じか)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpinningConn {
    /// 接続の通し番号 (ログの `conn#N` と `/connections` の `id`)
    pub id: u64,
    /// 宛先 (`host:port`)
    pub target: String,
    /// 受けてからの秒
    pub age_secs: u64,
    /// 半閉じの向き (`"client"` / `"origin"`。まだなら `None`)
    pub half_closed: Option<&'static str>,
    /// 直近の周期に空回りした回数
    pub spins_delta: u64,
}

/// 接続の表の単体試験 (T.34.2 で別のファイルに移した)。
#[cfg(test)]
#[path = "conn_table_tests.rs"]
mod tests;
