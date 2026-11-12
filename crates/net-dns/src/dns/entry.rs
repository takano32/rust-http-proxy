//! 表 (`TABLE`) の 1 行 (`Entry`) と、鍵の内側の判断 (`Entry::look` → `Lookup`。T34.55)、
//! 鍵の内側で決めて鍵の外で数える `Looked` / `MissKind`、
//! 行の追い出し (`evict_oldest`)、答えの集合が変わったかの比べ方 (`addrs_changed`)。T34.54 で
//! `dns.rs` から移した (`addrs_changed` は `dns_warm.rs` から。本体は変えていない)。
//! 欄は子 (`view`・`warm`・`pool`・`lookup`・試験) が直に触るので `pub(super)`。

use super::{MISS_COLD, MISS_EXPIRED, MISS_NEGATIVE, MISS_WARM_STALE, STALE_MAX, refresh_after};
use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

pub(super) struct Entry {
    pub(super) addrs: Vec<IpAddr>,
    pub(super) resolved_at: Instant,
    /// この名前を最後に引いた時刻 (T13.1)。**期限前に裏で引き直すのは「直近 TTL 内に
    /// 使われた熱いホスト」だけ**にするための印で、表に当たるたびに書く
    /// (既に鍵の内側なので鍵取りは増えない)。
    pub(super) last_used: Instant,
    /// **1 つ前**の使用時刻 (T14.1)。warm の判定 (「直近 W 秒に 2 回以上」) に要るのは
    /// これだけで、要求の経路は `last_used` を押し出すだけ (鍵は既に取っている)。
    pub(super) prev_used: Instant,
    /// warm な名前か = [`WARM_QUEUE`](super::WARM_QUEUE) に載っているか (T14.1)。**熱い経路で鍵を 2 つ
    /// 取らないための写し**で、待ち行列への出し入れと必ず同時に書く。
    pub(super) warm: bool,
    /// 次に裏で引き直す時刻 (`/dns` の `next_refresh_secs`。warm でなければ `None`)。
    pub(super) next_refresh: Option<Instant>,
    /// 裏で引き直している最中 (同じ名前の引き直しを 1 本に絞る旗)。
    pub(super) refreshing: bool,
    /// この名前で OS に問い合わせた回数 (`/dns?sort=misses`。T13.4)。
    /// **書くのは既に表の鍵を取っている場所だけ**なので、原子操作は増えない
    pub(super) misses: u64,
    /// この名前を期限前に裏で引き直した回数 (T13.4)
    pub(super) refreshes: u64,
    /// 引き直しで答えの集合が変わった回数 (`/dns` の `changes`。T14.37)。
    /// **最初に答えを得たときは数えない** (変化ではないので)
    pub(super) changes: u64,
    /// この名前のミスを種類別に数えたもの (`/dns` の `misses_by_kind`。T15.0 (7))。
    /// 並びは [`MissKind::index`] で、**和は `misses` と一致する**。
    /// 表いっぱい (4,096 件) でも +128 KiB
    pub(super) misses_by_kind: [u64; 4],
    /// **warm な状態で**この名前が引かれた回数 (`/dns` の `warm_requests`。T15.0 (7))。
    /// keep-warm が実際に何回の要求を救ったかは、これと `refreshes` を比べて読む
    /// (引き直し 1 回あたり何回の要求が当たったか)。比べる相手が累計なので、
    /// **warm を外れて入り直しても 0 に戻さない**
    pub(super) warm_requests: u64,
    /// 直近の失敗 (負のキャッシュ)
    pub(super) failed_at: Option<(Instant, io::ErrorKind, String)>,
    /// このホストで最後に接続できた族 (`Some(true)` = IPv6)。RFC 8305 §8 の
    /// 「過去の結果で優先する族を変える」ための記憶で、**TTL で引き直しても残す**
    /// (アドレスは変わっても、そのホストへどちらの族で届くかは変わりにくい)。
    pub(super) last_win_v6: Option<bool>,
}

impl Entry {
    /// まだ答えの無い入れ物 (族の記憶だけ先に置くことがある)。
    pub(super) fn empty(now: Instant, last_win_v6: Option<bool>) -> Entry {
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
            changes: 0,
            misses_by_kind: [0; 4],
            warm_requests: 0,
            failed_at: None,
            last_win_v6,
        }
    }
}

