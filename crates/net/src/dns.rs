//! 名前解決の結果を短時間キャッシュする。
//!
//! `getaddrinfo(3)` は TTL を返さないので、[`set_ttl`] で与えた固定の TTL (`PROXY_DNS_TTL_SECS`、
//! 既定 60 秒、0 で無効) だけ保持する。**直近 TTL 内に使われた名前は期限の 3/4 を過ぎたところで
//! 裏で引き直す**ので、熱いホストは期限切れのミス (デプロイ先で 1 回 約 10 ms、混むと 100 ms 超)
//! を払わない (T13.1)。
//!
//! それだけでは**熱さが TTL に縛られる**ので、間隔が TTL より長いホスト (デプロイ先の主要 3 件は
//! 2〜10 分間隔) は 1 つも救えなかった。そこで熱さの窓を TTL から切り離し、**直近 [`warm_window`]
//! 秒 (`PROXY_DNS_WARM_SECS`、既定 900 秒、0 で無効) に 2 回以上使われた名前 (= warm) は、
//! 使われていなくても 3/4 TTL ごとに裏で引き直し続ける** (keep-warm。T14.1)。warm でいられるのは
//! 同時に [`MAX_WARM`] 件までで、最後の使用から窓を過ぎた名前は待ち行列から外れる。
//!
//! 解決に失敗したときは [`STALE_MAX`] 以内の古い結果を使い (オリジンの
//! DNS 障害でトンネルが全滅しないように)、失敗そのものも [`negative_ttl`]
//! (`PROXY_DNS_NEGATIVE_SECS`、既定 60 秒、0 で覚えない) の間覚えて連続した再解決を抑える。
//! IP リテラルはキャッシュしない。

use crate::log_warn;
use crate::sync::LockExt;
use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
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
/// warm と見なす窓の既定 (`PROXY_DNS_WARM_SECS` で変える。0 で keep-warm を止める)。
///
/// デプロイ先の主要ホストのアクセス間隔は 2〜10 分 (`/dns` の 90 件のうち 60 秒以内に
/// 使われたものは 2 件しか無かった)。15 分あれば 3 件とも窓に入る (T14.1)。
pub const WARM: Duration = Duration::from_secs(900);
/// 保持するホスト数の上限 (超えたら最も古いものを捨てる)。
const MAX_ENTRIES: usize = 4096;
/// 同時に warm でいられる名前の上限 (超えたら最後の使用が最も古いものを外す)。
///
/// 最悪の引き直しは `MAX_WARM / (3/4 × TTL)` = 32 / 45 秒 ≈ **0.7 回/秒**。
/// デプロイ先のリゾルバ (Docker の内蔵 DNS、1 回 10 ms 前後) に迷惑をかけない数 (T14.1)。
const MAX_WARM: usize = 32;

static TTL_SECS: AtomicU64 = AtomicU64::new(60);
static NEGATIVE_SECS: AtomicU64 = AtomicU64::new(NEGATIVE.as_secs());
static WARM_SECS: AtomicU64 = AtomicU64::new(WARM.as_secs());
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
    /// **1 つ前**の使用時刻 (T14.1)。warm の判定 (「直近 W 秒に 2 回以上」) に要るのは
    /// これだけで、要求の経路は `last_used` を押し出すだけ (鍵は既に取っている)。
    prev_used: Instant,
    /// warm な名前か = [`WARM_QUEUE`] に載っているか (T14.1)。**熱い経路で鍵を 2 つ
    /// 取らないための写し**で、待ち行列への出し入れと必ず同時に書く。
    warm: bool,
    /// 次に裏で引き直す時刻 (`/dns` の `next_refresh_secs`。warm でなければ `None`)。
    next_refresh: Option<Instant>,
    /// 裏で引き直している最中 (同じ名前の引き直しを 1 本に絞る旗)。
    refreshing: bool,
    /// この名前で OS に問い合わせた回数 (`/dns?sort=misses`。T13.4)。
    /// **書くのは既に表の鍵を取っている場所だけ**なので、原子操作は増えない
    misses: u64,
    /// この名前を期限前に裏で引き直した回数 (T13.4)
    refreshes: u64,
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
            prev_used: now,
            warm: false,
            next_refresh: None,
            refreshing: false,
            misses: 0,
            refreshes: 0,
            failed_at: None,
            last_win_v6,
        }
    }
}

