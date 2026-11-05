//! 名前解決の結果を短時間キャッシュする。
//!
//! `getaddrinfo(3)` は TTL を返さないので、[`set_ttl`] で与えた固定の TTL (`PROXY_DNS_TTL_SECS`、
//! 既定 60 秒、0 で無効) だけ保持する。**直近 TTL 内に使われた名前は期限の 3/4 を過ぎたところで
//! 裏で引き直す**ので、熱いホストは期限切れのミス (デプロイ先で 1 回 約 10 ms、混むと 100 ms 超)
//! を払わない (T13.1)。
//!
//! それだけでは**熱さが TTL に縛られる**ので、間隔が TTL より長いホスト (デプロイ先の主要 3 件は
//! 2〜10 分間隔) は 1 つも救えなかった。そこで熱さの窓を TTL から切り離し、**直近 [`warm_window`]
//! 秒 (`PROXY_DNS_WARM_SECS`、既定 3,600 秒、0 で無効) に 2 回以上使われた名前 (= warm) は、
//! 使われていなくても 3/4 TTL ごとに裏で引き直し続ける** (keep-warm。T14.1)。warm でいられるのは
//! 同時に [`MAX_WARM`] 件までで、最後の使用から窓を過ぎた名前は待ち行列から外れる。
//! **T22.4 から既定は止めてある** ([`WARM_DEFAULT`] = 0): serve-stale (下) のもとでは、warm が
//! 救っていた期限切れのミスは古い答えの当たりになり、warm の引き直し (1 名前 80 回/時) はミスを
//! ほとんど減らさない。`PROXY_DNS_WARM_SECS=3600` で T21.3 までの動きに戻る。
//!
//! 解決に失敗したときは [`STALE_MAX`] 以内の古い結果を使い (オリジンの
//! DNS 障害でトンネルが全滅しないように)、失敗そのものも [`negative_ttl`]
//! (`PROXY_DNS_NEGATIVE_SECS`、既定 60 秒、0 で覚えない) の間覚えて連続した再解決を抑える。
//! IP リテラルはキャッシュしない。
//!
//! **serve-stale (RFC 8767。T21.3)**: TTL を過ぎていても齢が [`serve_stale`]
//! (`PROXY_DNS_STALE_SECS`、既定 21,600 秒、0 で止める) 未満の答えは、**同期で引かずにそのまま返し**、
//! その名前の引き直しを裏に 1 回だけ頼む (keep-warm と同じ `dns-refresh` スレッド)。間遠な名前の
//! 期限切れのミス (デプロイ先で 1 回 約 35 ms、ミスの 6 割) を利用者に待たせないため。古い答えで
//! 繋げなかったときは、呼び出し側 (`proxy-net-conn` の `connect_with`) が [`relookup_after_stale`] で
//! 引き直し、番地が変わっていれば 1 回だけ繋ぎ直す。

use crate::sync::LockExt;
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
// 本体では使わない。テスト (`use super::*`) の `thread::sleep` のために残す (T21.7)
#[cfg(test)]
use std::thread;
use std::time::{Duration, Instant};

// 責務ごとに子のファイルへ分けてある (T21.7。T.34.54 で `dns/` の下に置き直した)。子は親の
// private な表とカウンタをそのまま触るので、中身は `dns.rs` にあった頃と同じ。公開の名前は
// ここで出し直す (`proxy_net_dns::dns::…` のまま)。
//
// - `cost`: スレッドごとの費用の箱 (名前解決の費用・族・SYN の再送) と、その take / peek / note
// - `entry`: 表の 1 行 (`Entry`)、鍵の内側で決める `Looked` / `MissKind`、行の追い出し
// - `resolved`: 判定と接続が同じ答えを使うための `Resolved` と `resolve*` の入口
// - `lookup`: 表を引く本体 (`resolve_host_marked`) と、古い答えで繋げなかったときの引き直し
mod cost;
mod entry;
mod lookup;
mod view;
mod warm;
// 要求の経路の名前解決を上限付きの別のスレッド群に逃がし、締め切りで待つのをやめる (T.32.11)
mod pool;
mod resolved;
pub use cost::{
    note_family, note_resolve_cost, note_syn_retrans, peek_resolve_cost, resolve_cost_total,
    take_family, take_resolve_cost, take_syn_retrans,
};
use entry::{Entry, addrs_changed, evict_oldest};
pub use lookup::{relookup_after_stale, resolve_host_marked};
pub use pool::{
    LIMIT_DEFAULT as RESOLVER_LIMIT_DEFAULT, TIMEOUT_DEFAULT, counters as resolver_counters,
    is_timeout, limit as resolver_limit, limit_for_threads as resolver_limit_for_threads,
    set_limit as set_resolver_limit, set_timeout, timeout,
};
pub use resolved::{Resolved, resolve, resolve_host, resolve_with_pref, resolve_with_pref_marked};
pub use view::{DnsSort, MAX_ROW_ADDRS, TableRow, counters, entries, status_json, table};
use warm::{refresh_after, request_refresh, wake_refresher, warm_clear, warm_promote};

