//! 固定サイズの状態ファイル (`$HOME/.rust-http-proxy.rrd`)。
//!
//! 起動時に決めた大きさで確保し、以後は伸びない。中は固定長レコードの領域の並びで、
//! 履歴は環状 (古いものを上書き)、統計表は固定スロット。レコードごとに CRC-32 を持ち、
//! 途中で落ちて壊れたレコードは読み飛ばす。ヘッダにカーソルは持たず、各レコードの時刻から
//! 復元する (更新が 1 回の書込で済み、順序の問題が無い)。
//!
//! 領域の並びと大きさは [`Layout`] で決める。**版 2 のファイルは版 3 の形に詰め直して
//! 読み戻す** ([`Rrd::open`]。T14.14 — 統計を捨てない)。版 1 以前と壊れたファイルは
//! 今までどおり読み捨てて作り直す。
//!
//! T.34.1 で割った: 割り付けの数は `layout`、ペイロードの符号は `codec`、環は `ring`、
//! CRC は `crc`。ここに残るのは版の印と、固定長ファイルの読み書き ([`Fixed`] / [`Rrd`])。
//! **外から見える名前は全部ここから引ける** (`crate::rrd::Enc` のような書き方は割る前と同じに通る)。

mod codec;
pub mod crc;
mod layout;
pub mod ring;

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

pub use codec::{Dec, Enc};
pub use crc::crc32;
pub use layout::{
    BASELINE_RECORD, FILE_SIZE, Layout, OVERRIDE_RECORD, OVERRIDE_SLOTS, REGIONS, Region,
    SAMPLE_RECORD, STATS_RECORD, STATS_SLOTS,
};

/// ファイル先頭の識別子。レイアウトを変えたら末尾の版を上げる。
///
/// - 版 2 (T12.4): ホスト別の区間を 24 段に、履歴の標本を 62 項目に増やした。
/// - 版 3 (T14.14): 標本のレコードを 512 → 1,024 B、統計のスロットを 576 → 640 B、
///   ファイルを 4 → 8 MiB にして**予備を持たせた**。
///
/// **版 3 のまま領域を 1 つ足した** (`baseline`。T24.4)。足したのは領域の並びの**末尾**で、
/// 8 MiB の余白の中に収まるので前の領域のオフセットは 1 バイトも動かない。古い版 3 の
/// ファイルではその場所がゼロ = [`Layout::baseline`] は空として読める (版を上げない)。
///
/// **版 2 のファイルは読み戻して版 3 の形に詰め直す** ([`Rrd::open`]) ので、
/// 通算の統計は消えない。版 1 以前・短いファイル・先頭が壊れたファイルは
/// 今までどおり読み捨てて作り直す (そこまで面倒を見ると変換の道が増えるだけで、
/// 版 1 はデプロイ先にもう無い)。
const MAGIC: &[u8; 8] = b"SHPRRD03";
/// 変換元の版の識別子 (T12.4 の版 2)。**これ 1 つだけ**を変換の入口にする。
const MAGIC_V2: &[u8; 8] = b"SHPRRD02";

/// いまの版の番号 (`/status` の `state_file.version`)。印の末尾と食い違わないこと。
pub const VERSION: u32 = 3;
/// 変換元の版の番号 (`state_file.converted_from`)。
pub const VERSION_V2: u32 = 2;
const _: () = assert!(MAGIC[6] == b'0' + (VERSION / 10) as u8);
const _: () = assert!(MAGIC[7] == b'0' + (VERSION % 10) as u8);
const _: () = assert!(MAGIC_V2[7] == b'0' + (VERSION_V2 % 10) as u8);

/// 開いた固定長ファイル 1 本。読み書きはオフセット指定で、共有参照から行える。
///
/// 統計の `.rrd` ([`Rrd`]) も個票の `.recent` (T14.9) もこれを共有する: どちらも
/// 「**先頭 8 バイトの版の印 + 固定長レコードの領域の並び**」で、違うのは識別子と
/// 割り付けだけなので、伸びないことも CRC もここ 1 か所で面倒を見る。
pub struct Fixed {
    file: File,
    /// ファイルの大きさ (固定。以後は伸びない)
    pub total: u64,
}