/// 表を引いて出た答え。**数える (原子操作) のは鍵を放してから**なので、
/// 鍵の内側では「どれだったか」だけを持って出る (T12.4 (2))。
pub(super) enum Looked {
    /// 期限内の答え
    Hit(Vec<IpAddr>),
    /// 覚えている失敗より優先した古い答え
    Stale(Vec<IpAddr>),
    /// serve-stale で返す期限切れの答え (T21.3。ミスではない)
    Served(Vec<IpAddr>),
    /// 覚えている失敗 (負のキャッシュ)
    Negative(io::Error),
}

/// ミス 1 件の種類 (T15.0 (7))。**表の鍵の内側でしか決められない**: 鍵を放したあとの
/// 書き戻しは `entry().or_insert_with()` で入れ物を作り直すので、「ミスの直前の姿」が
/// 読めない。[`Looked`] と同じ流儀で、鍵の内側で決めて外で数える。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MissKind {
    /// 表にまだ答えが無い (初めての名前、追い出された名前、族の記憶だけの入れ物、TTL 0)
    Cold,
    /// 答えは持っていたが TTL を過ぎた (窓の外なので誰も引き直していない)
    Expired,
    /// **warm なのにミスした** = 裏の引き直しが間に合っていない (T15.4 の主役)
    WarmStale,
    /// 覚えている失敗の期限が切れたので引き直す (前回引けなかった名前)
    Negative,
}

impl MissKind {
    /// `misses_by_kind` の添字 (`/status` と `/dns` の並びと同じ)。
    pub(super) fn index(self) -> usize {
        match self {
            MissKind::Cold => 0,
            MissKind::Expired => 1,
            MissKind::WarmStale => 2,
            MissKind::Negative => 3,
        }
    }

    /// この種類を数える静的カウンタ。
    pub(super) fn counter(self) -> &'static AtomicU64 {
        match self {
            MissKind::Cold => &MISS_COLD,
            MissKind::Expired => &MISS_EXPIRED,
            MissKind::WarmStale => &MISS_WARM_STALE,
            MissKind::Negative => &MISS_NEGATIVE,
        }
    }
}

/// 表を 1 回引いたときに**鍵の内側で**決めたこと ([`Entry::look`] の答え。T34.55)。
/// 数える (原子操作) のも、待ち行列に入れたあとで起こすのも、鍵を放してから呼び出し側がやる。
pub(super) struct Lookup {
    /// このホストで最後に勝った族 (T12.1)
    pub(super) pref: Option<bool>,
    /// 表から答えが出たか (`None` = このあとミスになる)
    pub(super) found: Option<Looked>,
    /// ミスだったときの種類 (T15.0 (7))
    pub(super) miss_kind: MissKind,
    /// 裏の引き直しを頼むか (期限前の先回り (T13.1) と serve-stale (T21.3))
    pub(super) ask: bool,
    /// この参照で warm になったか (T14.1)。立っていれば呼び出し側が `warm_promote` を呼ぶ
    pub(super) promote: bool,
}

impl Lookup {
    /// 表に載っていない名前 (`cold` のミス)。
    pub(super) const ABSENT: Lookup = Lookup {
        pref: None,
        found: None,
        miss_kind: MissKind::Cold,
        ask: false,
        promote: false,
    };
}

