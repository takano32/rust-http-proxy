//! カーネルの TCP 統計を `/proc/net` から読む (std のみ。T14.12)。
//!
//! バーストのとき**カーネル側で何が起きていたか**を残すためのもの。とくに
//! 受け入れ待ち行列の溢れ (`ListenOverflows` / `ListenDrops`) は、溢れると
//! クライアントが SYN を 1〜3 秒後に再送するので、プロキシの統計には
//! 「遅い接続」としてすら残らない (T14.16 で 7,479 本/秒のときに実際に起きた)。
//!
//! 読むのは 3 つのファイルだけ:
//!
//! - `/proc/net/netstat` の `TcpExt:` 行 (名前の行と値の行が 2 行で 1 組)
//! - `/proc/net/snmp` の `Tcp:` 行 (同じ形式)
//! - `/proc/net/sockstat` の `TCP:` 行 (`名前 値` の並び)
//!
//! **5 秒の標本のときだけ**読むこと (要求ごとに読むと熱い経路にファイル読みが増える)。
//! Linux 以外や `/proc/net` が無い環境 (コンテナ) では該当する組が `None` になり、
//! 呼び出し側は `null` を出す。

use std::fs;
use std::path::Path;

/// `/proc/net/netstat` の `TcpExt:` 行から取る値 (どれも**起動からの累計**)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TcpExt {
    /// 受け入れ待ち行列 (backlog) が溢れて捨てた接続の数
    pub listen_overflows: u64,
    /// 待ち受けソケットで捨てた接続の数 (溢れ以外の理由も含む)
    pub listen_drops: u64,
    /// 再送タイマーが切れた回数
    pub tcp_timeouts: u64,
    /// SYN を再送した回数 (= 相手に届かなかった確立の試み)
    pub syn_retrans: u64,
    /// 再送しきれずに接続を諦めた回数
    pub abort_on_timeout: u64,
}

/// `/proc/net/snmp` の `Tcp:` 行から取る値。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TcpSnmp {
    /// 再送したセグメントの数 (累計)
    pub retrans_segs: u64,
    /// いま ESTABLISHED / CLOSE-WAIT にあるソケットの数 (値)
    pub curr_estab: u64,
}

/// `/proc/net/sockstat` の `TCP:` 行から取る値 (どれも**いまの値**)。
///
/// `inuse` は IPv4 の表だけを数える (IPv6 は `sockstat6`) が、`tw` (TIME_WAIT) は
/// カーネルが族をまたいで 1 つの死刑囚リストで持っているので全部が入る。
/// この `tw` が loopback の CONNECT のベンチを律速していたもの (TODO.md §1。
/// この機械は `tcp_max_tw_buckets = 32768`)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SockStat {
    pub inuse: u64,
    pub orphan: u64,
    /// TIME_WAIT のソケット数
    pub tw: u64,
    pub alloc: u64,
    /// TCP が使っているメモリ (ページ数)
    pub mem: u64,
}

/// 3 つの組。読めなかったものは `None` (呼び出し側は `null` を出す)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TcpStats {
    pub ext: Option<TcpExt>,
    pub snmp: Option<TcpSnmp>,
    pub sock: Option<SockStat>,
}

/// `/proc/net` から 3 つ読む。**5 秒の標本のときだけ**呼ぶこと。
pub fn tcp_stats() -> TcpStats {
    tcp_stats_in(Path::new("/proc/net"))
}

/// 読む先のディレクトリを差し替えられる版 (テスト用)。
pub fn tcp_stats_in(dir: &Path) -> TcpStats {
    let read = |name: &str| fs::read_to_string(dir.join(name)).ok();
    TcpStats {
        ext: read("netstat").as_deref().and_then(parse_netstat),
        snmp: read("snmp").as_deref().and_then(parse_snmp),
        sock: read("sockstat").as_deref().and_then(parse_sockstat),
    }
}

/// 「名前の行」と「値の行」が同じ接頭辞で 2 行並ぶ形式 (`netstat` と `snmp`)。
///
/// `Tcp:` は `TcpExt:` に前方一致しない (4 文字目が `:` でない) ので、
/// 接頭辞を `"Tcp:"` にしても `TcpExt:` の行は拾わない。
fn labelled_rows<'a>(text: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let mut rows = text.lines().filter_map(|l| l.strip_prefix(prefix));
    Some((rows.next()?, rows.next()?))
}

/// 名前と値を突き合わせて 1 つずつ渡す。組が無ければ `false`。
///
/// 数として読めない値 (`/proc/net/snmp` の `MaxConn` は `-1`) は黙って飛ばす。
/// 欲しい値はどれも符号なしなので、この扱いで困らない。
fn each_field(text: &str, prefix: &str, mut f: impl FnMut(&str, u64)) -> bool {
    let Some((names, values)) = labelled_rows(text, prefix) else {
        return false;
    };
    for (n, v) in names.split_whitespace().zip(values.split_whitespace()) {
        if let Ok(v) = v.parse::<u64>() {
            f(n, v);
        }
    }
    true
}

pub fn parse_netstat(text: &str) -> Option<TcpExt> {
    let mut e = TcpExt::default();
    let found = each_field(text, "TcpExt:", |n, v| match n {
        "ListenOverflows" => e.listen_overflows = v,
        "ListenDrops" => e.listen_drops = v,
        "TCPTimeouts" => e.tcp_timeouts = v,
        "TCPSynRetrans" => e.syn_retrans = v,
        "TCPAbortOnTimeout" => e.abort_on_timeout = v,
        _ => {}
    });
    found.then_some(e)
}

