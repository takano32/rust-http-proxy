//! ディスク層へのストリーミング書き込み。
//!
//! 本文全体を RAM に溜めず、届いたチャンクをそのまま一時ファイルへ書く。
//! 書き込み量が増えるにつれて `make_room` で場所を空け (バラスト縮小 → LRU 追い出し)、
//! 確保した分は確定するまで `in_flight` として容量計算に含めておく。完了時にペイロード長を
//! ヘッダーへ書き戻し、mtime を有効期限にして、索引ロックの下で本来のファイル名へ rename する。
//! 場所の確保は「今必要な分」だけ厳密に行い、空きがあるときだけ先読みして呼び出し回数を減らす。

use crate::sync::LockExt;
use std::fs::{self, File};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, UNIX_EPOCH};

use super::{DiskEntry, DiskTier, WriteOutcome};
use crate::cache::format::{self, Meta};
use crate::cache::key::CacheKey;
use crate::log_trace;
use crate::sysinfo;

/// 空きがあるときに先読みで確保しておく量 (書き込みごとに確保処理を走らせない)。
const ROOM_STEP: u64 = 16 * 1024 * 1024;
const BUF_SIZE: usize = 256 * 1024;

pub struct DiskWriter<'a> {
    tier: &'a DiskTier,
    key: CacheKey,
    path: PathBuf,
    tmp: PathBuf,
    file: Option<BufWriter<File>>,
    meta: Meta,
    header_len: u64,
    written: u64,
    /// 確保済みの容量 (tier.in_flight に計上されている)
    room: u64,
    max_object: u64,
    evicted: usize,
}

impl<'a> DiskWriter<'a> {
    pub(super) fn open(
        tier: &'a DiskTier,
        key: CacheKey,
        url: &str,
        meta: Meta,
        seq: u64,
        expected: Option<u64>,
        max_object: u64,
    ) -> io::Result<Self> {
        let header = format::header(url, &meta, 0);
        if header.len() + 64 > format::HEADER_MAX {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "URL too long for the cache header",
            ));
        }
        // 長さの分からない本文は予算の 1/4 までに抑え、1 本のダウンロードで L2 を空にしないようにする
        let max_object = if expected.is_none() {
            max_object.min(tier.entry_capacity() / 4)
        } else {
            max_object
        };
        let path = tier.path_for(key, meta.validators);
        let tmp = tier.shard_dir(key).join(format!("{}.{}.tmp", key, seq));
        let mut w = Self {
            tier,
            key,
            path,
            tmp: tmp.clone(),
            file: None,
            meta,
            header_len: header.len() as u64,
            written: 0,
            room: 0,
            max_object,
            evicted: 0,
        };
        // Content-Length が分かっていれば最初にまとめて場所を空ける
        w.ensure_room(expected.unwrap_or(0).saturating_add(header.len() as u64))?;
        let file = File::create(&tmp)?;
        w.file = Some(BufWriter::with_capacity(BUF_SIZE, file));
        w.write(&header)?;
        Ok(w)
    }

    /// `needed` バイトまで書けるよう場所を確保する。足りない (上限超過) なら Err。
    fn ensure_room(&mut self, needed: u64) -> io::Result<()> {
        if needed <= self.room {
            return Ok(());
        }
        if needed > self.max_object || needed > self.tier.entry_capacity() {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "object exceeds the disk cache limit",
            ));
        }
        let extra = needed - self.room;
        self.evicted += self.tier.make_room(extra);
        // 追い出しても入らない (他の書き込みが場所を取っている) なら諦める
        if self
            .tier
            .usage()
            .0
            .saturating_add(self.tier.in_flight_bytes())
            .saturating_add(extra)
            > self.tier.entry_capacity()
        {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "no room left in the disk cache",
            ));
        }
        self.tier.in_flight.fetch_add(extra, Ordering::Relaxed);
        self.room = needed;
        // 空きが残っていればその分だけ先読み (無ければ次のチャンクで厳密に空ける)
        let slack = self.tier.free_room().min(ROOM_STEP);
        if slack > 0 {
            self.tier.in_flight.fetch_add(slack, Ordering::Relaxed);
            self.room += slack;
        }
        Ok(())
    }

    fn release_room(&mut self) {
        if self.room > 0 {
            self.tier.in_flight.fetch_sub(self.room, Ordering::Relaxed);
            self.room = 0;
        }
    }

    pub fn write(&mut self, chunk: &[u8]) -> io::Result<()> {
        let next = self.written.saturating_add(chunk.len() as u64);
        self.ensure_room(next)?;
        let file = self.file.as_mut().expect("writer is open");
        self.tier.note_io(file.write_all(chunk))?;
        self.written = next;
        Ok(())
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// 書き込みを確定し、索引に登録する。`seq` は確定時点の LRU 順序。
    pub fn finish(mut self, seq: u64) -> io::Result<WriteOutcome> {
        let result = self.commit(seq);
        if result.is_err() {
            let _ = fs::remove_file(&self.tmp);
        }
        self.file = None;
        self.release_room();
        result
    }

    fn commit(&mut self, seq: u64) -> io::Result<WriteOutcome> {
        let mut w = self.file.take().expect("writer is open");
        self.tier.note_io(w.flush())?;
        let mut file = w.into_inner().map_err(|e| e.into_error())?;
        // ペイロード長を書き戻す (読むときに途中で切れたファイルを弾ける)
        let payload_len = self.written.saturating_sub(self.header_len);
        file.seek(SeekFrom::Start(format::len_field_offset(
            self.header_len as usize,
        )))?;
        self.tier
            .note_io(file.write_all(format::len_field(payload_len).as_bytes()))?;
        if let Some(t) = UNIX_EPOCH.checked_add(Duration::from_secs(self.meta.expires_at)) {
            let _ = file.set_modified(t);
        }
        sysinfo::drop_page_cache(&file);
        drop(file);

        // rename と索引更新は同じロックの下で行い、追い出しがこのファイルを消さないようにする
        {
            let mut index = self.tier.index.locked();
            fs::rename(&self.tmp, &self.path)?;
            let replaced = index.insert(self.key, DiskEntry::new(self.written, self.meta), seq);
            // バリデータの有無が変わると拡張子も変わるので、旧ファイルが孤児にならないよう消す
            if let Some(old) = &replaced
                && old.meta.validators != self.meta.validators
            {
                let _ = fs::remove_file(self.tier.path_for(self.key, old.meta.validators));
            }
            self.evicted += self.tier.enforce_entry_cap(&mut index);
        }
        log_trace!(
            None,
            "cache L2 wrote {} ({}B)",
            self.path.display(),
            self.written
        );
        Ok(WriteOutcome {
            stored: true,
            size: self.written,
            evicted: self.evicted,
        })
    }

    /// 途中で諦める (一時ファイルを消す)。
    pub fn abort(mut self) {
        self.file = None;
        let _ = fs::remove_file(&self.tmp);
        self.release_room();
    }
}

impl Drop for DiskWriter<'_> {
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = fs::remove_file(&self.tmp);
        }
        self.release_room();
    }
}
