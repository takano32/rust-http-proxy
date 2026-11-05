//! 攻撃者が数を決められる警告を絞る門 (`Throttle`。T.28.9) と、相手が長さを決められる文字列を
//! 切って載せる `Clip` (T.28.38)。T.34.56 で `log.rs` から移した (本体は変えていない)。

use std::sync::atomic::{AtomicU64, Ordering};

/// 攻撃者が数を決められる警告 (400 系) を絞る間隔 (秒。T.28.9)。
///
/// 上限の WARN (`connection limit reached`) と同じ 1 分に 1 回。
pub const WARN_EVERY_SECS: u64 = 60;

/// 同じ種類の警告を `period` 秒に 1 回へ絞る門 (T.28.9)。
///
/// 不正な要求は 1 本につき WARN 1 行で上限が無く、走査 (scanner) が来るとコンソールと
/// `/log` の環が攻撃の行で埋まる (環が流れて、本当に見たい警告が消える)。種類ごとに 1 つ
/// 置き、通したときは**前の行から抑えた本数**を返すので、行に添えて出す
/// ([`Skipped`] の `Display`)。本数の合計は呼ぶ側の計数 (`/status` の `rejected_requests`
/// など) が持つので、ここは「ログに出さなかった本数」だけを数える。
///
/// 通るのは断る経路だけなので、原子の読み 1 回と加算 1 回は通した要求に乗らない。
pub struct Throttle {
    /// 前に通した時刻 (epoch 秒) + 1。0 = まだ 1 度も通していない
    last: AtomicU64,
    /// 前に通してから抑えた本数
    skipped: AtomicU64,
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new()
    }
}

impl Throttle {
    pub const fn new() -> Throttle {
        Throttle {
            last: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
        }
    }

    /// 今の時刻で [`Throttle::pass_at`] を引く。
    pub fn pass(&self, period_secs: u64) -> Option<Skipped> {
        self.pass_at(crate::clock::now_epoch(), period_secs)
    }

    /// 前に通してから `period_secs` 以上たっていれば通す (`Some` = 1 行出してよい)。
    /// 同時に来たときは比較交換に勝った 1 本だけが通る。
    pub(crate) fn pass_at(&self, now: u64, period_secs: u64) -> Option<Skipped> {
        let last = self.last.load(Ordering::Relaxed);
        if (last == 0 || now.saturating_sub(last - 1) >= period_secs)
            && self
                .last
                .compare_exchange(last, now + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            return Some(Skipped(self.skipped.swap(0, Ordering::Relaxed)));
        }
        self.skipped.fetch_add(1, Ordering::Relaxed);
        None
    }
}

/// [`Throttle`] が前の行から抑えた本数。`Display` は 0 なら空、そうでなければ
/// ` (+N more since the previous line, not logged)` (行の終わりにそのまま付ける)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Skipped(pub u64);

impl std::fmt::Display for Skipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 == 0 {
            Ok(())
        } else {
            write!(f, " (+{} more since the previous line, not logged)", self.0)
        }
    }
}

/// ログに載せる、相手が長さを決められる文字列 (壊れた要求行など)。`Display` は先頭
/// [`CLIP_BYTES`] バイトまでを `{:?}` の形で書き、それより長ければ全体の長さを添える (T.28.38)。
/// 1 行の長さを相手に決めさせない (T.28.9 で行の数は 1 分 1 行にしたが、長さは 64 KiB まで出ていた)。
#[derive(Debug, Clone, Copy)]
pub struct Clip<'a>(pub &'a str);

/// [`Clip`] が載せる長さの上限 (バイト)。
pub(crate) const CLIP_BYTES: usize = 256;

impl std::fmt::Display for Clip<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.0;
        if s.len() <= CLIP_BYTES {
            return write!(f, "{:?}", s);
        }
        let mut cut = CLIP_BYTES;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        write!(f, "{:?}... ({} bytes)", &s[..cut], s.len())
    }
}
