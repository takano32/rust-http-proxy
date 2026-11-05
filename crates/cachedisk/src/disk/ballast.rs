//! ディスクのバラスト (`ballast.reserve`。予算の未使用分を fallocate で先行確保する)。
//!
//! T.34.90 で `mod.rs` から分けた。ファイルと長さは [`Ballast`] だけが書き、
//! [`DiskTier`] は鍵を取って呼び、戻り値でログと記帳 (`ballast_bytes`・`fill_failed`) をする。

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;

use super::{BALLAST_FILE, DiskTier, EDQUOT, ENOSPC};
use crate::config::MIB;
use crate::sync::LockExt;
use crate::sysinfo;
use crate::{log_debug, log_warn};

/// バラストの伸長はこれ以上の余裕があるときだけ行い、細かな伸縮を避ける。
const BALLAST_STEP: u64 = 16 * MIB;
/// 1 回の fallocate で伸ばす最大量 (巨大な 1 回呼び出しでスレッドを止めない)。
const BALLAST_GROW_MAX: u64 = 1024 * MIB;

/// バラストファイルとその長さ。
pub(super) struct Ballast {
    file: Option<File>,
    bytes: u64,
    supported: bool,
}

/// [`Ballast::grow`] の結果。
pub(super) struct Grow {
    /// 伸ばしたバイト数
    pub(super) added: u64,
    /// この FS では先行確保できない (バラストは 0 に戻して、以後は伸ばさない)
    pub(super) unsupported: Option<io::Error>,
    /// それ以外の失敗で途中で止めた (次のプローブでまた試す)
    pub(super) paused: Option<io::Error>,
    /// 止めた理由が ENOSPC / EDQUOT (割当の探索に「ここが限界」と渡す)
    pub(super) fill_failed: bool,
}

impl Ballast {
    pub(super) fn new(supported: bool) -> Ballast {
        Ballast {
            file: None,
            bytes: 0,
            supported,
        }
    }

    pub(super) fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(super) fn supported(&self) -> bool {
        self.supported
    }

    /// 空のバラストファイルを用意する。`reserve` が偽ならファイルを消すだけ。
    /// 開けなければ先行確保を諦める (`supported` を下ろして、その失敗を返す)。
    pub(super) fn prepare(&mut self, path: &Path, reserve: bool) -> io::Result<()> {
        self.bytes = 0;
        if !reserve {
            self.file = None;
            let _ = fs::remove_file(path);
            return Ok(());
        }
        // 前回分は空にして、予算計算のあとで改めて伸ばす
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
        {
            Ok(f) => {
                self.file = Some(f);
                Ok(())
            }
            Err(e) => {
                self.file = None;
                self.supported = false;
                Err(e)
            }
        }
    }

    /// `new_len` まで切り詰める (ファイルが無ければ 0 にする)。失敗したら長さは変えない。
    pub(super) fn shrink_to(&mut self, new_len: u64) -> io::Result<()> {
        match &self.file {
            Some(f) => {
                f.set_len(new_len)?;
                self.bytes = new_len;
            }
            None => self.bytes = 0,
        }
        Ok(())
    }

    /// `target` バイトまで [`BALLAST_STEP`] 以上の余裕がある間、伸ばす。
    pub(super) fn grow(&mut self, target: u64) -> Grow {
        let mut g = Grow {
            added: 0,
            unsupported: None,
            paused: None,
            fill_failed: false,
        };
        let Some(file) = self.file.as_ref().filter(|_| self.supported) else {
            return g;
        };
        while self.bytes.saturating_add(BALLAST_STEP) <= target {
            let step = (target - self.bytes).min(BALLAST_GROW_MAX);
            match sysinfo::preallocate(file, self.bytes, step) {
                Ok(()) => {
                    self.bytes += step;
                    g.added += step;
                }
                Err(e) if sysinfo::is_unsupported(&e) => {
                    let _ = file.set_len(0);
                    self.supported = false;
                    self.bytes = 0;
                    g.unsupported = Some(e);
                    break;
                }
                Err(e) => {
                    // 自分の先行確保が入らないだけなので、他者の圧迫 (ENOSPC) には数えない。
                    // 割当の探索には「ここが限界」の合図として渡す
                    g.fill_failed = matches!(e.raw_os_error(), Some(ENOSPC) | Some(EDQUOT));
                    g.paused = Some(e);
                    break;
                }
            }
        }
        g
    }

