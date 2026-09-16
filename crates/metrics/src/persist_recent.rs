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

/// 版の印。**レコードの並びを変えたら末尾を上げる** (古いファイルは読み捨てて作り直す)。
pub const MAGIC: &[u8; 8] = b"SHPREC02";

/// ファイルの大きさ (固定 4 MiB)。`.rrd` と同じで、以後は 1 バイトも伸びない。
pub const FILE_SIZE: u64 = 4 * 1024 * 1024;

/// 先頭の版の印を置く場所 (`.rrd` と同じ 4 KiB)。
const HEADER_SIZE: u64 = 4096;

/// 個票 1 件のレコード長 (末尾 4 B が CRC)。エラーとログはこちら。
const SMALL_RECORD: usize = 256;
/// 閉じた接続 1 件のレコード長。**256 B ではなく 512 B** なのは、段階の ms 6 つと
/// T14.5 の RTT / 再送 (両側) まで入れると 256 B に収まらないため
/// (いまの中身は 344 B。領域の大きさは 2 MiB のままで、件数が 8,192 → 4,096 になる)。
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

/// 固定幅の文字列の欄 (先頭 1 バイトが長さなので、入る中身は 1 バイト少ない)。
const W_CLIENT: usize = 48; // 接続元 IP (最長 45 B)
const W_TARGET: usize = 52; // `/recent` の宛先 (`MAX_RECENT_TARGET` 48 B)
const W_ETARGET: usize = 84; // `/errors` と写真の宛先 (`MAX_TARGET` 80 B)
const W_SNI: usize = MAX_SNI + 4; // 覗いた SNI (`MAX_SNI` 64 B。T14.38)
/// ログ 1 行の本文。レコード 256 B から通し番号・時刻・レベル・conn を引いた残り
/// (メモリのリングは 1 行 256 B まで持つので、**ファイルに残すときだけ 219 B に切る**)。
const W_MSG: usize = SMALL_RECORD - 4 - 8 * 4;
/// 出来事 1 件の説明 (`MAX_TEXT` 128 B + 長さの 1 B。T14.11)。
const W_TEXT: usize = MAX_TEXT + 4;

/// 1 レコードに収まることを**組み立て時に**確かめる (欄を足して溢れたらここで止まる)。
/// 数は各 `encode_*` が書く u64 の本数 (先頭の通し番号を含む) + 固定幅の文字列。
const CLOSED_PAYLOAD: usize = 8 * (13 + STAGES + 2 * SIDES) + W_CLIENT + W_TARGET + W_SNI;
const ERROR_PAYLOAD: usize = 8 * 7 + W_ETARGET + W_CLIENT;
const LOG_PAYLOAD: usize = 8 * 4 + W_MSG;
const EVENT_PAYLOAD: usize = 8 * 3 + W_TEXT;
const SHOT_PAYLOAD: usize = 8 * (18 + CONN_STATES + 2)
    + MAX_SHOT_CLIENTS * (W_CLIENT + 8)
    + MAX_SHOT_TARGETS * (W_ETARGET + 8);
const _: () = assert!(CLOSED_PAYLOAD <= CLOSED_RECORD - 4);
const _: () = assert!(ERROR_PAYLOAD <= SMALL_RECORD - 4);
const _: () = assert!(LOG_PAYLOAD <= SMALL_RECORD - 4);
const _: () = assert!(EVENT_PAYLOAD <= SMALL_RECORD - 4);
const _: () = assert!(SHOT_PAYLOAD <= SHOT_RECORD - 4);

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

fn encode_closed(seq: u64, e: &RecentEntry) -> Vec<u8> {
    let mut enc = Enc::new();
    enc.u64(seq)
        .u64(e.at)
        .u64(e.id)
        .u64(e.secs)
        .u64(e.up)
        .u64(e.down)
        .u64(e.parked_secs)
        .u64(e.requests as u64)
        .u64(e.parks as u64)
        .u64(e.status as u64)
        .u64(u64::from(e.connect))
        .u64(e.reason.code() as u64)
        // 確立までの SYN の再送 (T14.46)。**数値の末尾に足した** ので、前の版で
        // 書いたレコードは 0 (= 再送なし) で読み戻る
        .u64(e.syn_retrans as u64);
    for ms in e.stage_ms {
        enc.u64(ms);
    }
    // カーネルの RTT と再送 (クライアント側 / オリジン側。T14.5)
    for v in e.rtt_us {
        enc.u64(v as u64);
    }
    for v in e.retrans {
        enc.u64(v as u64);
    }
    enc.str(&e.client, W_CLIENT).str(&e.target, W_TARGET);
    // 覗いた SNI (T14.38)。空 = 覗いていない / 読めなかった (読み戻すと `None`)
    enc.str(e.sni.as_deref().unwrap_or(""), W_SNI);
    enc.0
}