static TABLE: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

/// warm な名前の待ち行列 (T14.1)。`(次に引き直す時刻, 名前)` の順に並ぶので、先頭が
/// いつも次の仕事になる。`refresher` スレッドが先頭の期限まで眠り、起きたら 1 件処理する。
///
/// **鍵の順は `TABLE` → `WARM_QUEUE`** (逆には取らない)。1 つの名前は高々 1 件。
static WARM_QUEUE: Mutex<BTreeSet<(Instant, String)>> = Mutex::new(BTreeSet::new());

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

/// いま warm な名前の数 (`/status` の `dns.warm`)。
pub fn warm_count() -> usize {
    WARM_QUEUE.locked().len()
}

/// 期限前に裏で引き直し始める齢 (TTL の 3/4)。warm な名前の引き直しの周期でもある。
///
/// `ttl * 3 / 4` と書くと大きな TTL で乗算が溢れるので引き算で出す。
fn refresh_after(ttl: Duration) -> Duration {
    ttl - ttl / 4
}

/// warm の待ち行列を空にし、表の印も下ろす (`clear` と `PROXY_DNS_WARM_SECS=0`)。
fn warm_clear() {
    let mut guard = TABLE.locked();
    let mut q = WARM_QUEUE.locked();
    if let Some(table) = guard.as_mut() {
        for (_, key) in q.iter() {
            if let Some(e) = table.get_mut(key) {
                e.warm = false;
                e.next_refresh = None;
            }
        }
    }
    q.clear();
}

/// この名前を warm にする (**表の鍵を握ったまま**呼ぶ。T14.1)。
///
/// 満杯 ([`MAX_WARM`]) なら**最後の使用がいちばん古い**名前を 1 つ外す。表から消えた名前が
/// 待ち行列に残っていれば、それを先に外す (`Option` の順で `None` が最小になる)。
fn warm_promote(table: &mut HashMap<String, Entry>, key: &str, now: Instant, ttl: Duration) {
    let at = now + refresh_after(ttl);
    let mut q = WARM_QUEUE.locked();
    // 同じ名前が 2 件並ばないように、古い予定があれば先に外す (表から追い出されて
    // 作り直された名前など)
    if let Some(old) = q.iter().find(|(_, k)| k == key).cloned() {
        q.remove(&old);
    }
    if q.len() >= MAX_WARM
        && let Some(victim) = q
            .iter()
            .min_by_key(|(_, k)| table.get(k.as_str()).map(|e| e.last_used))
            .cloned()
    {
        q.remove(&victim);
        if let Some(e) = table.get_mut(&victim.1) {
            e.warm = false;
            e.next_refresh = None;
        }
    }
    q.insert((at, key.to_string()));
    if let Some(e) = table.get_mut(key) {
        e.warm = true;
        e.next_refresh = Some(at);
    }
}

/// `refresher` スレッドの次の仕事。
enum Next {
    /// この名前を裏で引き直す (`refreshing` は立てたあと)
    Refresh(String),
    /// 次の期限までこれだけ待つ
    Wait(Duration),
    /// warm な名前が無い (要求が来るまで眠る)
    Idle,
}

