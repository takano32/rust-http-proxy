//! 名前解決の結果を短時間キャッシュする。
//!
//! `getaddrinfo(3)` は TTL を返さないので、[`set_ttl`] で与えた固定の TTL (`PROXY_DNS_TTL_SECS`、
//! 既定 60 秒、0 で無効) だけ保持する。**直近 TTL 内に使われた名前は期限の 3/4 を過ぎたところで
//! 裏で引き直す**ので、熱いホストは期限切れのミス (デプロイ先で 1 回 約 10 ms、混むと 100 ms 超)
//! を払わない (T13.1)。解決に失敗したときは [`STALE_MAX`] 以内の古い結果を使い (オリジンの
//! DNS 障害でトンネルが全滅しないように)、失敗そのものも [`negative_ttl`]
//! (`PROXY_DNS_NEGATIVE_SECS`、既定 60 秒、0 で覚えない) の間覚えて連続した再解決を抑える。
//! IP リテラルはキャッシュしない。

use crate::log_warn;
use crate::sync::LockExt;
use std::cell::Cell;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// 解決に失敗したとき、この時間以内の古い結果なら使う。
pub const STALE_MAX: Duration = Duration::from_secs(3600);
/// 失敗を覚えておく時間の既定 (`PROXY_DNS_NEGATIVE_SECS` で変える。0 で覚えない)。
///
/// 5 秒から 60 秒にした (T13.1)。デプロイ先の 58.6 時間で解決の失敗は 80 件・**1 回 約 2 秒**
/// あり、5 秒では「直後の再試行」(負のキャッシュの当たり 81 件) しか捉えられていなかった。
pub const NEGATIVE: Duration = Duration::from_secs(60);
/// 保持するホスト数の上限 (超えたら最も古いものを捨てる)。
const MAX_ENTRIES: usize = 4096;

static TTL_SECS: AtomicU64 = AtomicU64::new(60);
static NEGATIVE_SECS: AtomicU64 = AtomicU64::new(NEGATIVE.as_secs());
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static STALE: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU64 = AtomicU64::new(0);
/// 期限前に裏で引き直した回数 (T13.1)。利用者は待っていないので**ミスとは別に数える**。
static REFRESHES: AtomicU64 = AtomicU64::new(0);
/// ミスのときに `getaddrinfo` に費やした時間の合計 (us)。**ミスの経路でしか書かない**
/// ので、当たりの経路 (熱い方) には原子操作が 1 つも増えない (T12.4 (2))。
/// 裏の引き直しのぶんも入れない (待っていない時間を「ミス 1 回の値段」に混ぜない)。
static RESOLVE_US_SUM: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// このスレッドが直近に払った名前解決の費用 (us の合計と回数)。**ホスト別の内訳に
    /// 使う**もので、原子操作を増やさないための thread-local。読む側が 0 に戻す。
    ///
    /// `const` で初期化しているので destructor が登録されず、スレッドの終了中に触っても
    /// `AccessError` にならない (`Drop` を持つ thread-local と違って `with` で足りる)。
    static RESOLVE_COST: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
    /// このスレッドで直近に確立した接続の族 (`Some(true)` = IPv6)。
    /// 書くのは [`note_family`] (接続が確立した 1 か所だけ)、読む側が `None` に戻す。
    static LAST_FAMILY: Cell<Option<bool>> = const { Cell::new(None) };
}

/// 直近の名前解決の費用を読み、0 に戻す (ms の合計と回数)。
///
/// **測る前にも 1 回呼んで捨てること**。ここは要求をまたいで貯まる箱なので、
/// 前の要求が残していったぶんを次の要求のホストに付けないようにする。
pub fn take_resolve_cost() -> (u64, u64) {
    let (us, n) = RESOLVE_COST.replace((0, 0));
    // 1 ms 未満のミス (手元の loopback) は 0 ms として数える。デプロイ先の
    // ミスは Docker の内蔵 DNS 越しで ms の単位なので、この丸めで足りる
    ((us + 500) / 1000, n)
}

/// 直近に確立した接続の族を読み、`None` に戻す。
pub fn take_family() -> Option<bool> {
    LAST_FAMILY.replace(None)
}

/// 接続が確立した族を控える (`crate::net` の確立点だけが呼ぶ)。thread-local への
/// 書き込み 1 回で、原子操作もシステムコールも増えない。
pub fn note_family(v6: bool) {
    LAST_FAMILY.set(Some(v6));
}