fn decode_closed(p: &[u8]) -> Option<RecentEntry> {
    let mut d = Dec(p);
    let _seq = d.u64();
    let at = d.u64();
    let id = d.u64();
    let secs = d.u64();
    let up = d.u64();
    let down = d.u64();
    let parked_secs = d.u64();
    let requests = d.u64() as u32;
    let parks = d.u64() as u32;
    let status = d.u64() as u16;
    let connect = d.u64() != 0;
    let reason = CloseReason::from_code(d.u64() as u16);
    let syn_retrans = d.u64().min(u8::MAX as u64) as u8;
    let mut stage_ms = [0u64; STAGES];
    for slot in stage_ms.iter_mut() {
        *slot = d.u64();
    }
    let mut rtt_us = [0u32; SIDES];
    for slot in rtt_us.iter_mut() {
        *slot = d.u64() as u32;
    }
    let mut retrans = [0u32; SIDES];
    for slot in retrans.iter_mut() {
        *slot = d.u64() as u32;
    }
    let client = d.str(W_CLIENT);
    let target = d.str(W_TARGET);
    let sni = d.str(W_SNI);
    if at == 0 {
        return None;
    }
    Some(RecentEntry {
        id,
        at,
        client,
        target,
        connect,
        secs,
        requests,
        up,
        down,
        reason,
        status,
        parked_secs,
        parks,
        stage_ms,
        rtt_us,
        retrans,
        sni: (!sni.is_empty()).then(|| sni.into()),
        syn_retrans,
    })
}

fn encode_error(seq: u64, e: &ErrorEntry) -> Vec<u8> {
    let mut enc = Enc::new();
    enc.u64(seq)
        .u64(e.at)
        .u64(e.kind.code())
        .u64(e.cause.code())
        .u64(e.dns_ms)
        .u64(e.connect_ms)
        .u64(e.status as u64)
        .str(&e.target, W_ETARGET)
        .str(&e.client, W_CLIENT);
    enc.0
}

fn decode_error(p: &[u8]) -> Option<ErrorEntry> {
    let mut d = Dec(p);
    let _seq = d.u64();
    let at = d.u64();
    let kind = EntryKind::from_code(d.u64());
    let cause = EntryCause::from_code(d.u64());
    let dns_ms = d.u64();
    let connect_ms = d.u64();
    let status = d.u64() as u16;
    let target = d.str(W_ETARGET);
    let client = d.str(W_CLIENT);
    if at == 0 {
        return None;
    }
    Some(ErrorEntry {
        at,
        kind,
        target,
        cause,
        dns_ms,
        connect_ms,
        status,
        client,
    })
}

fn encode_event(seq: u64, e: &Event) -> Vec<u8> {
    let mut enc = Enc::new();
    enc.u64(seq)
        .u64(e.at)
        .u64(e.kind.code())
        .str(&e.text, W_TEXT);
    enc.0
}

fn decode_event(p: &[u8]) -> Option<Event> {
    let mut d = Dec(p);
    let _seq = d.u64();
    let at = d.u64();
    let kind = EventKind::from_code(d.u64());
    let text = d.str(W_TEXT);
    if at == 0 {
        return None;
    }
    Some(Event { at, kind, text })
}

fn encode_log(seq: u64, line: &Line) -> Vec<u8> {
    let mut enc = Enc::new();
    enc.u64(seq)
        .u64(line.at)
        .u64(line.level as u64)
        // 0 は「`[main]`」の印なので、接続の番号は 1 足して入れる
        .u64(line.conn.map(|c| c as u64 + 1).unwrap_or(0))
        .str(&line.msg, W_MSG);
    enc.0
}

fn decode_log(p: &[u8]) -> Option<Line> {
    let mut d = Dec(p);
    let _seq = d.u64();
    let at = d.u64();
    let level = Level::from_u8(d.u64().min(u8::MAX as u64) as u8);
    let conn = d.u64();
    let msg = d.str(W_MSG);
    if at == 0 {
        return None;
    }
    Some(Line {
        at,
        level,
        // 0 は `[main]` (接続の番号は 1 足して入れてある)
        conn: if conn > 0 {
            Some(conn as usize - 1)
        } else {
            None
        },
        msg,
    })
}