/// 解決に失敗したとき、この時間以内の古い結果なら使う。
pub const STALE_MAX: Duration = Duration::from_secs(3600);
/// 失敗を覚えておく時間の既定 (`PROXY_DNS_NEGATIVE_SECS` で変える。0 で覚えない)。
///
/// 5 秒から 60 秒にした (T13.1)。デプロイ先の 58.6 時間で解決の失敗は 80 件・**1 回 約 2 秒**
/// あり、5 秒では「直後の再試行」(負のキャッシュの当たり 81 件) しか捉えられていなかった。
pub const NEGATIVE: Duration = Duration::from_secs(60);
/// keep-warm を使うときの窓 (`PROXY_DNS_WARM_SECS=3600`。0 で keep-warm を止める)。T21.3 までの既定で、
/// T22.4 から既定は [`WARM_DEFAULT`] (0)。
///
/// デプロイ先の主要ホストのアクセス間隔は 2〜10 分 (`/dns` の 90 件のうち 60 秒以内に
/// 使われたものは 2 件しか無かった。T14.1)。900 秒から **3,600 秒**に広げた (T15.4):
/// `discord.com` は 2〜4 本の塊が約 1,800 秒おきに来るので 900 秒では毎回外れ、
/// T15.0 の版の 9 時間ではミス 26 回のうち 25 回が窓の外だった。
pub const WARM: Duration = Duration::from_secs(3600);
/// `PROXY_DNS_WARM_SECS` の既定 (T22.4)。**0 = keep-warm を止める** (裏の引き直しは、TTL の 3/4 を
/// 過ぎた熱い名前の先回り (T13.1) と、serve-stale で古い答えを返したときの 1 回だけになる)。
///
/// `scripts/dns-replay.py --grid` の再生で決めた (serve-stale 21,600 秒のもとで、雪像 3 枚 =
/// tokyo 2026-10-15・2026-10-17、nagoya 2026-10-08)。warm を止めると引き直しは今の 5〜27% になり、
/// `expired` + `warm_stale` のミスは 3 枚とも増えなかった (目安「引き直し半分以下・ミスの増え 1 日
/// 10 回以内」を満たしたのはこれだけ。枠 16 / 64 と窓 900 秒は届かない)。serve-stale では warm が
/// 期限切れのミスを救えるのは、間隔が「serve-stale の窓 〜 その窓 + warm の窓」の名前だけになる。
/// 代わりに古い答えで繋ぎに行く回が増える (再生で 4.6〜16 倍。`/status` の `dns.stale_hits`)。
/// T21.3 までの値 ([`WARM`] = 3,600 秒) に戻すときは `PROXY_DNS_WARM_SECS=3600`。
pub const WARM_DEFAULT: Duration = Duration::ZERO;
/// 期限切れの答えを同期で引かずに返してよい齢の既定 (`PROXY_DNS_STALE_SECS` で変える。0 で止める。T21.3)。
///
/// `scripts/dns-replay.py --serve-stale` の再生で決めた (雪像 3 枚 × 3,600 / 21,600 / 86,400 秒)。
/// 3,600 秒では tokyo (2026-10-15) の `expired` が 62%、nagoya (2026-10-01) が 79% しか減らず、
/// **21,600 秒が 3 枚とも「`expired` が半分以下・引き直し +10% 以内」を満たすいちばん短い窓**だった
/// (長いほど古い番地に繋ぎに行く見込みが増えるので、満たす中で短い方を採る)。
pub const SERVE_STALE: Duration = Duration::from_secs(21_600);
/// 保持するホスト数の上限 (超えたら最も古いものを捨てる)。
const MAX_ENTRIES: usize = 4096;
/// 同時に warm でいられる名前の上限 (超えたら最後の使用が最も古いものを外す)。
///
/// 最悪の引き直しは `MAX_WARM / (3/4 × TTL)` = 32 / 45 秒 ≈ **0.7 回/秒**。
/// デプロイ先のリゾルバ (Docker の内蔵 DNS、1 回 10 ms 前後) に迷惑をかけない数 (T14.1)。
const MAX_WARM: usize = 32;