pub fn parse_snmp(text: &str) -> Option<TcpSnmp> {
    let mut s = TcpSnmp::default();
    let found = each_field(text, "Tcp:", |n, v| match n {
        "RetransSegs" => s.retrans_segs = v,
        "CurrEstab" => s.curr_estab = v,
        _ => {}
    });
    found.then_some(s)
}

pub fn parse_sockstat(text: &str) -> Option<SockStat> {
    let row = text.lines().find_map(|l| l.strip_prefix("TCP:"))?;
    let mut s = SockStat::default();
    let mut it = row.split_whitespace();
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        let Ok(v) = v.parse::<u64>() else {
            continue;
        };
        match k {
            "inuse" => s.inuse = v,
            "orphan" => s.orphan = v,
            "tw" => s.tw = v,
            "alloc" => s.alloc = v,
            "mem" => s.mem = v,
            _ => {}
        }
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// この機械の `/proc/net/netstat` の実物の形 (**数値は架空**)。
    const NETSTAT: &str = "\
TcpExt: SyncookiesSent SyncookiesRecv SyncookiesFailed EmbryonicRsts PruneCalled RcvPruned OfoPruned OutOfWindowIcmps LockDroppedIcmps ArpFilter TW TWRecycled TWKilled PAWSActive PAWSEstab DelayedACKs DelayedACKLocked DelayedACKLost ListenOverflows ListenDrops TCPHPHits TCPPureAcks TCPHPAcks TCPTimeouts TCPSynRetrans TCPAbortOnTimeout
TcpExt: 0 0 0 0 60 0 0 0 0 0 12345 999 0 0 101 1610067 65429 3119 6170 6171 350991635 73976928 361215490 4242 909 7
IpExt: InNoRoutes InTruncatedPkts InMcastPkts
IpExt: 0 0 0
";

    /// 同じく `/proc/net/snmp` (数値は架空。`MaxConn` の `-1` も実物どおり)。
    const SNMP: &str = "\
Ip: Forwarding DefaultTTL InReceives
Ip: 1 64 566576000
Tcp: RtoAlgorithm RtoMin RtoMax MaxConn ActiveOpens PassiveOpens AttemptFails EstabResets CurrEstab InSegs OutSegs RetransSegs InErrs OutRsts InCsumErrors
Tcp: 1 200 120000 -1 24396217 24389734 4846 5379 16 568206999 568494504 17600 2 115637 0
Udp: InDatagrams NoPorts
Udp: 1 2
";

    /// 同じく `/proc/net/sockstat` (数値は架空)。
    const SOCKSTAT: &str = "\
sockets: used 332
TCP: inuse 43 orphan 0 tw 30912 alloc 50 mem 7
UDP: inuse 0 mem 436
UDPLITE: inuse 0
RAW: inuse 0
FRAG: inuse 0 memory 0
";

    #[test]
    fn reads_the_listen_overflows_and_the_retransmits_from_the_real_shape() {
        let e = parse_netstat(NETSTAT).unwrap();
        assert_eq!(e.listen_overflows, 6170);
        assert_eq!(e.listen_drops, 6171);
        assert_eq!(e.tcp_timeouts, 4242);
        assert_eq!(e.syn_retrans, 909);
        assert_eq!(e.abort_on_timeout, 7);
        let s = parse_snmp(SNMP).unwrap();
        assert_eq!(s.retrans_segs, 17600);
        assert_eq!(s.curr_estab, 16);
    }

    /// TIME_WAIT は `sockstat` の `tw` (§1 の `tcp_max_tw_buckets = 32768` に当たるもの)。
    #[test]
    fn reads_the_time_wait_count_from_sockstat() {
        let s = parse_sockstat(SOCKSTAT).unwrap();
        assert_eq!(s.tw, 30912);
        assert_eq!(s.inuse, 43);
        assert_eq!(s.alloc, 50);
        assert_eq!(s.mem, 7);
        assert_eq!(s.orphan, 0);
    }

    /// 読めない環境 (ディレクトリを差し替え) では 3 つとも `None`。
    #[test]
    fn missing_proc_net_is_none_everywhere() {
        let stats = tcp_stats_in(Path::new("/nonexistent/proc/net"));
        assert_eq!(stats, TcpStats::default());
        assert!(stats.ext.is_none() && stats.snmp.is_none() && stats.sock.is_none());
        // 組が 1 行しかない / 別の接頭辞しかないファイルも `None`
        assert!(parse_netstat("TcpExt: ListenOverflows\n").is_none());
        assert!(parse_snmp(NETSTAT).is_none());
        assert!(parse_sockstat("sockets: used 1\n").is_none());
    }

    /// 差し替えたディレクトリから 3 つとも読めること (実物と同じ形のファイルを置く)。
    #[test]
    fn reads_all_three_from_a_swapped_directory() {
        let dir = std::env::temp_dir().join(format!("shp-test-procnet-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("netstat"), NETSTAT).unwrap();
        fs::write(dir.join("snmp"), SNMP).unwrap();
        fs::write(dir.join("sockstat"), SOCKSTAT).unwrap();
        let stats = tcp_stats_in(&dir);
        assert_eq!(stats.ext.unwrap().listen_overflows, 6170);
        assert_eq!(stats.snmp.unwrap().retrans_segs, 17600);
        assert_eq!(stats.sock.unwrap().tw, 30912);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 実機では読めること (`/proc/net` があれば)。
    #[cfg(target_os = "linux")]
    #[test]
    fn reads_the_live_proc_net() {
        let stats = tcp_stats();
        // 待ち受けを 1 つも持たない機械でも 3 つのファイルはある
        assert!(stats.ext.is_some(), "/proc/net/netstat が読めない");
        assert!(stats.snmp.is_some(), "/proc/net/snmp が読めない");
        assert!(stats.sock.is_some(), "/proc/net/sockstat が読めない");
    }
}
