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

pub mod crc;
pub mod ring;

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

pub use crc::crc32;

/// ファイル先頭の識別子。レイアウトを変えたら末尾の版を上げる。
///
/// - 版 2 (T12.4): ホスト別の区間を 24 段に、履歴の標本を 62 項目に増やした。
/// - 版 3 (T14.14): 標本のレコードを 512 → 1,024 B、統計のスロットを 576 → 640 B、
///   ファイルを 4 → 8 MiB にして**予備を持たせた**。
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

const HEADER_SIZE: u64 = 4096;

/// ファイルの大きさ (固定)。**大事なのは伸びないこと**なので、領域の合計ではなく
/// 切りのよい 8 MiB に固定し、残りは次に項目が増えたときのための余白にする
/// (版を上げると変換の道を 1 本増やすことになるので、余白があるほど上げずに済む)。
///
/// 版 1 は約 1 MiB、版 2 は 4 MiB だった。版 3 (T14.14) は履歴 2,880 標本 × 1,024 B =
/// 2.81 MiB と統計 2,000 行 × 640 B = 1.22 MiB で、**4 MiB には入らない**
/// (実測 4,274,176 B)。
pub const FILE_SIZE: u64 = 8 * 1024 * 1024;

/// 版 2 の大きさとレコード長 (**変換のためだけに残してある**。T14.14)。
/// 領域の数・並び・本数は版 3 と同じなので、違うのはこの 3 つだけ。
const V2_FILE_SIZE: u64 = 4 * 1024 * 1024;
const V2_SAMPLE_RECORD: usize = 512;
const V2_STATS_RECORD: usize = 576;
/// 変換は「**末尾をゼロで伸ばすだけ**」なので、版 3 のレコードは版 2 以上であること。
const _: () = assert!(V2_SAMPLE_RECORD <= SAMPLE_RECORD);
const _: () = assert!(V2_STATS_RECORD <= STATS_RECORD);
const _: () = assert!(V2_FILE_SIZE <= FILE_SIZE);

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
    /// 領域が実際に使っている大きさ (末尾は余白)
    pub used: u64,
    /// ファイルの大きさ ([`FILE_SIZE`] 固定)
    pub total: u64,
}

/// 履歴 1 標本のレコード長。**版 3 で 512 → 1,024 B** (T14.14)。
/// いまの中身は 63 項目 × 8 B = 504 B + CRC 4 B で、**予備は 516 B (64 項目ぶん)**。
/// 標本の欄は「固定の順 + 予備」なので、以後は末尾に足すだけで版は上がらない
/// (`proxy_metrics::history::Sample` の `encode` / `decode`。読むときは無ければ 0)。
pub const SAMPLE_RECORD: usize = 1024;
/// ホスト別 / 接続元別 1 行のレコード長。名前 128 B + 項目 × 8 B + CRC 4 B。
/// 項目は 55 個 (T14.26 の `bytes_in` / `bytes_out` まで) で 568 B。**版 3 で
/// 576 → 640 B にして余白を 4 → 68 B に広げた** (T14.14。8 項目足してもまだ残る)。
pub const STATS_RECORD: usize = 640;
pub const OVERRIDE_RECORD: usize = 160;
pub const STATS_SLOTS: usize = 1000;
pub const OVERRIDE_SLOTS: usize = 256;

/// 領域の数 ([`Layout::regions`] の並び。版をまたいで突き合わせるのに使う)。
pub const REGIONS: usize = 6;

impl Layout {
    pub const fn current() -> Layout {
        Layout::of(SAMPLE_RECORD, STATS_RECORD, FILE_SIZE)
    }

    /// 版 2 (T12.4) の割り付け。**変換のためだけに残してある** (T14.14)。
    /// 領域の並びと本数は版 3 と同じで、違うのはレコード長とファイルの大きさだけ。
    const fn v2() -> Layout {
        Layout::of(V2_SAMPLE_RECORD, V2_STATS_RECORD, V2_FILE_SIZE)
    }

    /// レコード長とファイルの大きさを与えて割り付けを組む (版ごとの違いはここだけ)。
    const fn of(sample_record: usize, stats_record: usize, total: u64) -> Layout {
        let mut off = HEADER_SIZE;
        let history_fine = Region {
            offset: off,
            record_size: sample_record,
            count: 720,
        };
        off += history_fine.bytes();
        let history_minute = Region {
            offset: off,
            record_size: sample_record,
            count: 1440,
        };
        off += history_minute.bytes();
        let history_hour = Region {
            offset: off,
            record_size: sample_record,
            count: 720,
        };
        off += history_hour.bytes();
        let hosts = Region {
            offset: off,
            record_size: stats_record,
            count: STATS_SLOTS,
        };
        off += hosts.bytes();
        let clients = Region {
            offset: off,
            record_size: stats_record,
            count: STATS_SLOTS,
        };
        off += clients.bytes();
        let overrides = Region {
            offset: off,
            record_size: OVERRIDE_RECORD,
            count: OVERRIDE_SLOTS,
        };
        off += overrides.bytes();
        // 領域の合計 (`off`) はファイルに収まっていること。収まらない版を書いたら
        // ここで組み立てが止まる (実行時に気付くより早い)
        assert!(off <= total);
        Layout {
            history_fine,
            history_minute,
            history_hour,
            hosts,
            clients,
            overrides,
            used: off,
            total,
        }
    }

