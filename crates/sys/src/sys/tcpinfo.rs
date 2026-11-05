//! カーネルが持っている TCP 接続 1 本の様子 (`getsockopt(SOL_TCP, TCP_INFO)`。T14.5)。
//! 緩衝の読み方 ([`parse_tcp_info`]) は純粋な関数で、欄の位置の試験はそこで見る。

use std::os::fd::RawFd;

/// カーネルが持っている TCP 接続 1 本の様子 (`getsockopt(SOL_TCP, TCP_INFO)` の一部。T14.5)。
///
/// 平滑化 RTT が読めれば「物理 (往復) と自分 (それ以外)」が切り分けられ、再送の数で
/// 相手までの回線の質が分かる。読むのは**接続の終わりに 1 回だけ** (要求ごとには読まない)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpInfo {
    /// 平滑化 RTT (us)。`tcpi_rtt`
    pub rtt_us: u32,
    /// RTT のばらつき (us)。`tcpi_rttvar`
    pub rttvar_us: u32,
    /// いま再送中のセグメント数。`tcpi_retrans`
    pub retrans: u32,
    /// この接続で再送した通算。`tcpi_total_retrans`
    pub total_retrans: u32,
    /// 失われたと見なしたセグメント数。`tcpi_lost`
    pub lost: u32,
    /// 輻輳窓 (セグメント)。`tcpi_snd_cwnd`
    pub cwnd: u32,
    /// 経路 MTU。`tcpi_pmtu`
    pub pmtu: u32,
}

/// `struct tcp_info` の欄の位置 (バイト)。**先頭 8 バイトが `__u8` の旗** で、
/// 以後は `__u32` が並ぶ。値は動作環境の `/usr/include/linux/tcp.h` を
/// `offsetof` で実際に確かめたもの (2026-09-16、Linux 6.6 のヘッダーで
/// `sizeof(struct tcp_info)` は 280。カーネルが増えても**前の欄は動かない**)。
pub(super) mod offsets {
    pub const LOST: usize = 32;
    pub const RETRANS: usize = 36;
    pub const PMTU: usize = 60;
    pub const RTT: usize = 68;
    pub const RTTVAR: usize = 72;
    pub const SND_CWND: usize = 80;
    pub const TOTAL_RETRANS: usize = 100;
    /// 読む緩衝の大きさ (`tcpi_total_retrans` の次まで)。カーネルは `min(len, sizeof)`
    /// しか書かないので、これより新しい欄は取らないし、古いカーネルでも落ちない
    pub const LEN: usize = TOTAL_RETRANS + 4;
    /// ここまで書かれていれば RTT は読めた (`tcpi_rttvar` の次まで)
    pub const MIN_USEFUL: usize = RTTVAR + 4;
}

/// `TCP_INFO` (linux/tcp.h)。
const TCP_INFO: std::ffi::c_int = 11;

/// カーネルの RTT と再送を 1 本ぶん読む (`getsockopt` 1 回)。
///
/// 読めない相手 (TCP でない・もう閉じている) や、RTT の欄まで書かれなかった
/// 古いカーネルでは `None`。**呼ぶのは接続の終わりだけ** — 要求ごとに呼ぶと
/// システムコールが 1 要求 1 回増える。
pub fn tcp_info(fd: RawFd) -> Option<TcpInfo> {
    let mut buf = [0u8; offsets::LEN];
    let written = super::sockopt::get(fd, super::SOL_TCP, TCP_INFO, &mut buf).ok()?;
    parse_tcp_info(&buf, written as usize)
}

/// カーネルが `written` バイト書いた緩衝から欄を取る ([`tcp_info`] の判断の部)。
///
/// RTT の欄まで書かれていなければ `None`。書かれなかった後ろの欄は 0 のまま返る
/// (緩衝は 0 で作ってある)。
pub(super) fn parse_tcp_info(buf: &[u8; offsets::LEN], written: usize) -> Option<TcpInfo> {
    if written < offsets::MIN_USEFUL {
        return None;
    }
    let at = |off: usize| {
        if off + 4 > written {
            return 0;
        }
        u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
    };
    Some(TcpInfo {
        rtt_us: at(offsets::RTT),
        rttvar_us: at(offsets::RTTVAR),
        retrans: at(offsets::RETRANS),
        total_retrans: at(offsets::TOTAL_RETRANS),
        lost: at(offsets::LOST),
        cwnd: at(offsets::SND_CWND),
        pmtu: at(offsets::PMTU),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 欄の位置どおりに値を置いた緩衝を読む。欄の無い古いカーネル (短く書かれた) では 0 か `None`。
    #[test]
    fn reads_the_fields_at_their_offsets_and_tolerates_short_answers() {
        let mut buf = [0u8; offsets::LEN];
        let put = |buf: &mut [u8; offsets::LEN], off: usize, v: u32| {
            buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
        };
        put(&mut buf, offsets::LOST, 1);
        put(&mut buf, offsets::RETRANS, 2);
        put(&mut buf, offsets::PMTU, 1500);
        put(&mut buf, offsets::RTT, 24_000);
        put(&mut buf, offsets::RTTVAR, 3_000);
        put(&mut buf, offsets::SND_CWND, 10);
        put(&mut buf, offsets::TOTAL_RETRANS, 7);
        assert_eq!(
            parse_tcp_info(&buf, offsets::LEN),
            Some(TcpInfo {
                rtt_us: 24_000,
                rttvar_us: 3_000,
                retrans: 2,
                total_retrans: 7,
                lost: 1,
                cwnd: 10,
                pmtu: 1500,
            })
        );
        // `tcpi_rttvar` の次までしか書かれなかった: RTT は読めるが、後ろの欄は 0
        let short = parse_tcp_info(&buf, offsets::MIN_USEFUL).expect("RTT までは読める");
        assert_eq!(
            (short.rtt_us, short.rttvar_us, short.lost),
            (24_000, 3_000, 1)
        );
        assert_eq!((short.cwnd, short.total_retrans), (0, 0));
        // RTT まで書かれなければ使い物にならない
        assert_eq!(parse_tcp_info(&buf, offsets::MIN_USEFUL - 1), None);
        assert_eq!(parse_tcp_info(&buf, 0), None);
    }
}
