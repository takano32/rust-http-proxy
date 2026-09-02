//! 固定サイズの状態ファイル (`$HOME/.sorahost-http-proxy.rrd`)。
//!
//! 起動時に決めた大きさで確保し、以後は伸びない。中は固定長レコードの領域の並びで、
//! 履歴は環状 (古いものを上書き)、統計表は固定スロット。レコードごとに CRC-32 を持ち、
//! 途中で落ちて壊れたレコードは読み飛ばす。ヘッダにカーソルは持たず、各レコードの時刻から
//! 復元する (更新が 1 回の書込で済み、順序の問題が無い)。
//!
//! 領域の並びと大きさは [`Layout`] で決め、版が変わったら作り直す (統計は捨てる)。

pub mod crc;
pub mod ring;

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

pub use crc::crc32;

/// ファイル先頭の識別子。レイアウトを変えたら末尾の版を上げる。
const MAGIC: &[u8; 8] = b"SHPRRD01";
const HEADER_SIZE: u64 = 4096;

/// 領域: 固定長レコード `count` 本。`record_size` には末尾の CRC (4 バイト) を含む。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub offset: u64,
    pub record_size: usize,
    pub count: usize,
}

impl Region {
    pub const fn payload_size(&self) -> usize {
        self.record_size - 4
    }

    pub const fn bytes(&self) -> u64 {
        self.record_size as u64 * self.count as u64
    }
}

/// 全領域の配置。
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    /// 5 秒 × 720 (1 時間)
    pub history_fine: Region,
    /// 1 分 × 1440 (1 日)
    pub history_minute: Region,
    /// 1 時間 × 720 (30 日)
    pub history_hour: Region,
    pub hosts: Region,
    pub clients: Region,
    pub overrides: Region,
    pub total: u64,
}

pub const SAMPLE_RECORD: usize = 128;
pub const STATS_RECORD: usize = 320;
pub const OVERRIDE_RECORD: usize = 160;
pub const STATS_SLOTS: usize = 1000;
pub const OVERRIDE_SLOTS: usize = 256;

impl Layout {
    pub const fn current() -> Layout {
        let mut off = HEADER_SIZE;
        let history_fine = Region {
            offset: off,
            record_size: SAMPLE_RECORD,
            count: 720,
        };
        off += history_fine.bytes();
        let history_minute = Region {
            offset: off,
            record_size: SAMPLE_RECORD,
            count: 1440,
        };
        off += history_minute.bytes();
        let history_hour = Region {
            offset: off,
            record_size: SAMPLE_RECORD,
            count: 720,
        };
        off += history_hour.bytes();
        let hosts = Region {
            offset: off,
            record_size: STATS_RECORD,
            count: STATS_SLOTS,
        };
        off += hosts.bytes();
        let clients = Region {
            offset: off,
            record_size: STATS_RECORD,
            count: STATS_SLOTS,
        };
        off += clients.bytes();
        let overrides = Region {
            offset: off,
            record_size: OVERRIDE_RECORD,
            count: OVERRIDE_SLOTS,
        };
        off += overrides.bytes();
        Layout {
            history_fine,
            history_minute,
            history_hour,
            hosts,
            clients,
            overrides,
            total: off,
        }
    }
}

/// 開いた状態ファイル。読み書きはオフセット指定で、共有参照から行える。
pub struct Rrd {
    file: File,
    pub layout: Layout,
}

impl Rrd {
    /// 開く。無い・小さい・識別子が違うなら作り直す (ゼロ埋めで実サイズを確保)。
    /// 戻り値の `bool` は「作り直した」。
    pub fn open(path: &Path) -> io::Result<(Rrd, bool)> {
        let layout = Layout::current();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let mut magic = [0u8; 8];
        let ok = file.metadata()?.len() >= layout.total
            && file.read_exact_at(&mut magic, 0).is_ok()
            && &magic == MAGIC;
        if ok {
            return Ok((Rrd { file, layout }, false));
        }
        file.set_len(0)?;
        let zeros = vec![0u8; 64 * 1024];
        let mut off = 0u64;
        while off < layout.total {
            let n = (layout.total - off).min(zeros.len() as u64) as usize;
            file.write_all_at(&zeros[..n], off)?;
            off += n as u64;
        }
        file.write_all_at(MAGIC, 0)?;
        file.sync_all()?;
        Ok((Rrd { file, layout }, true))
    }

    /// 領域の `idx` 番目に書く。`payload` は `payload_size()` 以下 (残りはゼロ埋め)。
    pub fn write(&self, region: Region, idx: usize, payload: &[u8]) -> io::Result<()> {
        debug_assert!(idx < region.count);
        debug_assert!(payload.len() <= region.payload_size());
        let mut rec = vec![0u8; region.record_size];
        rec[..payload.len()].copy_from_slice(payload);
        let crc = crc32(&rec[..region.payload_size()]);
        rec[region.payload_size()..].copy_from_slice(&crc.to_le_bytes());
        self.file
            .write_all_at(&rec, region.offset + (idx * region.record_size) as u64)
    }

