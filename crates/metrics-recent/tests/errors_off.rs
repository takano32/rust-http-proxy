//! `PROXY_RECORDS=off` のとき、エラーの個票のリングは今までどおり何もしない (T20.3)。
//!
//! 記録の旗 ([`proxy_metrics_recent::records`]) は**プロセスに 1 つ**なので、クレートの中の
//! 単体テストで `off` にすると、同じバイナリで並んで走るほかのリングのテストの行が落ちる。
//! それでこの 1 本だけ別のバイナリ (= 別のプロセス) に置いてある。**ここにテストを足さない**
//! (足すなら旗を触らないものだけ)。

use proxy_metrics_recent::metrics::ErrCause;
use proxy_metrics_recent::recent::{EntryCause, EntryKind, ErrorEntry, ErrorRing, MAX_ERRORS};
use proxy_metrics_recent::records::{self, Mode};

fn same() -> ErrorEntry {
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

#[test]
fn records_off_drops_every_error_and_merges_nothing() {
    let ring = ErrorRing::new();
    records::set(Mode::Off);
    for _ in 0..1000 {
        ring.push(same());
    }
    // 行も通算も増えず、ファイルに書くものも無い (鍵も取らずに捨てている)
    let (got, total) = ring.recent(MAX_ERRORS);
    assert!(got.is_empty());
    assert_eq!(total, 0);
    assert!(ring.is_empty());
    assert!(ring.take_unwritten(usize::MAX).0.is_empty());
    // 読み戻しも入れない
    ring.restore(vec![same(), same()]);
    assert!(ring.is_empty());
    assert_eq!(ring.recent(MAX_ERRORS).1, 0);

    // `on` に戻すと、そこから数え始める (`off` の間の 1,000 回は `repeats` に入らない)
    records::set(Mode::On);
    ring.push(same());
    ring.push(same());
    let (got, total) = ring.recent(MAX_ERRORS);
    assert_eq!(got.len(), 1);
    assert_eq!(total, 2);
    assert_eq!(got[0].repeats, 2);
}