static TTL_SECS: AtomicU64 = AtomicU64::new(60);
static NEGATIVE_SECS: AtomicU64 = AtomicU64::new(NEGATIVE.as_secs());
static WARM_SECS: AtomicU64 = AtomicU64::new(WARM_DEFAULT.as_secs());
static SERVE_STALE_SECS: AtomicU64 = AtomicU64::new(SERVE_STALE.as_secs());
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static STALE: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU64 = AtomicU64::new(0);
/// 期限前に裏で引き直した回数 (T13.1)。利用者は待っていないので**ミスとは別に数える**。
static REFRESHES: AtomicU64 = AtomicU64::new(0);
/// 引き直しで**答えの集合が前と変わった**回数 (`/status` の `dns.changes`。T14.37)。
///
/// TTL 60 秒が長いか短いかは「答えがどれくらいの頻度で変わるか」で決まるが、その数字が
/// 無かった。引き直し (keep-warm と期限切れの再解決) は元々 1 つ前の答えを手元に持って
/// いるので、**比べて数えるだけ**で CDN のローテーション頻度が読める。書くのは引き直しの
/// 経路 (稀) だけで、当たりの経路には 1 命令も増えない。
static CHANGES: AtomicU64 = AtomicU64::new(0);
/// ミスのときに `getaddrinfo` に費やした時間の合計 (us)。**ミスの経路でしか書かない**
/// ので、当たりの経路 (熱い方) には原子操作が 1 つも増えない (T12.4 (2))。
/// 裏の引き直しのぶんも入れない (待っていない時間を「ミス 1 回の値段」に混ぜない)。
static RESOLVE_US_SUM: AtomicU64 = AtomicU64::new(0);
/// ミスの種類別の回数 (`/status` の `dns.misses_by_kind`。T15.0 (7))。並びは
/// [`MissKind::index`] (cold / expired / warm_stale / negative) で、**4 つの和は必ず
/// `MISSES` と一致する**。窓 (`PROXY_DNS_WARM_SECS`) を延ばすのと TTL を延ばすのとの
/// どちらが効くかは、この内訳でしか決まらない (T15.4 の材料): `expired` が主なら
/// 窓の外で期限が切れている、`warm_stale` が出ていれば裏の引き直しが間に合っていない。
static MISS_COLD: AtomicU64 = AtomicU64::new(0);
static MISS_EXPIRED: AtomicU64 = AtomicU64::new(0);
static MISS_WARM_STALE: AtomicU64 = AtomicU64::new(0);
static MISS_NEGATIVE: AtomicU64 = AtomicU64::new(0);
/// 裏の引き直しが失敗した回数と、その `getaddrinfo` に費やした時間 (us の合計と最大)。
/// **書くのは `dns-refresh` スレッド 1 本だけ**なので、要求の経路には 1 命令も増えない。
/// 引き直しは失敗しても `resolved_at` を進めない (= 期限が来ればミスになる) ので、
/// `misses_by_kind.warm_stale` と対で読む。
static REFRESH_FAILURES: AtomicU64 = AtomicU64::new(0);
static REFRESH_US_SUM: AtomicU64 = AtomicU64::new(0);
static REFRESH_US_MAX: AtomicU64 = AtomicU64::new(0);
/// 予定の時刻から [`REFRESH_LATE_AFTER`] 以上遅れて始まった引き直しの回数 (T15.0 (7))。
/// 引き直しは 1 本のスレッドが順にやるので、1 回 約 2 秒かかる `getaddrinfo` が続くと
/// 後ろが詰まる。**要求の経路が頼んだ先回り (`Msg::Refresh`) は予定の時刻を持たない**
/// ので数えない。
static REFRESH_LATE: AtomicU64 = AtomicU64::new(0);
/// 引き直しが「遅れた」と数える閾。
const REFRESH_LATE_AFTER: Duration = Duration::from_secs(5);
/// warm の枠 ([`MAX_WARM`]) が満杯で、新しく warm になる名前のために**ほかの名前を外した**回数
/// (`/status` の `dns.warm_evicted`。T16.0)。`/history` の `dns_warm` は 5 秒ごとの瞬間値なので、
/// 枠が埋まって押し出しが起きていたかはこれでしか分からない。
///
/// **「要求の経路は増やさない」の例外**: 書くのは [`warm_promote`] の満杯の枝だけ
/// (その名前が窓の中で 2 回目に使われ、かつ枠が満杯のとき)。既に `TABLE` と `WARM_QUEUE` の
/// 鍵を握っている内側の原子の加算 1 つで、システムコールも確保も増えない。
static WARM_EVICTED: AtomicU64 = AtomicU64::new(0);
/// 期限切れの答えを serve-stale で返した回数 (`/status` の `dns.stale_hits`。T21.3)。
/// **ミスには数えない** (`misses` / `misses_by_kind` に入らない)。`stale_served` (引き直しが
/// 失敗して古い答えで凌いだ回数) とは別物。書くのは期限切れの経路だけで、当たりの経路は増えない。
static STALE_HITS: AtomicU64 = AtomicU64::new(0);
/// 古い答えで繋げず、[`relookup_after_stale`] で引き直した回数 (`/status` の `dns.stale_retries`。T21.3)。
/// 表に新しい答えが既にあったときも数える。同期で `getaddrinfo` を待った回は
/// **ミス (`expired`) にも数える** (利用者が待ったので)。
static STALE_RETRIES: AtomicU64 = AtomicU64::new(0);

