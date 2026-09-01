//! L2 ディスク層。
//!
//! - 1 エントリ 1 ファイル (`<dir>/<上位 8bit の 16 進>/<キー 32 桁>.cache`) を 256 分割で保存。
//!   再検証できる (ETag / Last-Modified 付き) エントリは拡張子を `.vcache` にする
//! - ファイルの mtime に有効期限を書いておき、起動時の走査 ([`scan`]) ではヘッダーを読まない
//! - 本文はストリーミングで書く ([`writer`])。読むときもファイルからそのまま流す
//! - メモリ上の LRU インデックスで合計バイト数を管理する
//! - `reserve` 有効時は予算の未使用分を `ballast.reserve` に fallocate して先行確保する
//! - 書き込み・読み出し後はページキャッシュを手放し、L1 との二重キャッシュを避ける

mod scan;
pub mod writer;

pub use scan::ScanReport;
pub use writer::DiskWriter;

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::config::MIB;
use super::format::{self, Meta};
use super::key::CacheKey;
use super::lru::{LruEntry, Store};
use super::memory::is_garbage;
use crate::sysinfo;
use crate::{log_debug, log_warn};

pub const SHARDS: usize = 256;
const BALLAST_FILE: &str = "ballast.reserve";
/// バラストの伸長はこれ以上の余裕があるときだけ行い、細かな伸縮を避ける。
const BALLAST_STEP: u64 = 16 * MIB;
/// 1 回の fallocate で伸ばす最大量 (巨大な 1 回呼び出しでスレッドを止めない)。
const BALLAST_GROW_MAX: u64 = 1024 * MIB;
const ENOSPC: i32 = 28;

pub struct DiskEntry {
    pub size: u64,
    /// 起動走査で復元したエントリは `stored_at` が 0 (ヒット時にヘッダーから読み直す)。
    pub meta: Meta,
    last_used: u64,
}

impl DiskEntry {
    fn new(size: u64, meta: Meta) -> Self {
        Self {
            size,
            meta,
            last_used: 0,
        }
    }
}

impl LruEntry for DiskEntry {
    fn size(&self) -> u64 {
        self.size
    }
    fn last_used(&self) -> u64 {
        self.last_used
    }
    fn set_last_used(&mut self, seq: u64) {
        self.last_used = seq;
    }
}

/// ヒットしたエントリ。`file` はワイヤバイト列 (ステータス行) の先頭に位置している。
pub struct DiskHit {
    pub meta: Meta,
    pub file: File,
    pub size: u64,
}

struct Ballast {
    file: Option<File>,
    bytes: u64,
    supported: bool,
}

