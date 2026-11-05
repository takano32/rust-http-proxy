//! IPv4 / IPv6 デュアルスタックのためのネットワーク補助 (`PROXY_IPV6=off` で IPv4 のみ)。
//!
//! - 待ち受け: `[::]` と `0.0.0.0` の両方を試し、IPv6 が無い環境では IPv4 だけにフォールバック。
//!   IPv6 無効時は `0.0.0.0` のみ
//! - 接続: A / AAAA の両方を引き、IPv6 優先で 250 ms ずつずらして並行に試す (Happy Eyeballs,
//!   RFC 8305)。IPv6 無効時は A レコードだけ
//! - `[2001:db8::1]:8080` 形式のホスト・ポート解析と、v4-mapped アドレス (`::ffff:1.2.3.4`) の正規化
//!
//! 責務ごとに子のファイルへ分けてある (T34.3)。公開の名前はここで出し直すので、
//! 呼ぶ側の綴り (`net::connect` など) は `net.rs` が 1 枚だった頃と同じ:
//!
//! - `listen`: 待ち受け (backlog の既定と、デュアルスタックの `bind` の判断)
//! - `addrs`: アドレス列の純粋な並べ替え (v4-mapped の正規化、RFC 8305 §4 の交互配置)
//! - `ipv6`: IPv6 の全体の勝敗と「初めて見るホストで IPv6 を先頭に置くか」の判断 (`Ipv6Policy`)
//! - `happy_eyeballs`: 候補列へ並行に繋ぐ本体 (要求ごとに通る熱い経路) と間隔 (`PROXY_HE_STAGGER_MS`)
//! - `connect`: 名前解決と組み合わせた入口と、serve-stale の古い答えで繋げなかったときの繋ぎ直し

mod addrs;
mod connect;
mod happy_eyeballs;
mod ipv6;
mod listen;

#[cfg(test)]
mod ipv6_tests;
#[cfg(test)]
mod tests;

// `host:port` の分解と組み立ては `proxy-base` の `hostport` に置いてある
// (名前解決・接続・判定の 3 クレートが使うため)。今までどおり `net::split_host_port_ref`
// の綴りで呼べるように、このモジュールから出し直す。
pub use crate::hostport::{
    join_host_port, split_host_port, split_host_port_ref, with_default_port,
};
pub use addrs::{canonical_addr, canonical_ip};
pub use connect::{connect, connect_resolved, connect_with};
#[cfg(test)]
use happy_eyeballs::MAX_CANDIDATES;
pub use happy_eyeballs::{DEFAULT_STAGGER, STAGGER_RANGE_MS, connect_addr, set_stagger, stagger};
pub use ipv6::{
    IPV6_TEST_LOCK, ipv6_counters, ipv6_enabled, ipv6_status_json, ipv6_v4_first, note_canary_ipv6,
    set_ipv6_enabled,
};
pub use listen::STD_BACKLOG;
#[cfg(test)]
use listen::{DEFAULT_BACKLOG_CAP, bind_all_with};
pub use listen::{bind_all, default_backlog, describe_listener};