fn encode_shot(seq: u64, s: &BurstShot) -> Vec<u8> {
    let mut enc = Enc::new();
    enc.u64(seq)
        .u64(s.at)
        .u64(s.seq)
        .u64(s.active as u64)
        .u64(s.trigger_active as u64)
        .u64(s.max_conns as u64)
        .u64(s.threshold as u64)
        .u64(s.clients_distinct as u64)
        .u64(s.clients_other as u64)
        .u64(s.targets_distinct as u64)
        .u64(s.targets_other as u64)
        .u64(s.connects as u64)
        .u64(s.https as u64)
        .u64(s.evicted_idle)
        .u64(s.rejected_overload)
        .u64(s.threads)
        .u64(s.fds)
        .u64(s.max_fds);
    for n in s.states {
        enc.u64(n as u64);
    }
    let clients = s.clients.len().min(MAX_SHOT_CLIENTS);
    enc.u64(clients as u64);
    for (name, n) in s.clients.iter().take(clients) {
        enc.str(name, W_CLIENT).u64(*n as u64);
    }
    let targets = s.targets.len().min(MAX_SHOT_TARGETS);
    enc.u64(targets as u64);
    for (name, n) in s.targets.iter().take(targets) {
        enc.str(name, W_ETARGET).u64(*n as u64);
    }
    enc.0
}

