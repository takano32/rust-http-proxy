//! `PROXY_RECORDS=off` のとき、状態ファイルから読み戻す個票は入れず、`restored` も 0 (T35.4)。
//!
//! 0.35 までは、閉じた接続・山の写真・エラーの 3 本のリングが、off のときは読み戻した件を
//! 捨てるのに `restored` には渡された件数を立てていた (`/recent` `/bursts` `/errors` が空の
//! リングに N 件と出す)。記録の旗 ([`proxy_metrics_recent::records`]) は**プロセスに 1 つ**
//! なので、`errors_off.rs` と同じ理由でこの 1 本だけを別のバイナリに置く。**ここにテストを足さない**。

use proxy_metrics_recent::metrics::ErrCause;
use proxy_metrics_recent::recent::{
    BurstRing, BurstShot, CONN_STATES, CloseReason, EntryCause, EntryKind, ErrorEntry, ErrorRing,
    RecentEntry, RecentRing, SIDES, STAGES,
};
use proxy_metrics_recent::records::{self, Mode};

fn closed(id: u64) -> RecentEntry {
    RecentEntry {
        id,
        at: 1_000_000 + id,
        client: "10.0.0.1".to_string(),
        target: "example.net:443".to_string(),
        connect: true,
        secs: 1,
        requests: 0,
        up: 0,
        down: 0,
        reason: CloseReason::ClientEof,
        status: 0,
        parked_secs: 0,
        parks: 0,
        stage_ms: [0; STAGES],
        rtt_us: [0; SIDES],
        retrans: [0; SIDES],
        stall_ms: [0; SIDES],
        sni: None,
        syn_retrans: 0,
        spins: 0,
        half_closed: None,
    }
}

fn shot(seq: u64) -> BurstShot {
    BurstShot {
        at: 1_000_000 + seq,
        seq,
        active: 10,
        trigger_active: 10,
        max_conns: 12,
        threshold: 9,
        clients: vec![("10.0.0.1".to_string(), 10)],
        clients_distinct: 1,
        clients_other: 0,
        targets: Vec::new(),
        targets_distinct: 0,
        targets_other: 0,
        states: [0; CONN_STATES],
        connects: 10,
        https: 0,
        evicted_idle: 0,
        rejected_overload: 0,
        threads: 4,
        fds: 20,
        max_fds: 1024,
    }
}

fn error() -> ErrorEntry {
    ErrorEntry::new(
        EntryKind::Connect,
        "v6only.example.net:443",
        "198.51.100.7",
        502,
        EntryCause::Error(ErrCause::Unreachable),
        3,
        1300,
    )
}

/// 個票が残った状態で off にして読み戻すと、3 本とも何も入れず `restored` は 0。
/// `on` に戻せば今までどおり読み戻した件数を出す。
#[test]
fn records_off_restores_nothing_and_reports_zero_restored() {
    records::set(Mode::Off);
    let recent = RecentRing::new();
    recent.restore(vec![closed(1), closed(2), closed(3)]);
    assert_eq!(recent.len(), 0);
    assert_eq!(recent.restored(), 0, "/recent の restored");

    let bursts = BurstRing::new();
    bursts.restore(vec![shot(1), shot(2)]);
    assert_eq!(bursts.restored(), 0, "/bursts の restored");

    let errors = ErrorRing::new();
    errors.restore(vec![error(), error()]);
    assert!(errors.is_empty());
    assert_eq!(errors.restored(), 0, "/errors の restored");

    records::set(Mode::On);
    let recent = RecentRing::new();
    recent.restore(vec![closed(1), closed(2), closed(3)]);
    assert_eq!(recent.restored(), 3);
    let bursts = BurstRing::new();
    bursts.restore(vec![shot(1), shot(2)]);
    assert_eq!(bursts.restored(), 2);
    let errors = ErrorRing::new();
    errors.restore(vec![error(), error()]);
    assert_eq!(errors.restored(), 2);
}