    /// 領域の全レコードを読み、CRC が合うものだけ `(idx, payload)` で返す。
    pub fn read_all(&self, region: Region) -> io::Result<Vec<(usize, Vec<u8>)>> {
        let mut buf = vec![0u8; region.bytes() as usize];
        self.file.read_exact_at(&mut buf, region.offset)?;
        let mut out = Vec::new();
        for (idx, rec) in buf.chunks_exact(region.record_size).enumerate() {
            let payload = &rec[..region.payload_size()];
            let stored = u32::from_le_bytes(rec[region.payload_size()..].try_into().unwrap());
            if stored == crc32(payload) && !payload.iter().all(|&b| b == 0) {
                out.push((idx, payload.to_vec()));
            }
        }
        Ok(out)
    }

    /// `idx` 番目が空 (全ゼロ) か。
    pub fn read_all_is_empty_at(&self, region: Region, idx: usize) -> bool {
        let mut rec = vec![0u8; region.record_size];
        match self
            .file
            .read_exact_at(&mut rec, region.offset + (idx * region.record_size) as u64)
        {
            Ok(()) => rec.iter().all(|&b| b == 0),
            Err(_) => true,
        }
    }

    /// レコードを消す (ゼロ埋め; CRC も合わなくなる)。
    pub fn clear(&self, region: Region, idx: usize) -> io::Result<()> {
        let rec = vec![0u8; region.record_size];
        self.file
            .write_all_at(&rec, region.offset + (idx * region.record_size) as u64)
    }
}

/// 固定長ペイロードの組み立て (u64 はリトルエンディアン、文字列は長さ 1 バイト + 本体)。
pub struct Enc(pub Vec<u8>);

impl Enc {
    pub fn new() -> Self {
        Enc(Vec::with_capacity(STATS_RECORD))
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// 固定幅 `width` の文字列 (先頭 1 バイトが長さ、超える分は切る)。
    pub fn str(&mut self, s: &str, width: usize) -> &mut Self {
        let max = width - 1;
        let mut end = s.len().min(max);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.0.push(end as u8);
        self.0.extend_from_slice(&s.as_bytes()[..end]);
        self.0.resize(self.0.len() + (max - end), 0);
        self
    }
}

impl Default for Enc {
    fn default() -> Self {
        Self::new()
    }
}

/// [`Enc`] の逆。足りなければ 0 / 空文字列。
pub struct Dec<'a>(pub &'a [u8]);

impl Dec<'_> {
    pub fn u64(&mut self) -> u64 {
        if self.0.len() < 8 {
            self.0 = &[];
            return 0;
        }
        let v = u64::from_le_bytes(self.0[..8].try_into().unwrap());
        self.0 = &self.0[8..];
        v
    }
    pub fn str(&mut self, width: usize) -> String {
        if self.0.len() < width {
            self.0 = &[];
            return String::new();
        }
        let len = (self.0[0] as usize).min(width - 1);
        let s = String::from_utf8_lossy(&self.0[1..1 + len]).into_owned();
        self.0 = &self.0[width..];
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("shp-rrd-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn creates_fixed_size_file_and_round_trips_records() {
        let path = tmp("basic");
        let (rrd, created) = Rrd::open(&path).unwrap();
        assert!(created);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), rrd.layout.total);
        assert!(rrd.layout.total < 2 * 1024 * 1024, "about 1 MiB");
        let r = rrd.layout.overrides;
        assert!(rrd.read_all(r).unwrap().is_empty());
        let mut e = Enc::new();
        e.u64(42).str("ads.example.com", 128);
        rrd.write(r, 3, &e.0).unwrap();
        rrd.write(r, 0, &Enc::new().u64(1).str("a", 128).0).unwrap();
        let all = rrd.read_all(r).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].0, 3);
        let mut d = Dec(&all[1].1);
        assert_eq!(d.u64(), 42);
        assert_eq!(d.str(128), "ads.example.com");
        // 再オープンで残っている / サイズも変わらない
        let (rrd2, created) = Rrd::open(&path).unwrap();
        assert!(!created);
        assert_eq!(rrd2.read_all(r).unwrap().len(), 2);
        rrd2.clear(r, 3).unwrap();
        assert_eq!(rrd2.read_all(r).unwrap().len(), 1);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), rrd.layout.total);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_record_is_skipped() {
        let path = tmp("corrupt");
        let (rrd, _) = Rrd::open(&path).unwrap();
        let r = rrd.layout.history_fine;
        rrd.write(r, 5, &Enc::new().u64(7).0).unwrap();
        // 1 バイト壊す
        rrd.file
            .write_all_at(&[0xFF], r.offset + (5 * r.record_size) as u64 + 3)
            .unwrap();
        assert!(rrd.read_all(r).unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn strings_are_truncated_on_char_boundaries() {
        let mut e = Enc::new();
        e.str(&"あ".repeat(100), 16);
        assert_eq!(e.0.len(), 16);
        let mut d = Dec(&e.0);
        assert_eq!(d.str(16), "あ".repeat(5));
    }
}
