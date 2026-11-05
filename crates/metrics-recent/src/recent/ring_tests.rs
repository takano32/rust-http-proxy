//! 閉じた接続のリングの単体試験 (T34.2 で `conns.rs` の `conn_tests` から移した。名前はそのまま)。

use super::*;
use crate::recent::{CloseReason, SIDES, STAGES};

/// 中身はどうでもよい閉じた接続 1 件 (`id` と `at` だけを変える)。
fn entry(id: u64) -> RecentEntry {
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

/// まだファイルに書いていない件は古い順に取り出し、取り出した所まで印が進む。`max` を越えるぶんは
/// 古い方から落として件数だけ返す。読み戻した件は書き直さない (T14.9。T34.2 で直に押さえる)。
#[test]
fn unwritten_entries_come_out_oldest_first_and_restored_ones_are_not_rewritten() {
    let ring = RecentRing::new();
    assert_eq!(ring.take_unwritten(usize::MAX).0.len(), 0);
    for id in 0..5 {
        ring.push(entry(id));
    }
    let (got, dropped) = ring.take_unwritten(3);
    assert_eq!(
        got.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![2, 3, 4],
        "古い方から 2 件落とし、残りは古い順"
    );
    assert_eq!(dropped, 2);
    assert!(
        ring.take_unwritten(usize::MAX).0.is_empty(),
        "印は取り出した所まで進む"
    );
    ring.push(entry(5));
    assert_eq!(ring.take_unwritten(usize::MAX).0[0].id, 5);
    // 読み戻した件は `restored` に数え、書き直さない
    let fresh = RecentRing::new();
    fresh.restore(vec![entry(7), entry(8)]);
    assert_eq!(fresh.restored(), 2);
    assert_eq!(fresh.len(), 2);
    assert!(fresh.take_unwritten(usize::MAX).0.is_empty());
    assert_eq!(fresh.select(0, "").0[0].id, 8, "新しい順");
}

/// `since` と `client` で絞れ、新しい順に返ること。
#[test]
fn the_closed_ring_filters_and_overwrites_the_oldest() {
    let ring = RecentRing::new();
    for i in 0..(MAX_RECENT as u64 + 5) {
        ring.push(RecentEntry {
            id: i,
            at: 1_000_000 + i,
            client: if i % 2 == 0 { "10.0.0.1" } else { "10.0.0.2" }.to_string(),
            target: "example.net:443".to_string(),
            connect: true,
            secs: i,
            requests: 0,
            up: i,
            down: i * 2,
            reason: CloseReason::ClientEof,
            status: 0,
            parked_secs: 0,
            parks: 0,
            stage_ms: [0, i, 0, 0, 0, 0],
            rtt_us: [0; SIDES],
            retrans: [0; SIDES],
            stall_ms: [0; SIDES],
            sni: None,
            syn_retrans: 0,
            spins: 0,
            half_closed: None,
        });
    }
    let (all, total) = ring.select(0, "");
    assert_eq!(total, MAX_RECENT as u64 + 5);
    assert_eq!(all.len(), MAX_RECENT, "リングは 2,000 件で頭打ち");
    assert_eq!(all[0].id, MAX_RECENT as u64 + 4, "新しい順");
    assert_eq!(all[MAX_RECENT - 1].id, 5, "最古は 2,000 件前");
    // 接続元で絞る
    let (mine, _) = ring.select(0, "10.0.0.2");
    assert_eq!(mine.len(), MAX_RECENT / 2);
    assert!(mine.iter().all(|e| e.client == "10.0.0.2"));
    // 時刻で絞る (それ以降に開いたものだけ)
    let (fresh, _) = ring.select(1_000_000 + MAX_RECENT as u64, "");
    assert_eq!(fresh.len(), 5);
    assert!(ring.select(9_999_999, "").0.is_empty());
}
