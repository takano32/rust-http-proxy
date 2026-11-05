//! 待ちの決め方: `poll` の関心 ([`interest`])・渡す記述子 ([`poll_set`])・待つ長さ
//! ([`wait_budget`])・詰まりの計測 ([`stall_clock`] / [`charge_stall`])。
//!
//! `interest` と `wait_budget` は純粋 (ソケットも時計も見ない) で、単体試験で縛る。
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use super::dir::Dir;
use crate::recent::{CLIENT_SIDE, ORIGIN_SIDE, SIDES};
use crate::sys::{POLLERR, POLLHUP, POLLIN, POLLOUT, PollFd};
use proxy_base::minrate::RateGuard;

/// `poll` に渡す記述子。**関心 (`events`) が無いなら `-1`** (T15.5)。
///
/// `poll(2)` は負の記述子を無視して `revents` を 0 にする。関心が 0 の記述子を
/// そのまま渡すと、`events` に関係なく返る `POLLERR` / `POLLHUP` で
/// 「誰も面倒を見ない起床」が起きて空回りになる (呼ぶ側の注記を見よ)。
#[inline(always)]
fn poll_fd(sock: &TcpStream, events: i16) -> RawFd {
    if events == 0 { -1 } else { sock.as_raw_fd() }
}

/// どちらも動かなかったときに待つ関心 (`socks` の添字ごと)。
///
/// 終わった向き (`done`) は何も立てない。読む口が開いていて中継が空なら送信元に
/// `POLLIN`、渡しきれていないバイトがあれば送信先に `POLLOUT`。
#[inline(always)]
pub(super) fn interest(dirs: &[Dir; 2]) -> [i16; 2] {
    let mut events = [0i16; 2];
    for d in dirs.iter() {
        if d.done {
            continue;
        }
        if !d.src_eof && d.pending == 0 {
            events[d.src] |= POLLIN;
        }
        if d.pending > 0 {
            events[d.dst] |= POLLOUT;
        }
    }
    events
}

/// `poll` に渡す 2 本 (関心の無い記述子は `-1`)。
///
/// **関心の無い記述子は `poll` に渡さない** (T15.5)。`poll(2)` は
/// `events` が 0 でも `POLLERR` / `POLLHUP` を必ず返すので、渡すと
/// 「誰も面倒を見ない起床」が生まれる: 上の輪は `readable` を立てるだけで、
/// その記述子を読む向きが `done` なら輪の先頭で飛ばされ、反対の向きは
/// 自分の送信元しか見ていないので 1 バイトも進まない (`progressed = false`)。
/// `poll` が 0 を返さないのでアイドル打ち切りにも永久に当たらず、
/// **1 本のトンネルが 1 スレッドを回し続ける** (2026-09-18 のデプロイ先は
/// この形のトンネル 2 本で CPU 割り当て 0.5 コアを 27 時間食い切っていた。
/// `mtalk.google.com:5228`、齢 25.4 時間、通算 9 kB、0 bps)。
///
/// 負の記述子は `poll(2)` が無視して `revents` を 0 にするので、これで
/// **`poll` が返した起床は必ず、その記述子への `fill` か `drain` の
/// システムコール 1 回につながる**という不変条件が立つ (関心が `POLLIN`
/// なら `fill` が EOF かエラーを返して進み、`POLLOUT` なら `drain` が
/// 書けるかエラーを返して進む)。どの向きも `done` でなければ `POLLIN` か
/// `POLLOUT` を 1 つは立てる ([`interest`]) ので、両方 0 になるのは
/// 「全部 `done`」= この手前で輪を抜けるときだけ。死んだ半閉じの
/// トンネルは今までどおりアイドル打ち切り (`PROXY_TUNNEL_IDLE_SECS`) で閉じる
#[inline(always)]
pub(super) fn poll_set(socks: &[TcpStream; 2], events: [i16; 2]) -> [PollFd; 2] {
    [
        PollFd::new(poll_fd(&socks[0], events[0]), events[0]),
        PollFd::new(poll_fd(&socks[1], events[1]), events[1]),
    ]
}