static TABLE: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

/// warm な名前の待ち行列 (T14.1)。`(次に引き直す時刻, 名前)` の順に並ぶので、先頭が
/// いつも次の仕事になる。`refresher` スレッドが先頭の期限まで眠り、起きたら 1 件処理する。
///
/// **鍵の順は `TABLE` → `WARM_QUEUE`** (逆には取らない)。1 つの名前は高々 1 件。
static WARM_QUEUE: Mutex<BTreeSet<(Instant, String)>> = Mutex::new(BTreeSet::new());

/// 解決の回数 (`HITS` / `MISSES`) を数えるテストは、表もカウンタも全テストで共有している
/// ので直列に回す。`dns` と `acl` と `net` のテストが同じロックを取る。
///
/// **T15.12 でクレートを 3 つに割ってからは、`acl` と `net` のテストは別プロセスで走る**
/// ので、このロックが効くのは同じクレートの中だけになった (`proxy-metrics-recent` の
/// `events::TEST_LOCK` と同じ作法で、クレートをまたぐために `pub` にしてある)。
pub static RESOLVE_TEST_LOCK: Mutex<()> = Mutex::new(());

/// キャッシュの TTL。0 で無効 (毎回解決)。
pub fn set_ttl(ttl: Duration) {
    TTL_SECS.store(ttl.as_secs(), Ordering::Relaxed);
}

pub fn ttl() -> Duration {
    Duration::from_secs(TTL_SECS.load(Ordering::Relaxed))
}

/// 失敗を覚えておく時間 (`PROXY_DNS_NEGATIVE_SECS`)。0 で覚えない。
pub fn set_negative_ttl(ttl: Duration) {
    NEGATIVE_SECS.store(ttl.as_secs(), Ordering::Relaxed);
}

pub fn negative_ttl() -> Duration {
    Duration::from_secs(NEGATIVE_SECS.load(Ordering::Relaxed))
}

/// warm と見なす窓 (`PROXY_DNS_WARM_SECS`)。0 で keep-warm を止める (T13.1 の動きに戻る)。
///
/// 0 にしたときは待ち行列もその場で空にする (次の期限まで最長 3/4 TTL 待たない)。
pub fn set_warm_window(window: Duration) {
    WARM_SECS.store(window.as_secs(), Ordering::Relaxed);
    if window.is_zero() {
        warm_clear();
    }
}

pub fn warm_window() -> Duration {
    Duration::from_secs(WARM_SECS.load(Ordering::Relaxed))
}

/// 期限切れの答えを同期で引かずに返してよい齢 (`PROXY_DNS_STALE_SECS`。T21.3)。0 で止める
/// (今までどおり期限切れは同期で引く)。TTL 以下の値も実質止めたのと同じ。
pub fn set_serve_stale(window: Duration) {
    SERVE_STALE_SECS.store(window.as_secs(), Ordering::Relaxed);
}

pub fn serve_stale() -> Duration {
    Duration::from_secs(SERVE_STALE_SECS.load(Ordering::Relaxed))
}

/// いま warm な名前の数 (`/status` の `dns.warm`)。
pub fn warm_count() -> usize {
    WARM_QUEUE.locked().len()
}

/// `getaddrinfo` の代わりに呼ぶ関数の型 ([`set_resolver_for_test`])。
pub type ResolveFn = fn(&str, u16) -> io::Result<Vec<IpAddr>>;

/// 名前解決の差し替え (**テストの口**。T.32.2)。`None` なら `getaddrinfo` を呼ぶ。
///
/// 鍵を取るのは OS に問い合わせるミスの経路だけ (1 回 ms の単位) で、当たりの経路と
/// IP リテラルは触らない。本番の経路からは書かない。
static RESOLVER_FOR_TEST: Mutex<Option<ResolveFn>> = Mutex::new(None);