fn decode_shot(p: &[u8]) -> Option<BurstShot> {
    let mut d = Dec(p);
    let _seq = d.u64();
    let at = d.u64();
    let shot_seq = d.u64();
    let active = d.u64() as usize;
    let trigger_active = d.u64() as usize;
    let max_conns = d.u64() as usize;
    let threshold = d.u64() as usize;
    let clients_distinct = d.u64() as usize;
    let clients_other = d.u64() as u32;
    let targets_distinct = d.u64() as usize;
    let targets_other = d.u64() as u32;
    let connects = d.u64() as u32;
    let https = d.u64() as u32;
    let evicted_idle = d.u64();
    let rejected_overload = d.u64();
    let threads = d.u64();
    let fds = d.u64();
    let max_fds = d.u64();
    let mut states = [0u32; CONN_STATES];
    for slot in states.iter_mut() {
        *slot = d.u64() as u32;
    }
    let n = (d.u64() as usize).min(MAX_SHOT_CLIENTS);
    let mut clients = Vec::with_capacity(n);
    for _ in 0..n {
        let name = d.str(W_CLIENT);
        clients.push((name, d.u64() as u32));
    }
    let n = (d.u64() as usize).min(MAX_SHOT_TARGETS);
    let mut targets = Vec::with_capacity(n);
    for _ in 0..n {
        let name = d.str(W_ETARGET);
        targets.push((name, d.u64() as u32));
    }
    if at == 0 {
        return None;
    }
    Some(BurstShot {
        at,
        seq: shot_seq,
        active,
        trigger_active,
        max_conns,
        threshold,
        clients,
        clients_distinct,
        clients_other,
        targets,
        targets_distinct,
        targets_other,
        states,
        connects,
        https,
        evicted_idle,
        rejected_overload,
        threads,
        fds,
        max_fds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{BlockCause, ErrCause};
    use crate::recent::{CloseReason, RecentEntry};

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "shp-recent-{}-{}-{:?}",
            name,
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn closed_entry(id: u64) -> RecentEntry {
        RecentEntry {
            id,
            at: 1_700_000_000 + id,
            client: "203.0.113.7".to_string(),
            target: "mtalk.google.com:5228".to_string(),
            connect: true,
            secs: 42,
            requests: 3,
            up: 1234,
            down: 5678,
            reason: CloseReason::Error(ErrCause::Refused),
            status: 200,
            parked_secs: 7,
            parks: 2,
            stage_ms: [1, 2, 3, 0, 0, 0],
            rtt_us: [1234, 5678],
            retrans: [0, 2],
            sni: Some("mtalk.google.com".into()),
            syn_retrans: 2,
        }
    }

    /// 割り付けが **4 MiB ちょうど**であること (伸びないことと同じくらい大事な性質:
    /// 余らせると次の版で「入るはず」と誤解する)。
    #[test]
    fn the_layout_fills_exactly_four_mib() {
        let l = Layout::current();
        assert_eq!(l.total, 4 * 1024 * 1024);
        assert_eq!(l.used, l.total, "ヘッダー + 4 領域でちょうど埋まる");
        assert_eq!(l.closed.bytes(), 2 * 1024 * 1024);
        assert_eq!(l.closed.record_size, 512);
        assert_eq!(l.errors.bytes(), 512 * 1024);
        assert_eq!(l.bursts.bytes(), 512 * 1024);
        assert_eq!(l.events.bytes(), 128 * 1024);
        assert_eq!(l.log.bytes(), 913_408);
        // 1 レコードの中身が入ること (組み立て時の assert と同じものを数字で残す)
        assert_eq!(
            (
                CLOSED_PAYLOAD,
                ERROR_PAYLOAD,
                LOG_PAYLOAD,
                EVENT_PAYLOAD,
                SHOT_PAYLOAD
            ),
            (352, 188, 252, 156, 2016)
        );
    }

    #[test]
    fn records_round_trip() {
        let e = closed_entry(9);
        // 書いた長さが上の定数と一致すること (欄を足したら両方直す)
        assert_eq!(encode_closed(1, &e).len(), CLOSED_PAYLOAD);
        assert_eq!(decode_closed(&encode_closed(1, &e)).unwrap(), e);

        let err = ErrorEntry::new(
            EntryKind::Connect,
            "ads.example.com:443",
            "198.51.100.3",
            403,
            EntryCause::Blocked(BlockCause::Blocklist),
            12,
            34,
        );
        assert_eq!(encode_error(1, &err).len(), ERROR_PAYLOAD);
        assert_eq!(decode_error(&encode_error(1, &err)).unwrap(), err);

        let line = Line {
            at: 1_700_000_123,
            level: Level::Warn,
            conn: Some(42),
            msg: "origin connect failed".to_string(),
        };
        assert_eq!(decode_log(&encode_log(1, &line)).unwrap(), line);
        let main_line = Line { conn: None, ..line };
        assert_eq!(decode_log(&encode_log(2, &main_line)).unwrap(), main_line);

        let event = Event {
            at: 1_700_000_300,
            kind: EventKind::Shutdown,
            text: "stop signal received".to_string(),
        };
        assert_eq!(encode_event(1, &event).len(), EVENT_PAYLOAD);
        assert_eq!(decode_event(&encode_event(1, &event)).unwrap(), event);

        let shot = BurstShot {
            at: 1_700_000_200,
            seq: 3,
            active: 5,
            trigger_active: 5,
            max_conns: 8,
            threshold: 4,
            clients: vec![("127.0.0.1".to_string(), 5)],
            clients_distinct: 1,
            clients_other: 0,
            targets: vec![("127.0.0.1:1234".to_string(), 5)],
            targets_distinct: 1,
            targets_other: 0,
            states: [0, 0, 5, 0, 0],
            connects: 5,
            https: 0,
            evicted_idle: 1,
            rejected_overload: 2,
            threads: 13,
            fds: 21,
            max_fds: 1024,
        };
        assert_eq!(
            encode_shot(1, &shot).len(),
            8 * 25 + (W_CLIENT + 8) + (W_ETARGET + 8)
        );
        assert_eq!(decode_shot(&encode_shot(1, &shot)).unwrap(), shot);
    }

    /// 長すぎる欄は**切れるが壊れない** (レコードからはみ出さない)。
    #[test]
    fn oversized_fields_are_clipped_not_overflowed() {
        let mut e = closed_entry(1);
        e.client = "x".repeat(200);
        e.target = "y".repeat(200);
        let rec = encode_closed(1, &e);
        assert!(rec.len() <= CLOSED_RECORD - 4, "{}", rec.len());
        let back = decode_closed(&rec).unwrap();
        assert_eq!(back.client.len(), W_CLIENT - 1);
        assert_eq!(back.target.len(), W_TARGET - 1);

        let line = Line {
            at: 1,
            level: Level::Error,
            conn: None,
            msg: "z".repeat(crate::log::MAX_LOG_LINE),
        };
        let rec = encode_log(1, &line);
        assert_eq!(rec.len(), LOG_PAYLOAD);
        assert_eq!(decode_log(&rec).unwrap().msg.len(), W_MSG - 1);
    }

    /// **リングが回ってもファイルは 4,194,304 B のまま**で、新しい方が残ること
    /// (受け入れ基準: 10,000 件閉じても伸びない)。
    #[test]
    fn the_file_never_grows_when_the_ring_wraps() {
        let path = tmp("wrap");
        let metrics = Metrics::new();
        let (file, restored) = RecentFile::open(path.clone()).unwrap();
        assert!(restored.created);
        assert!(restored.closed.is_empty());
        for id in 0..10_000u64 {
            metrics.closed.push(closed_entry(id));
            file.write_new(&metrics);
        }
        assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
        assert_eq!(file.write_errors(), 0);
        drop(file);

        // 開き直すと、ファイルに残っている 8,192 件のうち新しい方から
        // メモリのリングのぶん (2,000 件) が返る
        let (file, restored) = RecentFile::open(path.clone()).unwrap();
        assert!(!restored.created, "版が同じなら作り直さない");
        assert_eq!(restored.closed.len(), MAX_RECENT);
        assert_eq!(
            restored.closed.first().unwrap().id,
            10_000 - MAX_RECENT as u64
        );
        assert_eq!(restored.closed.last().unwrap().id, 9_999);
        // 続きの通し番号から書ける (上書きしない)
        let m2 = Metrics::new();
        m2.closed.push(closed_entry(10_000));
        assert_eq!(file.write_new(&m2).closed, 1);
        drop(file);
        let (_, restored) = RecentFile::open(path.clone()).unwrap();
        assert_eq!(restored.closed.last().unwrap().id, 10_000);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
        let _ = std::fs::remove_file(&path);
    }

    /// 版が違う / 先頭が壊れたファイルは**読み捨てて作り直す** (落ちない)。
    #[test]
    fn a_corrupt_or_old_file_is_rebuilt() {
        let path = tmp("corrupt");
        let metrics = Metrics::new();
        {
            let (file, _) = RecentFile::open(path.clone()).unwrap();
            metrics.closed.push(closed_entry(1));
            assert_eq!(file.write_new(&metrics).closed, 1);
        }
        // 先頭 (版の印) を潰す
        let mut raw = std::fs::read(&path).unwrap();
        raw[..8].copy_from_slice(b"XXXXXXXX");
        std::fs::write(&path, &raw).unwrap();
        let (file, restored) = RecentFile::open(path.clone()).unwrap();
        assert!(restored.created, "版の印が違えば作り直す");
        assert!(restored.closed.is_empty(), "中身は捨てる");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
        // 短いファイルも同じ
        drop(file);
        std::fs::write(&path, b"SHPREC01").unwrap();
        let (_, restored) = RecentFile::open(path.clone()).unwrap();
        assert!(restored.created);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
        let _ = std::fs::remove_file(&path);
    }

    /// 1 周期に書くのは 64 KiB まで (越えた分は古い方から落ち、メモリには残る)。
    #[test]
    fn one_tick_writes_at_most_sixty_four_kib() {
        assert_eq!(
            TICK_CLOSED * CLOSED_RECORD
                + TICK_ERRORS * SMALL_RECORD
                + TICK_BURSTS * SHOT_RECORD
                + TICK_EVENTS * SMALL_RECORD
                + TICK_LOG * SMALL_RECORD,
            64 * 1024
        );
        let path = tmp("tick");
        let metrics = Metrics::new();
        let (file, _) = RecentFile::open(path.clone()).unwrap();
        for id in 0..(TICK_CLOSED as u64 + 40) {
            metrics.closed.push(closed_entry(id));
        }
        assert_eq!(file.write_new(&metrics).closed, TICK_CLOSED);
        // 落ちたのは古い方 (新しい 160 件が残る)
        drop(file);
        let (_, restored) = RecentFile::open(path.clone()).unwrap();
        assert_eq!(
            restored.closed.len(),
            TICK_CLOSED,
            "落ちた 40 件は書かれない"
        );
        assert_eq!(
            restored.closed.last().unwrap().id,
            TICK_CLOSED as u64 + 39,
            "残るのは新しい方"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 読み戻した件は**書き直さない** (印が通算に合っている)。
    #[test]
    fn restored_records_are_not_written_again() {
        let path = tmp("install");
        let first = Metrics::new();
        {
            let (file, _) = RecentFile::open(path.clone()).unwrap();
            for id in 0..3u64 {
                first.closed.push(closed_entry(id));
            }
            assert_eq!(file.write_new(&first).closed, 3);
        }
        let second = Metrics::new();
        let (file, restored) = RecentFile::open(path.clone()).unwrap();
        // ログのリングはプロセスに 1 つなので、他のテストの警告が混じりうる。
        // ここで見るのは閉じた接続の数だけ
        assert_eq!(restored.install(&second)[0], 3);
        assert_eq!(second.closed.len(), 3);
        assert_eq!(second.closed.restored(), 3);
        assert_eq!(
            file.write_new(&second).closed,
            0,
            "読み戻した分は書き直さない"
        );
        // 新しい 1 件だけが増える
        second.closed.push(closed_entry(99));
        assert_eq!(file.write_new(&second).closed, 1);
        drop(file);
        let (_, restored) = RecentFile::open(path.clone()).unwrap();
        assert_eq!(restored.closed.len(), 4);
        let _ = std::fs::remove_file(&path);
    }
}
