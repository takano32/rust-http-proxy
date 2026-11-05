//! スレッドごとの費用の箱 (名前解決の費用・直近に確立した族・SYN の再送回数) と、その読み書き。
//! **原子操作を増やさないための thread-local** で、読む側が 0 に戻す。T.34.54 で `dns.rs` から移した
//! (本体は変えていない)。公開の道は親の `dns` が出し直すので `proxy_net_dns::dns::…` のまま。

use super::{MISSES, RESOLVE_US_SUM};
use std::cell::Cell;
use std::sync::atomic::Ordering;

thread_local! {
    /// このスレッドが直近に払った名前解決の費用 (us の合計と回数)。**ホスト別の内訳に
    /// 使う**もので、原子操作を増やさないための thread-local。読む側が 0 に戻す。
    ///
    /// `const` で初期化しているので destructor が登録されず、スレッドの終了中に触っても
    /// `AccessError` にならない (`Drop` を持つ thread-local と違って `with` で足りる)。
    pub(super) static RESOLVE_COST: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
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
/// `PROXY_ALLOW_LOCAL=false` (既定) では入口の ACL (`crates/server/src/conn/serve_one.rs` の `acl::resolve_target`)
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