/// 待ち行列の先頭を見て、期限が来ていれば 1 件処理する (T14.1)。
///
/// 使われなくなった名前 (最後の使用から窓を過ぎた) はここで warm を外す = 引き直しが止まる。
/// **鍵は `TABLE` → `WARM_QUEUE` の順**。引き直しそのものは鍵を放してから呼び出し側がやる。
fn warm_next(now: Instant) -> Next {
    let ttl = ttl();
    let window = warm_window();
    let mut guard = TABLE.locked();
    let table = guard.get_or_insert_with(HashMap::new);
    let mut q = WARM_QUEUE.locked();
    loop {
        let Some(first) = q.first().cloned() else {
            return Next::Idle;
        };
        if first.0 > now {
            return Next::Wait(first.0 - now);
        }
        q.remove(&first);
        let (_, key) = first;
        let Some(e) = table.get_mut(&key) else {
            // 表から消えた名前 (`MAX_ENTRIES` で追い出された) は黙って落とす
            continue;
        };
        if !e.warm || ttl.is_zero() || window.is_zero() || now.duration_since(e.last_used) >= window
        {
            // 窓の外に出た = 誰も使っていない。ここで止める (引き直し続けない)
            e.warm = false;
            e.next_refresh = None;
            continue;
        }
        let at = now + refresh_after(ttl);
        e.next_refresh = Some(at);
        q.insert((at, key.clone()));
        if e.refreshing {
            // 要求の経路が既に引き直しを頼んでいる。二重には引かない
            continue;
        }
        e.refreshing = true;
        return Next::Refresh(key);
    }
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

/// **表を通さずに** OS へ問い合わせる (canary。T14.10)。
///
/// 表には書かず、当たり外れも数えない: canary が測るのは**リゾルバの実力**で、
/// 1 分に 1 回の自分の問い合わせを利用者の統計 (`dns.hits` / `dns.misses` と
/// ミス 1 回の平均) に混ぜてしまうと、両方が読めなくなる。keep-warm (T14.1) の
/// 窓にも入らない (canary が触った名前が warm になっては本末転倒)。
pub fn resolve_uncached(host: &str, port: u16) -> io::Result<Vec<IpAddr>> {
    system_resolve(host, port)
}

/// `dns-refresh` スレッドへの用件。
enum Msg {
    /// この名前を 1 回だけ引き直す (期限前の先回り。T13.1)
    Refresh(String),
    /// warm な名前が増えた / 期限が早まった。待ち時間を計算し直す (T14.1)
    Wake,
}

/// 裏で引き直す名前の受け口。**最初の引き直しのときに遅延起動する** (引き直しが 1 回も
/// 起きないプロセスではスレッドを作らない)。要求ごとにスレッドを起こさないための 1 本で、
/// プロファイル (`--lite`) に依らず同じ。スレッドが作れなかったら `None` (以後は先回りせず、
/// 今までどおり期限切れの同期のミスで引き直す)。
static REFRESHER: OnceLock<Option<Sender<Msg>>> = OnceLock::new();

/// `dns-refresh` スレッドへの送り口 (無ければ遅延起動する)。
fn refresher() -> Option<&'static Sender<Msg>> {
    REFRESHER
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Msg>();
            match thread::Builder::new()
                .name("dns-refresh".into())
                .spawn(move || refresher_loop(rx))
            {
                Ok(_) => Some(tx),
                Err(e) => {
                    log_warn!(None, "could not start the dns-refresh thread: {}", e);
                    None
                }
            }
        })
        .as_ref()
}

/// 裏の引き直しをする 1 本のスレッド。**warm な名前の待ち行列の期限まで眠り**、
/// その間に来た用件 (先回りの依頼・起こし) を捌く。送り口は `OnceLock` の中で
/// 生き続けるので、この繰り返しは終わらない。
fn refresher_loop(rx: mpsc::Receiver<Msg>) {
    loop {
        let next = warm_next(Instant::now());
        let got = match next {
            Next::Refresh(key) => {
                refresh_one(&key);
                // 期限の来た名前が他にもあるかもしれないので、すぐ次を見る
                continue;
            }
            Next::Wait(d) => match rx.recv_timeout(d) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            Next::Idle => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return,
            },
        };
        if let Some(Msg::Refresh(host)) = got {
            refresh_one(&host);
        }
    }
}

/// 裏の引き直しを頼む。**表の鍵を放してから呼ぶこと** (送り先が詰まっても表を止めない)。
fn request_refresh(key: String) {
    match refresher() {
        // 送れなかった (スレッドが無い) ときは旗を戻す。戻さないとこの名前は
        // 二度と先回りされない
        Some(tx) => {
            if let Err(back) = tx.send(Msg::Refresh(key))
                && let Msg::Refresh(k) = back.0
            {
                clear_refreshing(&k);
            }
        }
        None => clear_refreshing(&key),
    }
}

