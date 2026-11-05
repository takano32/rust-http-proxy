use super::*;

#[test]
fn grace_doubles_after_short_parks_and_resets_after_a_long_one() {
    // T.32.19: 0.5 秒ごとに起こされるトンネルは 100 → 200 → 400 → 800 → 1,000 ms で止まる
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
