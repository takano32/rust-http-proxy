//! 日次の自動 `/snapshot` 保存 (`$HOME/.rust-http-proxy/snapshots/`、30 日ぶん。T14.34)。
//!
//! `scripts/collect-deployed.sh` は外から回す必要があり、回し忘れると個票
//! (2,000 件 = 数時間ぶん) は消える。プロキシ自身が **1 日 1 回 (UTC 0 時)** `/snapshot` を
//! `$HOME` に書いておけば、見に行ったときに必ず 30 日ぶんの個票がある。
//!
//! - 書くのは **history スレッドだけ** ([`tick`] は 5 秒に 1 回、原子の読み 1 回で戻る)。
//!   利用者の要求の経路には 1 命令も足していない (費用 0)。
//! - **UTC の日付が変わった標本**で、**終わった日の名前** (`<YYYY-MM-DD>.json`) の
//!   1 ファイルを書く (1 日 1 ファイル、[`MAX_FILE`] = `/snapshot` と同じ 4 MiB)。
//! - 31 個目を書いたら最古を消す ([`DEFAULT_DAYS`] = 30、`PROXY_SNAPSHOT_DAYS`。**0 で止める**)。
//! - 起動時に**いちばん新しいファイルの日付**を覚え ([`Writer::new`])、同じ日を 2 度書かない
//!   (T14.20 の日次の要約と同じ作法)。
//! - `PROXY_STATS_PERSIST=off` では [`configure`] を呼ばないので 1 ファイルも書かない。
//! - ディスクの予算 (T12.5 の `keep_free`) を越えるなら**書かずに** `/events` に 1 件
//!   (`state_file` の種類。T14.11)。
//! - 読む口は `/snapshots` (一覧) と `/snapshots/<date>` (そのままの JSON)。
//!
//! **`/snapshot` を組むのは `proxy-endpoints` の仕事**なので、ここは
//! [`set_builder`] で預かった閉包を日付の変わり目に 1 回呼ぶだけにしてある
//! (下の層 `proxy-metrics` から上の層を呼ぶと依存が輪になる。T14.11 の
//! `events::poll` と同じ判断で、**向きは上から預ける**)。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::cache::Cache;
use crate::history::Sample;
use crate::sync::LockExt;
use crate::{log_debug, log_info, log_warn};

/// 1 ファイルの上限 (4 MiB。`/snapshot` の上限 `endpoints::recent::MAX_SNAPSHOT` と同じ値)。
pub const MAX_FILE: u64 = 4 * 1024 * 1024;
/// 残す日数の既定 (`PROXY_SNAPSHOT_DAYS`)。
pub const DEFAULT_DAYS: usize = 30;
/// 残す日数の上限 (書き間違いで `$HOME` を埋めないための歯止め。4 MiB × 365 = 1.4 GiB)。
pub const MAX_KEPT_DAYS: usize = 365;
/// 置き場所 (`$HOME/.rust-http-proxy/snapshots/`)。状態ファイル (`$HOME/.rust-http-proxy.rrd`) と
/// 違ってディレクトリなので、名前を分けてある。
pub const DIR_NAME: &str = ".rust-http-proxy";
pub const SUB_DIR: &str = "snapshots";
/// ファイルの拡張子 (一覧に出すのはこの形の名前だけ)。
const EXT: &str = ".json";
const SECS_PER_DAY: u64 = 86_400;

/// 既定の置き場所 (`$HOME/.rust-http-proxy/snapshots/`)。`$HOME` が無ければ `None`。
pub fn default_dir() -> Option<PathBuf> {
    crate::envfile::env_path().map(|p| p.with_file_name(DIR_NAME).join(SUB_DIR))
}

/// UTC の日 (epoch 秒 / 86,400) を `YYYY-MM-DD` に。
pub fn day_name(day: u64) -> String {
    let (y, mo, d, ..) = crate::log::civil_from_epoch(day.saturating_mul(SECS_PER_DAY));
    format!("{:04}-{:02}-{:02}", y, mo, d)
}

