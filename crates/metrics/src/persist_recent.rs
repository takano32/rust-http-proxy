//! 個票の永続化 (`$HOME/.rust-http-proxy.recent`。T14.9)。
//!
//! [`crate::recent`] のリングはプロセスのメモリだけなので、**再デプロイのたびに直前の
//! 個票が全部消える**。デプロイ後の様子見 (T14.99) は「前の版で何が起きていたか」も
//! 読みたいので、`.rrd` とは**別の固定長ファイル**に落とす
//! (`.rrd` の版を上げると統計を捨てることになるため、あちらは触らない)。
//!
//! - 大きさは [`FILE_SIZE`] (4 MiB) 固定。中は [`crate::rrd::ring::Ring`] を流用した
//!   4 本の環状の領域で、いっぱいになったら最古を上書きする (**伸びない**)。
//! - 書くのは **history スレッドの 5 秒の周期だけ**。各リングの「まだ書いていない件」
//!   ([`crate::recent::RecentRing::take_unwritten`]) を追記する。
//!   **接続の経路は今までどおりメモリのリングに書くだけで、1 命令も増えない。**
//! - レコードの先頭 8 バイトは**通し番号** (時刻ではない): 同じ秒に閉じた接続が並んでも
//!   書いた順に戻せるようにするため。0 は「空」の印なので 1 から始める。
//! - 版の印 ([`MAGIC`]) が違うファイル・短いファイル・先頭が壊れたファイルは
//!   **読み捨てて作り直す** (`.rrd` と同じ方針。個票は運用の参考値)。
//! - `PROXY_STATS_PERSIST=off` では [`crate::persist::Store`] ごと作らないので、
//!   このファイルも作らない (`/recent` などの `"persisted"` が `false` になる)。

use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::events::{Event, EventKind, MAX_EVENTS, MAX_TEXT};
use crate::log::{Level, Line};
use crate::metrics::Metrics;
use crate::recent::{
    BurstShot, CONN_STATES, CloseReason, EntryCause, EntryKind, ErrorEntry, MAX_BURSTS, MAX_ERRORS,
    MAX_RECENT, MAX_SHOT_CLIENTS, MAX_SHOT_TARGETS, MAX_SNI, RecentEntry, SIDES, STAGES,
};
use crate::rrd::ring::Ring;
use crate::rrd::{Dec, Enc, Fixed, Region};
use crate::sync::LockExt;
use crate::{log_info, log_warn};

// レコードの中身 (固定幅の欄・組み立て時の大きさの検査・`encode_*` / `decode_*`) は割ってある (T34.29)
mod codec;

use codec::{
    decode_closed, decode_error, decode_event, decode_log, decode_shot, encode_closed,
    encode_error, encode_event, encode_log, encode_shot,
};

/// 版の印。**レコードの並びを変えたら末尾を上げる** (古いファイルは読み捨てて作り直す)。
///
/// 版 3 = T14.55。T14.46 が閉じた接続のレコードの**数値の途中** (`reason` の直後、
/// `stage_ms` の前) に `syn_retrans` を差し込んだのに版 2 のままだったので、それより前に
/// 書いたファイルは `stage_ms` 以降が 1 つずれて読めていた。**捨てるだけ**で実害は無い
/// (`.rrd` とは独立したファイルで、統計はそちらに残る)。
pub const MAGIC: &[u8; 8] = b"SHPREC03";

/// ファイルの大きさ (固定 4 MiB)。`.rrd` と同じで、以後は 1 バイトも伸びない。
pub const FILE_SIZE: u64 = 4 * 1024 * 1024;

/// 先頭の版の印を置く場所 (`.rrd` と同じ 4 KiB)。
const HEADER_SIZE: u64 = 4096;

/// 個票 1 件のレコード長 (末尾 4 B が CRC)。エラーとログはこちら。
const SMALL_RECORD: usize = 256;
/// 閉じた接続 1 件のレコード長。**256 B ではなく 512 B** なのは、段階の ms 6 つと
/// T14.5 の RTT / 再送 (両側) まで入れると 256 B に収まらないため
/// (いまの中身は 392 B = T14.46 の `syn_retrans`・T14.42 の `stall_ms`・T15.0 (4) の `spins` と半閉じ 2 本 込み、余白 116 B。領域の大きさは 2 MiB のままで、件数が 8,192 → 4,096 になる)。
const CLOSED_RECORD: usize = 512;
/// 山の写真 1 枚のレコード長 (接続元 16 + 宛先 10 の名前が入る)。
const SHOT_RECORD: usize = 4096;