    /// 全領域を**割り付けの順**に返す (変換で版 2 と 1 対 1 に突き合わせるため)。
    pub const fn regions(&self) -> [Region; REGIONS] {
        [
            self.history_fine,
            self.history_minute,
            self.history_hour,
            self.hosts,
            self.clients,
            self.overrides,
        ]
    }
}

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
        let (rrd, opened) = Rrd::open(&path).unwrap();
        assert!(opened.created);
        assert_eq!(opened.converted_from, None, "新規は変換ではない");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), rrd.layout.total);
        assert_eq!(rrd.layout.total, FILE_SIZE, "8 MiB 固定");
        assert_eq!(FILE_SIZE, 8_388_608);
        assert!(
            rrd.layout.used <= rrd.layout.total,
            "領域が余白に食い込まない"
        );
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
        let (rrd2, opened) = Rrd::open(&path).unwrap();
        assert!(!opened.created);
        assert_eq!(rrd2.read_all(r).unwrap().len(), 2);
        rrd2.clear(r, 3).unwrap();
        assert_eq!(rrd2.read_all(r).unwrap().len(), 1);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), rrd.layout.total);
        let _ = std::fs::remove_file(&path);
    }

    /// 版 2 のファイルを 1 本作る (領域の `idx` 番目に `payload` を置く)。
    fn write_v2(path: &std::path::Path, recs: &[(usize, usize, Vec<u8>)]) {
        let v2 = Layout::v2();
        let (f, created) = Fixed::open(path, MAGIC_V2, v2.total).unwrap();
        assert!(created);
        for (region, idx, payload) in recs {
            f.write(v2.regions()[*region], *idx, payload).unwrap();
        }
    }

    #[test]
    fn a_version_two_file_is_converted_in_place_and_keeps_every_record() {
        let path = tmp("convert");
        // 履歴 (環状の領域 0) 3 本、ホスト 1 行、上書き 1 件を版 2 の形で置く
        let sample = |t: u64| {
            let mut e = Enc::new();
            e.u64(t).u64(t * 100);
            e.0
        };
        let host = {
            let mut e = Enc::new();
            e.str("connect://fixture.invalid:443", 128).u64(7);
            e.0
        };
        let over = {
            let mut e = Enc::new();
            e.u64(1).str("blocked.invalid", 128);
            e.0
        };
        write_v2(
            &path,
            &[
                (0, 5, sample(1_700_000_005)),
                (0, 6, sample(1_700_000_010)),
                (0, 7, sample(1_700_000_015)),
                (3, 0, host),
                (5, 2, over),
            ],
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), V2_FILE_SIZE);

        let (rrd, opened) = Rrd::open(&path).unwrap();
        assert_eq!(opened.converted_from, Some(2));
        assert!(!opened.created, "変換したファイルは「作り直した」ではない");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
        // 中身は添字ごとそのまま (末尾がゼロで伸びただけ)
        let hosts = rrd.read_all(rrd.layout.hosts).unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].0, 0);
        assert_eq!(
            hosts[0].1.len(),
            STATS_RECORD - 4,
            "版 3 の長さに伸びている"
        );
        let mut d = Dec(&hosts[0].1);
        assert_eq!(d.str(128), "connect://fixture.invalid:443");
        assert_eq!(d.u64(), 7);
        assert_eq!(d.u64(), 0, "予備はゼロで読める");
        // 環状の領域は時刻の順に戻り、続きから書ける
        let (mut ring, got) = ring::Ring::load(&rrd, rrd.layout.history_fine).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(Dec(&got[0]).u64(), 1_700_000_005);
        assert_eq!(Dec(&got[2]).u64(), 1_700_000_015);
        ring.push(&rrd, &sample(1_700_000_020)).unwrap();
        let (_, got) = ring::Ring::load(&rrd, rrd.layout.history_fine).unwrap();
        assert_eq!(got.len(), 4);
        assert_eq!(Dec(&got[3]).u64(), 1_700_000_020);
        assert_eq!(rrd.read_all(rrd.layout.overrides).unwrap().len(), 1);
        assert_eq!(rrd.read_all(rrd.layout.clients).unwrap().len(), 0);
        // 2 回目の起動は変換しない (もう版 3 なので読むだけ)
        drop(rrd);
        let (rrd, opened) = Rrd::open(&path).unwrap();
        assert_eq!(opened, Opened::default(), "変換は 1 回だけ");
        assert_eq!(rrd.read_all(rrd.layout.hosts).unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_half_written_conversion_is_thrown_away_on_the_next_start() {
        let path = tmp("halfway");
        let row = {
            let mut e = Enc::new();
            e.str("a.invalid", 128).u64(1);
            e.0
        };
        write_v2(&path, &[(3, 0, row)]);
        // 変換の途中で落ちた形 = 8 MiB に伸びていて版の印が無い
        {
            let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.set_len(FILE_SIZE).unwrap();
            std::os::unix::fs::FileExt::write_all_at(&f, &[0u8; 8], 0).unwrap();
        }
        let (rrd, opened) = Rrd::open(&path).unwrap();
        assert!(opened.created, "印の無いファイルは作り直す");
        assert_eq!(opened.converted_from, None);
        assert_eq!(rrd.read_all(rrd.layout.hosts).unwrap().len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_version_one_file_is_still_thrown_away() {
        let path = tmp("v1");
        let mut buf = vec![0u8; 1_064_960];
        buf[..8].copy_from_slice(b"SHPRRD01");
        std::fs::write(&path, &buf).unwrap();
        let (rrd, opened) = Rrd::open(&path).unwrap();
        assert!(opened.created);
        assert_eq!(opened.converted_from, None);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
        assert_eq!(rrd.layout.used, 4_274_176, "領域の合計 (余白は 4 MiB 強)");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_record_is_skipped() {
        let path = tmp("corrupt");
        let (rrd, _) = Rrd::open(&path).unwrap();
        let r = rrd.layout.history_fine;
        rrd.write(r, 5, &Enc::new().u64(7).0).unwrap();
        // 1 バイト壊す
        rrd.inner
            .file
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