/// `YYYY-MM-DD` として読める文字列か。
///
/// `/snapshots/<date>` はこれを通った文字列しかファイル名に使わない
/// (`..` や `/` を書かれても置き場所の外へ出ない)。
pub fn is_date(s: &str) -> bool {
    s.len() == 10
        && s.as_bytes().iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
}

/// `YYYY-MM-DD` を UTC の日に戻す (並べ替えと「最新の日付」に使う)。
fn day_of(name: &str) -> Option<u64> {
    if !is_date(name) {
        return None;
    }
    let y: i64 = name[0..4].parse().ok()?;
    let m: i64 = name[5..7].parse().ok()?;
    let d: i64 = name[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant の days_from_civil (`log::civil_from_epoch` の逆)
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days).ok()
}

/// `/snapshot` を組む閉包 (預けるのは `proxy-endpoints`。[`set_builder`])。
type Builder = Box<dyn Fn() -> String + Send + Sync>;

static BUILDER: OnceLock<Mutex<Option<Builder>>> = OnceLock::new();

fn builder() -> &'static Mutex<Option<Builder>> {
    BUILDER.get_or_init(|| Mutex::new(None))
}

/// `/snapshot` を組む閉包を預ける (**起動時に 1 回**、上の層から)。
///
/// 預かっていなければ日付が変わっても何も書かない (組み方を知らないため)。
pub fn set_builder(f: Builder) {
    *builder().locked() = Some(f);
}

/// 預かった閉包で `/snapshot` を 1 枚組む (**日付の変わり目のときだけ**呼ぶ)。
fn build() -> Option<String> {
    builder().locked().as_ref().map(|f| f())
}

/// 日次の `/snapshot` を書くもの。
///
/// 大域の状態 ([`configure`] / [`tick`]) はこれを 1 つ持っているだけで、中身は全部ここにある
/// (単体テストは大域に触らずこれを直に動かせる。T14.20 の `daily::Writer` と同じ作り)。
pub struct Writer {
    dir: Option<PathBuf>,
    /// 残す日数 (`PROXY_SNAPSHOT_DAYS`。0 なら書かない)
    days: usize,
    /// いま見ている日 (最初の標本で決まる。0 = まだ 1 本も見ていない)
    day: u64,
    /// 最後に書いた日 (起動時にいちばん新しいファイルの日付から)
    last_written: u64,
    written: u64,
    errors: u64,
    /// ディスクの予算で書かなかった回数
    skipped: u64,
}

impl Writer {
    /// 置き場所と日数を決める。**いちばん新しいファイルの日付を読む**
    /// (同じ日を 2 度書かないため)。
    pub fn new(dir: Option<PathBuf>, days: usize) -> Writer {
        let days = days.min(MAX_KEPT_DAYS);
        let dir = dir.filter(|_| days > 0);
        let last_written = dir.as_deref().map(newest_day).unwrap_or(0);
        Writer {
            dir,
            days,
            day: 0,
            last_written,
            written: 0,
            errors: 0,
            skipped: 0,
        }
    }