/// 領域ごとのレコード数。**合計がちょうど [`FILE_SIZE`]** になるように選んである
/// (T14.9 の割り付け: ヘッダー 4 KiB、閉じた接続 2 MiB、エラー 512 KiB、写真 512 KiB、
/// ログ 1,020 KiB)。写真が 256 枚ではなく 128 枚なのは 4 MiB に収めるため、
/// ログが 4,096 行ではなく 4,080 行なのは**先頭 4 KiB のヘッダーのぶん**
/// (メモリのリングは 1,000 行なので、4,080 行でもその 4 倍ある)。
/// 閉じた接続が 4,096 件なのは 1 件 512 B ([`CLOSED_RECORD`]) にしたため
/// (それでもメモリのリング 2,000 件の 2 倍持てる)。出来事 (T14.11) の 128 KiB は
/// ログから分けた (ログは 4,080 → 3,568 行。メモリのリング 1,000 行の 3.5 倍は残る)。
pub const CLOSED_SLOTS: usize = 4096;
pub const ERROR_SLOTS: usize = 2048;
pub const BURST_SLOTS: usize = 128;
pub const EVENT_SLOTS: usize = 512;
pub const LOG_SLOTS: usize = 3568;

/// 1 周期 (5 秒) に書くレコードの上限。**合計ちょうど 64 KiB** で、越えた分は
/// 古い方から落とす (メモリのリングには残っている。落とした数は `/status` の `dropped`)。
/// 閉じた接続の 80 件/5 秒 = 16 本/秒 は、デプロイ先の実測 (43 本/時) の 1,300 倍。
const TICK_CLOSED: usize = 80;
const TICK_ERRORS: usize = 32;
const TICK_BURSTS: usize = 2;
const TICK_EVENTS: usize = 8;
const TICK_LOG: usize = 24;

/// 4 本の領域の割り付け。
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub closed: Region,
    pub errors: Region,
    pub bursts: Region,
    pub events: Region,
    pub log: Region,
    /// 領域が実際に使っている大きさ (ちょうど [`FILE_SIZE`])
    pub used: u64,
    pub total: u64,
}

impl Layout {
    pub const fn current() -> Layout {
        let mut off = HEADER_SIZE;
        let closed = Region {
            offset: off,
            record_size: CLOSED_RECORD,
            count: CLOSED_SLOTS,
        };
        off += closed.bytes();
        let errors = Region {
            offset: off,
            record_size: SMALL_RECORD,
            count: ERROR_SLOTS,
        };
        off += errors.bytes();
        let bursts = Region {
            offset: off,
            record_size: SHOT_RECORD,
            count: BURST_SLOTS,
        };
        off += bursts.bytes();
        let events = Region {
            offset: off,
            record_size: SMALL_RECORD,
            count: EVENT_SLOTS,
        };
        off += events.bytes();
        let log = Region {
            offset: off,
            record_size: SMALL_RECORD,
            count: LOG_SLOTS,
        };
        off += log.bytes();
        // 収まらない割り付けを書いたらここで止まる (実行時に気付くより早い)
        assert!(off <= FILE_SIZE);
        Layout {
            closed,
            errors,
            bursts,
            events,
            log,
            used: off,
            total: FILE_SIZE,
        }
    }
}

/// 起動時に読み戻した個票。[`Restored::install`] でメモリのリングへ入れる。
pub struct Restored {
    /// ファイルを作り直したか (版が違った / 無かった / 壊れていた)
    pub created: bool,
    pub closed: Vec<RecentEntry>,
    pub errors: Vec<ErrorEntry>,
    pub bursts: Vec<BurstShot>,
    pub events: Vec<Event>,
    pub log: Vec<Line>,
}

impl Restored {
    /// メモリのリングへ入れる (**起動時に 1 回だけ**)。戻り値は
    /// 閉じた接続 / エラー / 写真 / 出来事 / ログの件数。
    pub fn install(mut self, metrics: &Metrics) -> [usize; 5] {
        // 記録の一括 off (T14.41)。`off` で起動したら**前の起動のぶんも読み戻さない**
        // (ファイルは消さない = `on` に戻して再起動すればまた読める)
        if !crate::records::recording() {
            return [0; 5];
        }
        // `hashed` で起動したとき、前の起動が `on` だったら生の IP が入っている。
        // 読み戻すときに**同じ 1 関数**を通して 16 進に直す (前の起動の塩は残って
        // いないので、どのみち今の起動の値とは突き合わせられない)
        if crate::records::mode() == crate::records::Mode::Hashed {
            for e in &mut self.closed {
                e.client = crate::records::client_key(&e.client).into_owned();
            }
            for e in &mut self.errors {
                e.client = crate::records::client_key(&e.client).into_owned();
            }
            for shot in &mut self.bursts {
                for (client, _) in &mut shot.clients {
                    *client = crate::records::client_key(client).into_owned();
                }
            }
        }
        let counts = [
            self.closed.len(),
            self.errors.len(),
            self.bursts.len(),
            self.events.len(),
            self.log.len(),
        ];
        metrics.closed.restore(self.closed);
        metrics.errors.restore(self.errors);
        metrics.bursts.restore(self.bursts);
        crate::events::restore(self.events);
        crate::log::restore(self.log);
        counts
    }
}

