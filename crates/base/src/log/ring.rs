//! `/log` のリング (warn 以上の直近 [`MAX_LOG_LINES`] 行。T13.4) と、状態ファイルへの書き出し・
//! 読み戻し (T14.9)。T34.56 で `log.rs` から移した。T34.57 で、満杯なら上書きして進む処理と
//! 添字の計算を `LogRing` のメソッドに寄せ、自由関数は鍵を取って委譲するだけにした。

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

/// `/log` のリングの中身 (T34.57 で、出し入れと添字の計算をここのメソッドに寄せた)。
///
/// 満杯 (`cap` 行) になるまでは末尾に足し、満杯からは最も古い行 (`next`) を上書きして進む。
/// 通算 (`total`) と書いた所までの通算 (`written`) は件数で持つ (上書きで位置がずれるため)。
/// 鍵は持たないので、単体試験では直に作って回せる (本番は [`RECENT`] の 1 つだけ)。
struct LogRing {
    buf: Vec<Line>,
    next: usize,
    total: u64,
    /// **状態ファイルに書いた所までの通算** (T14.9。`total` との差が「まだ書いていない行」)
    written: u64,
    /// 起動時に状態ファイルから読み戻した行数 (`/log` の `"restored"`)
    restored: usize,
    /// 覚える行数 (本番は [`MAX_LOG_LINES`])
    cap: usize,
}

impl LogRing {
    const fn new(cap: usize) -> LogRing {
        LogRing {
            buf: Vec::new(),
            next: 0,
            total: 0,
            written: 0,
            restored: 0,
            cap,
        }
    }

    /// 1 行足す (満杯なら最も古いものを上書きして進む)。通算も 1 つ進める。
    fn push(&mut self, line: Line) {
        self.total += 1;
        if self.buf.len() < self.cap {
            self.buf.push(line);
            return;
        }
        let at = self.next;
        self.buf[at] = line;
        self.next = (at + 1) % self.cap;
    }

    /// いちばん古い行の添字 (満杯になる前は `next == 0` なので先頭が最古)。
    fn start(&self) -> usize {
        if self.buf.len() < self.cap {
            0
        } else {
            self.next
        }
    }

    /// 直近 `n` 行を**新しい順**で写す。
    fn recent(&self, n: usize) -> Vec<Line> {
        let len = self.buf.len();
        let n = n.min(len);
        let start = self.start();
        // 最古の 1 つ手前 (`start + len - 1`) が最新
        (0..n)
            .map(|i| self.buf[(start + len - 1 - i) % len].clone())
            .collect()
    }

    /// まだ書いていない行を**古い順**で `max` 行まで取り出し、書いた印を通算に合わせる。
    /// 2 つ目は `max` を越えて落とした行数。
    fn take_unwritten(&mut self, max: usize) -> (Vec<Line>, u64) {
        let len = self.buf.len();
        let pending = self.total.saturating_sub(self.written).min(len as u64) as usize;
        self.written = self.total;
        if pending == 0 {
            return (Vec::new(), 0);
        }
        let take = pending.min(max);
        let start = self.start();
        let out = ((len - take)..len)
            .map(|i| self.buf[(start + i) % len].clone())
            .collect();
        (out, (pending - take) as u64)
    }

    /// 読み戻しの締め: 書いた印を通算に合わせ、読み戻した行数を覚える (読み戻した行は
    /// [`LogRing::push`] で足す。読み戻した行は書き直さない)。
    fn mark_restored(&mut self, n: usize) {
        self.written = self.total;
        self.restored = n;
    }

    /// 空にする (置き場の確保は残す)。
    fn clear(&mut self) {
        self.buf.clear();
        self.next = 0;
        self.total = 0;
        self.written = 0;
        self.restored = 0;
    }
}

/// 覚える本文を [`MAX_LOG_LINE`] バイトで切る (文字の途中で切らない)。
fn clip_line(msg: &str) -> &str {
    let mut end = msg.len().min(MAX_LOG_LINE);
    while end > 0 && !msg.is_char_boundary(end) {
        end -= 1;
    }
    &msg[..end]
}

/// warn 以上の直近の行 (`/log`)。**書くのは警告とエラーのときだけ**なので、
/// 熱い経路 (info のアクセスログ) はこの鍵を 1 度も取らない。
static RECENT: Mutex<LogRing> = Mutex::new(LogRing::new(MAX_LOG_LINES));

/// 直近 `n` 行を**新しい順**で返す。2 つ目は起動からの通算 (捨てた分も含む)。
pub fn recent(n: usize) -> (Vec<Line>, u64) {
    let r = RECENT.locked();
    (r.recent(n), r.total)
}

/// 覚えている行数。
pub fn recent_len() -> usize {
    RECENT.locked().buf.len()
}

/// リングを空にする (テスト用)。
pub fn clear_recent() {
    RECENT.locked().clear();
}

/// まだ状態ファイルに書いていない行を**古い順**で取り出す (T14.9)。
///
/// 呼ぶのは **history スレッドだけ** (5 秒ごと)。`max` を越える分は古い方から落とし、
/// 落とした行数を 2 つ目に返す (1 周期の書き込みを抑えるため。リングには残っている)。
/// 印を位置ではなく通算の件数にしてあるのは、リングが古い行を上書きするため。
pub fn take_unwritten(max: usize) -> (Vec<Line>, u64) {
    RECENT.locked().take_unwritten(max)
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
///
/// **鍵は 1 行ごとに取り直す** (T34.57 でメソッドに寄せたときも前のまま残した。そのため
/// `LogRing` に `restore` は置かず、[`LogRing::push`] と [`LogRing::mark_restored`] を順に呼ぶ)。
/// 1 回だけ取る形に変えると、読み戻しの最中に別のスレッドが出した警告の扱い (今は通算に
/// 混ざって「書いた」印の内側に入り、状態ファイルには書かれない) が変わるため。
pub fn restore(lines: Vec<Line>) {
    let n = lines.len().min(MAX_LOG_LINES);
    for line in lines {
        RECENT.locked().push(line);
    }
    RECENT.locked().mark_restored(n);
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
        msg: clip_line(msg).to_string(),
    };
    RECENT.locked().push(line);
}

/// `LogRing` を直に作って回す単体試験 (T34.57。プロセス全体の鍵は要らない)。
#[cfg(test)]
mod tests;
