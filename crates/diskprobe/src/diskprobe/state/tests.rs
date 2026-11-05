//! `Saved` と `learn` の単体試験 (T.34.63)。ファイルも時計も触らない。

use super::*;

fn sample() -> Saved {
    Saved {
        mode: Mode::Slow,
        confirmed: 1024 * MIB,
        cap: 1536 * MIB,
        raised_at: 11,
        started_at: 12,
        alive: 13,
        learned: Some(2048 * MIB),
        learned_at: 14,
        early_stops: 1,
        found: Some(3 * MIB),
        found_at: 15,
    }
}

#[test]
fn the_state_file_has_eleven_lines_in_a_fixed_order() {
    assert_eq!(
        sample().to_text(),
        "mode=slow\nconfirmed=1073741824\ncap=1610612736\nraised_at=11\nstarted_at=12\nalive=13\n\
         learned=2147483648\nlearned_at=14\nearly_stops=1\nfound=3145728\nfound_at=15\n"
    );
    // 無いものは 0 で書く
    let fresh = Saved::fresh(7).to_text();
    assert_eq!(fresh.lines().count(), 11);
    assert!(fresh.starts_with("mode=fast\nconfirmed=536870912\ncap=536870912\nraised_at=7\n"));
    assert!(fresh.ends_with("learned=0\nlearned_at=0\nearly_stops=0\nfound=0\nfound_at=0\n"));
}

#[test]
fn the_state_file_round_trips() {
    assert_eq!(Saved::parse(&sample().to_text()), sample());
    let fresh = Saved::fresh(99);
    assert_eq!(Saved::parse(&fresh.to_text()), fresh);
}

#[test]
fn a_damaged_state_file_reads_missing_keys_as_defaults() {
    let s = Saved::parse("mode=what\ncap=abc\nlearned=0\n junk \nalive= 5 \n");
    assert_eq!(
        s,
        Saved {
            mode: Mode::Fast,
            confirmed: 0,
            cap: START,
            raised_at: 0,
            started_at: 0,
            alive: 5,
            learned: None,
            learned_at: 0,
            early_stops: 0,
            found: None,
            found_at: 0,
        }
    );
    assert_eq!(Saved::parse("").confirmed, 0);
}

#[test]
fn learn_without_a_state_file_starts_fast() {
    let (s, note) = learn(None, 100);
    assert_eq!((s, note), (Saved::fresh(100), Note::Fresh));
}

#[test]
fn learn_keeps_a_recent_value_and_reprobes_an_old_one() {
    let mut saved = Saved::fresh(0);
    saved.learned = Some(800 * MIB);
    saved.learned_at = 1_000;
    let (s, note) = learn(Some(&saved), 1_000 + RELEARN_SECS - 1);
    assert_eq!(note, Note::LearnedEarlier(800 * MIB));
    assert_eq!(
        (s.confirmed, s.cap, s.learned, s.learned_at),
        (800 * MIB, 800 * MIB, Some(800 * MIB), 1_000)
    );
    // 7 日たったら探り直す (fast なら fast のまま、学んだ値は捨てる)
    let (s, note) = learn(Some(&saved), 1_000 + RELEARN_SECS);
    assert_eq!(note, Note::FastAgain);
    assert_eq!((s.learned, s.confirmed, s.cap), (None, START, START));
}

#[test]
fn learn_switches_to_slow_when_fast_was_stopped_early() {
    let mut saved = Saved::fresh(0);
    saved.started_at = 1_000;
    saved.alive = 1_030;
    let (s, note) = learn(Some(&saved), 2_000);
    assert_eq!(note, Note::SwitchToSlow(30));
    assert_eq!((s.mode, s.confirmed, s.cap), (Mode::Slow, 0, START));
    assert_eq!((s.raised_at, s.started_at, s.alive), (2_000, 2_000, 2_000));
    // 10 分以上生きていれば fast のまま
    saved.alive = 1_000 + CONFIRM_SECS;
    assert_eq!(learn(Some(&saved), 2_000).1, Note::FastAgain);
}

#[test]
fn learn_takes_the_confirmed_value_when_slow_was_stopped_after_a_raise() {
    let saved = Saved {
        mode: Mode::Slow,
        confirmed: START + FAST_STEP,
        cap: START + 2 * FAST_STEP,
        raised_at: 5_000,
        started_at: 1_000,
        alive: 5_040,
        ..Saved::fresh(0)
    };
    let (s, note) = learn(Some(&saved), 6_000);
    assert_eq!(
        note,
        Note::LearnedAfterRaise(START + FAST_STEP, 40, START + 2 * FAST_STEP)
    );
    assert_eq!(
        (s.mode, s.learned, s.learned_at, s.confirmed, s.cap),
        (
            Mode::Slow,
            Some(START + FAST_STEP),
            6_000,
            START + FAST_STEP,
            START + FAST_STEP
        )
    );
}

#[test]
fn learn_counts_early_stops_in_slow_and_gives_the_floor_on_the_second() {
    let mut saved = Saved {
        mode: Mode::Slow,
        confirmed: 0,
        cap: START,
        started_at: 1_000,
        alive: 1_030,
        ..Saved::fresh(0)
    };
    let (s, note) = learn(Some(&saved), 2_000);
    assert_eq!(note, Note::SlowAgain);
    assert_eq!(
        (s.early_stops, s.confirmed, s.cap, s.learned),
        (1, 0, START, None)
    );
    saved.early_stops = 1;
    let (s, note) = learn(Some(&saved), 2_000);
    assert_eq!(note, Note::Tiny);
    assert_eq!(
        (s.learned, s.confirmed, s.cap, s.learned_at),
        (Some(FLOOR), FLOOR, FLOOR, 2_000)
    );
}

#[test]
fn learn_resumes_the_slow_probe_otherwise() {
    let saved = Saved {
        mode: Mode::Slow,
        confirmed: START + FAST_STEP,
        cap: START + FAST_STEP,
        started_at: 1_000,
        alive: 1_000 + CONFIRM_SECS + 5,
        ..Saved::fresh(0)
    };
    let (s, note) = learn(Some(&saved), 9_000);
    assert_eq!(note, Note::Resume(START + FAST_STEP));
    assert_eq!(
        (s.confirmed, s.cap, s.early_stops),
        (START + FAST_STEP, START + FAST_STEP, 0)
    );
    // 上限は最低値より下げない
    let tiny = Saved {
        cap: 0,
        confirmed: 0,
        ..saved
    };
    assert_eq!(learn(Some(&tiny), 9_000).1, Note::Resume(FLOOR));
}
