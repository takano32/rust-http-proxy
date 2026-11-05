//! 「真になるまで待つ」の道具 (T.34.113 で各試験の手書きの待ちループを寄せた)。

use std::thread;
use std::time::{Duration, Instant};

/// 待つループの刻み。[`POLL_MAX`] 回で 10 秒 (負荷の高い CI でも落ちない幅)。
pub const POLL_STEP: Duration = Duration::from_millis(20);
/// 待つループの回数の上限 ([`wait_until`] と `endpoint_json` の引き直しが共有する)。
pub const POLL_MAX: usize = 500;

/// 待つ上限。回数で数えるか (1 回の確かめが遅い機械でも、確かめる回数は減らない)、
/// 始めてからの経過で数えるか (「N 秒以内に」を確かめたいとき)。
#[derive(Clone, Copy, Debug)]
pub enum Limit {
    /// 確かめて `step` 眠る、をこの回数まで
    Tries(usize),
    /// 確かめて外れたら経過を見て、この長さを過ぎていたら諦める
    Within(Duration),
}

/// `probe` が `Some` を返すまで `step` 刻みで待ち、その値を返す。上限を過ぎたら `what` を
/// 文面に入れて落ちる。
pub fn wait_for<T>(
    what: &str,
    step: Duration,
    limit: Limit,
    mut probe: impl FnMut() -> Option<T>,
) -> T {
    let started = Instant::now();
    let mut tries = 0usize;
    loop {
        if let Some(v) = probe() {
            return v;
        }
        tries += 1;
        let over = match limit {
            Limit::Tries(n) => tries >= n,
            Limit::Within(d) => started.elapsed() >= d,
        };
        if over {
            panic!("timed out waiting: {}", what);
        }
        thread::sleep(step);
    }
}

/// `cond` が真になるまで `step` 刻みで待つ (上限を過ぎたら `what` を文面に入れて落ちる)。
pub fn wait_until_for(mut cond: impl FnMut() -> bool, what: &str, step: Duration, limit: Limit) {
    wait_for(what, step, limit, || cond().then_some(()))
}

/// `cond` が真になるまで最大 10 秒待つ (負荷の高い CI でも落ちない幅)。
pub fn wait_until(cond: impl Fn() -> bool, what: &str) {
    wait_until_for(cond, what, POLL_STEP, Limit::Tries(POLL_MAX))
}
