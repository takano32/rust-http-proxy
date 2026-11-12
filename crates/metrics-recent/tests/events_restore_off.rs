//! `PROXY_RECORDS=off` のとき、`events::restore` は何も入れず、`restored` も 0 (T36.14)。
//!
//! T35.4 で閉じた接続・山の写真・エラーの 3 本はそうしたが、出来事のリングだけは旗を見ずに
//! 入れて、渡された件数を立てていた (起動の経路は off なら呼ばないので表には出ない)。
//! 記録の旗も出来事のリングも**プロセスに 1 つ**なので、`restore_off.rs` と同じ理由でこの
//! 1 本だけを別のバイナリに置く。**ここにテストを足さない**。

use proxy_metrics_recent::events::{self, Event, EventKind};
use proxy_metrics_recent::records::{self, Mode};

fn event(at: u64) -> Event {
    Event {
        at,
        kind: EventKind::Start,
        text: format!("start {at}"),
    }
}

/// off で読み戻すと何も入れず `restored` は 0。`on` に戻せば今までどおり読み戻した件数を出す。
#[test]
fn records_off_restores_no_events_and_reports_zero_restored() {
    records::set(Mode::Off);
    events::clear();
    events::restore(vec![event(1), event(2), event(3)]);
    assert_eq!(events::len(), 0);
    assert_eq!(events::restored_count(), 0, "/events の restored");

    records::set(Mode::On);
    events::clear();
    events::restore(vec![event(1), event(2), event(3)]);
    assert_eq!(events::len(), 3);
    assert_eq!(events::restored_count(), 3);
}