impl Entry {
    /// 要求の経路で表に当たった行を読み、使った印を書き、答え・ミスの種類・裏の引き直しを決める
    /// (T34.55 で `resolve_host_marked` の鍵の内側をそのまま移した。判断の順は前と同じ)。
    ///
    /// 時刻と設定値は引数で受けるので、表 (`TABLE`) も `Instant::now()` も経ずに試験できる。
    /// `negative` と `serve_stale` は**要る枝でだけ呼ぶ** (当たりの経路で原子を読まないため。
    /// 本番は `negative_ttl` / `serve_stale` を渡す)。
    #[inline]
    pub(super) fn look(
        &mut self,
        now: Instant,
        ttl: Duration,
        window: Duration,
        negative: impl Fn() -> Duration,
        serve_stale: impl Fn() -> Duration,
    ) -> Lookup {
        let e = self;
        let pref = e.last_win_v6;
        let mut found: Option<Looked> = None;
        let mut miss_kind = MissKind::Cold;
        let mut ask = false;
        // **`warm` はここで読む** (T15.0 (7)): 下の `warm_promote` が `e.warm = true` を
        // 書くので、あとで読むと「2 回目の使用でミスした」が `warm_stale` に化ける
        let was_warm = e.warm;
        // 「熱い」= **この参照の 1 つ前**の使用が TTL 以内。印はここで更新する
        let idle = now.duration_since(e.last_used);
        let hot = idle < ttl;
        // warm = 直近 W 秒に **2 回以上**使われた名前。この参照が 2 回目なら
        // 待ち行列に入れる (1 回だけの名前は入れない。引き直しても二度と来ない)。
        // **答えを持っている名前だけ**を warm にする: 引けない名前は先回りする
        // 期限を持たず (要求の経路は負のキャッシュで止まる)、1 回 約 2 秒かかるので、
        // 待ち行列に入れると `dns-refresh` の 1 本が本当に熱い名前を待たせる
        let promote = !e.warm && !window.is_zero() && idle < window && !e.addrs.is_empty();
        e.prev_used = e.last_used;
        // warm の間に来た要求を数える (T15.0 (7)。**「要求の経路は増やさない」の
        // 例外その 1**: 既に握っている鍵の内側の、非原子の加算 1 つ・分岐なしで、
        // システムコールも確保も増えない)
        e.warm_requests += u64::from(was_warm);
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
            let negative = negative();
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
            } else if !e.addrs.is_empty() && age < serve_stale() {
                // serve-stale (T21.3): 期限は切れたが齢が窓の中 = 古い答えをそのまま返し、
                // 裏で 1 回だけ引き直す (同じ名前を二重に頼まない)。**覚えている失敗が
                // あるときは上の枝が先** (負のキャッシュの間はリゾルバを叩かない)
                ask = !e.refreshing;
                if ask {
                    e.refreshing = true;
                }
                found = Some(Looked::Served(e.addrs.clone()));
            }
        }
        if found.is_none() {
            // ここまで来た = 表では答えられない = このあとミスになる。種類の優先順位は
            // **warm_stale → negative → expired → cold** (T15.0 (7))。`refresh_one` は
            // 失敗しても `failed_at` を書かない (上のコメント) ので、warm な名前に
            // 載っている失敗は要求の経路が書いたもの = 「引き直しが間に合っていない」の
            // 証拠は `warm` の旗の方が強い
            miss_kind = if was_warm {
                MissKind::WarmStale
            } else if e.failed_at.is_some() {
                MissKind::Negative
            } else if !e.addrs.is_empty() {
                MissKind::Expired
            } else {
                // 答えを持っていない入れ物 (`remember_family` が族の記憶だけ置いた等)
                MissKind::Cold
            };
        }
        Lookup {
            pref,
            found,
            miss_kind,
            ask,
            promote,
        }
    }
}

/// 1 行捨てる。**答えの無い行 (失敗の行・族の記憶だけの行) を先に**、その中で古い順 (T32.2)。
///
/// 答えのある行は serve-stale (21,600 秒) と古い答えで凌ぐ窓のために持っているが、答えの無い
/// 行が持っているのは負のキャッシュ (既定 60 秒) だけで、捨てても次の 1 回が OS に行くだけ。
/// 全部を舐める (4,096 行で 1 回 数十 us) が、呼ぶのは OS に問い合わせたミス (ms の単位) と
/// 族の記憶の新しい行のときだけなので、古い順の印は持たない。
pub(super) fn evict_oldest(table: &mut HashMap<String, Entry>) {
    if let Some(k) = table
        .iter()
        .min_by_key(|(_, e)| (!e.addrs.is_empty(), e.resolved_at))
        .map(|(k, _)| k.clone())
    {
        table.remove(&k);
    }
}

/// 引き直しで**答えの集合**が変わったか (T14.37)。
///
/// **順序は見ない**: `getaddrinfo` は同じ答えを毎回違う順で返すことがあり (CDN の
/// ラウンドロビン、`rotate` を有効にした resolv.conf)、並びの入れ替わりを「変化」と
/// 数えると `changes` が「本当に別のサーバーに振られた回数」として読めなくなる。
///
/// **前の答えが空のとき (= 最初の解決、族の記憶だけ置いた入れ物) は変化と数えない。**
/// 1 つの名前のアドレスは多くても十数本で、`getaddrinfo` は同じアドレスを 2 度返さない
/// ので、本数と両向きの包含で足りる (並べ替えも確保もしない)。
pub(super) fn addrs_changed(old: &[IpAddr], new: &[IpAddr]) -> bool {
    if old.is_empty() || old.len() != new.len() {
        return !old.is_empty();
    }
    !new.iter().all(|a| old.contains(a)) || !old.iter().all(|a| new.contains(a))
}

/// `Entry::look` の単体試験 (表も時計も経ない。T34.55)。
#[cfg(test)]
mod tests;