    /// 先行確保を止める (ファイルを閉じて消す。以後は伸ばさない)。
    pub(super) fn disable(&mut self, path: &Path) {
        self.supported = false;
        self.bytes = 0;
        self.file = None;
        let _ = fs::remove_file(path);
    }

    /// 全部を手放す (終了時。ファイルを 0 に切り詰めてから消す)。
    pub(super) fn drop_all(&mut self, path: &Path) {
        if let Some(f) = self.file.take() {
            let _ = f.set_len(0);
        }
        self.bytes = 0;
        let _ = fs::remove_file(path);
    }
}

impl DiskTier {
    pub fn reserve_active(&self) -> bool {
        self.reserve && self.ballast.locked().supported()
    }

    /// 先行確保を止めてバラストファイルを消す (tmpfs 上など、確保しても意味がない場合)。
    pub fn disable_reserve(&self) {
        let mut b = self.ballast.locked();
        b.disable(&self.dir.join(BALLAST_FILE));
        self.ballast_bytes.store(0, Ordering::Relaxed);
    }

    /// バラストファイルを空の状態で用意する。開けなければ先行確保だけ諦める (致命的にはしない)。
    pub(super) fn prepare_ballast(&self) {
        let path = self.dir.join(BALLAST_FILE);
        let mut b = self.ballast.locked();
        self.ballast_bytes.store(0, Ordering::Relaxed);
        if let Err(e) = b.prepare(&path, self.reserve) {
            log_warn!(
                None,
                "disk reservation unavailable ({}): {} (using limit only)",
                path.display(),
                e
            );
        }
    }

    pub(super) fn shrink_ballast(&self, by: u64) {
        let mut b = self.ballast.locked();
        if b.bytes() == 0 {
            return;
        }
        let new_len = b.bytes().saturating_sub(by);
        if let Err(e) = b.shrink_to(new_len) {
            log_warn!(None, "disk reservation shrink failed: {}", e);
        }
        self.ballast_bytes.store(b.bytes(), Ordering::Relaxed);
    }

    /// 抱えてよい合計 (エントリ + バラスト) がこの値になるまでバラストを切り詰める。
    /// 段階化で上限が下がったときに呼ぶ (予算が縮んだときの [`Self::make_room`] と同じ道)。
    pub fn release_ballast_to(&self, max_owned: u64) {
        let over = self.owned().saturating_sub(max_owned);
        if over > 0 {
            self.shrink_ballast(over);
        }
    }

    /// 予算の未使用分をバラストファイルで埋める。戻り値は追加したバイト数。
    ///
    /// `max_owned` は「エントリ + バラストがこれを超えないところまで」の上限
    /// (段階化。`PROXY_CACHE_RESERVE` の既定)。`eager` は `u64::MAX`。
    pub fn fill_ballast(&self, max_owned: u64) -> u64 {
        if !self.reserve || !self.is_ready() {
            return 0;
        }
        // 書き込み中の分も埋めてはいけない
        let target = self
            .capacity()
            .min(max_owned)
            .saturating_sub(self.usage().0)
            .saturating_sub(self.in_flight_bytes());
        let mut b = self.ballast.locked();
        if !b.supported() || b.file.is_none() {
            return 0;
        }
        let g = b.grow(target);
        if let Some(e) = &g.unsupported {
            log_warn!(
                None,
                "disk reservation unavailable on {}: {} (using limit only)",
                self.dir.display(),
                e
            );
        }
        if let Some(e) = &g.paused {
            if g.fill_failed {
                self.fill_failed.store(true, Ordering::Relaxed);
            }
            log_debug!(None, "disk reservation paused: {}", e);
        }
        self.ballast_bytes.store(b.bytes(), Ordering::Relaxed);
        g.added
    }

    /// バラストを手放す (終了時)。
    pub fn shutdown(&self) {
        let mut b = self.ballast.locked();
        b.drop_all(&self.dir.join(BALLAST_FILE));
        self.ballast_bytes.store(0, Ordering::Relaxed);
    }
}