/// warm な名前が増えたことを `dns-refresh` スレッドに伝える (待ち時間の計算し直し)。
/// **表の鍵を放してから呼ぶこと**。
fn wake_refresher() {
    if let Some(tx) = refresher() {
        let _ = tx.send(Msg::Wake);
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
        entry.refreshes += 1;
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

/// 表を引いて出た答え。**数える (原子操作) のは鍵を放してから**なので、
/// 鍵の内側では「どれだったか」だけを持って出る (T12.4 (2))。
enum Looked {
    /// 期限内の答え
    Hit(Vec<IpAddr>),
    /// 覚えている失敗より優先した古い答え
    Stale(Vec<IpAddr>),
    /// 覚えている失敗 (負のキャッシュ)
    Negative(io::Error),
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
    let window = warm_window();
    let mut pref = None;
    // 表から答えが出たか (**数えるのは鍵を放してから**。T12.4 (2))
    let mut found: Option<Looked> = None;
    // 期限前の先回りを頼むか (T13.1) / この参照で warm になったか (T14.1)
    let (mut ask, mut promote) = (false, false);
    {
        // 取り合いを数える (T14.3 (3))。空いていれば `locked` と同じ費用
        let mut guard = TABLE.locked_counted(&crate::sync::LOCK_CONTENDED[crate::sync::LOCK_DNS]);
        let table = guard.get_or_insert_with(HashMap::new);
        if let Some(e) = table.get_mut(&key) {
            pref = e.last_win_v6;
            // 「熱い」= **この参照の 1 つ前**の使用が TTL 以内。印はここで更新する
            let idle = now.duration_since(e.last_used);
            let hot = idle < ttl;
            // warm = 直近 W 秒に **2 回以上**使われた名前。この参照が 2 回目なら
            // 待ち行列に入れる (1 回だけの名前は入れない。引き直しても二度と来ない)。
            // **答えを持っている名前だけ**を warm にする: 引けない名前は先回りする
            // 期限を持たず (要求の経路は負のキャッシュで止まる)、1 回 約 2 秒かかるので、
            // 待ち行列に入れると `dns-refresh` の 1 本が本当に熱い名前を待たせる
            promote = !e.warm && !window.is_zero() && idle < window && !e.addrs.is_empty();
            e.prev_used = e.last_used;
            e.last_used = now;
            let age = now.duration_since(e.resolved_at);
            if !e.addrs.is_empty() && age < ttl {
                // 期限の 3/4 を過ぎた熱いホストは、答えは今の表から返して (ヒット)、
                // 裏で 1 回だけ引き直す。利用者は期限切れのミス (デプロイ先で 約 10 ms、
                // 混むと 100 ms 超) を待たない (T13.1)
                ask = hot && !e.refreshing && age >= refresh_after(ttl);
                if ask {
                    e.refreshing = true;
                }
                found = Some(Looked::Hit(e.addrs.clone()));
            } else {
                let negative = negative_ttl();
                if let Some((at, kind, msg)) = &e.failed_at
                    && !negative.is_zero()
                    && now.duration_since(*at) < negative
                {
                    // 覚えている失敗の内側でも、**古い答えがあるなら繋ぐ方を選ぶ**。
                    // 負のキャッシュが 60 秒になったので、ここでエラーを返すと
                    // 「古い答えで凌ぐ」窓 (`STALE_MAX`) がそのぶん潰れる (T13.1)
                    found = Some(if !e.addrs.is_empty() && age < STALE_MAX {
                        Looked::Stale(e.addrs.clone())
                    } else {
                        Looked::Negative(io::Error::new(*kind, msg.clone()))
                    });
                }
            }
            if promote {
                warm_promote(table, &key, now, ttl);
            }
        }
    }
    if promote {
        wake_refresher();
    }
    if ask {
        request_refresh(key.clone());
    }
    match found {
        Some(Looked::Hit(addrs)) => {
            HITS.fetch_add(1, Ordering::Relaxed);
            return Ok((addrs, pref));
        }
        Some(Looked::Stale(addrs)) => {
            STALE.fetch_add(1, Ordering::Relaxed);
            return Ok((addrs, pref));
        }
        Some(Looked::Negative(e)) => {
            FAILURES.fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
        None => {}
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
            slot.misses += 1;
            slot.failed_at = None;
            if slot.last_win_v6.is_none() {
                slot.last_win_v6 = pref;
            }
            Ok((addrs, slot.last_win_v6))
        }
        Err(e) => {
            let entry = table.entry(key).or_insert_with(|| Entry::empty(now, None));
            entry.misses += 1;
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

/// 覚えている結果を全部捨てる (`.env` の TTL 変更やテスト用)。warm の待ち行列も空にする。
pub fn clear() {
    let mut guard = TABLE.locked();
    let mut q = WARM_QUEUE.locked();
    if let Some(t) = guard.as_mut() {
        t.clear();
    }
    q.clear();
}

/// `/dns` の並べ替えの鍵 (T13.4)。知らない値は既定 (`age`) に倒す
/// (`/status?sort=` と同じ方針で、綴り違いで 400 を返すより今までどおり返す方が安全)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DnsSort {
    /// 解決してからの経過が長い順 (既定。期限に近いものが上に来る)
    #[default]
    Age,
    /// ホスト名の順
    Host,
    /// OS に問い合わせた回数の多い順 (先回りが効いていない名前が上に来る)
    Misses,
}

impl DnsSort {
    pub fn from_param(v: &str) -> DnsSort {
        match v {
            "host" => DnsSort::Host,
            "misses" => DnsSort::Misses,
            _ => DnsSort::Age,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            DnsSort::Age => "age",
            DnsSort::Host => "host",
            DnsSort::Misses => "misses",
        }
    }
}

/// 1 行に出すアドレスの上限 (CDN は A / AAAA を 10 本以上返すことがある)。
pub const MAX_ROW_ADDRS: usize = 8;

/// `/dns` の 1 行 (T13.4)。表の中身をそのまま写したもの。
#[derive(Debug, Clone)]
pub struct TableRow {
    pub host: String,
    /// 覚えているアドレス (多いときは [`MAX_ROW_ADDRS`] 本で切る)
    pub addrs: Vec<IpAddr>,
    /// 覚えている本数 (切る前)
    pub addr_count: usize,
    /// 解決してからの秒
    pub age_secs: u64,
    /// 残り TTL (秒。0 = 期限切れ)
    pub ttl_left: u64,
    /// 最後に引いてからの秒
    pub idle_secs: u64,
    /// このホストで最後に勝った族 (`Some(true)` = IPv6。T12.1)
    pub win_v6: Option<bool>,
    /// 負のキャッシュ: (何秒前に失敗したか, 理由)
    pub failed: Option<(u64, String)>,
    /// 裏で引き直している最中か (T13.1)
    pub refreshing: bool,
    /// warm な名前か = 使われなくても 3/4 TTL ごとに引き直す名前か (T14.1)
    pub warm: bool,
    /// 次に裏で引き直すまでの秒 (warm でなければ `None`。T14.1)
    pub next_refresh_secs: Option<u64>,
    /// OS に問い合わせた回数と、期限前に裏で引き直した回数
    pub misses: u64,
    pub refreshes: u64,
}

impl TableRow {
    /// `/dns` の 1 要素。
    pub fn to_json(&self) -> String {
        let addrs: Vec<String> = self.addrs.iter().map(|a| format!("\"{}\"", a)).collect();
        let failed = match &self.failed {
            Some((secs, msg)) => format!(
                "{{\"secs_ago\":{},\"error\":\"{}\"}}",
                secs,
                // 理由は `getaddrinfo` の文言なので短い。念のため 120 B で切る
                crate::json::escape(&clip(msg, 120))
            ),
            None => "null".to_string(),
        };
        format!(
            "{{\"host\":\"{}\",\"addrs\":[{}],\"addr_count\":{},\"age_secs\":{},\"ttl_left\":{},\"idle_secs\":{},\"win_v6\":{},\"failed\":{},\"refreshing\":{},\"warm\":{},\"next_refresh_secs\":{},\"misses\":{},\"refreshes\":{}}}",
            crate::json::escape(&self.host),
            addrs.join(","),
            self.addr_count,
            self.age_secs,
            self.ttl_left,
            self.idle_secs,
            match self.win_v6 {
                Some(true) => "true",
                Some(false) => "false",
                None => "null",
            },
            failed,
            self.refreshing,
            self.warm,
            match self.next_refresh_secs {
                Some(s) => s.to_string(),
                None => "null".to_string(),
            },
            self.misses,
            self.refreshes,
        )
    }
}

/// 文字列を `max` バイト以内に切る (文字の途中で切らない)。
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// 名前解決の表の中身 (`/dns`。T13.4)。**鍵の内側では複製だけして、並べ替えも
/// 組み立ても外でやる** (表は要求の経路が取る鍵なので、長く握らない)。
pub fn table(sort: DnsSort) -> Vec<TableRow> {
    let now = Instant::now();
    let ttl = ttl();
    let mut rows: Vec<TableRow> = {
        let guard = TABLE.locked();
        let Some(t) = guard.as_ref() else {
            return Vec::new();
        };
        t.iter()
            .map(|(host, e)| {
                let age = now.saturating_duration_since(e.resolved_at);
                TableRow {
                    host: host.clone(),
                    addrs: e.addrs.iter().take(MAX_ROW_ADDRS).copied().collect(),
                    addr_count: e.addrs.len(),
                    age_secs: age.as_secs(),
                    ttl_left: ttl.saturating_sub(age).as_secs(),
                    idle_secs: now.saturating_duration_since(e.last_used).as_secs(),
                    win_v6: e.last_win_v6,
                    failed: e.failed_at.as_ref().map(|(at, _, msg)| {
                        (now.saturating_duration_since(*at).as_secs(), msg.clone())
                    }),
                    refreshing: e.refreshing,
                    warm: e.warm,
                    next_refresh_secs: e
                        .next_refresh
                        .map(|at| at.saturating_duration_since(now).as_secs()),
                    misses: e.misses,
                    refreshes: e.refreshes,
                }
            })
            .collect()
    };
    // 同点は名前で崩す (どの鍵でも順序が 1 つに決まる。`/status?sort=` と同じ方針)
    rows.sort_by(|a, b| match sort {
        DnsSort::Age => b
            .age_secs
            .cmp(&a.age_secs)
            .then_with(|| a.host.cmp(&b.host)),
        DnsSort::Host => a.host.cmp(&b.host),
        DnsSort::Misses => b.misses.cmp(&a.misses).then_with(|| a.host.cmp(&b.host)),
    });
    rows
}

/// 覚えている名前の数 (`/dns` の `count`)。
pub fn entries() -> usize {
    TABLE.locked().as_ref().map_or(0, HashMap::len)
}

/// `/status` の `"dns"` 要素。
pub fn status_json() -> String {
    let entries = entries();
    let (us, misses) = resolve_cost_total();
    format!(
        "{{\"ttl_secs\":{},\"negative_ttl_secs\":{},\"warm_secs\":{},\"entries\":{},\"warm\":{},\"hits\":{},\"misses\":{},\"stale_served\":{},\"negative_hits\":{},\"refreshes\":{},\"miss_ms_sum\":{:.1},\"miss_avg_ms\":{:.2}}}",
        TTL_SECS.load(Ordering::Relaxed),
        NEGATIVE_SECS.load(Ordering::Relaxed),
        WARM_SECS.load(Ordering::Relaxed),
        entries,
        warm_count(),
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
        e.prev_used = e.last_used;
    }

    /// warm な名前か (T14.1)。
    fn is_warm(host: &str) -> bool {
        TABLE
            .locked()
            .as_ref()
            .and_then(|t| t.get(host))
            .is_some_and(|e| e.warm)
    }

    /// 次に裏で引き直すまでの ms (warm でなければ `None`)。秒で見ると切り捨てで
    /// 1 秒ずれるので ms で測る。
    fn next_in_ms(host: &str) -> Option<u128> {
        let now = Instant::now();
        TABLE
            .locked()
            .as_ref()
            .and_then(|t| t.get(host))
            .and_then(|e| e.next_refresh)
            .map(|at| at.saturating_duration_since(now).as_millis())
    }

    /// 次の引き直しがおよそ `want` 秒後に予定されていること。
    fn assert_next_in(host: &str, want: u64, what: &str) {
        let ms = next_in_ms(host).unwrap_or_else(|| panic!("{}: 予定が無い", what));
        let want = u128::from(want) * 1000;
        assert!(
            ms + 500 >= want && ms <= want,
            "{}: 次の引き直しは {} ms 後 (期待 {} ms)",
            what,
            ms,
            want
        );
    }

    /// 「次に引き直す時刻」を今にして `dns-refresh` を起こす (秒を待たずに周期を回す)。
    fn warm_due_now(host: &str) {
        let now = Instant::now();
        {
            let mut guard = TABLE.locked();
            let table = guard.get_or_insert_with(HashMap::new);
            let mut q = WARM_QUEUE.locked();
            let old: Vec<_> = q.iter().filter(|(_, k)| k == host).cloned().collect();
            for o in old {
                q.remove(&o);
            }
            q.insert((now, host.to_string()));
            if let Some(e) = table.get_mut(host) {
                e.next_refresh = Some(now);
            }
        }
        wake_refresher();
    }

    /// warm が外れるまで待つ (最大 5 秒)。
    fn wait_unwarm(host: &str) {
        for _ in 0..500 {
            if !is_warm(host) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("{} is still warm", host);
    }

    /// 表に答えを直接置く (`getaddrinfo` を呼ばずに「当たる名前」を作る)。
    fn put(host: &str, last_used_ago: Duration) {
        let now = Instant::now();
        let mut guard = TABLE.locked();
        let table = guard.get_or_insert_with(HashMap::new);
        let mut e = Entry::empty(now, None);
        e.addrs = vec![IpAddr::from([127, 0, 0, 1])];
        e.last_used = now - last_used_ago;
        e.prev_used = e.last_used;
        table.insert(host.to_string(), e);
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

    /// T14.1 (a): TTL 4 秒・W 60 秒で t=0 と t=10 に使った名前は warm になり、
    /// 使われないまま 3 秒 (3/4 TTL) ごとに裏で引き直されるので **t=40 でもヒット**する。
    ///
    /// 3 秒ずつ実時間を待たずに、`warm_due_now` で「次に引き直す時刻」を今にして回す。
    #[test]
    fn a_warm_name_is_kept_fresh_while_it_is_not_used() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_ttl(Duration::from_secs(4));
        set_warm_window(Duration::from_secs(60));
        clear();
        let host = "localhost";
        // t=0: 表に無いので同期で引く (ミス)。1 回目なので warm にはしない
        resolve_host(host, 80).unwrap();
        assert!(!is_warm(host), "1 回目の使用では warm にしない");

        // t=10: TTL 4 秒は切れているのでミスだが、直近 60 秒に **2 回目**なので warm になる
        age_entry(host, Duration::from_secs(10), Duration::from_secs(10));
        let (_, m0, _, r0) = tally();
        resolve_host(host, 80).unwrap();
        assert_eq!(tally().1 - m0, 1, "t=10 はまだミス (先回りはこれから)");
        assert!(is_warm(host), "2 回目の使用で warm");
        assert_next_in(host, 3, "warm になった直後");

        // t=13, 16, ... 37: 使われていないが裏で引き直し続ける
        for i in 1..=9 {
            warm_due_now(host);
            wait_refreshes(r0 + i);
            assert!(is_warm(host), "{} 回目の引き直しでも warm のまま", i);
            assert_next_in(host, 3, "引き直しの直後");
        }

        // t=40: 最後の引き直しから 3 秒しか経っていない (最後の使用からは 30 秒)
        age_entry(host, Duration::from_secs(3), Duration::from_secs(30));
        let (h0, m0, _, _) = tally();
        resolve_host(host, 80).unwrap();
        let (h1, m1, _, _) = tally();
        assert_eq!((h1 - h0, m1 - m0), (1, 0), "t=40 でもヒット");
        set_ttl(Duration::from_secs(60));
        clear();
    }

    /// T14.1 (b): 1 回しか使われていない名前は warm にならないので、裏の引き直しも
    /// 走らず t=10 で**ミス**になる (誰も来ない名前に `getaddrinfo` を払わない)。
    #[test]
    fn a_name_used_once_never_becomes_warm() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_ttl(Duration::from_secs(4));
        set_warm_window(Duration::from_secs(60));
        clear();
        let host = "localhost";
        resolve_host(host, 80).unwrap();
        assert!(!is_warm(host));
        assert_eq!(warm_count(), 0, "待ち行列は空のまま");

        // t=10: 1 回しか使われていないので誰も引き直していない = 期限切れのミス
        age_entry(host, Duration::from_secs(10), Duration::from_secs(10));
        let (h0, m0, _, r0) = tally();
        resolve_host(host, 80).unwrap();
        let (h1, m1, _, r1) = tally();
        assert_eq!((h1 - h0, m1 - m0), (0, 1), "t=10 はミス");
        assert_eq!(r1, r0, "裏の引き直しは 1 回も走っていない");
        set_ttl(Duration::from_secs(60));
        clear();
    }

    /// T14.1 (c): 最後の使用から W 秒過ぎたら warm を外して引き直しを止める。
    #[test]
    fn a_warm_name_stops_when_nobody_uses_it() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_ttl(Duration::from_secs(4));
        set_warm_window(Duration::from_secs(60));
        clear();
        let host = "localhost";
        resolve_host(host, 80).unwrap();
        age_entry(host, Duration::from_secs(1), Duration::from_secs(1));
        resolve_host(host, 80).unwrap();
        assert!(is_warm(host), "2 回目の使用で warm");

        // 最後の使用から 61 秒 (W = 60 秒の外)
        age_entry(host, Duration::from_secs(1), Duration::from_secs(61));
        let r0 = tally().3;
        warm_due_now(host);
        wait_unwarm(host);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(tally().3, r0, "窓の外に出た名前は引き直さない");
        assert_eq!(warm_count(), 0, "待ち行列からも外れる");
        assert_eq!(next_in_ms(host), None);
        set_ttl(Duration::from_secs(60));
        clear();
    }

    /// T14.1 (d): 同時に warm でいられるのは [`MAX_WARM`] 件まで。33 個目を入れると
    /// **最後の使用がいちばん古い** 1 件が外れる。
    #[test]
    fn the_warm_set_is_capped_and_drops_the_oldest() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_ttl(Duration::from_secs(60));
        set_warm_window(WARM);
        clear();
        let names: Vec<String> = (0..=MAX_WARM).map(|i| format!("t141-warm-{i}")).collect();
        // 答えを表に直接置いてから引く = 当たり + 2 回目の使用 で warm になる
        // (`getaddrinfo` は 1 回も呼ばない)
        for n in &names {
            put(n, Duration::from_secs(1));
        }
        for (i, n) in names.iter().enumerate() {
            resolve_host(n, 80).unwrap();
            assert!(is_warm(n), "{} は warm", n);
            let want = (i + 1).min(MAX_WARM);
            assert_eq!(warm_count(), want, "{} 件目", i + 1);
        }
        assert!(!is_warm(&names[0]), "33 件目で最古 ({}) が外れる", names[0]);
        assert_eq!(next_in_ms(&names[0]), None);
        assert!(is_warm(&names[MAX_WARM]), "いちばん新しいものは warm");
        clear();
    }

    /// T14.1 (e): `PROXY_DNS_WARM_SECS=0` なら keep-warm は働かず、T13.1 の動き
    /// (直近 TTL 内に使われた名前だけ、当たりのついでに 1 回引き直す) に戻る。
    #[test]
    fn zero_warm_window_falls_back_to_t131() {
        let _guard = RESOLVE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_ttl(Duration::from_secs(60));
        set_warm_window(Duration::ZERO);
        clear();
        let host = "localhost";
        resolve_host(host, 80).unwrap();
        // t=50: 期限の 3/4 を過ぎた「熱い」名前 → 当たりを返して裏で 1 回引き直す
        age_entry(host, Duration::from_secs(50), Duration::from_secs(50));
        let (h0, m0, _, r0) = tally();
        resolve_host(host, 80).unwrap();
        let (h1, m1, _, _) = tally();
        assert_eq!((h1 - h0, m1 - m0), (1, 0), "T13.1 どおり当たり");
        wait_refreshes(r0 + 1);
        assert!(!is_warm(host), "0 なら warm にしない");
        assert_eq!(warm_count(), 0, "待ち行列は空のまま");

        // t=61: 引き直しから 11 秒なのでまだ期限内 (T13.1 の (a) と同じ)
        age_entry(host, Duration::from_secs(11), Duration::from_secs(70));
        let (h2, m2, _, _) = tally();
        resolve_host(host, 80).unwrap();
        let (h3, m3, _, _) = tally();
        assert_eq!((h3 - h2, m3 - m2), (1, 0), "t=61 でもヒット");
        set_warm_window(WARM);
        clear();
    }
}
