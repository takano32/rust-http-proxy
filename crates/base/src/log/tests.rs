//! `log` の単体試験 (T.34.56 で `log.rs` の `mod tests` をそのまま移した)。`RING_TEST_LOCK` の綴りは
//! `minrate.rs`・`records.rs` の試験が引くので `crate::log::tests::RING_TEST_LOCK` のまま。

/// T.28.38: 長い文字列は先頭 256 バイトと全体の長さだけ (短いものは `{:?}` のまま)。
#[test]
fn clip_cuts_long_values_and_keeps_short_ones() {
    assert_eq!(Clip("GET / HTTP/1.1").to_string(), "\"GET / HTTP/1.1\"");
    let long = "x".repeat(60 * 1024);
    let shown = Clip(&long).to_string();
    assert!(shown.len() < 300, "{}", shown.len());
    assert!(
        shown.ends_with("... (61440 bytes)"),
        "{}",
        &shown[shown.len() - 30..]
    );
    // 文字の途中で切らない
    let wide = "あ".repeat(200);
    assert!(Clip(&wide).to_string().ends_with("... (600 bytes)"));
}

use super::*;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;

#[test]
fn test_level_parse() {
    assert_eq!(Level::parse("TRACE"), Some(Level::Trace));
    assert_eq!(Level::parse(" debug "), Some(Level::Debug));
    assert_eq!(Level::parse("nonsense"), None);
}

#[test]
fn test_level_ordering() {
    assert!(Level::Error < Level::Info);
    assert!(Level::Trace > Level::Debug);
}

/// 1 分に 1 回だけ通し、通すときに前の行から抑えた本数を返す (T.28.9)。
#[test]
fn throttle_passes_once_per_period_and_reports_what_it_held_back() {
    let t = Throttle::new();
    let t0 = 1_800_000_000;
    // 最初の 1 本は通る (抑えた本数は 0 で、行には何も足さない)
    assert_eq!(t.pass_at(t0, 60), Some(Skipped(0)));
    assert_eq!(Skipped(0).to_string(), "");
    // 同じ分のうちは 99 本とも通さない
    for i in 0..99 {
        assert_eq!(t.pass_at(t0 + i % 60, 60), None, "{}", i);
    }
    // 59 秒たっても通さない
    assert_eq!(t.pass_at(t0 + 59, 60), None);
    // 60 秒たった次の 1 本は通り、抑えた 100 本を添える
    let s = t.pass_at(t0 + 60, 60).expect("1 分たったら通す");
    assert_eq!(s, Skipped(100));
    assert_eq!(
        s.to_string(),
        " (+100 more since the previous line, not logged)"
    );
    // 数え直しは 0 から
    assert_eq!(t.pass_at(t0 + 61, 60), None);
    assert_eq!(t.pass_at(t0 + 200, 60), Some(Skipped(1)));
    // 時計が戻っても (前より小さい時刻) 通さない
    assert_eq!(t.pass_at(t0, 60), None);
}