    /// 履歴の標本の時刻を 1 つ見る。**日が変わっていたら終わった日の 1 ファイルを書く**。
    ///
    /// `build` (`/snapshot` を組む) と `space` (書き先の空きとマージンを測る) は
    /// **日付の変わり目のときだけ**呼ぶ (静かな日は 5 秒ごとに割り算 1 回で戻る)。
    /// 戻り値は書いたファイル。
    pub fn add(
        &mut self,
        t: u64,
        build: impl FnOnce() -> Option<String>,
        space: impl FnOnce(&Path) -> (Option<u64>, u64),
    ) -> Option<PathBuf> {
        let dir = self.dir.clone()?;
        let day = t / SECS_PER_DAY;
        let finished = match self.day {
            0 => {
                // 起動して最初の標本。この日が終わるまでは何も書かない
                self.day = day;
                return None;
            }
            cur if cur == day => return None,
            cur => cur,
        };
        self.day = day;
        if finished <= self.last_written {
            // その日のファイルが既にある (同じ日に 2 回起動した / 別のプロセスが書いた)
            log_debug!(
                None,
                "daily snapshot {}: {} is already there, not writing it twice",
                dir.display(),
                day_name(finished)
            );
            return None;
        }
        let name = day_name(finished);
        let Some(body) = build() else {
            // `/snapshot` の組み方を預かっていない (本番では起動時に預かる)
            log_debug!(
                None,
                "daily snapshot {}: no snapshot builder",
                dir.display()
            );
            return None;
        };
        let path = dir.join(format!("{}{}", name, EXT));
        let need = body.len() as u64;
        if need > MAX_FILE {
            // `/snapshot` 側が 4 MiB で切っているので本来ここへは来ない (念のための歯止め)
            self.note_skip(
                &path,
                &format!("{} B is over the {} B limit", need, MAX_FILE),
            );
            return None;
        }
        let (free, keep_free) = space(&dir);
        if !fits(free, keep_free, need) {
            self.note_skip(
                &path,
                &format!(
                    "only {} free and {} must stay free (needs {} B)",
                    free.map(mib).unwrap_or_else(|| "?".into()),
                    mib(keep_free),
                    need
                ),
            );
            return None;
        }
        match write_file(&path, &body) {
            Ok(()) => {
                self.last_written = finished;
                self.written += 1;
                let dropped = prune(&dir, self.days);
                log_info!(
                    None,
                    "daily snapshot {} ({} B, keeping {} days{})",
                    path.display(),
                    need,
                    self.days,
                    if dropped > 0 {
                        format!(", dropped {} older", dropped)
                    } else {
                        String::new()
                    }
                );
                Some(path)
            }
            Err(e) => {
                self.errors += 1;
                if self.errors == 1 {
                    log_warn!(
                        None,
                        "daily snapshot {}: write failed: {}",
                        path.display(),
                        e
                    );
                    // 出来事の時系列に 1 件 (**最初の 1 回だけ**。T14.11 と同じ口)
                    crate::events::note_state_file(&path, "write", &e);
                }
                None
            }
        }
    }

    /// 書かずに終わったことを `/events` に 1 件残す (`state_file` の種類。T14.11)。
    fn note_skip(&mut self, path: &Path, why: &str) {
        self.skipped += 1;
        log_warn!(
            None,
            "daily snapshot {}: not written: {}",
            path.display(),
            why
        );
        crate::events::push(
            crate::events::EventKind::StateFile,
            &format!("{}: not written: {}", path.display(), why),
        );
    }

    /// 書いた数・書けなかった数・予算で見送った数。
    pub fn counts(&self) -> (u64, u64, u64) {
        (self.written, self.errors, self.skipped)
    }

    /// 書き先 (`PROXY_SNAPSHOT_DAYS=0` と `PROXY_STATS_PERSIST=off` では `None`)。
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }
}

/// 書いてよいか (ディスクの予算。T12.5 の `keep_free` を割り込まない)。
///
/// 空きが測れない環境 (`statvfs` の無い OS) では**今までどおり書く**
/// (キャッシュの予算が `FALLBACK_DISK` に倒すのと同じ方針)。
fn fits(free: Option<u64>, keep_free: u64, need: u64) -> bool {
    match free {
        None => true,
        Some(free) => free >= need.saturating_add(keep_free),
    }
}

fn mib(b: u64) -> String {
    format!("{} MiB", b / (1024 * 1024))
}

/// 一時ファイルに書いてから置き換える (読む側に途中の JSON を見せない)。
fn write_file(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, body.as_bytes())?;
    fs::rename(&tmp, path)
}

/// 置いてある日付 (古い順)。`<YYYY-MM-DD>.json` だけを見る。
fn existing(dir: &Path) -> Vec<String> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut days: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let date = name.strip_suffix(EXT)?;
            is_date(date).then(|| date.to_string())
        })
        .collect();
    // 名前が `YYYY-MM-DD` なので、文字列の並びがそのまま日付の並び
    days.sort_unstable();
    days
}