/// 名前解決の関数を差し替える (**テストの口**。T.32.2)。`None` で `getaddrinfo` に戻す。
/// 引けない名前を何万個も作る試験や、応答しないリゾルバの試験 (結合試験は同じプロセスの中で
/// プロキシを立てるので、ここを差し替えれば要求の経路の名前解決が変わる) のため。
#[doc(hidden)]
pub fn set_resolver_for_test(f: Option<ResolveFn>) {
    *RESOLVER_FOR_TEST.locked() = f;
}

fn system_resolve(host: &str, port: u16) -> io::Result<Vec<IpAddr>> {
    if let Some(f) = *RESOLVER_FOR_TEST.locked() {
        return f(host, port);
    }
    let addrs: Vec<IpAddr> = (host, port).to_socket_addrs()?.map(|a| a.ip()).collect();
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve host",
        ));
    }
    Ok(addrs)
}

/// **表を通さずに** OS へ問い合わせる (canary。T14.10)。
///
/// 表には書かず、当たり外れも数えない: canary が測るのは**リゾルバの実力**で、
/// 1 分に 1 回の自分の問い合わせを利用者の統計 (`dns.hits` / `dns.misses` と
/// ミス 1 回の平均) に混ぜてしまうと、両方が読めなくなる。keep-warm (T14.1) の
/// 窓にも入らない (canary が触った名前が warm になっては本末転倒)。
pub fn resolve_uncached(host: &str, port: u16) -> io::Result<Vec<IpAddr>> {
    system_resolve(host, port)
}

/// `misses_by_kind` の JSON (`/status` の `dns` と `/dns` の 1 行で同じ形)。
/// 並びは [`MissKind::index`]。
fn misses_by_kind_json(v: &[u64; 4]) -> String {
    format!(
        "{{\"cold\":{},\"expired\":{},\"warm_stale\":{},\"negative\":{}}}",
        v[0], v[1], v[2], v[3]
    )
}

/// 表に答えを 1 件、**齢を指定して**置く (**テストの口**。結合テストが serve-stale の
/// 「答えが変わった名前」を作るため。T21.3)。本番の経路からは呼ばない。
#[doc(hidden)]
pub fn put_answer_for_test(host: &str, addrs: &[IpAddr], age: Duration) {
    let now = Instant::now();
    let mut guard = TABLE.locked();
    let table = guard.get_or_insert_with(HashMap::new);
    let mut e = Entry::empty(now.checked_sub(age).unwrap_or(now), None);
    e.addrs = addrs.to_vec();
    table.insert(host.to_ascii_lowercase(), e);
}

/// このホストで最後に接続できた族 (`Some(true)` = IPv6)。覚えていなければ `None`。
pub fn preferred_family(host: &str) -> Option<bool> {
    if host.is_empty() || host.parse::<IpAddr>().is_ok() {
        return None;
    }
    let key = host.to_ascii_lowercase();
    TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(&key))
        .and_then(|e| e.last_win_v6)
}

/// このホストで勝った族を覚える。**答えが変わるときだけ**呼ぶこと (定常状態では鍵を取らない)。
/// IP リテラルは覚えない (族は見れば分かるし、表に載せる意味が無い)。
pub fn remember_family(host: &str, v6: bool) {
    // TTL 0 (キャッシュ無効) のときは読む側が表を見ないので、覚えても引かれない
    if host.is_empty() || ttl().is_zero() || host.parse::<IpAddr>().is_ok() {
        return;
    }
    let key = host.to_ascii_lowercase();
    let mut guard = TABLE.locked();
    let table = guard.get_or_insert_with(HashMap::new);
    if let Some(e) = table.get_mut(&key) {
        e.last_win_v6 = Some(v6);
        return;
    }
    if table.len() >= MAX_ENTRIES {
        evict_oldest(table);
    }
    // まだ引いていないホスト (TTL 0 や解決を経ない経路) でも記憶だけは置ける。
    // `addrs` が空なので当たりにはならず、次の解決で埋まる
    table.insert(key, Entry::empty(Instant::now(), Some(v6)));
}

/// 覚えている結果を全部捨てる (`.env` の TTL 変更やテスト用)。warm の待ち行列も空にする。
pub fn clear() {
    let mut guard = TABLE.locked();
    let mut q = WARM_QUEUE.locked();
    if let Some(t) = guard.as_mut() {
        t.clear();
    }
    q.clear();
}

/// serve-stale (T21.3) の単体テスト。dns.rs が 2,000 行を超えていたので別のファイルに置く。
#[cfg(test)]
mod stale_tests;

/// `dns` の単体テスト (T21.7 で別のファイルに移した)。
#[cfg(test)]
mod tests;
