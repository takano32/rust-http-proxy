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
use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
// 本体では使わない。テスト (`use super::*`) の `thread::sleep` のために残す (T21.7)
#[cfg(test)]
use std::thread;
use std::time::{Duration, Instant};

// 責務ごとに子のファイルへ分けてある (T21.7)。子は親の private な表とカウンタをそのまま触るので、
// 中身は `dns.rs` にあった頃と同じ。公開の名前はここで出し直す (`proxy_net_dns::dns::…` のまま)。
#[path = "dns_view.rs"]
mod view;
#[path = "dns_warm.rs"]
mod warm;
pub use view::{DnsSort, MAX_ROW_ADDRS, TableRow, counters, entries, status_json, table};
use warm::{
    addrs_changed, refresh_after, request_refresh, wake_refresher, warm_clear, warm_promote,
};

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
    /// このスレッドで直近に確立した接続の **SYN の再送回数** (T14.46)。
    /// 書くのは [`note_syn_retrans`] (確立した直後の 1 か所だけ)、読む側が 0 に戻す。
    /// 確立直後の `tcpi_total_retrans` は SYN の再送しか数えていないので、
    /// **値がそのまま「この接続は確立に何回 SYN を送り直したか」**になる
    static LAST_SYN_RETRANS: Cell<u8> = const { Cell::new(0) };
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

/// 直近の名前解決の費用を**読むだけ** (0 に戻さない。T15.0 (1))。丸め方は
/// [`take_resolve_cost`] と同じなので、同じ箱を読んでいる限り値は一致する。
///
/// 使い道は 1 つで、**自分の時計を始める前に「もう払われているぶん」を控える**こと。
/// `PROXY_ALLOW_LOCAL=false` (既定) では入口の ACL (`crates/server/src/lib.rs` の `acl::resolve_target`)
/// がトンネルの時計より前に名前を引くので、あとで `take` した費用には**自分の窓の外**の
/// ぶんが混ざっている。それを引かずに「全体 − 名前解決」をすると、接続の段が 0 に潰れる。
pub fn peek_resolve_cost() -> (u64, u64) {
    let (us, n) = RESOLVE_COST.get();
    ((us + 500) / 1000, n)
}