/// いちばん新しいファイルの日 (無ければ 0)。
fn newest_day(dir: &Path) -> u64 {
    existing(dir)
        .last()
        .and_then(|d| day_of(d))
        .unwrap_or_default()
}

/// `days` 個を超えたぶんを**古い方から**消す。消した数を返す。
fn prune(dir: &Path, days: usize) -> usize {
    let all = existing(dir);
    let over = all.len().saturating_sub(days);
    let mut dropped = 0;
    for date in all.into_iter().take(over) {
        let path = dir.join(format!("{}{}", date, EXT));
        match fs::remove_file(&path) {
            Ok(()) => dropped += 1,
            Err(e) => log_warn!(
                None,
                "daily snapshot {}: cannot drop: {}",
                path.display(),
                e
            ),
        }
    }
    dropped
}

/// `/snapshots` に返す一覧の 1 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    /// `YYYY-MM-DD` (その日の分)
    pub date: String,
    pub bytes: u64,
    /// 書いた時刻 (mtime。epoch 秒。読めなければ 0)
    pub t: u64,
}

/// `/snapshots` に返す中身。
pub struct Listing {
    /// 置き場所 (`PROXY_STATS_PERSIST=off` と `PROXY_SNAPSHOT_DAYS=0` なら `None`)
    pub dir: Option<PathBuf>,
    /// 残す日数 (`PROXY_SNAPSHOT_DAYS`)
    pub days: usize,
    /// 置いてあるもの (**古い順**。`/daily` と同じ並び)
    pub files: Vec<Saved>,
    /// 合計のバイト数
    pub bytes: u64,
}

static WRITER: OnceLock<Mutex<Writer>> = OnceLock::new();
/// [`configure`] 済みか。**[`tick`] を原子の読み 1 回で素通しさせるための旗**
/// (`PROXY_STATS_PERSIST=off` / `PROXY_SNAPSHOT_DAYS=0` と、履歴スレッドを回す試験で鍵を取らない)。
static ENABLED: AtomicBool = AtomicBool::new(false);
/// **テスト用の口**: 標本の時刻に足す秒数 ([`shift_days_for_test`])。
static SHIFT: AtomicU64 = AtomicU64::new(0);

fn writer() -> &'static Mutex<Writer> {
    WRITER.get_or_init(|| Mutex::new(Writer::new(None, 0)))
}

/// 置き場所と日数を決める (起動時に 1 回。`PROXY_STATS_PERSIST=off` では呼ばない)。
pub fn configure(dir: Option<PathBuf>, days: usize) {
    let w = Writer::new(dir, days);
    let on = w.dir.is_some();
    *writer().locked() = w;
    ENABLED.store(on, Ordering::Relaxed);
}

/// **history スレッドの周期から 1 回だけ呼ぶ** (5 秒に 1 回)。
///
/// [`configure`] していなければ原子の読み 1 回で戻る。組むのも空きを測るのも
/// **日付の変わり目のときだけ**なので、ふだんの費用は割り算 1 回。
pub fn tick(cache: &Cache, s: &Sample) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let t = s.t.saturating_add(SHIFT.load(Ordering::Relaxed));
    writer().locked().add(t, build, |dir| {
        // T12.5 のディスクのマージン (プローブが決めた「空けておく量」)。
        // キャッシュを切っている設定では 0 = 空きがある限り書く
        (free_bytes(dir), cache.snapshot().disk_keep_free)
    });
}

/// 書き先の空き (バイト)。まだディレクトリが無ければその親で測る。
fn free_bytes(dir: &Path) -> Option<u64> {
    crate::sysinfo::fs_info(dir)
        .or_else(|| dir.parent().and_then(crate::sysinfo::fs_info))
        .map(|f| f.available)
}

