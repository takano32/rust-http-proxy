//! 状態ファイルの**割り付け** ([`Layout`] と領域 [`Region`]、レコード長と大きさの定数)。
//! 読み書きは `Fixed` / `Rrd` (`mod.rs`) の仕事で、ここは数だけを決める (I/O は無い)。

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
pub(super) const V2_FILE_SIZE: u64 = 4 * 1024 * 1024;
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
    /// 起動から 60 秒の heap の基準 1 本 (`{at, bytes}`。T24.4)。**足したのは末尾**
    pub baseline: Region,
    /// 領域が実際に使っている大きさ (末尾は余白)
    pub used: u64,
    /// ファイルの大きさ ([`FILE_SIZE`] 固定)
    pub total: u64,
}

/// 履歴 1 標本のレコード長。**版 3 で 512 → 1,024 B** (T14.14)。
/// 版 3 を始めたときの中身は 63 項目 × 8 B = 504 B + CRC 4 B で、**予備は 516 B (64 項目ぶん)**。
/// 標本の欄は「固定の順 + 予備」なので、以後は末尾に足すだけで版は上がらない
/// (`proxy_metrics::history::Sample` の `encode` / `decode`。読むときは無ければ 0)。
/// いまは T15.0 の 20・T16.0 の 3・T28.45 の 17 (「確立 − RTT」の窓 16 と
/// `dns_fail_misses`) を足して 103 項目 = 824 B、T29.12 の 16 (名前解決を除いた「確立 − RTT」の窓) で
/// 119 項目 = 952 B、T30.3 の 2 (443・80 以外の宛先への試みとそのエラー) で 121 項目 = 968 B、T32.16 の 1 (名前解決に失敗した試みが払った ms) で 122 項目 = 976 B、T33.3 の 2 (443・80 宛ての、名前解決に成功したミスの回数と ms) で 124 項目 = 992 B、予備は 3 項目ぶん。
/// **T28.45 より前のファイルもそのまま読める** (足した 17 項目は 0 = 空の窓で読み戻る)
pub const SAMPLE_RECORD: usize = 1024;
/// ホスト別 / 接続元別 1 行のレコード長。名前 128 B + 項目 × 8 B + CRC 4 B。
/// 項目は 55 個 (T14.26 の `bytes_in` / `bytes_out` まで) で 568 B。**版 3 で
/// 576 → 640 B にして余白を 4 → 68 B に広げた** (T14.14。8 項目足してもまだ残る)。
pub const STATS_RECORD: usize = 640;
pub const OVERRIDE_RECORD: usize = 160;
pub const STATS_SLOTS: usize = 1000;
pub const OVERRIDE_SLOTS: usize = 256;
/// heap の基準 1 本のレコード長 (`at` = epoch 秒 + `bytes` = バイト + CRC。T24.4)。
/// 中身は 2 × u64 = 16 B で、残りは予備 (あとで欄を足しても版を上げない)。
pub const BASELINE_RECORD: usize = 32;

/// 領域の数 ([`Layout::regions`] の並び。版をまたいで突き合わせるのに使う)。
pub const REGIONS: usize = 7;

impl Layout {
    pub const fn current() -> Layout {
        Layout::of(SAMPLE_RECORD, STATS_RECORD, FILE_SIZE)
    }

    /// 版 2 (T12.4) の割り付け。**変換のためだけに残してある** (T14.14)。
    /// 領域の並びと本数は版 3 と同じで、違うのはレコード長とファイルの大きさだけ。
    pub(super) const fn v2() -> Layout {
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
        // heap の基準 (T24.4)。**並びの末尾に足した**ので、前の 6 領域のオフセットは
        // 版 3 のまま (古いファイルではここが余白のゼロ = 空として読める)
        let baseline = Region {
            offset: off,
            record_size: BASELINE_RECORD,
            count: 1,
        };
        off += baseline.bytes();
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
            baseline,
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
            self.baseline,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 領域は先頭 4 KiB のあとに隙間なく並び、版 3 の合計は 4,274,208 B (余白は 4 MiB 弱)。
    #[test]
    fn the_current_layout_packs_the_regions_without_gaps() {
        let l = Layout::current();
        assert_eq!(l.total, FILE_SIZE);
        assert_eq!(l.used, 4_274_208);
        let regions = l.regions();
        assert_eq!(regions[0].offset, HEADER_SIZE);
        for w in regions.windows(2) {
            assert_eq!(
                w[0].offset + w[0].bytes(),
                w[1].offset,
                "{:?} → {:?}",
                w[0],
                w[1]
            );
        }
        let last = regions[REGIONS - 1];
        assert_eq!(last, l.baseline, "末尾は heap の基準");
        assert_eq!(last.offset + last.bytes(), l.used);
        assert_eq!(
            (
                l.history_fine.count,
                l.history_minute.count,
                l.history_hour.count
            ),
            (720, 1440, 720)
        );
        assert_eq!((l.hosts.count, l.clients.count), (STATS_SLOTS, STATS_SLOTS));
        assert_eq!(l.history_fine.record_size, SAMPLE_RECORD);
        assert_eq!(l.hosts.record_size, STATS_RECORD);
        assert_eq!(l.overrides.record_size, OVERRIDE_RECORD);
    }

    /// 版 2 の割り付けは領域の並びと本数が同じで、レコード長とファイルの大きさだけが違う。
    #[test]
    fn the_version_two_layout_differs_only_in_record_sizes_and_total() {
        let (v2, v3) = (Layout::v2(), Layout::current());
        assert_eq!(v2.total, 4 * 1024 * 1024);
        assert!(v2.used <= v2.total);
        for (a, b) in v2.regions().iter().zip(v3.regions()) {
            assert_eq!(a.count, b.count);
        }
        assert_eq!(v2.history_fine.record_size, 512);
        assert_eq!(v2.hosts.record_size, 576);
        assert_eq!(v2.overrides.record_size, v3.overrides.record_size);
    }

    /// 領域の大きさとペイロード (CRC の 4 B を引いたもの)。
    #[test]
    fn a_region_knows_its_bytes_and_its_payload() {
        let r = Region {
            offset: 4096,
            record_size: 160,
            count: 256,
        };
        assert_eq!(r.payload_size(), 156);
        assert_eq!(r.bytes(), 40_960);
    }
}