/// `getaddrinfo` に費やした時間の合計 (us) と回数 (`/metrics` と `/status`)。
pub fn resolve_cost_total() -> (u64, u64) {
    (
        RESOLVE_US_SUM.load(Ordering::Relaxed),
        MISSES.load(Ordering::Relaxed),
    )
}

struct Entry {
    addrs: Vec<IpAddr>,
    resolved_at: Instant,
    /// この名前を最後に引いた時刻 (T13.1)。**期限前に裏で引き直すのは「直近 TTL 内に
    /// 使われた熱いホスト」だけ**にするための印で、表に当たるたびに書く
    /// (既に鍵の内側なので鍵取りは増えない)。
    last_used: Instant,
    /// 裏で引き直している最中 (同じ名前の引き直しを 1 本に絞る旗)。
    refreshing: bool,
    /// 直近の失敗 (負のキャッシュ)
    failed_at: Option<(Instant, io::ErrorKind, String)>,
    /// このホストで最後に接続できた族 (`Some(true)` = IPv6)。RFC 8305 §8 の
    /// 「過去の結果で優先する族を変える」ための記憶で、**TTL で引き直しても残す**
    /// (アドレスは変わっても、そのホストへどちらの族で届くかは変わりにくい)。
    last_win_v6: Option<bool>,
}

impl Entry {
    /// まだ答えの無い入れ物 (族の記憶だけ先に置くことがある)。
    fn empty(now: Instant, last_win_v6: Option<bool>) -> Entry {
        Entry {
            addrs: Vec::new(),
            resolved_at: now,
            last_used: now,
            refreshing: false,
            failed_at: None,
            last_win_v6,
        }
    }
}

static TABLE: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

/// 解決の回数 (`HITS` / `MISSES`) を数えるテストは、表もカウンタも全テストで共有している
/// ので直列に回す。`dns` と `acl` と `net` のテストが同じロックを取る。
#[cfg(test)]
pub(crate) static RESOLVE_TEST_LOCK: Mutex<()> = Mutex::new(());

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

/// 期限前に裏で引き直し始める齢 (TTL の 3/4)。
///
/// `ttl * 3 / 4` と書くと大きな TTL で乗算が溢れるので引き算で出す。
fn refresh_after(ttl: Duration) -> Duration {
    ttl - ttl / 4
}

fn system_resolve(host: &str, port: u16) -> io::Result<Vec<IpAddr>> {
    let addrs: Vec<IpAddr> = (host, port).to_socket_addrs()?.map(|a| a.ip()).collect();
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve host",
        ));
    }
    Ok(addrs)
}

/// 裏で引き直す名前の受け口。**最初の引き直しのときに遅延起動する** (引き直しが 1 回も
/// 起きないプロセスではスレッドを作らない)。要求ごとにスレッドを起こさないための 1 本で、
/// プロファイル (`--lite`) に依らず同じ。スレッドが作れなかったら `None` (以後は先回りせず、
/// 今までどおり期限切れの同期のミスで引き直す)。
static REFRESHER: OnceLock<Option<Sender<String>>> = OnceLock::new();

/// 裏の引き直しを頼む。**表の鍵を放してから呼ぶこと** (送り先が詰まっても表を止めない)。
fn request_refresh(key: String) {
    let tx = REFRESHER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<String>();
        match thread::Builder::new()
            .name("dns-refresh".into())
            .spawn(move || {
                // 送り口は `OnceLock` の中で生き続けるので、この繰り返しは終わらない
                for host in rx {
                    refresh_one(&host);
                }
            }) {
            Ok(_) => Some(tx),
            Err(e) => {
                log_warn!(None, "could not start the dns-refresh thread: {}", e);
                None
            }
        }
    });
    match tx {
        // 送れなかった (スレッドが無い) ときは旗を戻す。戻さないとこの名前は
        // 二度と先回りされない
        Some(tx) => {
            if let Err(back) = tx.send(key) {
                clear_refreshing(&back.0);
            }
        }
        None => clear_refreshing(&key),
    }
}