/// `/snapshots` の一覧。
pub fn list() -> Listing {
    let w = writer().locked();
    let (dir, days) = (w.dir.clone(), w.days);
    drop(w);
    let Some(dir) = dir else {
        return Listing {
            dir: None,
            days,
            files: Vec::new(),
            bytes: 0,
        };
    };
    let files: Vec<Saved> = existing(&dir)
        .into_iter()
        .map(|date| {
            let meta = fs::metadata(dir.join(format!("{}{}", date, EXT))).ok();
            Saved {
                date,
                bytes: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                t: meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            }
        })
        .collect();
    let bytes = files.iter().map(|f| f.bytes).sum();
    Listing {
        dir: Some(dir),
        days,
        files,
        bytes,
    }
}

/// `/snapshots/<date>` の中身 (そのままの JSON)。無ければ `None`。
///
/// 読むのは `<YYYY-MM-DD>.json` だけで、[`MAX_FILE`] を超えるファイル (手で置かれたもの) は
/// 読まない。
pub fn read(date: &str) -> Option<String> {
    if !is_date(date) {
        return None;
    }
    let dir = writer().locked().dir.clone()?;
    let path = dir.join(format!("{}{}", date, EXT));
    let len = fs::metadata(&path).ok()?.len();
    if len > MAX_FILE {
        log_warn!(
            None,
            "snapshot {}: {} B is too big to serve",
            path.display(),
            len
        );
        return None;
    }
    fs::read_to_string(&path).ok()
}