pub struct DiskTier {
    dir: PathBuf,
    ready: AtomicBool,
    index: Mutex<Store<DiskEntry>>,
    capacity: AtomicU64,
    ballast: Mutex<Ballast>,
    ballast_bytes: AtomicU64,
    reserve: bool,
    /// 直近のプローブ以降に自分の書き込みが ENOSPC になった回数
    enospc: AtomicU64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct WriteOutcome {
    pub stored: bool,
    pub size: u64,
    pub evicted: usize,
}

impl DiskTier {
    pub fn new(dir: PathBuf, reserve: bool) -> Self {
        Self {
            dir,
            ready: AtomicBool::new(false),
            index: Mutex::new(Store::default()),
            capacity: AtomicU64::new(0),
            ballast: Mutex::new(Ballast {
                file: None,
                bytes: 0,
                supported: reserve,
            }),
            ballast_bytes: AtomicU64::new(0),
            reserve,
            enospc: AtomicU64::new(0),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    pub fn capacity(&self) -> u64 {
        self.capacity.load(Ordering::Relaxed)
    }

    pub fn set_capacity(&self, bytes: u64) {
        self.capacity.store(bytes, Ordering::Relaxed);
    }

    /// (エントリ合計バイト, 件数)
    pub fn usage(&self) -> (u64, usize) {
        let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
        (index.bytes(), index.len())
    }

    pub fn ballast_bytes(&self) -> u64 {
        self.ballast_bytes.load(Ordering::Relaxed)
    }

    /// エントリ + バラスト (自分がファイルシステムから取っている分)。
    pub fn owned(&self) -> u64 {
        self.usage().0.saturating_add(self.ballast_bytes())
    }

    /// 上限までの空き (エントリとバラストを除いた分)。
    pub fn free_room(&self) -> u64 {
        self.capacity().saturating_sub(self.owned())
    }

    pub fn reserve_active(&self) -> bool {
        self.reserve
            && self
                .ballast
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .supported
    }

    /// 先行確保を止めてバラストファイルを消す (tmpfs 上など、確保しても意味がない場合)。
    pub fn disable_reserve(&self) {
        let mut b = self.ballast.lock().unwrap_or_else(|p| p.into_inner());
        b.supported = false;
        b.bytes = 0;
        b.file = None;
        self.ballast_bytes.store(0, Ordering::Relaxed);
        let _ = fs::remove_file(self.dir.join(BALLAST_FILE));
    }

    /// 直近のプローブ以降の ENOSPC 回数を取り出してリセットする。
    pub fn take_enospc(&self) -> u64 {
        self.enospc.swap(0, Ordering::Relaxed)
    }

    /// I/O 結果を通過させつつ、ENOSPC を数える。
    pub(super) fn note_io<T>(&self, r: io::Result<T>) -> io::Result<T> {
        if let Err(e) = &r
            && e.raw_os_error() == Some(ENOSPC)
        {
            self.enospc.fetch_add(1, Ordering::Relaxed);
        }
        r
    }

    pub(super) fn shard_dir(&self, key: CacheKey) -> PathBuf {
        self.dir.join(format!("{:02x}", key.shard()))
    }

    pub fn path_for(&self, key: CacheKey, validators: bool) -> PathBuf {
        let ext = if validators { "vcache" } else { "cache" };
        self.shard_dir(key).join(format!("{}.{}", key, ext))
    }

    /// ディレクトリを準備し、既存ファイルからインデックスを復元する。
    pub fn init(&self, clock: &AtomicU64, now: u64, max_stale: u64) -> io::Result<ScanReport> {
        fs::create_dir_all(&self.dir)?;
        for shard in 0..SHARDS {
            fs::create_dir_all(self.dir.join(format!("{:02x}", shard)))?;
        }
        let mut report = ScanReport::default();
        self.scan_root(now, max_stale, &mut report)?;
        {
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            for shard in 0..SHARDS {
                let dir = self.dir.join(format!("{:02x}", shard));
                self.scan_shard(&dir, &mut index, clock, now, max_stale, &mut report);
            }
        }
        self.prepare_ballast()?;
        self.ready.store(true, Ordering::Relaxed);
        Ok(report)
    }

    fn prepare_ballast(&self) -> io::Result<()> {
        let path = self.dir.join(BALLAST_FILE);
        let mut b = self.ballast.lock().unwrap_or_else(|p| p.into_inner());
        b.bytes = 0;
        self.ballast_bytes.store(0, Ordering::Relaxed);
        if self.reserve {
            // 前回分は空にして、予算計算のあとで改めて伸ばす
            b.file = Some(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&path)?,
            );
        } else {
            b.file = None;
            let _ = fs::remove_file(&path);
        }
        Ok(())
    }

    /// インデックス上のメタ情報 (`stored_at` は当てにしないこと)。
    pub fn lookup(&self, key: CacheKey) -> Option<Meta> {
        let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
        index.get(key).map(|e| e.meta)
    }

    pub fn touch(&self, key: CacheKey, seq: u64) {
        let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
        index.touch(key, seq);
    }

    /// ファイルを開いてヘッダーを読み、ワイヤバイト列の先頭に位置づけて返す。
    /// 有効期限は (再検証で延びている可能性があるので) インデックス側を採用する。
    pub fn open(&self, key: CacheKey) -> io::Result<Option<DiskHit>> {
        let Some(indexed) = self.lookup(key) else {
            return Ok(None);
        };
        let path = self.path_for(key, indexed.validators);
        let mut file = File::open(&path)?;
        let Some((mut meta, offset)) = format::read_header(&mut file)? else {
            return Ok(None);
        };
        meta.expires_at = meta.expires_at.max(indexed.expires_at);
        let size = file.metadata()?.len().saturating_sub(offset);
        Ok(Some(DiskHit { meta, file, size }))
    }

    /// ファイル全体を読む (小さなエントリ・テスト用)。
    pub fn read(&self, key: CacheKey) -> io::Result<Option<(Meta, Vec<u8>)>> {
        let Some(meta) = self.lookup(key) else {
            return Ok(None);
        };
        let path = self.path_for(key, meta.validators);
        let result = format::read_entry(&path);
        if let Ok(f) = File::open(&path) {
            sysinfo::drop_page_cache(&f);
        }
        result
    }

    pub fn remove(&self, key: CacheKey) {
        let removed = {
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            index.remove(key)
        };
        // インデックスに無くても両方の名前を試して消す
        let validators = removed.map(|e| e.meta.validators);
        for v in [true, false] {
            if validators.is_none_or(|x| x == v) {
                let _ = fs::remove_file(self.path_for(key, v));
            }
        }
    }

    /// 再検証に成功したので有効期限を延ばす (インデックスと mtime)。
    pub fn refresh(&self, key: CacheKey, expires_at: u64, seq: u64) -> bool {
        let validators = {
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            let Some(e) = index.get_mut(key) else {
                return false;
            };
            e.meta.expires_at = expires_at;
            let validators = e.meta.validators;
            index.touch(key, seq);
            validators
        };
        scan::set_expiry_mtime(&self.path_for(key, validators), expires_at);
        true
    }

    /// ストリーミング書き込みを開始する。収まらない・準備できていない場合は `Ok(None)`。
    pub fn begin(
        &self,
        key: CacheKey,
        url: &str,
        meta: Meta,
        seq: u64,
        expected: Option<u64>,
        max_object: u64,
    ) -> io::Result<Option<DiskWriter<'_>>> {
        if !self.is_ready() {
            return Ok(None);
        }
        match DiskWriter::open(self, key, url, meta, seq, expected, max_object) {
            Ok(w) => Ok(Some(w)),
            Err(e) if e.kind() == io::ErrorKind::FileTooLarge => Ok(None),
            Err(e) => self.note_io(Err(e)),
        }
    }

