//! エラーの個票とリングの単体試験 (T34.2 で `errors.rs` から移した。名前はそのまま)。

use super::*;
use crate::recent::CloseReason;

fn entry(n: u64) -> ErrorEntry {
    ErrorEntry::new(
        EntryKind::Connect,
        &format!("h{}.example.net:443", n),
        "127.0.0.1",
        502,
        EntryCause::Error(ErrCause::Refused),
        0,
        1,
    )
}

#[test]
fn the_ring_keeps_the_newest_entries_first() {
    let ring = ErrorRing::new();
    assert!(ring.is_empty());
    for i in 0..3 {
        ring.push(entry(i));
    }
    let (got, total) = ring.recent(10);
    assert_eq!(total, 3);
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].target, "h2.example.net:443");
    assert_eq!(got[2].target, "h0.example.net:443");
    assert_eq!(ring.recent(2).0.len(), 2);
}

#[test]
fn the_ring_overwrites_the_oldest_when_full() {
    let ring = ErrorRing::new();
    for i in 0..(MAX_ERRORS as u64 + 7) {
        ring.push(entry(i));
    }
    let (got, total) = ring.recent(MAX_ERRORS);
    assert_eq!(total, MAX_ERRORS as u64 + 7);
    assert_eq!(got.len(), MAX_ERRORS);
    // 最新は最後に書いたもの、最古は 500 件前
    assert_eq!(
        got[0].target,
        format!("h{}.example.net:443", MAX_ERRORS as u64 + 6)
    );
    assert_eq!(got[MAX_ERRORS - 1].target, "h7.example.net:443");
}

/// 1 件は 300 B 以内 (500 件で 150 KB。256 KiB の上限に対して余裕がある)。
///
/// T20.3 で `repeats` (最大 10 桁) と `last_at` (10 桁) を末尾に足したので、
/// 上限を 256 → 300 B に上げた (鍵 2 つで 22 B + 値 20 B)。
#[test]
fn one_entry_fits_in_300_bytes() {
    let long = "a".repeat(300);
    let e = ErrorEntry::new(
        EntryKind::Forward,
        &long,
        "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6xx",
        502,
        EntryCause::Error(ErrCause::Unreachable),
        u64::MAX,
        u64::MAX,
    );
    assert!(e.target.len() <= MAX_TARGET, "{}", e.target.len());
    assert!(e.client.len() <= MAX_CLIENT, "{}", e.client.len());
    let mut e = e;
    e.repeats = u32::MAX;
    let json = e.to_json();
    assert!(json.len() <= 300, "1 件が {} B", json.len());
    assert!(json.contains("\"kind\":\"forward\""));
    assert!(json.contains("\"cause\":\"unreachable\""));
}

/// 同じ失敗 1 件ぶん (IPv6 だけの宛先への 502 の形。T20.3)。
fn same(connect_ms: u64) -> ErrorEntry {
    ErrorEntry::new(
        EntryKind::Connect,
        "v6only.example.net:443",
        "198.51.100.7",
        502,
        EntryCause::Error(ErrCause::Unreachable),
        3,
        connect_ms,
    )
}