/// **テスト用の口**: 標本の時刻に `days` 日を足して日付の変わり目を作る (1 日待たないため)。
///
/// 本番では誰も呼ばない (`0` のまま = 標本の時刻そのもの)。T14.20 の単体テストが
/// `Writer` に好きな時刻を渡すのと同じ狙いで、**実バイナリの配線 (履歴スレッド →
/// `/snapshot` の組み立て → ファイル) を通したまま**境目だけを差し替える。
#[doc(hidden)]
pub fn shift_days_for_test(days: u64) {
    SHIFT.store(days.saturating_mul(SECS_PER_DAY), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = SECS_PER_DAY;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rhp-t1434-{}-{}-{:?}",
            name,
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&p);
        p
    }

    /// 組んだことにする `/snapshot` (中身は形だけ)。
    fn body() -> Option<String> {
        Some("{\"taken_at\":1,\"parts\":[\"status\"],\"status\":{}}".to_string())
    }

    /// 空きは潤沢 (予算では断らない)。
    fn roomy(_dir: &Path) -> (Option<u64>, u64) {
        (Some(64 * 1024 * 1024 * 1024), 0)
    }

    fn dates(dir: &Path) -> Vec<String> {
        existing(dir)
    }

    #[test]
    fn a_file_is_written_when_the_utc_day_changes() {
        let dir = tmp("rollover");
        let mut w = Writer::new(Some(dir.clone()), DEFAULT_DAYS);
        let base = 20_000 * DAY;
        // 同じ日のあいだは 1 つも書かない
        for i in 0..4 {
            assert_eq!(w.add(base + 86_000 + i * 5, body, roomy), None);
        }
        assert!(dates(&dir).is_empty(), "{:?}", dates(&dir));
        // 日が変わった最初の標本で 1 ファイル (**終わった日**の名前)
        let path = w.add(base + DAY + 5, body, roomy).expect("書かれていない");
        assert_eq!(path.file_name().unwrap(), "2024-10-04.json");
        assert_eq!(dates(&dir), vec!["2024-10-04"]);
        assert_eq!(w.counts(), (1, 0, 0));
        // 同じ日のうちは増えない
        assert_eq!(w.add(base + DAY + 10, body, roomy), None);
        assert_eq!(dates(&dir).len(), 1);
        // 次の日も 1 つ増える
        w.add(base + 2 * DAY, body, roomy).expect("2 日目");
        assert_eq!(dates(&dir), vec!["2024-10-04", "2024-10-05"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn starting_twice_on_the_same_day_keeps_one_file() {
        let dir = tmp("twice");
        let base = 20_100 * DAY;
        let mut first = Writer::new(Some(dir.clone()), DEFAULT_DAYS);
        first.add(base + 100, body, roomy);
        first.add(base + DAY + 100, body, roomy).expect("1 つ目");
        assert_eq!(dates(&dir).len(), 1);
        // 同じ日をもう一度またぐ起動 (置いてある日付を見て書かない)
        let mut second = Writer::new(Some(dir.clone()), DEFAULT_DAYS);
        second.add(base + 200, body, roomy);
        assert_eq!(second.add(base + DAY + 200, body, roomy), None);
        assert_eq!(dates(&dir).len(), 1, "{:?}", dates(&dir));
        assert_eq!(second.counts(), (0, 0, 0));
        // その次の日はちゃんと増える
        second.add(base + 2 * DAY, body, roomy).expect("次の日");
        assert_eq!(dates(&dir).len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 31 個目を書いたら最古が消える (`PROXY_SNAPSHOT_DAYS` 既定 30)。
    #[test]
    fn the_oldest_is_dropped_at_the_thirty_first() {
        let dir = tmp("keep30");
        let base = 20_200 * DAY;
        let mut w = Writer::new(Some(dir.clone()), DEFAULT_DAYS);
        w.add(base, body, roomy);
        for i in 1..=30 {
            w.add(base + i * DAY, body, roomy).expect("毎日 1 つ");
        }
        let all = dates(&dir);
        assert_eq!(all.len(), 30, "{:?}", all);
        assert_eq!(all.first(), Some(&day_name(20_200)));
        // 31 個目 (= 通算 31 日目) で最古が 1 つ消えて 30 のまま
        w.add(base + 31 * DAY, body, roomy).expect("31 個目");
        let all = dates(&dir);
        assert_eq!(all.len(), 30, "{:?}", all);
        assert_eq!(all.first(), Some(&day_name(20_201)));
        assert_eq!(all.last(), Some(&day_name(20_230)));
        assert_eq!(w.counts(), (31, 0, 0));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `PROXY_SNAPSHOT_DAYS=0` は止める (書き先を持たない)。
    #[test]
    fn zero_days_writes_nothing() {
        let dir = tmp("zero");
        let mut w = Writer::new(Some(dir.clone()), 0);
        assert!(w.dir().is_none());
        let base = 20_300 * DAY;
        w.add(base, body, roomy);
        assert_eq!(w.add(base + DAY, body, roomy), None);
        assert!(!dir.exists(), "ディレクトリを作っている");
        // 書き先が無いときも同じ
        let mut none = Writer::new(None, DEFAULT_DAYS);
        none.add(base, body, roomy);
        assert_eq!(none.add(base + DAY, body, roomy), None);
        assert_eq!(none.counts(), (0, 0, 0));
    }

    /// ディスクの予算 (T12.5 の `keep_free`) を越えるなら書かない。
    #[test]
    fn the_disk_budget_stops_the_write() {
        // 見送ったことを `/events` に 1 件残すので、リング 1 本きりの鍵を取る
        let _g = crate::events::TEST_LOCK.locked();
        let dir = tmp("budget");
        let base = 20_400 * DAY;
        let mut w = Writer::new(Some(dir.clone()), DEFAULT_DAYS);
        w.add(base, body, roomy);
        // 空き 1 MiB・空けておく量 1 MiB なので 1 バイトも書けない
        let tight = |_: &Path| (Some(1024 * 1024), 1024 * 1024);
        assert_eq!(w.add(base + DAY, body, tight), None);
        assert!(dates(&dir).is_empty(), "{:?}", dates(&dir));
        assert_eq!(w.counts(), (0, 0, 1));
        // 次の日に空きが戻れば書く (見送った日はもう来ない)
        w.add(base + 2 * DAY, body, roomy)
            .expect("空きが戻ったら書く");
        assert_eq!(dates(&dir), vec![day_name(20_401)]);
        // 判定そのもの
        assert!(fits(None, 1 << 30, 4 << 20), "測れない環境では書く");
        assert!(fits(Some(100), 0, 100));
        assert!(!fits(Some(99), 0, 100));
        assert!(!fits(Some(1 << 30), 1 << 30, 1));
        let _ = fs::remove_dir_all(&dir);
    }

    /// 組み方を預かっていなければ書かない (依存の輪を作らないための形)。
    #[test]
    fn nothing_is_written_without_a_builder() {
        let dir = tmp("nobuilder");
        let base = 20_500 * DAY;
        let mut w = Writer::new(Some(dir.clone()), DEFAULT_DAYS);
        w.add(base, || None, roomy);
        assert_eq!(w.add(base + DAY, || None, roomy), None);
        assert!(dates(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// 日付の名前は往復できる (`day_name` ↔ `day_of`)、読めない名前は入れない。
    #[test]
    fn dates_round_trip_and_bad_names_are_refused() {
        for day in [0u64, 1, 19_000, 20_713, 40_000] {
            let name = day_name(day);
            assert!(is_date(&name), "{}", name);
            assert_eq!(day_of(&name), Some(day), "{}", name);
        }
        // 目印 (T14.20 の `daily` の単体テストと同じ日)
        assert_eq!(day_name(20_000), "2024-10-04");
        for bad in [
            "",
            "2026-9-16",
            "2026-09-16.json",
            "../../etc/passwd",
            "2026-09-1x",
            "2026/09/16",
            "2026-13-01",
            "2026-09-32",
        ] {
            assert!(day_of(bad).is_none(), "{}", bad);
        }
        assert!(!is_date("../../etc"));
        // 一覧に入るのは `<YYYY-MM-DD>.json` だけ
        let dir = tmp("names");
        fs::create_dir_all(&dir).unwrap();
        for name in [
            "2026-09-15.json",
            "2026-09-16.json",
            "notes.txt",
            "2026-09-17.json.tmp",
            "snapshot.json",
        ] {
            fs::write(dir.join(name), "{}").unwrap();
        }
        assert_eq!(dates(&dir), vec!["2026-09-15", "2026-09-16"]);
        assert_eq!(newest_day(&dir), day_of("2026-09-16").unwrap());
        assert_eq!(newest_day(Path::new("/nonexistent/rhp-t1434")), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 大域の口 ([`configure`] / [`tick`] / [`list`] / [`read`]) も 1 度通しておく
    /// (**この単体テストだけが大域に触る**)。
    #[test]
    fn the_global_writer_records_and_reads_back() {
        let dir = tmp("global");
        let cache = Cache::new(proxy_cache::config::CacheConfig::disabled());
        let sample = |t: u64| Sample {
            t,
            ..Sample::default()
        };
        set_builder(Box::new(|| {
            "{\"taken_at\":1,\"parts\":[\"status\"]}".to_string()
        }));
        configure(Some(dir.clone()), DEFAULT_DAYS);
        let base = 20_600 * DAY;
        tick(&cache, &sample(base + 10));
        assert!(list().files.is_empty(), "日が変わるまでは空");
        tick(&cache, &sample(base + DAY + 10));
        let l = list();
        assert_eq!(l.days, DEFAULT_DAYS);
        assert_eq!(l.dir.as_deref(), Some(dir.as_path()));
        assert_eq!(l.files.len(), 1);
        assert_eq!(l.files[0].date, day_name(20_600));
        assert!(l.files[0].bytes > 0 && l.bytes == l.files[0].bytes);
        assert!(read(&day_name(20_600)).unwrap().contains("\"taken_at\""));
        assert!(read(&day_name(20_599)).is_none(), "置いていない日");
        assert!(read("../../etc/passwd").is_none());
        // 書かない設定に戻すと `tick` は原子の読み 1 回で戻り、`list` は空
        configure(None, DEFAULT_DAYS);
        tick(&cache, &sample(base + 2 * DAY));
        assert!(list().dir.is_none() && list().files.is_empty());
        assert!(read(&day_name(20_600)).is_none());
        assert_eq!(dates(&dir).len(), 1, "置いてあるものには手を付けない");
        let _ = fs::remove_dir_all(&dir);
    }
}
