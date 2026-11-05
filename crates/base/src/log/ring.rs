//! `/log` のリング (warn 以上の直近 [`MAX_LOG_LINES`] 行。T13.4) と、状態ファイルへの書き出し・
//! 読み戻し (T14.9)。T.34.56 で `log.rs` から移した (本体は変えていない)。

use super::Level;
use crate::sync::LockExt;
use std::sync::Mutex;

/// `/log` に覚えておく行数 (固定)。1 行 [`MAX_LOG_LINE`] B なので最悪でも 256 KiB。
pub const MAX_LOG_LINES: usize = 1000;
/// 1 行に覚える長さ (バイト)。長い警告はここで切る。
pub const MAX_LOG_LINE: usize = 256;

/// リングに覚えている 1 行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// いつ (epoch 秒)
    pub at: u64,
    pub level: Level,
    /// ログの `[conn#N]` (無ければ `[main]`)
    pub conn: Option<usize>,
    pub msg: String,
}

#[derive(Default)]
struct LogRing {
    buf: Vec<Line>,
    next: usize,
    total: u64,
    /// **状態ファイルに書いた所までの通算** (T14.9。`total` との差が「まだ書いていない行」)
    written: u64,
    /// 起動時に状態ファイルから読み戻した行数 (`/log` の `"restored"`)
    restored: usize,
}

/// warn 以上の直近の行 (`/log`)。**書くのは警告とエラーのときだけ**なので、
/// 熱い経路 (info のアクセスログ) はこの鍵を 1 度も取らない。
static RECENT: Mutex<LogRing> = Mutex::new(LogRing {
    buf: Vec::new(),
    next: 0,
    total: 0,
    written: 0,
    restored: 0,
});

/// 直近 `n` 行を**新しい順**で返す。2 つ目は起動からの通算 (捨てた分も含む)。
pub fn recent(n: usize) -> (Vec<Line>, u64) {
    let r = RECENT.locked();
    let len = r.buf.len();
    let n = n.min(len);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        // `next` の 1 つ手前が最新 (満杯になる前は `next == 0` なので末尾が最新)
        let start = if len < MAX_LOG_LINES { len } else { r.next };
        out.push(r.buf[(start + len - 1 - i) % len].clone());
    }
    (out, r.total)
}

/// 覚えている行数。
pub fn recent_len() -> usize {
    RECENT.locked().buf.len()
}

/// リングを空にする (テスト用)。
pub fn clear_recent() {
    let mut r = RECENT.locked();
    r.buf.clear();
    r.next = 0;
    r.total = 0;
    r.written = 0;
    r.restored = 0;
}

/// まだ状態ファイルに書いていない行を**古い順**で取り出す (T14.9)。
///
/// 呼ぶのは **history スレッドだけ** (5 秒ごと)。`max` を越える分は古い方から落とし、
/// 落とした行数を 2 つ目に返す (1 周期の書き込みを抑えるため。リングには残っている)。
/// 印を位置ではなく通算の件数にしてあるのは、リングが古い行を上書きするため。
pub fn take_unwritten(max: usize) -> (Vec<Line>, u64) {
    let mut r = RECENT.locked();
    let len = r.buf.len();
    let pending = r.total.saturating_sub(r.written).min(len as u64) as usize;
    r.written = r.total;
    if pending == 0 {
        return (Vec::new(), 0);
    }
    let take = pending.min(max);
    let start = if len < MAX_LOG_LINES { 0 } else { r.next };
    let out = ((len - take)..len)
        .map(|i| r.buf[(start + i) % len].clone())
        .collect();
    (out, (pending - take) as u64)
}

/// `/log` のリングの置き場を [`MAX_LOG_LINES`] 行ぶん確保して触る (**起動時に 1 回だけ**。T17.8)。
///
/// 触るのは行の固定部 (`Line` の大きさ × 行数) だけで、本文の `String` は行が来たときに
/// 確保する。返すのは触ったバイト数。呼ぶのは既定のプロファイルの Linux だけ。
pub fn prefault() -> usize {
    crate::prefault::vec(&mut RECENT.locked().buf, MAX_LOG_LINES)
}

/// 状態ファイルから読み戻す (**起動時に 1 回だけ**。T14.9)。
///
/// 読み戻した行は**書き直さない** (印を通算に合わせる)。行数は `/log` の
/// `"restored"` に出す。
pub fn restore(lines: Vec<Line>) {
    let n = lines.len().min(MAX_LOG_LINES);
    for line in lines {
        let mut r = RECENT.locked();
        r.total += 1;
        if r.buf.len() < MAX_LOG_LINES {
            r.buf.push(line);
            continue;
        }
        let at = r.next;
        r.buf[at] = line;
        r.next = (at + 1) % MAX_LOG_LINES;
    }
    let mut r = RECENT.locked();
    r.written = r.total;
    r.restored = n;
}

/// 再起動前から引き継いだ行数 (`/log` の `"restored"`)。
pub fn restored_count() -> usize {
    RECENT.locked().restored
}

/// warn 以上の 1 行をリングに写す (満杯なら最も古いものを上書き)。
pub(super) fn remember(level: Level, conn_id: Option<usize>, msg: &str) {
    let line = Line {
        at: crate::clock::now_epoch(),
        level,
        conn: conn_id,
        // 文字の途中で切らない
        msg: {
            let mut end = msg.len().min(MAX_LOG_LINE);
            while end > 0 && !msg.is_char_boundary(end) {
                end -= 1;
            }
            msg[..end].to_string()
        },
    };
    let mut r = RECENT.locked();
    r.total += 1;
    if r.buf.len() < MAX_LOG_LINES {
        r.buf.push(line);
        return;
    }
    let at = r.next;
    r.buf[at] = line;
    r.next = (at + 1) % MAX_LOG_LINES;
}