/// 1 回の書き込みで足したレコード数の内訳 ([`RecentFile::write_new`] の戻り値)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Written {
    pub closed: usize,
    pub errors: usize,
    pub bursts: usize,
    pub events: usize,
    pub log: usize,
}

impl Written {
    pub fn total(&self) -> usize {
        self.closed + self.errors + self.bursts + self.events + self.log
    }
}

/// 4 本のリングのカーソルと、次の通し番号。
struct Rings {
    closed: Ring,
    errors: Ring,
    bursts: Ring,
    events: Ring,
    log: Ring,
    /// 閉じた接続 / エラー / 写真 / 出来事 / ログ の次の通し番号 (1 始まり)
    seq: [u64; 5],
}

/// 開いた個票のファイル。
pub struct RecentFile {
    file: Fixed,
    pub path: PathBuf,
    /// **鍵を取るのは history スレッドだけ** (5 秒に 1 回)。要求の経路は触らない
    rings: Mutex<Rings>,
    /// 何か書いた周期の数と、書いたレコードの数
    writes: AtomicU64,
    records: AtomicU64,
    /// 1 周期の上限を越えて落とした件数 (メモリのリングには残っている)
    dropped: AtomicU64,
    write_errors: AtomicU64,
    /// 最後の / いちばん長かった書き込みの所要 (us)
    last_us: AtomicU64,
    max_us: AtomicU64,
}

impl RecentFile {
    /// 開き (無ければ作り)、個票を読み戻す。
    pub fn open(path: PathBuf) -> io::Result<(RecentFile, Restored)> {
        let l = Layout::current();
        let (file, created) = Fixed::open(&path, MAGIC, l.total)?;
        let (closed, closed_recs) = Ring::load(&file, l.closed)?;
        let (errors, error_recs) = Ring::load(&file, l.errors)?;
        let (bursts, burst_recs) = Ring::load(&file, l.bursts)?;
        let (events, event_recs) = Ring::load(&file, l.events)?;
        let (log, log_recs) = Ring::load(&file, l.log)?;
        let seq = [
            next_seq(&closed_recs),
            next_seq(&error_recs),
            next_seq(&burst_recs),
            next_seq(&event_recs),
            next_seq(&log_recs),
        ];
        let restored = Restored {
            created,
            closed: decode_tail(&closed_recs, MAX_RECENT, decode_closed),
            errors: decode_tail(&error_recs, MAX_ERRORS, decode_error),
            bursts: decode_tail(&burst_recs, MAX_BURSTS, decode_shot),
            events: decode_tail(&event_recs, MAX_EVENTS, decode_event),
            log: decode_tail(&log_recs, crate::log::MAX_LOG_LINES, decode_log),
        };
        let f = RecentFile {
            file,
            path,
            rings: Mutex::new(Rings {
                closed,
                errors,
                bursts,
                events,
                log,
                seq,
            }),
            writes: AtomicU64::new(0),
            records: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            write_errors: AtomicU64::new(0),
            last_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
        };
        Ok((f, restored))
    }