/// 同時に叩いても、1 分の間に通るのは 1 本だけ (比較交換)。
#[test]
fn throttle_lets_exactly_one_of_many_threads_through() {
    let t = std::sync::Arc::new(Throttle::new());
    let passed = std::sync::Arc::new(AtomicU64::new(0));
    let hs: Vec<_> = (0..8)
        .map(|_| {
            let (t, passed) = (t.clone(), passed.clone());
            std::thread::spawn(move || {
                for _ in 0..1000 {
                    if t.pass_at(1_800_000_000, 60).is_some() {
                        passed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(passed.load(Ordering::Relaxed), 1);
    assert_eq!(t.pass_at(1_800_000_060, 60), Some(Skipped(7_999)));
}

#[test]
fn test_civil_from_epoch() {
    // 2026-09-02T01:23:45Z
    assert_eq!(civil_from_epoch(1_788_312_225), (2026, 9, 2, 1, 23, 45));
    // UNIX epoch
    assert_eq!(civil_from_epoch(0), (1970, 1, 1, 0, 0, 0));
}

#[test]
fn test_access_log_is_info_level() {
    // ログ水準はプロセスに 1 つなので、水準を触るテストは直列に回す
    let _guard = crate::sync::LockExt::locked(&RING_TEST_LOCK);
    set_level(Level::Info);
    assert!(enabled(Level::Info));
    assert!(!enabled(Level::Debug));
    // 既定レベルでアクセスログが出ること (パニックしないこと) を確認する
    access(
        1,
        &Access {
            client: "127.0.0.1",
            method: "GET",
            target: "http://example.com/",
            version: "HTTP/1.1",
            status: "200",
            bytes: 42,
            duration_ms: 1.5,
            cache: "MISS",
        },
    );
}

/// タイムスタンプが以前の `format!("{:04}-{:02}-...{:03}Z")` と 1 バイトも変わらないこと。
/// 秒を使い回すので、同じ秒を続けたり戻したりする並びも通す。
#[test]
fn test_timestamp_matches_the_old_format() {
    let cases = [
        (0u64, 0u32),
        (1_788_312_225, 678),
        (1_788_312_225, 7),
        (1_788_312_225, 70),
        (1_788_312_226, 0),
        (1_788_312_225, 999),
        (951_782_400, 1),       // 2000-02-29 (うるう年)
        (253_402_300_799, 999), // 9999-12-31T23:59:59
        (253_402_300_800, 0),   // 10000-01-01 (年が 5 桁でも `{:04}` は詰めない)
    ];
    for (secs, millis) in cases {
        let mut buf = Vec::new();
        push_timestamp_at(&mut buf, secs, millis);
        let (y, mo, d, h, mi, s) = civil_from_epoch(secs);
        let expected = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            y, mo, d, h, mi, s, millis
        );
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            expected,
            "{} {}",
            secs,
            millis
        );
    }
}

/// 行の頭が以前の `format!("{} {} [conn#{}] {}\n")` と同じ並びであること。
#[test]
fn test_prefix_matches_the_old_format() {
    let mut buf = Vec::new();
    push_prefix(&mut buf, Level::Info, Some(12));
    let line = String::from_utf8(buf).unwrap();
    assert_eq!(line.len(), 24 + " INFO  [conn#12] ".len());
    assert_eq!(&line[24..], " INFO  [conn#12] ");

    let mut buf = Vec::new();
    push_prefix(&mut buf, Level::Error, None);
    let line = String::from_utf8(buf).unwrap();
    assert_eq!(&line[24..], " ERROR [main] ");
}

/// アクセスログの本体が以前の `format!` と 1 バイトも変わらないこと。
/// 経過時間の `{:.1}` は丸めが効く値 (0.05 / 0.25 / 0.35) も通す。
#[test]
fn test_access_line_matches_the_old_format() {
    let base = Access {
        client: "127.0.0.1",
        method: "GET",
        target: "http://example.com/",
        version: "HTTP/1.1",
        status: "200",
        bytes: 1234,
        duration_ms: 12.34,
        cache: "HIT(memory)",
    };
    let cases = [
        base,
        Access {
            client: "::1",
            method: "CONNECT",
            target: "example.com:443",
            status: "200",
            bytes: 0,
            duration_ms: 0.0,
            cache: "BYPASS(tunnel)",
            ..base
        },
        Access {
            method: "HEAD",
            version: "HTTP/1.0",
            status: "304",
            bytes: 1,
            duration_ms: 0.05,
            cache: "HIT(disk,304) age=3s",
            ..base
        },
        Access {
            status: "206",
            bytes: u64::MAX,
            duration_ms: 0.25,
            cache: "HIT(memory) age=0s ttl_left=59s range=0-99",
            ..base
        },
        Access {
            method: "POST",
            status: "502",
            bytes: 9,
            duration_ms: 0.35,
            cache: "MISS truncated",
            ..base
        },
        Access {
            bytes: 7,
            duration_ms: 1234.5678,
            cache: "MISS stored ttl=60s",
            ..base
        },
    ];
    for rec in cases {
        let mut buf = Vec::new();
        push_access(&mut buf, &rec);
        let expected = format!(
            "ACCESS {} \"{} {} {}\" {} {}B {:.1}ms cache={}",
            rec.client,
            rec.method,
            rec.target,
            rec.version,
            rec.status,
            rec.bytes,
            rec.duration_ms,
            rec.cache
        );
        assert_eq!(String::from_utf8(buf).unwrap(), expected);
    }
}

/// warn 以上だけがリングに入り、`info` のアクセスログは入らない (T13.4)。
#[test]
fn only_warnings_and_errors_reach_the_ring() {
    let _guard = crate::sync::LockExt::locked(&RING_TEST_LOCK);
    clear_recent();
    set_level(Level::Info);
    log_line(Level::Info, Some(1), "this is info");
    access(
        1,
        &Access {
            client: "127.0.0.1",
            method: "GET",
            target: "http://example.com/",
            version: "HTTP/1.1",
            status: "200",
            bytes: 42,
            duration_ms: 1.5,
            cache: "MISS",
        },
    );
    assert_eq!(recent_len(), 0, "info もアクセスログも写さない");
    log_line(Level::Warn, Some(7), "502 Bad Gateway");
    log_line(Level::Error, None, "accept failed");
    let (got, total) = recent(10);
    assert_eq!(total, 2);
    assert_eq!(got.len(), 2);
    // 新しい順
    assert_eq!(got[0].level, Level::Error);
    assert_eq!(got[0].conn, None);
    assert_eq!(got[0].msg, "accept failed");
    assert_eq!(got[1].level, Level::Warn);
    assert_eq!(got[1].conn, Some(7));
    assert!(got[1].at > 1_700_000_000, "{}", got[1].at);
    clear_recent();
}

/// レベルを error に下げたら warn は出ないので、リングにも入らない。
#[test]
fn the_ring_follows_the_log_level() {
    let _guard = crate::sync::LockExt::locked(&RING_TEST_LOCK);
    clear_recent();
    set_level(Level::Error);
    log_line(Level::Warn, None, "not logged");
    assert_eq!(recent_len(), 0);
    log_line(Level::Error, None, "logged");
    assert_eq!(recent_len(), 1);
    set_level(Level::Info);
    clear_recent();
}

/// 1 行は 256 B までで切り、1,000 行で頭打ち (古いものを上書き)。
#[test]
fn the_ring_clips_long_lines_and_wraps_at_1000() {
    let _guard = crate::sync::LockExt::locked(&RING_TEST_LOCK);
    clear_recent();
    set_level(Level::Info);
    log_line(Level::Warn, None, &"x".repeat(1000));
    assert_eq!(recent(1).0[0].msg.len(), MAX_LOG_LINE);
    clear_recent();
    for i in 0..(MAX_LOG_LINES + 5) {
        log_line(Level::Warn, None, &format!("line {}", i));
    }
    let (got, total) = recent(MAX_LOG_LINES + 100);
    assert_eq!(total as usize, MAX_LOG_LINES + 5);
    assert_eq!(got.len(), MAX_LOG_LINES);
    assert_eq!(got[0].msg, format!("line {}", MAX_LOG_LINES + 4));
    assert_eq!(got[MAX_LOG_LINES - 1].msg, "line 5");
    clear_recent();
}

/// リングもログ水準もプロセスに 1 組なので、この束は直列に回す。
///
/// **`records::tests` の旗を触るテストも同じ鍵を取る** (T18.4)。`log_line` は warn 以上を
/// リングに写す前に `records::recording()` を見るので、向こうが別の鍵のまま `Mode::Off` に
/// している一瞬に重なると、その行だけリングに入らず通算が 1〜2 行足りなくなる
/// (`the_ring_clips_long_lines_and_wraps_at_1000` が `left: 1004, right: 1005` で落ちた)。
pub(crate) static RING_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn test_timestamp_format() {
    let ts = timestamp();
    assert_eq!(ts.len(), 24, "unexpected timestamp: {}", ts);
    assert!(ts.ends_with('Z'));
    assert_eq!(&ts[4..5], "-");
    assert_eq!(&ts[10..11], "T");
}