/// 名前解決の費用 (us と回数) をこのスレッドの箱に足す (**テストの口**)。
///
/// 本番でここに書くのは [`resolve_host`] のミスの経路 1 か所だけ (原子と同じ場所で
/// 書いている) なので、呼ぶのはテストだけ。入口の ACL が**時計より前に**払った状態を
/// 作って、[`peek_resolve_cost`] を使う引き算 (T15.0 (1)) を確かめるために使う。
pub fn note_resolve_cost(us: u64, misses: u64) {
    RESOLVE_COST.set({
        let (s, n) = RESOLVE_COST.get();
        (s + us, n + misses)
    });
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

/// 直近に確立した接続の SYN の再送回数を読み、0 に戻す (T14.46)。
///
/// **測る前にも 1 回呼んで捨てること** ([`take_resolve_cost`] と同じ理由。
/// 前の要求が残していったぶんを次のホストに付けない)。
pub fn take_syn_retrans() -> u8 {
    LAST_SYN_RETRANS.replace(0)
}

/// 確立した接続の SYN の再送回数を控える (`crate::net` の確立点だけが呼ぶ)。
/// thread-local への書き込み 1 回で、原子操作は増えない (`getsockopt` 1 回は
/// 呼ぶ側が払う)。255 で頭打ち。
pub fn note_syn_retrans(n: u8) {
    LAST_SYN_RETRANS.set(n);
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
    /// 引き直しで答えの集合が変わった回数 (`/dns` の `changes`。T14.37)。
    /// **最初に答えを得たときは数えない** (変化ではないので)
    changes: u64,
    /// この名前のミスを種類別に数えたもの (`/dns` の `misses_by_kind`。T15.0 (7))。
    /// 並びは [`MissKind::index`] で、**和は `misses` と一致する**。
    /// 表いっぱい (4,096 件) でも +128 KiB
    misses_by_kind: [u64; 4],
    /// **warm な状態で**この名前が引かれた回数 (`/dns` の `warm_requests`。T15.0 (7))。
    /// keep-warm が実際に何回の要求を救ったかは、これと `refreshes` を比べて読む
    /// (引き直し 1 回あたり何回の要求が当たったか)。比べる相手が累計なので、
    /// **warm を外れて入り直しても 0 に戻さない**
    warm_requests: u64,
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
            changes: 0,
            misses_by_kind: [0; 4],
            warm_requests: 0,
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

/// 判定 (ACL) が引いた答えを、そのまま接続まで運ぶための入れ物 (T12.7)。
///
/// ローカル宛ての判定 (`proxy_net::acl::resolve_target`) と、その直後の接続
/// (`proxy_net::net::connect_with`) は**同じ答えを使う**。名前解決が 1 要求 1 回で
/// 済むだけでなく、`PROXY_DNS_TTL_SECS=0` (キャッシュ無効) でも判定と接続が
/// 別の答えを引かない (DNS rebinding で判定をすり抜けられない)。
pub struct Resolved<'a> {
    /// 解決したホスト名。**別のホスト宛ての接続には使わない**ための鍵で、
    /// 突き合わせは大小無視 (要求ごとに小文字の複製を作らない)
    host: &'a str,
    addrs: Vec<IpAddr>,
    /// このホストで最後に勝った族 (T12.1)。同じ鍵取りで一緒に受け取っておく
    preferred: Option<bool>,
    /// serve-stale で返した期限切れの答えか (T21.3)。立っていれば、接続側は繋げなかったとき
    /// 1 回だけ引き直して繋ぎ直す ([`relookup_after_stale`])
    stale: bool,
}

impl<'a> Resolved<'a> {
    pub fn new(host: &'a str, addrs: Vec<IpAddr>, preferred: Option<bool>) -> Resolved<'a> {
        Resolved {
            host,
            addrs,
            preferred,
            stale: false,
        }
    }

    /// serve-stale で返した答えだという印を付ける (T21.3)。
    pub fn with_stale(mut self, stale: bool) -> Resolved<'a> {
        self.stale = stale;
        self
    }

    /// serve-stale で返した期限切れの答えか (T21.3)。
    pub fn is_stale(&self) -> bool {
        self.stale
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
    resolve_with_pref_marked(addr_str).map(|(addrs, pref, _)| (addrs, pref))
}

/// [`resolve_with_pref`] に「serve-stale で返した期限切れの答えか」の印を添えて返す (T21.3)。
pub fn resolve_with_pref_marked(
    addr_str: &str,
) -> io::Result<(Vec<SocketAddr>, Option<bool>, bool)> {
    let (host, port) = crate::hostport::split_host_port_ref(addr_str);
    let Some(port) = port else {
        return addr_str
            .to_socket_addrs()
            .map(|i| (i.collect(), None, false));
    };
    // IP リテラルは表も確保も要らない (熱い経路: ベンチも `127.0.0.1:port` で来る)
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok((vec![SocketAddr::new(ip, port)], None, false));
    }
    let (addrs, pref, stale) = resolve_host_marked(host, port)?;
    Ok((with_port(&addrs, port), pref, stale))
}

/// 表を引いて出た答え。**数える (原子操作) のは鍵を放してから**なので、
/// 鍵の内側では「どれだったか」だけを持って出る (T12.4 (2))。
enum Looked {
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
enum MissKind {
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
    fn index(self) -> usize {
        match self {
            MissKind::Cold => 0,
            MissKind::Expired => 1,
            MissKind::WarmStale => 2,
            MissKind::Negative => 3,
        }
    }

    /// この種類を数える静的カウンタ。
    fn counter(self) -> &'static AtomicU64 {
        match self {
            MissKind::Cold => &MISS_COLD,
            MissKind::Expired => &MISS_EXPIRED,
            MissKind::WarmStale => &MISS_WARM_STALE,
            MissKind::Negative => &MISS_NEGATIVE,
        }
    }
}

/// `misses_by_kind` の JSON (`/status` の `dns` と `/dns` の 1 行で同じ形)。
/// 並びは [`MissKind::index`]。
fn misses_by_kind_json(v: &[u64; 4]) -> String {
    format!(
        "{{\"cold\":{},\"expired\":{},\"warm_stale\":{},\"negative\":{}}}",
        v[0], v[1], v[2], v[3]
    )
}

/// ホスト名を解決して**アドレスだけ**を返す (T12.7)。`port` は `getaddrinfo` に渡す
/// サービス番号で、キャッシュの鍵はホスト名だけ (返す側でポートを付ける)。
///
/// IP リテラルは表を使わない (解決するものが無い)。`PROXY_DNS_TTL_SECS=0` の
/// ときも表を使わないが、**OS に投げた回数はミスとして数える**
/// (`dns.hits + dns.misses` が「解決した回数」を表すため。T12.7)。
pub fn resolve_host(host: &str, port: u16) -> io::Result<(Vec<IpAddr>, Option<bool>)> {
    resolve_host_marked(host, port).map(|(addrs, pref, _)| (addrs, pref))
}

/// [`resolve_host`] に「serve-stale で返した期限切れの答えか」の印 (3 つ目) を添えて返す (T21.3)。
/// 印が立つのは serve-stale の回だけで、IP リテラル・TTL 0・負のキャッシュ・引き直しの失敗で
/// 凌いだ古い答え (`stale_served`) では立たない。
pub fn resolve_host_marked(host: &str, port: u16) -> io::Result<(Vec<IpAddr>, Option<bool>, bool)> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok((vec![ip], None, false));
    }
    let ttl = ttl();
    if ttl.is_zero() {
        MISSES.fetch_add(1, Ordering::Relaxed);
        // 表を触らないので「直前の姿」も無い = いつも `cold` (T15.0 (7))
        MISS_COLD.fetch_add(1, Ordering::Relaxed);
        return system_resolve(host, port).map(|a| (a, None, false));
    }
    let key = host.to_ascii_lowercase();
    let now = Instant::now();
    let window = warm_window();
    let mut pref = None;
    // 表から答えが出たか (**数えるのは鍵を放してから**。T12.4 (2))
    let mut found: Option<Looked> = None;
    // ミスだったときの種類 (T15.0 (7))。表に載っていない名前は `cold`
    let mut miss_kind = MissKind::Cold;
    // 期限前の先回りを頼むか (T13.1) / この参照で warm になったか (T14.1)
    let (mut ask, mut promote) = (false, false);
    {
        // 取り合いを数える (T14.3 (3))。空いていれば `locked` と同じ費用
        let mut guard = TABLE.locked_counted(&crate::sync::LOCK_CONTENDED[crate::sync::LOCK_DNS]);
        let table = guard.get_or_insert_with(HashMap::new);
        if let Some(e) = table.get_mut(&key) {
            pref = e.last_win_v6;
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
            promote = !e.warm && !window.is_zero() && idle < window && !e.addrs.is_empty();
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
            return Ok((addrs, pref, false));
        }
        Some(Looked::Stale(addrs)) => {
            STALE.fetch_add(1, Ordering::Relaxed);
            return Ok((addrs, pref, false));
        }
        Some(Looked::Served(addrs)) => {
            // ミスではない (`misses` にも `misses_by_kind` にも入れない。T21.3)
            STALE_HITS.fetch_add(1, Ordering::Relaxed);
            return Ok((addrs, pref, true));
        }
        Some(Looked::Negative(e)) => {
            FAILURES.fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
        None => {}
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    // 種類別も同じ場所で 1 つ (T15.0 (7)。**4 つの和は `MISSES` と一致する**)
    miss_kind.counter().fetch_add(1, Ordering::Relaxed);
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
            // 期限切れの引き直しで答えが変わったか (T14.37。裏の引き直しと同じ物差し)。
            // ここは既にミスの経路 (`getaddrinfo` を待ったあと) なので、鍵の内側の
            // 比較 1 回と原子 1 つは測れない
            if addrs_changed(&slot.addrs, &addrs) {
                slot.changes += 1;
                CHANGES.fetch_add(1, Ordering::Relaxed);
            }
            slot.addrs = addrs.clone();
            slot.resolved_at = now;
            slot.last_used = now;
            slot.misses += 1;
            slot.misses_by_kind[miss_kind.index()] += 1;
            slot.failed_at = None;
            if slot.last_win_v6.is_none() {
                slot.last_win_v6 = pref;
            }
            Ok((addrs, slot.last_win_v6, false))
        }
        Err(e) => {
            let entry = table.entry(key).or_insert_with(|| Entry::empty(now, None));
            entry.misses += 1;
            entry.misses_by_kind[miss_kind.index()] += 1;
            entry.failed_at = Some((now, e.kind(), e.to_string()));
            if !entry.addrs.is_empty() && now.duration_since(entry.resolved_at) < STALE_MAX {
                STALE.fetch_add(1, Ordering::Relaxed);
                let pref = entry.last_win_v6;
                return Ok((entry.addrs.clone(), pref, false));
            }
            Err(e)
        }
    }
}

