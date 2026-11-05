//! 閉じた接続の固定長リング ([`RecentRing`]。`/recent`。`recent` の一部。T.34.2 で
//! `recent/mod.rs` から移した)。
//!
//! 書くのは**接続の終了で 1 回** ([`RecentRing::push`])。読む口 (`/recent`) と、ファイルへ
//! 追記する history スレッド ([`RecentRing::take_unwritten`]) は同じ鍵の内側で絞る。
//! 「まだ書いていない件」の数え方は `errors.rs` の [`drain_unwritten`] と共通。
//!
//! 行を動かしただけで中身は変えていない。外から見える名前は `recent` の `pub use` でそのまま。

use std::sync::Mutex;

use super::RecentEntry;
use super::errors::drain_unwritten;
use crate::sync::LockExt;

/// 閉じた接続の個票を何件覚えておくか (固定。T14.4)。
///
/// デプロイ先は 72.7 時間で接続 3,100 本 = 43 本/時 なので、2,000 件あれば
/// **山のあった時間帯を丸ごと** さかのぼれる。1 件は下の切り詰めで 256 B 以内に収まる。
pub const MAX_RECENT: usize = 2000;

/// 閉じた接続の固定長リング (`/recent`。T14.4)。
///
/// 書くのは**接続の終了で 1 回**だけ ([`ConnTable::unregister`] と同じ場所)。
/// `/connections` が「いま」しか見せないのに対し、ここは「起きたこと」を残す口で、
/// バーストのとき誰が何を開いたか・遅かった 1 本がどの段階で遅かったかを後から読む。
pub struct RecentRing {
    inner: Mutex<RecentBuf>,
}

#[derive(Default)]
struct RecentBuf {
    buf: Vec<RecentEntry>,
    /// 次に書く位置 (`buf` が満杯になってからだけ意味を持つ)
    next: usize,
    /// 起動からの通算 (捨てた分も含む)
    total: u64,
    /// **ファイルに書いた所までの通算** (T14.9)
    written: u64,
    /// 起動時に状態ファイルから読み戻した件数 (`/recent` の `"restored"`)
    restored: usize,
}

impl Default for RecentRing {
    fn default() -> Self {
        RecentRing::new()
    }
}

impl RecentRing {
    pub fn new() -> RecentRing {
        RecentRing {
            inner: Mutex::new(RecentBuf::default()),
        }
    }

    /// 1 件書く (満杯なら最も古いものを上書きする)。**接続の終了で 1 回だけ。**
    ///
    /// `PROXY_RECORDS=off` なら**鍵も取らずに捨てる** (T14.41)。
    pub fn push(&self, entry: RecentEntry) {
        if !crate::records::recording() {
            return;
        }
        let mut r = self.inner.locked();
        r.total += 1;
        if r.buf.len() < MAX_RECENT {
            r.buf.push(entry);
            return;
        }
        let at = r.next;
        r.buf[at] = entry;
        r.next = (at + 1) % MAX_RECENT;
    }

    /// 条件に合うものを**新しい順**で返す。2 つ目は起動からの通算 (捨てた分も含む)。
    ///
    /// 絞りはここ (鍵の内側) で済ませる。`since` は「開いた時刻がこれ以降」、
    /// `client` は接続元の完全一致 (空なら絞らない)。
    pub fn select(&self, since: u64, client: &str) -> (Vec<RecentEntry>, u64) {
        let r = self.inner.locked();
        let len = r.buf.len();
        let mut out = Vec::with_capacity(len.min(MAX_RECENT));
        for i in 0..len {
            let start = if len < MAX_RECENT { len } else { r.next };
            let at = (start + len - 1 - i) % len;
            let e = &r.buf[at];
            if e.at < since || (!client.is_empty() && e.client != client) {
                continue;
            }
            out.push(e.clone());
        }
        (out, r.total)
    }

    pub fn len(&self) -> usize {
        self.inner.locked().buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// まだファイルに書いていない件を古い順で取り出す (**history スレッドだけが呼ぶ**。T14.9)。
    pub fn take_unwritten(&self, max: usize) -> (Vec<RecentEntry>, u64) {
        let mut r = self.inner.locked();
        let g = &mut *r;
        drain_unwritten(&g.buf, g.next, MAX_RECENT, g.total, &mut g.written, max)
    }

    /// 置き場を [`MAX_RECENT`] 件ぶん確保して触る (**起動時に 1 回だけ**。T17.8)。
    ///
    /// 触るのは 1 件の固定部だけで、文字列は書くときに確保する。返すのは触ったバイト数。
    pub fn prefault(&self) -> usize {
        crate::prefault::vec(&mut self.inner.locked().buf, MAX_RECENT)
    }

    /// 状態ファイルから読み戻す (**起動時に 1 回だけ**。T14.9)。
    pub fn restore(&self, entries: Vec<RecentEntry>) {
        let n = entries.len().min(MAX_RECENT);
        for e in entries {
            self.push(e);
        }
        let mut r = self.inner.locked();
        r.written = r.total;
        r.restored = n;
    }

    /// 再起動前から引き継いだ件数 (`/recent` の `"restored"`)。
    pub fn restored(&self) -> usize {
        self.inner.locked().restored
    }
}

/// 閉じた接続のリングの単体試験 (T.34.2 で別のファイルに移した)。
#[cfg(test)]
#[path = "ring_tests.rs"]
mod tests;