impl Fixed {
    /// 開く。無い・小さい・識別子が違うなら作り直す (ゼロ埋めで実サイズを確保)。
    /// 戻り値の `bool` は「作り直した」。
    pub fn open(path: &Path, magic: &[u8; 8], total: u64) -> io::Result<(Fixed, bool)> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let mut head = [0u8; 8];
        let ok = file.metadata()?.len() >= total
            && file.read_exact_at(&mut head, 0).is_ok()
            && &head == magic;
        if ok {
            return Ok((Fixed { file, total }, false));
        }
        file.set_len(0)?;
        let zeros = vec![0u8; 64 * 1024];
        let mut off = 0u64;
        while off < total {
            let n = (total - off).min(zeros.len() as u64) as usize;
            file.write_all_at(&zeros[..n], off)?;
            off += n as u64;
        }
        file.write_all_at(magic, 0)?;
        file.sync_all()?;
        Ok((Fixed { file, total }, true))
    }

    /// **作り直さずに**開く (古い版を読み戻すため。T14.14)。版の印か大きさが
    /// 合わなければ `None` — 呼び出し側は今までどおり [`Fixed::open`] で作り直す。
    pub fn open_existing(path: &Path, magic: &[u8; 8], total: u64) -> io::Result<Option<Fixed>> {
        let file = match OpenOptions::new().read(true).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut head = [0u8; 8];
        if file.metadata()?.len() < total
            || file.read_exact_at(&mut head, 0).is_err()
            || &head != magic
        {
            return Ok(None);
        }
        Ok(Some(Fixed { file, total }))
    }

    /// 版の印を書き直す (`[0u8; 8]` を渡すと「壊れている」ファイルになる)。
    /// 変換の途中で落ちたファイルを次の起動が捨てられるよう、**印は最後に書く**
    /// ため (T14.14)。書いたら `fsync` する。
    fn set_magic(&self, magic: &[u8; 8]) -> io::Result<()> {
        self.file.write_all_at(magic, 0)?;
        self.file.sync_all()
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

    /// レコードを消す (ゼロ埋め; CRC も合わなくなる)。
    pub fn clear(&self, region: Region, idx: usize) -> io::Result<()> {
        let rec = vec![0u8; region.record_size];
        self.file
            .write_all_at(&rec, region.offset + (idx * region.record_size) as u64)
    }
}

/// 開いた状態ファイル (`$HOME/.rust-http-proxy.rrd`)。[`Fixed`] に [`Layout`] を添えたもの。
pub struct Rrd {
    inner: Fixed,
    pub layout: Layout,
}

/// 状態ファイルを開いた結果 (T14.14)。`/status` の `state_file` に出す。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Opened {
    /// 作り直した (無い・短い・版 1 以前・壊れている)。**変換したときは `false`**
    pub created: bool,
    /// 読み戻して詰め直した元の版 (変換していなければ `None`)
    pub converted_from: Option<u32>,
    /// その変換にかかった時間 (ms。起動時 1 回だけ)
    pub convert_ms: u64,
}

impl Rrd {
    /// 開く。無い・小さい・識別子が違う (= 版が違う) なら作り直す。
    /// **版 2 のファイルだけは中身を読み戻し、版 3 の形に詰め直して書き戻す**
    /// (T14.14。通算の統計を捨てないため)。
    pub fn open(path: &Path) -> io::Result<(Rrd, Opened)> {
        let layout = Layout::current();
        let started = std::time::Instant::now();
        // 版 2 なら、作り直す前に領域ごとに読んでおく (ファイルはこのあと 8 MiB に
        // 作り直されるので、退避を残さずその場で書き換えられる)
        let old = read_v2(path);
        let (inner, created) = Fixed::open(path, MAGIC, layout.total)?;
        let rrd = Rrd { inner, layout };
        let Some(old) = old else {
            return Ok((
                rrd,
                Opened {
                    created,
                    ..Opened::default()
                },
            ));
        };
        // 印をいったん消してから書く: 途中で落ちたファイルは次の起動で
        // 「壊れている」= 作り直す になる (中途半端に混ざった版は残さない)
        rrd.inner.set_magic(&[0u8; 8])?;
        for (region, recs) in layout.regions().iter().zip(old.iter()) {
            for (idx, payload) in recs {
                rrd.write(*region, *idx, payload)?;
            }
        }
        rrd.inner.set_magic(MAGIC)?;
        Ok((
            rrd,
            Opened {
                created: false,
                converted_from: Some(VERSION_V2),
                convert_ms: started.elapsed().as_millis() as u64,
            },
        ))
    }
}

/// 領域 1 つぶんの `(添字, ペイロード)` の列 (変換の途中で持つもの)。
type Records = Vec<(usize, Vec<u8>)>;

/// 版 2 のファイルなら領域ごとの `(添字, ペイロード)` を返す (それ以外は `None`)。
///
/// 読めない・壊れている・別の版はすべて `None` = 「今までどおり作り直す」。
/// **中身は 1 バイトも読み替えない**: どちらの版のレコードも「固定の順 + 予備」で、
/// 版 3 のレコードは版 2 より長いだけなので、**末尾がゼロで伸びれば**そのまま
/// 同じ意味になる (足りない欄を 0 にするのは [`Dec`] の仕事)。
fn read_v2(path: &Path) -> Option<[Records; REGIONS]> {
    let layout = Layout::v2();
    let f = Fixed::open_existing(path, MAGIC_V2, layout.total)
        .ok()
        .flatten()?;
    let mut out: [Records; REGIONS] = Default::default();
    for (slot, region) in out.iter_mut().zip(layout.regions()) {
        *slot = f.read_all(region).ok()?;
    }
    Some(out)
}

/// `rrd.write(..)` `Ring::load(&rrd, ..)` をそのまま通すため (中身は [`Fixed`])。
impl std::ops::Deref for Rrd {
    type Target = Fixed;
    fn deref(&self) -> &Fixed {
        &self.inner
    }
}

#[cfg(test)]
mod tests;
