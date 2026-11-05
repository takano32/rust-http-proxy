//! 閉じた理由の単体試験 (T34.2 で `conns.rs` の `conn_tests` から移した。名前はそのまま)。

use super::*;
use crate::recent::{CLOSE_REASON_NAMES, CLOSE_REASONS};

/// `error:<原因>` の綴りと、9 つの理由が往復できること。
#[test]
fn every_close_reason_survives_the_round_trip() {
    let all = [
        (CloseReason::ClientEof, "client_eof"),
        (CloseReason::ServerEof, "server_eof"),
        (CloseReason::IdleTimeout, "idle_timeout"),
        (CloseReason::KeepaliveTimeout, "keepalive_timeout"),
        (CloseReason::Evicted, "evicted"),
        (CloseReason::Limit, "limit"),
        (CloseReason::Shutdown, "shutdown"),
        (CloseReason::ClientDead, "client_dead"),
        (CloseReason::Error(ErrCause::Refused), "error:refused"),
        (CloseReason::Error(ErrCause::Dns), "error:dns"),
        (CloseReason::Error(ErrCause::Other), "error:other"),
        // T25.0。DNS の失敗は内訳の綴りで出る
        (CloseReason::ErrorDns(DnsCause::Nxdomain), "error:nxdomain"),
        (CloseReason::ErrorDns(DnsCause::DnsTemp), "error:dns_temp"),
        // T29.10。408 は確立の締め切り (`error:timeout`) と別の綴り
        (CloseReason::HeaderTimeout, "error:header_timeout"),
    ];
    for (reason, name) in all {
        assert_eq!(reason.text(), name);
        assert_eq!(CloseReason::from_code(reason.code()), reason, "{}", name);
    }
    // **既存の符号は動かさない** (T25.0 の 2 つは `ClientDead` の後ろ)
    assert_eq!(CloseReason::ClientDead.code(), 16);
    assert_eq!(CloseReason::ErrorDns(DnsCause::Nxdomain).code(), 17);
    assert_eq!(CloseReason::ErrorDns(DnsCause::DnsTemp).code(), 18);
    assert_eq!(CloseReason::HeaderTimeout.code(), 19);
    assert_eq!(
        CloseReason::HeaderTimeout.index(),
        CloseReason::Error(ErrCause::Timeout).index()
    );
    // 分布は `error:*` の 1 列に畳む (列は 8 つのまま)
    assert_eq!(
        CloseReason::ErrorDns(DnsCause::Nxdomain).index(),
        CloseReason::Error(ErrCause::Dns).index()
    );
    // `/history` の分布は 8 列のままで、`client_dead` は `shutdown` に畳む (T14.52)
    assert_eq!(
        CloseReason::ClientDead.index(),
        CloseReason::Shutdown.index()
    );
    assert_eq!(CLOSE_REASON_NAMES.len(), CLOSE_REASONS);
    assert!(!CLOSE_REASON_NAMES.contains(&"client_dead"));
}