    /// まだ書いていない個票をファイルへ追記する (**history スレッドの 5 秒の周期から**。
    /// 停止シグナルでも最後にもう 1 回)。戻り値は書いたレコード数の内訳。
    ///
    /// 4 本のリングから「前回書いた所から先」を取り出すだけなので、**接続の経路とは
    /// 一切すれ違わない** (取るのは各リングの鍵 1 回ずつと、このファイルの鍵 1 回)。
    pub fn write_new(&self, metrics: &Metrics) -> Written {
        let started = Instant::now();
        let (closed, d0) = metrics.closed.take_unwritten(TICK_CLOSED);
        let (errors, d1) = metrics.errors.take_unwritten(TICK_ERRORS);
        let (bursts, d2) = metrics.bursts.take_unwritten(TICK_BURSTS);
        let (events, d3) = crate::events::take_unwritten(TICK_EVENTS);
        let (lines, d4) = crate::log::take_unwritten(TICK_LOG);
        let dropped = d0 + d1 + d2 + d3 + d4;
        let w = Written {
            closed: closed.len(),
            errors: errors.len(),
            bursts: bursts.len(),
            events: events.len(),
            log: lines.len(),
        };
        let n = w.total();
        if n == 0 {
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
            return w;
        }
        {
            let mut r = self.rings.locked();
            let g = &mut *r;
            for e in &closed {
                let p = encode_closed(bump(&mut g.seq[0]), e);
                self.note("closed", g.closed.push(&self.file, &p));
            }
            for e in &errors {
                let p = encode_error(bump(&mut g.seq[1]), e);
                self.note("errors", g.errors.push(&self.file, &p));
            }
            for s in &bursts {
                let p = encode_shot(bump(&mut g.seq[2]), s);
                self.note("bursts", g.bursts.push(&self.file, &p));
            }
            for e in &events {
                let p = encode_event(bump(&mut g.seq[3]), e);
                self.note("events", g.events.push(&self.file, &p));
            }
            for line in &lines {
                let p = encode_log(bump(&mut g.seq[4]), line);
                self.note("log", g.log.push(&self.file, &p));
            }
        }
        let us = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        self.last_us.store(us, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.records.fetch_add(n as u64, Ordering::Relaxed);
        self.dropped.fetch_add(dropped, Ordering::Relaxed);
        w
    }

    fn note(&self, what: &str, r: io::Result<()>) {
        if let Err(e) = r {
            let n = self.write_errors.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                log_warn!(
                    None,
                    "recent file {}: {} write failed: {}",
                    self.path.display(),
                    what,
                    e
                );
            }
        }
    }

    pub fn write_errors(&self) -> u64 {
        self.write_errors.load(Ordering::Relaxed)
    }

    /// `/status` の `state_file.recent` 要素。
    pub fn status_json(&self) -> String {
        let l = Layout::current();
        format!(
            "{{\"path\":{},\"bytes\":{},\"writes\":{},\"records\":{},\"dropped\":{},\"write_errors\":{},\"last_write_us\":{},\"max_write_us\":{},\"slots\":{{\"closed\":{},\"errors\":{},\"bursts\":{},\"events\":{},\"log\":{}}}}}",
            crate::json::quote(&self.path.display().to_string()),
            l.total,
            self.writes.load(Ordering::Relaxed),
            self.records.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed),
            self.write_errors.load(Ordering::Relaxed),
            self.last_us.load(Ordering::Relaxed),
            self.max_us.load(Ordering::Relaxed),
            l.closed.count,
            l.errors.count,
            l.bursts.count,
            l.events.count,
            l.log.count,
        )
    }
}

/// `$HOME/.rust-http-proxy.recent` (状態ファイルの隣)。
pub fn path_next_to(rrd: &std::path::Path) -> PathBuf {
    rrd.with_file_name(".rust-http-proxy.recent")
}

/// 状態ファイルの隣の個票ファイルを開く (開けなければ警告して `None`。
/// 個票の永続化なしで動くだけで、統計の `.rrd` は生きたまま)。
pub fn open_beside(rrd: &std::path::Path) -> Option<(RecentFile, Restored)> {
    let path = path_next_to(rrd);
    match RecentFile::open(path.clone()) {
        Ok(v) => Some(v),
        Err(e) => {
            log_warn!(
                None,
                "recent file {}: {} (individual records will not survive restarts)",
                path.display(),
                e
            );
            None
        }
    }
}

/// 読み戻した件数を起動ログに 1 行出す (`.rrd` の 1 行と同じ形)。
pub fn log_restored(file: &RecentFile, created: bool, counts: [usize; 5]) {
    log_info!(
        None,
        "recent file {} ({} KiB, {}): {} closed connections, {} errors, {} burst shots, {} events, {} log lines restored",
        file.path.display(),
        FILE_SIZE / 1024,
        if created { "created" } else { "opened" },
        counts[0],
        counts[1],
        counts[2],
        counts[3],
        counts[4]
    );
}

/// 次の通し番号 (レコードの先頭 8 バイト)。`Ring::load` が鍵の順に並べて返すので、
/// 最後のレコードの番号がいちばん大きい。
fn next_seq(recs: &[Vec<u8>]) -> u64 {
    recs.last().map(|p| Dec(p).u64()).unwrap_or(0) + 1
}

fn bump(seq: &mut u64) -> u64 {
    let v = *seq;
    *seq += 1;
    v
}

/// 読み戻したレコードのうち**新しい方から `cap` 件**を解く
/// (ファイルの方がメモリのリングより多く持っているため)。
fn decode_tail<T>(recs: &[Vec<u8>], cap: usize, decode: fn(&[u8]) -> Option<T>) -> Vec<T> {
    let skip = recs.len().saturating_sub(cap);
    recs.iter().skip(skip).filter_map(|p| decode(p)).collect()
}

#[cfg(test)]
mod tests;