/// 待つ長さ (ms。負なら無期限) と、最小の速さの窓で縮めたか。
///
/// 両方向とも暇 (`parkable`) なら、待つのは猶予 (`grace_ms`) のあいだだけで、空振りしたら
/// 預ける。そうでなければ無通信の締め切り (`timeout_ms`)。
///
/// 書けずに待つ向きは、最小の速さの窓の残り (`rate_left(向き)`) より長く眠らない (T29.34)。
/// 窓の残りを読むのは `POLLOUT` が立っている向きだけ。
/// 読まない相手はめったに `POLLOUT` を返さないので、無通信の締め切り (300 秒) まで
/// 眠ると判定が遅れる。縮めたときは、`poll` が 0 で戻ってもアイドル打ち切りにしない
#[inline(always)]
pub(super) fn wait_budget(
    grace_ms: Option<i32>,
    parkable: bool,
    timeout_ms: i32,
    rate_left: impl Fn(usize) -> Option<Duration>,
    events: [i16; SIDES],
) -> (i32, bool) {
    let mut wait_ms = match grace_ms {
        Some(g) if parkable => g,
        _ => timeout_ms,
    };
    let mut capped = false;
    for (side, ev) in events.iter().enumerate() {
        if ev & POLLOUT != 0
            && let Some(left) = rate_left(side)
        {
            let ms = left.as_millis().clamp(1, i32::MAX as u128) as i32;
            if wait_ms < 0 || ms < wait_ms {
                wait_ms = ms;
                capped = true;
            }
        }
    }
    (wait_ms, capped)
}

/// 中継の詰まりの向き (T14.42) を測る起点。**書けなくて待ちに入る回だけ**時計を
/// 読む: `events` に `POLLOUT` が立つのは、直前の `drain` (splice) が
/// `EAGAIN` で止まって `pending` が残っている方向だけなので、
/// **書けば必ず入る相手 (loopback) では `Instant::now()` を 1 度も
/// 呼ばない** — ここの比較 1 回で終わる。64 KiB ごとでも splice ごとでも
/// なく「待ちに入る回」なので、`--lite` でも同じように読む
/// (T14.25 の「最初の EOF の 1 回」と同じ扱い)
#[inline(always)]
pub(super) fn stall_clock(events: [i16; SIDES]) -> Option<Instant> {
    ((events[CLIENT_SIDE] | events[ORIGIN_SIDE]) & POLLOUT != 0).then(Instant::now)
}

/// 待ち終わったのでもう 1 回読み、**書けなかった向きだけ**に積む。
/// 両方詰まっていれば両方に積む (どちらも「その間書けなかった」ため)。
/// 最小の速さを割ったら `true` (呼ぶ側が閉じる)。
#[inline(always)]
pub(super) fn charge_stall(
    stall_from: Option<Instant>,
    events: [i16; SIDES],
    stall_us: &mut [u64; SIDES],
    rate: &mut [RateGuard; SIDES],
) -> bool {
    let mut too_slow = false;
    if let Some(t) = stall_from {
        let elapsed = t.elapsed();
        let waited = elapsed.as_micros().min(u64::MAX as u128) as u64;
        for (side, ev) in events.iter().enumerate() {
            if ev & POLLOUT != 0 {
                stall_us[side] = stall_us[side].saturating_add(waited);
                // 同じ待ちを最小の速さの見張りにも積む (T29.34)
                too_slow |= rate[side].note(elapsed, 0).is_err();
            }
        }
    }
    too_slow
}

/// `poll` が返した起床を向きの「読める」に写す。
#[inline(always)]
pub(super) fn mark_readable(dirs: &mut [Dir; 2], fds: &[PollFd; 2]) {
    for d in dirs.iter_mut() {
        // HUP / ERR でも read して EOF を確かめる
        if fds[d.src].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
            d.readable = true;
        }
    }
}
