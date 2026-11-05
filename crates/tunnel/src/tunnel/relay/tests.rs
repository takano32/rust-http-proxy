use super::*;

#[test]
fn grace_doubles_after_short_parks_and_resets_after_a_long_one() {
    // T32.19: 0.5 秒ごとに起こされるトンネルは 100 → 200 → 400 → 800 → 1,000 ms で止まる
    let short = Duration::from_millis(400);
    let mut g = MIN_PARK_GRACE;
    let mut seen = Vec::new();
    for _ in 0..6 {
        g = next_grace(g, MIN_PARK_GRACE, short);
        seen.push(g.as_millis());
    }
    assert_eq!(seen, [200, 400, 800, 1000, 1000, 1000]);
    // 長く眠ったあとは下限に戻る (本当に暇なトンネルは今までどおり早く預ける)
    assert_eq!(
        next_grace(g, MIN_PARK_GRACE, Duration::from_secs(5)),
        MIN_PARK_GRACE
    );
    // 下限が上限より長い設定 (`PROXY_PARK_GRACE_MS=3000`) では伸ばさず縮めもしない
    let base = Duration::from_secs(3);
    assert_eq!(next_grace(base, base, short), base);
}

/// 向き 1 本の状態から作る (`dirs[1]` は終わった向きにして、`dirs[0]` だけを見る)。
fn one_dir(done: bool, src_eof: bool, pending: usize) -> [Dir; 2] {
    let mut d = Dir::new(0, 1);
    d.done = done;
    d.src_eof = src_eof;
    d.pending = pending;
    let mut other = Dir::new(1, 0);
    other.done = true;
    [d, other]
}

#[test]
fn interest_covers_the_eight_states_of_one_direction() {
    use crate::sys::{POLLIN, POLLOUT};
    // (done, src_eof, pending) → [送信元 (0) の関心, 送信先 (1) の関心]
    let cases: [((bool, bool, usize), [i16; 2]); 8] = [
        ((false, false, 0), [POLLIN, 0]),
        ((false, false, 5), [0, POLLOUT]),
        ((false, true, 0), [0, 0]),
        ((false, true, 5), [0, POLLOUT]),
        // 終わった向きは何も立てない (`poll` に渡すと空回りの起床になる。T15.5)
        ((true, false, 0), [0, 0]),
        ((true, false, 5), [0, 0]),
        ((true, true, 0), [0, 0]),
        ((true, true, 5), [0, 0]),
    ];
    for ((done, eof, pending), want) in cases {
        assert_eq!(
            interest(&one_dir(done, eof, pending)),
            want,
            "done={done} src_eof={eof} pending={pending}"
        );
    }
}

#[test]
fn interest_ors_both_directions_on_the_same_socket() {
    use crate::sys::{POLLIN, POLLOUT};
    let mut dirs = [Dir::new(0, 1), Dir::new(1, 0)];
    assert_eq!(interest(&dirs), [POLLIN, POLLIN]);
    // 上りが渡しきれていない: オリジン側は「読む」と「書く」の両方を待つ
    dirs[0].pending = 10;
    assert_eq!(interest(&dirs), [0, POLLIN | POLLOUT]);
}

/// 窓の残りを配列で渡す形 (試験用)。
fn budget(
    grace_ms: Option<i32>,
    parkable: bool,
    timeout_ms: i32,
    left: [Option<Duration>; 2],
    events: [i16; 2],
) -> (i32, bool) {
    wait_budget(grace_ms, parkable, timeout_ms, |side| left[side], events)
}

#[test]
fn wait_budget_reads_the_window_only_for_the_sides_waiting_to_write() {
    use crate::sys::{POLLIN, POLLOUT};
    // `POLLOUT` の無い向きの窓を読みに行ったら落とす (前と同じく、読むのは書けずに待つ向きだけ)
    let left = |side: usize| {
        assert_eq!(side, 1, "POLLOUT の無い向きの窓を読んだ");
        Some(Duration::from_millis(20))
    };
    assert_eq!(
        wait_budget(None, false, 300_000, left, [POLLIN, POLLOUT]),
        (20, true)
    );
}

#[test]
fn wait_budget_picks_the_grace_only_when_parkable() {
    let none = [None; 2];
    assert_eq!(budget(Some(100), true, 300_000, none, [0, 0]), (100, false));
    assert_eq!(
        budget(Some(100), false, 300_000, none, [0, 0]),
        (300_000, false)
    );
    assert_eq!(budget(None, true, 300_000, none, [0, 0]), (300_000, false));
    // 無期限 (`PROXY_TUNNEL_IDLE_SECS=0`) はそのまま負
    assert_eq!(budget(None, false, -1, none, [0, 0]), (-1, false));
}

#[test]
fn wait_budget_caps_only_the_directions_waiting_to_write() {
    use crate::sys::{POLLIN, POLLOUT};
    let ms = Duration::from_millis;
    // `POLLOUT` が無い向きの窓は見ない
    assert_eq!(
        budget(None, false, 300_000, [Some(ms(50)), None], [POLLIN, 0]),
        (300_000, false)
    );
    // 書けずに待つ向きは窓の残りまでしか眠らない (T29.34)
    assert_eq!(
        budget(None, false, 300_000, [Some(ms(50)), None], [POLLOUT, 0]),
        (50, true)
    );
    // 無期限でも縮める
    assert_eq!(
        budget(None, false, -1, [None, Some(ms(70))], [0, POLLOUT]),
        (70, true)
    );
    // 窓のほうが長ければ縮めない
    assert_eq!(
        budget(
            Some(100),
            true,
            300_000,
            [Some(ms(500)), None],
            [POLLOUT, 0]
        ),
        (100, false)
    );
    // 両方なら短いほう。残りが 0 でも 1 ms は眠る (`poll(0)` の空回りにしない)
    let both = [Some(ms(40)), Some(ms(30))];
    assert_eq!(
        budget(None, false, 300_000, both, [POLLOUT, POLLOUT]),
        (30, true)
    );
    assert_eq!(
        budget(
            None,
            false,
            300_000,
            [Some(Duration::ZERO), None],
            [POLLOUT, 0]
        ),
        (1, true)
    );
}