/// 同じ失敗を 1,000 回書いても 1 行で、`repeats` と通算が 1,000 (T20.3)。
///
/// ファイルへ書くのも行ができたときの 1 回だけ (まとめた回は書かない)。
#[test]
fn a_thousand_repeats_of_one_failure_stay_in_one_row() {
    let ring = ErrorRing::new();
    ring.push(same(1));
    // 1 行目はここで書かれる。以後の 999 回は行を足さないので、書くものが無い
    assert_eq!(ring.take_unwritten(usize::MAX).0.len(), 1);
    for i in 1..1000u64 {
        let mut e = same(1000 + i);
        e.at += i;
        ring.push(e);
    }
    let (got, total) = ring.recent(MAX_ERRORS);
    assert_eq!(got.len(), 1);
    assert_eq!(ring.len(), 1);
    assert_eq!(total, 1000);
    assert_eq!(got[0].repeats, 1000);
    // 行の時刻は最初の 1 回、`last_at` と ms は最後の 1 回
    assert_eq!(got[0].last_at, got[0].at + 999);
    assert_eq!(got[0].connect_ms, 1999);
    assert_eq!(got[0].dns_ms, 3);
    let (unwritten, dropped) = ring.take_unwritten(usize::MAX);
    assert!(unwritten.is_empty(), "まとめた回を書き直している");
    assert_eq!(dropped, 0);
    // JSON は足すだけ: 今までの 8 つの鍵の後ろに `repeats` と `last_at`
    let json = got[0].to_json();
    assert!(
        json.ends_with(&format!(
            ",\"status\":502,\"client\":\"198.51.100.7\",\"repeats\":1000,\"last_at\":{}}}",
            got[0].last_at
        )),
        "{}",
        json
    );
    println!(
        "T20.3 同じ失敗 1,000 回: {} 行 / total {} / repeats {} / {} B",
        got.len(),
        total,
        got[0].repeats,
        json.len()
    );
}

/// まとめていない行は `repeats` 1・`last_at` は `at` と同じ (T20.3)。
#[test]
fn a_single_failure_reports_one_repeat_and_its_own_time() {
    let e = entry(1);
    assert_eq!(e.repeats, 1);
    assert_eq!(e.last_at, e.at);
    let json = e.to_json();
    assert!(
        json.ends_with(&format!(",\"repeats\":1,\"last_at\":{}}}", e.at)),
        "{}",
        json
    );
}

/// 間に別の失敗が入っても、新しい方から 8 件の中なら同じ行に足す (T20.3)。
#[test]
fn another_failure_in_between_does_not_split_the_row() {
    let ring = ErrorRing::new();
    ring.push(same(1));
    ring.push(entry(1));
    ring.push(same(2));
    let (got, total) = ring.recent(10);
    assert_eq!(total, 3);
    assert_eq!(got.len(), 2);
    // 並びは行ができた順の新しい順のまま (足された行が前に出てこない)
    assert_eq!(got[0].target, "h1.example.net:443");
    assert_eq!(got[0].repeats, 1);
    assert_eq!(got[1].target, "v6only.example.net:443");
    assert_eq!(got[1].repeats, 2);
    assert_eq!(got[1].connect_ms, 2);

    // 間に 7 件 (= 同じ失敗は新しい方から 8 件目) まではまとまる
    let ring = ErrorRing::new();
    ring.push(same(1));
    for i in 0..(MERGE_WINDOW as u64 - 1) {
        ring.push(entry(i));
    }
    ring.push(same(2));
    let (got, total) = ring.recent(MAX_ERRORS);
    assert_eq!(total, MERGE_WINDOW as u64 + 1);
    assert_eq!(got.len(), MERGE_WINDOW);
    assert_eq!(got[MERGE_WINDOW - 1].repeats, 2);
}

/// 9 件前の同じ失敗は別の行 (鍵の中で比べるのは最大 8 件。T20.3)。
#[test]
fn the_same_failure_nine_rows_back_starts_a_new_row() {
    let ring = ErrorRing::new();
    ring.push(same(1));
    for i in 0..(MERGE_WINDOW as u64) {
        ring.push(entry(i));
    }
    ring.push(same(2));
    let (got, total) = ring.recent(MAX_ERRORS);
    assert_eq!(total, MERGE_WINDOW as u64 + 2);
    assert_eq!(got.len(), MERGE_WINDOW + 2);
    assert_eq!(got[0].target, "v6only.example.net:443");
    assert_eq!(got[0].repeats, 1);
    assert_eq!(got[MERGE_WINDOW + 1].target, "v6only.example.net:443");
    assert_eq!(got[MERGE_WINDOW + 1].repeats, 1);
    // 次の 1 回は新しい方の行に足す
    ring.push(same(3));
    let (got, _) = ring.recent(MAX_ERRORS);
    assert_eq!(got.len(), MERGE_WINDOW + 2);
    assert_eq!(got[0].repeats, 2);
    assert_eq!(got[MERGE_WINDOW + 1].repeats, 1);
}