    /// 一括書き込み (小さなエントリ・テスト用)。
    pub fn write(
        &self,
        key: CacheKey,
        url: &str,
        data: &[u8],
        meta: Meta,
        seq: u64,
    ) -> io::Result<WriteOutcome> {
        let Some(mut w) = self.begin(key, url, meta, seq, Some(data.len() as u64), u64::MAX)?
        else {
            return Ok(WriteOutcome::default());
        };
        w.write(data)?;
        w.finish()
    }

    /// `extra` バイトを追加しても上限に収まるよう、バラスト → LRU の順に空ける。追い出し件数を返す。
    pub fn make_room(&self, extra: u64) -> usize {
        let capacity = self.capacity();
        let over = self
            .usage()
            .0
            .saturating_add(extra)
            .saturating_add(self.ballast_bytes())
            .saturating_sub(capacity);
        if over > 0 {
            self.shrink_ballast(over);
        }
        let mut evicted = 0;
        loop {
            let victim = {
                let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
                if index.bytes().saturating_add(extra) <= capacity {
                    break;
                }
                index.pop_lru()
            };
            let Some((key, e)) = victim else {
                break;
            };
            let _ = fs::remove_file(self.path_for(key, e.meta.validators));
            evicted += 1;
            log_debug!(None, "cache L2 EVICT key={} freed={}B", key, e.size);
        }
        evicted
    }

    pub fn enforce(&self) -> usize {
        self.make_room(0)
    }

    fn shrink_ballast(&self, by: u64) {
        let mut b = self.ballast.lock().unwrap_or_else(|p| p.into_inner());
        if b.bytes == 0 {
            return;
        }
        let new_len = b.bytes.saturating_sub(by);
        match &b.file {
            Some(f) => match f.set_len(new_len) {
                Ok(()) => b.bytes = new_len,
                Err(e) => log_warn!(None, "disk reservation shrink failed: {}", e),
            },
            None => b.bytes = 0,
        }
        self.ballast_bytes.store(b.bytes, Ordering::Relaxed);
    }

    /// 予算の未使用分をバラストファイルで埋める。戻り値は追加したバイト数。
    pub fn fill_ballast(&self) -> u64 {
        if !self.reserve || !self.is_ready() {
            return 0;
        }
        let target = self.capacity().saturating_sub(self.usage().0);
        let mut b = self.ballast.lock().unwrap_or_else(|p| p.into_inner());
        let Ballast {
            file,
            bytes,
            supported,
        } = &mut *b;
        let Some(file) = file.as_ref().filter(|_| *supported) else {
            return 0;
        };
        let mut added = 0u64;
        while bytes.saturating_add(BALLAST_STEP) <= target {
            let step = (target - *bytes).min(BALLAST_GROW_MAX);
            match sysinfo::preallocate(file, *bytes, step) {
                Ok(()) => {
                    *bytes += step;
                    added += step;
                }
                Err(e) if sysinfo::is_unsupported(&e) => {
                    *supported = false;
                    let _ = file.set_len(0);
                    *bytes = 0;
                    log_warn!(
                        None,
                        "disk reservation unavailable on {}: {} (using limit only)",
                        self.dir.display(),
                        e
                    );
                    break;
                }
                Err(e) => {
                    if e.raw_os_error() == Some(ENOSPC) {
                        self.enospc.fetch_add(1, Ordering::Relaxed);
                    }
                    log_debug!(None, "disk reservation paused: {}", e);
                    break;
                }
            }
        }
        self.ballast_bytes.store(*bytes, Ordering::Relaxed);
        added
    }

    /// 期限切れで再検証できない、または期限から `max_stale` 秒以上経ったエントリを削除する。
    pub fn sweep(&self, now: u64, max_stale: u64) -> usize {
        let keys = {
            let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            index.keys_where(|e| is_garbage(&e.meta, now, max_stale))
        };
        for key in &keys {
            self.remove(*key);
        }
        keys.len()
    }

    /// バラストを手放す (終了時)。
    pub fn shutdown(&self) {
        let mut b = self.ballast.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(f) = b.file.take() {
            let _ = f.set_len(0);
        }
        b.bytes = 0;
        self.ballast_bytes.store(0, Ordering::Relaxed);
        let _ = fs::remove_file(self.dir.join(BALLAST_FILE));
    }
}