/// 裏で 1 回引き直して表に書き戻す (`dns-refresh` スレッドの中だけ)。
///
/// **ミスとしては数えない**: 利用者はこの時間を待っていないので、`misses` と
/// `miss_avg_ms` (`sorahost_dns_seconds_*`) に混ぜると「ミス 1 回の値段」が読めなくなる。
fn refresh_one(key: &str) {
    // `getaddrinfo` にポート (サービス) は渡らない。std は service を NULL で引いて、
    // 返ってきたアドレスにあとからポートを詰めるので、ここは 0 でよい
    let result = system_resolve(key, 0);
    let now = Instant::now();
    {
        let mut guard = TABLE.locked();
        let Some(entry) = guard.as_mut().and_then(|t| t.get_mut(key)) else {
            // 途中で `clear()` された。引き直した答えを蘇らせない
            return;
        };
        entry.refreshing = false;
        if let Ok(addrs) = result {
            // 答えが変わっていれば差し替える。族の記憶 (T12.1) は引き継ぐ
            entry.addrs = addrs;
            entry.resolved_at = now;
            entry.failed_at = None;
        }
        // 失敗したら古い答えをそのまま残す (`resolved_at` も動かさないので、期限が来たら
        // 今までどおり同期で引き直す)。**失敗も覚えない** — 裏の失敗で利用者の経路に
        // エラーを配らないため
    }
    REFRESHES.fetch_add(1, Ordering::Relaxed);
}

/// 引き直しの旗を下ろす (頼めなかったとき)。
fn clear_refreshing(key: &str) {
    if let Some(e) = TABLE.locked().as_mut().and_then(|t| t.get_mut(key)) {
        e.refreshing = false;
    }
}

/// 判定 (ACL) が引いた答えを、そのまま接続まで運ぶための入れ物 (T12.7)。
///
/// ローカル宛ての判定 ([`crate::acl::resolve_target`]) と、その直後の接続
/// ([`crate::net::connect_with`]) は**同じ答えを使う**。名前解決が 1 要求 1 回で
/// 済むだけでなく、`PROXY_DNS_TTL_SECS=0` (キャッシュ無効) でも判定と接続が
/// 別の答えを引かない (DNS rebinding で判定をすり抜けられない)。
pub struct Resolved<'a> {
    /// 解決したホスト名。**別のホスト宛ての接続には使わない**ための鍵で、
    /// 突き合わせは大小無視 (要求ごとに小文字の複製を作らない)
    host: &'a str,
    addrs: Vec<IpAddr>,
    /// このホストで最後に勝った族 (T12.1)。同じ鍵取りで一緒に受け取っておく
    preferred: Option<bool>,
}

impl<'a> Resolved<'a> {
    pub fn new(host: &'a str, addrs: Vec<IpAddr>, preferred: Option<bool>) -> Resolved<'a> {
        Resolved {
            host,
            addrs,
            preferred,
        }
    }

    pub fn addrs(&self) -> &[IpAddr] {
        &self.addrs
    }

    /// ホスト名の借用を手放してアドレス列だけを取り出す (別のスコープへ運ぶとき)。
    pub fn into_addrs(self) -> Vec<IpAddr> {
        self.addrs
    }

    pub fn preferred(&self) -> Option<bool> {
        self.preferred
    }

    /// `host` の答えなら、ポートを付けた候補列を返す (別のホストなら `None` = 引き直す)。
    pub fn socket_addrs_for(&self, host: &str, port: u16) -> Option<Vec<SocketAddr>> {
        self.host
            .eq_ignore_ascii_case(host)
            .then(|| with_port(&self.addrs, port))
    }
}

/// `addr_str` (`host:port`) を解決する。キャッシュがあれば OS に問い合わせない。
pub fn resolve(addr_str: &str) -> io::Result<Vec<SocketAddr>> {
    resolve_with_pref(addr_str).map(|(addrs, _)| addrs)
}

/// [`resolve`] に「最後に勝った族」の記憶を添えて返す (T12.1)。**表を引くのは 1 回だけ**
/// にするためで、接続側が別に [`preferred_family`] を呼ぶと鍵を 2 回取ることになる。
pub fn resolve_with_pref(addr_str: &str) -> io::Result<(Vec<SocketAddr>, Option<bool>)> {
    let (host, port) = crate::net::split_host_port_ref(addr_str);
    let Some(port) = port else {
        return addr_str.to_socket_addrs().map(|i| (i.collect(), None));
    };
    // IP リテラルは表も確保も要らない (熱い経路: ベンチも `127.0.0.1:port` で来る)
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok((vec![SocketAddr::new(ip, port)], None));
    }
    let (addrs, pref) = resolve_host(host, port)?;
    Ok((with_port(&addrs, port), pref))
}