/// 5 つの鍵 (`kind`・`target`・`cause`・`status`・`client`) のどれかが違えば別の行 (T20.3)。
#[test]
fn a_different_client_cause_kind_status_or_target_is_another_row() {
    let ring = ErrorRing::new();
    let base = same(1);
    ring.push(base.clone());
    let mut other_client = base.clone();
    other_client.client = "198.51.100.8".to_string();
    let mut other_cause = base.clone();
    other_cause.cause = EntryCause::Error(ErrCause::Timeout);
    let mut other_kind = base.clone();
    other_kind.kind = EntryKind::Forward;
    let mut other_status = base.clone();
    other_status.status = 504;
    let mut other_target = base.clone();
    other_target.target = "v6only.example.net:8443".to_string();
    for e in [
        other_client,
        other_cause,
        other_kind,
        other_status,
        other_target,
    ] {
        ring.push(e);
    }
    let (got, total) = ring.recent(MAX_ERRORS);
    assert_eq!(total, 6);
    assert_eq!(got.len(), 6);
    assert!(got.iter().all(|e| e.repeats == 1));
    // ms と時刻だけが違うものは同じ行
    let mut later = base.clone();
    later.at += 140;
    later.dns_ms = 9;
    ring.push(later);
    let (got, total) = ring.recent(MAX_ERRORS);
    assert_eq!(total, 7);
    assert_eq!(got.len(), 6);
    assert_eq!(got[5].repeats, 2);
    assert_eq!(got[5].dns_ms, 9);
}

/// 満杯で一巡したあとも、新しい方から 8 件を正しく見ること (T20.3)。
#[test]
fn merging_still_finds_the_newest_rows_after_the_ring_wraps() {
    let ring = ErrorRing::new();
    for i in 0..(MAX_ERRORS as u64 + 3) {
        ring.push(entry(i));
    }
    // 最新 (h502) と、新しい方から 8 件目 (h495) に足す。9 件目 (h494) は別の行になる
    let newest = MAX_ERRORS as u64 + 2;
    ring.push(entry(newest));
    ring.push(entry(newest - 7));
    let (got, total) = ring.recent(MAX_ERRORS);
    assert_eq!(total, MAX_ERRORS as u64 + 5);
    assert_eq!(got[0].repeats, 2);
    assert_eq!(got[7].repeats, 2);
    ring.push(entry(newest - 8));
    let (got, _) = ring.recent(MAX_ERRORS);
    assert_eq!(got[0].target, format!("h{}.example.net:443", newest - 8));
    assert_eq!(got[0].repeats, 1);
    assert_eq!(got[9].repeats, 1);
}

/// 読み戻した行はまとめない (ファイルの 1 行 = 1 行、`repeats` 1。T20.3)。
///
/// 読み戻したあとの同じ失敗は、今までどおり新しい方の行に足す。書き直しもしない。
#[test]
fn restored_rows_are_not_merged_and_not_rewritten() {
    let ring = ErrorRing::new();
    ring.restore(vec![same(1), same(2), same(3)]);
    let (got, total) = ring.recent(MAX_ERRORS);
    assert_eq!(got.len(), 3);
    assert_eq!(total, 3);
    assert_eq!(ring.restored(), 3);
    assert!(got.iter().all(|e| e.repeats == 1));
    assert!(ring.take_unwritten(usize::MAX).0.is_empty());
    ring.push(same(4));
    let (got, total) = ring.recent(MAX_ERRORS);
    assert_eq!(got.len(), 3);
    assert_eq!(total, 4);
    assert_eq!(got[0].repeats, 2);
    assert!(ring.take_unwritten(usize::MAX).0.is_empty());
    // 新しい行ができたら、その 1 行だけ書く
    ring.push(entry(1));
    let (unwritten, _) = ring.take_unwritten(usize::MAX);
    assert_eq!(unwritten.len(), 1);
    assert_eq!(unwritten[0].target, "h1.example.net:443");
}

