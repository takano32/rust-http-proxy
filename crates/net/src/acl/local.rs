//! ローカル宛て・私設アドレス宛ての判定 (`PROXY_ALLOW_LOCAL`) と、判定と接続で同じ答えを使う
//! `resolve_target`、自己ベンチの相手役の除外 (T14.43)。T34.65 で `acl.rs` から移した (本体は変えていない)。

use crate::dns::Resolved;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, Ordering};

/// ループバック・リンクローカル (`169.254.0.0/16`, `fe80::/10`) と、未指定アドレス。
/// クラウドのメタデータ (`169.254.169.254`) 経由の SSRF を防ぐために使う。
/// `PROXY_ALLOW_LOCAL=private` でも断るのはこちら (私設アドレスは [`is_private_ip`])。
pub fn is_local_ip(ip: IpAddr) -> bool {
    match crate::net::canonical_ip(ip) {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_link_local() || v4.is_unspecified(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// 私設アドレス (T28.41): `10.0.0.0/8`・`172.16.0.0/12`・`192.168.0.0/16`・
/// `100.64.0.0/10` (CGNAT の共有アドレス、RFC 6598)・`fc00::/7` (ULA)。
/// v4-mapped IPv6 (`::ffff:10.0.0.1`) は [`crate::net::canonical_ip`] で IPv4 に直してから見る。
pub(crate) fn is_private_ip(ip: IpAddr) -> bool {
    match crate::net::canonical_ip(ip) {
        // `Ipv4Addr::is_shared` (100.64/10) はまだ安定版に無いので前置で見る
        IpAddr::V4(v4) => v4.is_private() || (u32::from(v4) >> 22) == (0x6440_0000 >> 22),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// `PROXY_ALLOW_LOCAL` の 3 つの書き方 (T28.41)。どこ宛てを 403 で断るか。
///
/// | 値 | ループバック・リンクローカル・未指定 | 私設アドレス |
/// |---|---|---|
/// | `off` (既定) | 断る | 断る |
/// | `private` | 断る | 通す |
/// | `on` | 通す | 通す (判定そのものを飛ばす) |
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AllowLocal {
    #[default]
    Off,
    Private,
    On,
}

impl AllowLocal {
    /// 私設アドレス宛てを通すか (`private` と `on`)。
    pub fn allows_private(self) -> bool {
        self != AllowLocal::Off
    }

    /// `/config` と `--check` に出す値 (JSON)。`on` / `off` は今までどおり真偽で出し
    /// (`true` / `false`)、`private` だけ文字列で出す。
    pub fn json(self) -> &'static str {
        match self {
            AllowLocal::Off => "false",
            AllowLocal::Private => "\"private\"",
            AllowLocal::On => "true",
        }
    }
}

/// 宛て先の IP を断るか (`allow_private` は `PROXY_ALLOW_LOCAL=private`)。
pub(super) fn is_refused_ip(ip: IpAddr, allow_private: bool) -> bool {
    is_local_ip(ip) || (!allow_private && is_private_ip(ip))
}

/// `host[:port]` の宛先がローカル宛てかを判定し、**判定に使った答えを一緒に返す** (T12.7)。
///
/// 名前はここで 1 回だけ解決し、接続側はこの答えをそのまま使う
/// ([`crate::net::connect_with`])。こうしないと判定と接続で表を 2 回引くことになり、
/// `PROXY_DNS_TTL_SECS=0` では `getaddrinfo` を 2 回呼んで**別の答えで接続しうる**
/// (DNS rebinding で判定をすり抜けられる)。
///
/// IP リテラルは解決するものが無いので `None` (接続側もその場で使う)。
/// 解決できないものはローカル扱いにしない (先で 502 になる)。
///
/// 断るのはループバック・リンクローカル・未指定と、`allow_private` が偽 (`PROXY_ALLOW_LOCAL`
/// が既定の `off`) なら私設アドレスも (T28.41)。名前は答えの**どれか 1 つ**が当たれば断る。
pub fn resolve_target(host_or_addr: &str, allow_private: bool) -> (bool, Option<Resolved<'_>>) {
    let (host, port) = crate::net::split_host_port_ref(host_or_addr);
    if let Ok(ip) = host.parse::<IpAddr>() {
        // 自己ベンチ (T14.43) の相手役だけは `PROXY_ALLOW_LOCAL=off` でも通す (3 秒だけ)
        return (
            is_refused_ip(ip, allow_private) && !is_self_bench_target(ip, port),
            None,
        );
    }
    match crate::dns::resolve_host_marked(host, port.unwrap_or(80)) {
        Ok((addrs, preferred, stale)) => {
            let local = addrs.iter().any(|&ip| is_refused_ip(ip, allow_private));
            // serve-stale の古い答えなら印を運ぶ (接続側が繋げなかったときに引き直す。T21.3)
            (
                local,
                Some(Resolved::new(host, addrs, preferred).with_stale(stale)),
            )
        }
        // 解決できないときだけ名前で見る (リゾルバが壊れていても localhost は止める)
        Err(_) => (host.eq_ignore_ascii_case("localhost"), None),
    }
}

/// 起動時の自己ベンチ (T14.43) が使う**自分の中の相手役**のポート (`0` = 無し)。
///
/// 自己ベンチの宛先は `127.0.0.1` の使い捨てポート 2 つ (固定応答のオリジンと、すぐ閉じる
/// sink) で、どちらも**このプロセスの中**にある。`PROXY_ALLOW_LOCAL=off` (既定) のままでは
/// 自分の中のオリジンにも 403 を返してしまい、測れるのが「403 を返す費用」になってしまうので、
/// **自己ベンチが回っている 3 秒だけ**この 2 ポートを判定から外す。
/// 開けるのも閉じるのも `crates/run/src/startup.rs` の自己ベンチの前後 1 回ずつで、外から同じポートを
/// 指されても行き先は 1 KiB を返すオリジンか、すぐ閉じる sink しかない。
static SELF_BENCH_PORTS: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

/// 自己ベンチの相手役のポートを覚える (空のスライスで**閉じる**)。
pub fn set_self_bench_ports(ports: &[u16]) {
    for (i, slot) in SELF_BENCH_PORTS.iter().enumerate() {
        slot.store(ports.get(i).copied().unwrap_or(0) as u32, Ordering::Relaxed);
    }
}

/// いま自己ベンチが使っているループバックの宛先か。
///
/// **費用は「ループバック宛ての IP リテラル」のときの原子読み 1〜2 回だけ**。
/// [`resolve_target`] で [`is_local_ip`] が真になった後にしか呼ばないので、
/// 普通の要求 (名前宛て・外向きの IP) では 1 命令も増えない。
fn is_self_bench_target(ip: IpAddr, port: Option<u16>) -> bool {
    let Some(port) = port.filter(|p| *p != 0) else {
        return false;
    };
    ip.is_loopback()
        && SELF_BENCH_PORTS
            .iter()
            .any(|slot| slot.load(Ordering::Relaxed) == u32::from(port))
}

/// [`resolve_target`] の、答えが要らない呼び出し側のための版 (既定の `off` = 私設アドレスも断る)。
/// 呼ぶのは試験だけなので `#[cfg(test)]` (T34.65 で外から使われていないことを確かめて狭めた)。
#[cfg(test)]
pub(crate) fn is_local_target(host_or_addr: &str) -> bool {
    resolve_target(host_or_addr, false).0
}