/// ホスト名を解決して**アドレスだけ**を返す (T12.7)。`port` は `getaddrinfo` に渡す
/// サービス番号で、キャッシュの鍵はホスト名だけ (返す側でポートを付ける)。
///
/// IP リテラルは表を使わない (解決するものが無い)。`PROXY_DNS_TTL_SECS=0` の
/// ときも表を使わないが、**OS に投げた回数はミスとして数える**
/// (`dns.hits + dns.misses` が「解決した回数」を表すため。T12.7)。
pub fn resolve_host(host: &str, port: u16) -> io::Result<(Vec<IpAddr>, Option<bool>)> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok((vec![ip], None));
    }
    let ttl = ttl();
    if ttl.is_zero() {
        MISSES.fetch_add(1, Ordering::Relaxed);
        return system_resolve(host, port).map(|a| (a, None));
    }
    let key = host.to_ascii_lowercase();
    let now = Instant::now();
    let mut pref = None;
    {
        let mut guard = TABLE.locked();
        if let Some(e) = guard.get_or_insert_with(HashMap::new).get_mut(&key) {
            pref = e.last_win_v6;
            // 「熱い」= **この参照の 1 つ前**の使用が TTL 以内。印はここで更新する
            let hot = now.duration_since(e.last_used) < ttl;
            e.last_used = now;
            let age = now.duration_since(e.resolved_at);
            if !e.addrs.is_empty() && age < ttl {
                // 期限の 3/4 を過ぎた熱いホストは、答えは今の表から返して (ヒット)、
                // 裏で 1 回だけ引き直す。利用者は期限切れのミス (デプロイ先で 約 10 ms、
                // 混むと 100 ms 超) を待たない (T13.1)
                let ask = hot && !e.refreshing && age >= refresh_after(ttl);
                if ask {
                    e.refreshing = true;
                }
                let addrs = e.addrs.clone();
                drop(guard);
                HITS.fetch_add(1, Ordering::Relaxed);
                if ask {
                    request_refresh(key);
                }
                return Ok((addrs, pref));
            }
            let negative = negative_ttl();
            if let Some((at, kind, msg)) = &e.failed_at
                && !negative.is_zero()
                && now.duration_since(*at) < negative
            {
                // 覚えている失敗の内側でも、**古い答えがあるなら繋ぐ方を選ぶ**。
                // 負のキャッシュが 60 秒になったので、ここでエラーを返すと
                // 「古い答えで凌ぐ」窓 (`STALE_MAX`) がそのぶん潰れる (T13.1)
                if !e.addrs.is_empty() && age < STALE_MAX {
                    let addrs = e.addrs.clone();
                    STALE.fetch_add(1, Ordering::Relaxed);
                    return Ok((addrs, pref));
                }
                FAILURES.fetch_add(1, Ordering::Relaxed);
                return Err(io::Error::new(*kind, msg.clone()));
            }
        }
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    // ミスのときだけ `Instant` を 2 回読む (当たりの経路は 1 命令も増えない。T12.4 (2))
    let t0 = Instant::now();
    let result = system_resolve(host, port);
    let us = t0.elapsed().as_micros().min(u64::MAX as u128) as u64;
    RESOLVE_US_SUM.fetch_add(us, Ordering::Relaxed);
    RESOLVE_COST.set({
        let (s, n) = RESOLVE_COST.get();
        (s + us, n + 1)
    });
    let mut guard = TABLE.locked();
    let table = guard.get_or_insert_with(HashMap::new);
    match result {
        Ok(addrs) => {
            if table.len() >= MAX_ENTRIES && !table.contains_key(&key) {
                evict_oldest(table);
            }
            // 引き直しでも族の記憶は引き継ぐ。裏の引き直しが走っている最中なら
            // その旗も残す (同じ名前を 2 本引きに行かせない)
            let slot = table.entry(key).or_insert_with(|| Entry::empty(now, pref));
            slot.addrs = addrs.clone();
            slot.resolved_at = now;
            slot.last_used = now;
            slot.failed_at = None;
            if slot.last_win_v6.is_none() {
                slot.last_win_v6 = pref;
            }
            Ok((addrs, slot.last_win_v6))
        }
        Err(e) => {
            let entry = table.entry(key).or_insert_with(|| Entry::empty(now, None));
            entry.failed_at = Some((now, e.kind(), e.to_string()));
            if !entry.addrs.is_empty() && now.duration_since(entry.resolved_at) < STALE_MAX {
                STALE.fetch_add(1, Ordering::Relaxed);
                let pref = entry.last_win_v6;
                return Ok((entry.addrs.clone(), pref));
            }
            Err(e)
        }
    }
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

fn with_port(addrs: &[IpAddr], port: u16) -> Vec<SocketAddr> {
    addrs.iter().map(|&ip| SocketAddr::new(ip, port)).collect()
}

fn evict_oldest(table: &mut HashMap<String, Entry>) {
    if let Some(k) = table
        .iter()
        .min_by_key(|(_, e)| e.resolved_at)
        .map(|(k, _)| k.clone())
    {
        table.remove(&k);
    }
}

/// 覚えている結果を全部捨てる (`.env` の TTL 変更やテスト用)。
pub fn clear() {
    if let Some(t) = TABLE.locked().as_mut() {
        t.clear();
    }
}

/// `/status` の `"dns"` 要素。
pub fn status_json() -> String {
    let entries = TABLE.locked().as_ref().map_or(0, HashMap::len);
    let (us, misses) = resolve_cost_total();
    format!(
        "{{\"ttl_secs\":{},\"negative_ttl_secs\":{},\"entries\":{},\"hits\":{},\"misses\":{},\"stale_served\":{},\"negative_hits\":{},\"refreshes\":{},\"miss_ms_sum\":{:.1},\"miss_avg_ms\":{:.2}}}",
        TTL_SECS.load(Ordering::Relaxed),
        NEGATIVE_SECS.load(Ordering::Relaxed),
        entries,
        HITS.load(Ordering::Relaxed),
        MISSES.load(Ordering::Relaxed),
        STALE.load(Ordering::Relaxed),
        FAILURES.load(Ordering::Relaxed),
        REFRESHES.load(Ordering::Relaxed),
        us as f64 / 1000.0,
        if misses == 0 {
            0.0
        } else {
            us as f64 / 1000.0 / misses as f64
        },
    )
}

/// Prometheus 用のカウンタ (hits, misses, stale, negative, refresh)。
pub fn counters() -> [u64; 5] {
    [
        HITS.load(Ordering::Relaxed),
        MISSES.load(Ordering::Relaxed),
        STALE.load(Ordering::Relaxed),
        FAILURES.load(Ordering::Relaxed),
        REFRESHES.load(Ordering::Relaxed),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached(host: &str) -> Option<(usize, bool)> {
        TABLE
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|t| t.get(host))
            .map(|e| (e.addrs.len(), e.failed_at.is_some()))
    }

    /// (hits, misses, negative_hits, refreshes)。
    fn tally() -> (u64, u64, u64, u64) {
        let [hits, misses, _, negative, refreshes] = counters();
        (hits, misses, negative, refreshes)
    }

    /// 表の 1 件の時計を巻き戻す (秒を待たずに「t=50 秒」の状態を作る)。
    fn age_entry(host: &str, resolved_ago: Duration, used_ago: Duration) {
        let now = Instant::now();
        let mut guard = TABLE.locked();
        let e = guard
            .as_mut()
            .and_then(|t| t.get_mut(host))
            .expect("表に載っている");
        e.resolved_at = now - resolved_ago;
        e.last_used = now - used_ago;
    }

    /// 覚えている失敗の時計を巻き戻す。
    fn age_failure(host: &str, ago: Duration) {
        let now = Instant::now();
        let mut guard = TABLE.locked();
        let f = guard
            .as_mut()
            .and_then(|t| t.get_mut(host))
            .and_then(|e| e.failed_at.as_mut())
            .expect("失敗を覚えている");
        f.0 = now - ago;
    }

    /// 裏の引き直しが `want` 回になるまで待つ (最大 5 秒)。
    fn wait_refreshes(want: u64) {
        for _ in 0..500 {
            if REFRESHES.load(Ordering::Relaxed) >= want {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "refreshes = {} (expected {})",
            REFRESHES.load(Ordering::Relaxed),
            want
        );
    }

    #[test]
    fn second_lookup_is_served_from_cache() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let a = resolve("LocalHost:1234").unwrap();
        assert!(a.iter().all(|s| s.port() == 1234));
        let (n, failed) = cached("localhost").expect("cached under the lowercase name");
        assert_eq!(n, a.len());
        assert!(!failed);
        let b = resolve("localhost:4321").unwrap();
        assert_eq!(b.len(), a.len());
        assert!(b.iter().all(|s| s.port() == 4321));
    }

    #[test]
    fn ip_literals_bypass_the_cache() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let a = resolve("127.0.0.1:9").unwrap();
        assert_eq!(a, vec!["127.0.0.1:9".parse().unwrap()]);
        assert!(cached("127.0.0.1").is_none());
        let v6 = resolve("[::1]:9").unwrap();
        assert_eq!(v6, vec!["[::1]:9".parse().unwrap()]);
    }

    /// 勝った族の記憶は TTL で引き直しても残る (T12.1)。
    #[test]
    fn the_winning_family_survives_a_relookup() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let host = "localhost";
        remember_family("LocalHost", true);
        assert_eq!(preferred_family(host), Some(true));
        assert_eq!(preferred_family("127.0.0.1"), None, "IP リテラルは覚えない");
        // TTL 切れにして引き直させる
        {
            let mut guard = TABLE.locked();
            let table = guard.get_or_insert_with(HashMap::new);
            if let Some(e) = table.get_mut(host) {
                e.resolved_at = Instant::now() - Duration::from_secs(24 * 3600);
            }
        }
        let (addrs, pref) = resolve_with_pref("localhost:1234").unwrap();
        assert!(!addrs.is_empty());
        assert_eq!(pref, Some(true), "引き直しても記憶は残る");
        remember_family(host, false);
        assert_eq!(preferred_family(host), Some(false));
    }

    #[test]
    fn failure_is_remembered_briefly() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let host = "no-such-host.invalid";
        let e1 = resolve(&format!("{host}:80")).unwrap_err();
        assert_eq!(cached(host), Some((0, true)));
        let e2 = resolve(&format!("{host}:80")).unwrap_err();
        assert_eq!(e1.kind(), e2.kind());
        assert!(status_json().contains("\"negative_hits\":"));
    }

    /// T13.1 (a): 直近 TTL 内に使われた名前は期限の 3/4 で裏で引き直され、
    /// 期限を過ぎたはずの t=61 秒でも**ヒット**する (ミスにならない)。
    #[test]
    fn a_hot_name_is_refreshed_before_it_expires() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_ttl(Duration::from_secs(60));
        clear();
        let host = "localhost";
        // t=0: 表に無いので同期で引く (ミス)
        let (_, m0, _, _) = tally();
        resolve_host(host, 80).unwrap();
        assert_eq!(tally().1 - m0, 1, "t=0 はミス");

        // t=50: 期限 (60 秒) の 3/4 を過ぎ、直近 TTL 内に使われている
        age_entry(host, Duration::from_secs(50), Duration::from_secs(50));
        let (h1, m1, _, r1) = tally();
        let (addrs, _) = resolve_host(host, 80).unwrap();
        assert!(!addrs.is_empty());
        let (h2, m2, _, _) = tally();
        assert_eq!((h2 - h1, m2 - m1), (1, 0), "t=50 は表から返す (ヒット)");
        wait_refreshes(r1 + 1);

        // t=61: 裏で引き直したので期限内 (引き直しから 11 秒後の姿にする)
        age_entry(host, Duration::from_secs(11), Duration::ZERO);
        let (h3, m3, _, _) = tally();
        resolve_host(host, 80).unwrap();
        let (h4, m4, _, _) = tally();
        assert_eq!((h4 - h3, m4 - m3), (1, 0), "t=61 でもヒット");
    }

    /// T13.1 (b): 解決してから使われていない名前は、今までどおり期限切れでミスになる
    /// (熱くないので裏では引き直さない = 誰も見ない名前に `getaddrinfo` を払わない)。
    #[test]
    fn an_idle_name_still_expires() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_ttl(Duration::from_secs(60));
        clear();
        let host = "localhost";
        resolve_host(host, 80).unwrap();
        // t=61: 解決してから一度も使われていない
        age_entry(host, Duration::from_secs(61), Duration::from_secs(61));
        let (h0, m0, _, r0) = tally();
        resolve_host(host, 80).unwrap();
        let (h1, m1, _, r1) = tally();
        assert_eq!((h1 - h0, m1 - m0), (0, 1), "使われていない名前はミス");
        assert_eq!(r1, r0, "裏の引き直しは走らない");
    }

    /// T13.1 (c): 同じ名前に同時に 10 本来ても、裏の引き直しは 1 本だけ
    /// (要求ごとにスレッドも `getaddrinfo` も増やさない)。
    #[test]
    fn ten_callers_trigger_one_refresh() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_ttl(Duration::from_secs(60));
        clear();
        let host = "localhost";
        resolve_host(host, 80).unwrap();
        age_entry(host, Duration::from_secs(50), Duration::from_secs(1));
        let (_, m0, _, r0) = tally();
        let hands: Vec<_> = (0..10)
            .map(|_| thread::spawn(|| resolve_host("localhost", 80).unwrap()))
            .collect();
        for h in hands {
            h.join().unwrap();
        }
        assert_eq!(tally().1, m0, "10 本とも表から返る (ミスは増えない)");
        wait_refreshes(r0 + 1);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(tally().3, r0 + 1, "引き直したのは 1 本だけ");
    }

    /// T13.1 (d): 失敗した名前は `PROXY_DNS_NEGATIVE_SECS` (既定 60 秒) の間 OS に
    /// 問い合わせず、過ぎたら問い合わせる。
    #[test]
    fn a_failure_is_remembered_for_the_negative_ttl() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_ttl(Duration::from_secs(60));
        set_negative_ttl(NEGATIVE);
        clear();
        let host = "no-such-host-t131.invalid";
        let e1 = resolve_host(host, 80).unwrap_err();
        assert_eq!(cached(host), Some((0, true)));

        // 10 秒後: 覚えた失敗をそのまま返す (OS には行かない)
        age_failure(host, Duration::from_secs(10));
        let (_, m0, n0, _) = tally();
        let e2 = resolve_host(host, 80).unwrap_err();
        assert_eq!(e1.kind(), e2.kind(), "同じエラーを返す");
        let (_, m1, n1, _) = tally();
        assert_eq!(m1, m0, "10 秒後は OS に問い合わせない");
        assert_eq!(n1 - n0, 1, "負のキャッシュの当たりが 1 増える");

        // 61 秒後: 覚えた失敗は切れているので問い合わせる
        age_failure(host, Duration::from_secs(61));
        let (_, m2, _, _) = tally();
        resolve_host(host, 80).unwrap_err();
        assert_eq!(tally().1 - m2, 1, "61 秒後は OS に問い合わせる");
    }

    /// T13.1 (e): `PROXY_DNS_NEGATIVE_SECS=0` なら失敗を覚えない。
    #[test]
    fn zero_negative_ttl_does_not_remember_failures() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_ttl(Duration::from_secs(60));
        clear();
        set_negative_ttl(Duration::ZERO);
        let host = "no-such-host-t131-zero.invalid";
        let (_, m0, n0, _) = tally();
        resolve_host(host, 80).unwrap_err();
        resolve_host(host, 80).unwrap_err();
        let (_, m1, n1, _) = tally();
        set_negative_ttl(NEGATIVE);
        assert_eq!(m1 - m0, 2, "0 なら毎回 OS に問い合わせる");
        assert_eq!(n1, n0, "負のキャッシュには当たらない");
    }

    /// T13.1: 失敗を覚えている間でも、`STALE_MAX` 以内の古い答えがあれば繋ぐ方を選ぶ
    /// (負のキャッシュが 60 秒になったので、ここでエラーを返すと窓が 60 秒潰れる)。
    #[test]
    fn a_remembered_failure_does_not_hide_a_stale_answer() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_ttl(Duration::from_secs(60));
        set_negative_ttl(NEGATIVE);
        clear();
        // 表に直接置く (**他のテストと同じ名前を汚さない**。失敗を覚えた印は古い答えを
        // 配っても消えないので、`localhost` でやると後続のテストがそれを見る)
        let host = "t131-stale.invalid";
        let addrs = vec![IpAddr::from([127, 0, 0, 1])];
        let now = Instant::now();
        {
            let mut guard = TABLE.locked();
            let table = guard.get_or_insert_with(HashMap::new);
            // 期限切れ (61 秒前) の答え + 直近の失敗
            let mut e = Entry::empty(now - Duration::from_secs(61), None);
            e.addrs = addrs.clone();
            e.failed_at = Some((now, io::ErrorKind::NotFound, "boom".into()));
            table.insert(host.to_string(), e);
        }
        let (_, m0, _, _) = tally();
        let (again, _) = resolve_host(host, 80).expect("古い答えで繋ぐ");
        assert_eq!(again, addrs, "古い答えをそのまま返す");
        assert_eq!(tally().1, m0, "OS には問い合わせない");
        clear();
    }
}