/// `io::Error` から閉じた理由を決める (T25.0)。DNS だけ内訳を持つ。
#[test]
fn errors_closing_a_connection_carry_the_dns_split() {
    use std::io::{Error, ErrorKind};
    let nx = Error::other("failed to lookup address information: Name or service not known");
    let temp =
        Error::other("failed to lookup address information: Temporary failure in name resolution");
    assert_eq!(
        CloseReason::error_from_io(&nx),
        CloseReason::ErrorDns(DnsCause::Nxdomain)
    );
    assert_eq!(
        CloseReason::error_from_io(&temp),
        CloseReason::ErrorDns(DnsCause::DnsTemp)
    );
    // DNS 以外は今までどおり 8 つの原因の綴り
    assert_eq!(
        CloseReason::error_from_io(&Error::new(ErrorKind::ConnectionRefused, "x")),
        CloseReason::Error(ErrCause::Refused)
    );
    // `/errors` の `cause` も同じ 2 つ (符号 20・21 で往復する)
    for (cause, name) in [
        (EntryCause::ErrorDns(DnsCause::Nxdomain), "nxdomain"),
        (EntryCause::ErrorDns(DnsCause::DnsTemp), "dns_temp"),
    ] {
        assert_eq!(cause.name(), name);
        assert_eq!(EntryCause::from_code(cause.code()), cause);
        let e = ErrorEntry::new(
            EntryKind::Connect,
            "imap.example.jp:993",
            "198.51.100.7",
            502,
            cause,
            2,
            0,
        );
        let json = e.to_json();
        assert!(
            json.contains(&format!("\"cause\":\"{}\"", name)),
            "{}",
            json
        );
        assert!(json.len() <= 256, "1 件が {} B", json.len());
    }
}

/// 403 の個票は `/errors` に `acl` / `blocklist` の名前で出る (T14.2)。
///
/// 集計 (`errors_by_cause`) には乗らないので、[`ErrCause`] とは別の型で持つ。
#[test]
fn blocked_entries_carry_their_own_cause_names() {
    for (cause, name) in [
        (BlockCause::Acl, "acl"),
        (BlockCause::Blocklist, "blocklist"),
        (BlockCause::ConnectPort, "connect_port"),
        (BlockCause::Local, "local"),
    ] {
        let e = ErrorEntry::new(
            EntryKind::Connect,
            "ads.example.net:443",
            "198.51.100.7",
            403,
            EntryCause::Blocked(cause),
            0,
            0,
        );
        let json = e.to_json();
        assert!(
            json.contains(&format!("\"cause\":\"{}\"", name)),
            "{}",
            json
        );
        assert!(json.contains("\"status\":403"), "{}", json);
        assert!(json.contains("\"kind\":\"connect\""), "{}", json);
        assert!(json.len() <= 256, "1 件が {} B", json.len());
    }
}

/// 時刻は渡した値で、まとめる前の `last_at` も同じ (T34.48)。
#[test]
fn an_entry_at_a_given_time_carries_it_in_at_and_last_at() {
    let e = ErrorEntry::at(
        1_800_000_123,
        EntryKind::Connect,
        "h.example.net:443",
        "127.0.0.1",
        502,
        EntryCause::Error(ErrCause::Refused),
        7,
        9,
    );
    assert_eq!(
        (e.at, e.last_at, e.repeats),
        (1_800_000_123, 1_800_000_123, 1)
    );
    assert_eq!(
        e.to_json(),
        "{\"at\":1800000123,\"kind\":\"connect\",\"target\":\"h.example.net:443\",\"cause\":\"refused\",\"dns_ms\":7,\"connect_ms\":9,\"status\":502,\"client\":\"127.0.0.1\",\"repeats\":1,\"last_at\":1800000123}"
    );
}
