//! `LogRing` を直に作って回す単体試験 (T.34.57)。プロセスで 1 つの `RECENT` には触らないので、
//! `RING_TEST_LOCK` は握らない。

use super::*;

fn line(at: u64) -> Line {
    Line {
        at,
        level: Level::Warn,
        conn: None,
        msg: format!("m{at}"),
    }
}

fn ats(lines: &[Line]) -> Vec<u64> {
    lines.iter().map(|l| l.at).collect()
}

#[test]
fn a_ring_fills_then_overwrites_the_oldest() {
    let mut r = LogRing::new(3);
    for at in 1..=3 {
        r.push(line(at));
    }
    assert_eq!(
        (r.start(), r.next, r.total),
        (0, 0, 3),
        "満杯になるまでは先頭が最古"
    );
    r.push(line(4));
    r.push(line(5));
    assert_eq!(ats(&r.buf), vec![4, 5, 3], "最も古いものから上書きする");
    assert_eq!((r.start(), r.total), (2, 5));
}

#[test]
fn recent_returns_the_newest_first_before_and_after_wrapping() {
    let mut r = LogRing::new(3);
    assert!(r.recent(5).is_empty());
    r.push(line(1));
    r.push(line(2));
    assert_eq!(ats(&r.recent(5)), vec![2, 1]);
    for at in 3..=7 {
        r.push(line(at));
    }
    assert_eq!(ats(&r.recent(5)), vec![7, 6, 5]);
    assert_eq!(ats(&r.recent(2)), vec![7, 6]);
}

#[test]
fn take_unwritten_returns_the_oldest_first_and_moves_the_mark() {
    let mut r = LogRing::new(4);
    for at in 1..=3 {
        r.push(line(at));
    }
    assert_eq!(r.take_unwritten(10), (vec![line(1), line(2), line(3)], 0));
    assert_eq!(
        r.take_unwritten(10),
        (Vec::new(), 0),
        "書いたものは 2 度出さない"
    );
    for at in 4..=6 {
        r.push(line(at));
    }
    // 4 行の環で 6 まで来た: まだ書いていないのは 4・5・6 (環に残っている順)
    assert_eq!(ats(&r.take_unwritten(10).0), vec![4, 5, 6]);
}

#[test]
fn take_unwritten_drops_the_oldest_beyond_max_and_lines_already_overwritten() {
    let mut r = LogRing::new(3);
    for at in 1..=5 {
        r.push(line(at));
    }
    // まだ書いていない 5 行のうち、環に残っているのは 3 行。`max` 2 なら古い 1 行を落とす
    let (got, dropped) = r.take_unwritten(2);
    assert_eq!((ats(&got), dropped), (vec![4, 5], 1));
    assert_eq!(r.written, 5);
}

#[test]
fn restored_lines_are_not_written_again() {
    let mut r = LogRing::new(3);
    for at in 1..=5 {
        r.push(line(at));
    }
    r.mark_restored(3);
    assert_eq!((r.restored, r.written, r.total), (3, 5, 5));
    assert_eq!(r.take_unwritten(10), (Vec::new(), 0));
    r.push(line(6));
    assert_eq!(
        ats(&r.take_unwritten(10).0),
        vec![6],
        "読み戻しの後に来た行だけ書く"
    );
}

#[test]
fn clear_empties_the_ring_but_keeps_its_room() {
    let mut r = LogRing::new(3);
    r.buf.reserve(3);
    for at in 1..=4 {
        r.push(line(at));
    }
    r.mark_restored(1);
    r.clear();
    assert!(r.buf.is_empty() && r.buf.capacity() >= 3);
    assert_eq!((r.next, r.total, r.written, r.restored), (0, 0, 0, 0));
    r.push(line(9));
    assert_eq!(ats(&r.recent(3)), vec![9]);
}

#[test]
fn clip_line_cuts_at_256_bytes_on_a_char_boundary() {
    assert_eq!(clip_line("short"), "short");
    let long = "x".repeat(300);
    assert_eq!(clip_line(&long).len(), MAX_LOG_LINE);
    // 3 バイトの文字は 256 の手前 (255) で切る
    let wide = "あ".repeat(100);
    assert_eq!(clip_line(&wide).len(), 255);
    assert_eq!(clip_line(""), "");
}