/// serve-stale で返した古い答えで繋げなかったときに、**1 回だけ**引き直す (T21.3)。
///
/// 返すのは**番地の集合が `old` から変わったときだけ** `Some((新しい答え, 族の記憶))` で、
/// 呼び出し側はそのときだけ繋ぎ直す (同じ番地へ繋ぎ直しても同じ失敗をするだけなので)。
/// 引けなかったときも `None` (呼び出し側は元の接続のエラーを返す)。
///
/// 裏の引き直し (古い答えを返したときに頼んだもの) が既に済んでいれば、表の新しい答えを使い
/// `getaddrinfo` は待たない。まだなら同期で引いて表に書き戻し、その回は**ミス (`expired`) に数える**
/// (利用者が待ったので。`stale_retries` はどちらの場合も 1 つ増える)。
/// 要求の経路で呼ばれるのは「古い答えで `refused` / `unreachable` / `timeout`」のときだけ。
pub fn relookup_after_stale(
    host: &str,
    port: u16,
    old: &[IpAddr],
) -> Option<(Vec<IpAddr>, Option<bool>)> {
    let ttl = ttl();
    if ttl.is_zero() || host.parse::<IpAddr>().is_ok() {
        return None;
    }
    STALE_RETRIES.fetch_add(1, Ordering::Relaxed);
    let key = host.to_ascii_lowercase();
    // 裏の引き直しが先に済んでいれば、その答えを使う
    let fresh = TABLE
        .locked()
        .as_ref()
        .and_then(|t| t.get(&key))
        .filter(|e| {
            !e.addrs.is_empty() && Instant::now().saturating_duration_since(e.resolved_at) < ttl
        })
        .map(|e| (e.addrs.clone(), e.last_win_v6));
    let (addrs, pref) = match fresh {
        Some(f) => f,
        None => {
            // 同期で引く。数え方は `resolve_host` のミスの経路と同じ (4 種の和 = `misses`)
            MISSES.fetch_add(1, Ordering::Relaxed);
            MISS_EXPIRED.fetch_add(1, Ordering::Relaxed);
            let t0 = Instant::now();
            let result = system_resolve(host, port);
            let now = Instant::now();
            let us = now
                .saturating_duration_since(t0)
                .as_micros()
                .min(u64::MAX as u128) as u64;
            RESOLVE_US_SUM.fetch_add(us, Ordering::Relaxed);
            RESOLVE_COST.set({
                let (s, n) = RESOLVE_COST.get();
                (s + us, n + 1)
            });
            let mut changed = false;
            let mut pref = None;
            {
                let mut guard = TABLE.locked();
                if let Some(e) = guard.as_mut().and_then(|t| t.get_mut(&key)) {
                    e.misses += 1;
                    e.misses_by_kind[MissKind::Expired.index()] += 1;
                    pref = e.last_win_v6;
                    if let Ok(addrs) = &result {
                        changed = addrs_changed(&e.addrs, addrs);
                        e.changes += u64::from(changed);
                        e.addrs = addrs.clone();
                        e.resolved_at = now;
                        e.failed_at = None;
                    }
                }
            }
            if changed {
                CHANGES.fetch_add(1, Ordering::Relaxed);
            }
            (result.ok()?, pref)
        }
    };
    addrs_changed(old, &addrs).then_some((addrs, pref))
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

/// serve-stale (T21.3) の単体テスト。dns.rs が 2,000 行を超えているので別のファイルに置く。
#[cfg(test)]
#[path = "dns_stale_tests.rs"]
mod stale_tests;

/// `dns` の単体テスト (T21.7 で別のファイルに移した)。
#[cfg(test)]
#[path = "dns_tests.rs"]
mod tests;
