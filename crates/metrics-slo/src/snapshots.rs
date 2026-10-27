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
//! **起動と停止の 1 枚** (T.28.20): 締めの判定の「前」(再デプロイの直前) と「0 時間」(直後) は
//! 撮った機械にしか残らなかったので、同じ置き場所に **`<YYYY-MM-DDTHHMMSSZ>-shutdown.json`**
//! (停止シグナルで状態ファイルを書く前。[`take_shutdown`]) と **`-start.json`** (起動から
//! `memory.baseline` を書くのと同じ 60 秒。[`take_start_if_due`]) も残す ([`Moment`])。
//! 名前の時刻は撮った時刻 (UTC)。日次の 1 枚とは数え方を分け (`/snapshots` の `moments`)、
//! 残すのは [`Moment`] の種類ごとに `PROXY_SNAPSHOT_DAYS` 日以内かつ新しい方から
//! `PROXY_SNAPSHOT_DAYS` 枚まで (再起動を繰り返しても置き場所を埋めない)。
//! **停止の 1 枚は `recent` を 256 KiB で切らずに全部入れ、日次の 1 枚は 1 MiB まで持つ**
//! (T.28.20・T.28.46。組み方は上の層。呼ぶときに `recent` の部の上限を渡す)。
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
/// 起動と停止の 1 枚 (T.28.20) はこれ。**日次の 1 枚だけ** [`DAILY_MAX_FILE`] (T.29.16)。
pub const MAX_FILE: u64 = 4 * 1024 * 1024;

/// **日次の 1 枚だけ**の上限 (6 MiB。T.29.16)。
///
/// 0.28.48 が書いた日次 `2026-10-26` は 3,692,328 B (4 MiB の 88%。`history` 1,550,971 B は T.28.45 の 17 項目で
/// 増え、`recent` 896,914 B は T.28.46 の 2,000 行)。4 MiB を越えると `recent` を 256 KiB (約 580 行) に戻し、
/// T.28.18 の問題 (締めの「前」の接続のエラーの行が「判定できず」) に黙って戻るので、日次だけ上限を上げて
/// 余白を作る (3.7 MB は 6 MiB の 59%)。`/snapshot` (要求で組む 1 枚) と起動・停止の 1 枚は 4 MiB のまま
pub const DAILY_MAX_FILE: u64 = 6 * 1024 * 1024;

/// **日次の 1 枚だけ**の `recent` の部の上限 (1 MiB。T.28.46、利用者の決定 10)。
///
/// いつもの `/snapshot` は部ごと 256 KiB で切るので、日次の `recent` は約 580 行で環の
/// 2,000 件を割り、締めの「前」に日次を使うと接続のエラーの行 (試行 1,000 本が下限) が
/// 「判定できず」になった (T.28.18)。1 MiB なら 1 件 400〜500 B の行で環が全部入る。
/// 全体が 4 MiB ([`MAX_FILE`]) を越えるなら、停止の 1 枚 (T.28.20) と同じく**切った `recent` に戻す**
pub const DAILY_RECENT_MAX: usize = 1024 * 1024;
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
///
/// 1 つ目の引数は **`recent` の部の上限** (`None` = `/snapshot` と同じく部ごと 256 KiB で切る。起動の 1 枚)。
/// 停止の 1 枚は [`MAX_FILE`] (= 2,000 件を全部。T.28.20)、日次は [`DAILY_RECENT_MAX`] (T.28.46)。
/// 2 つ目は**全体の上限** (日次は [`DAILY_MAX_FILE`]、ほかは [`MAX_FILE`]。T.29.16)。
/// `Some` のときは、全体が上限を越えるなら切った `recent` に戻す。
pub type Builder = Box<dyn Fn(Option<usize>, usize) -> String + Send + Sync>;

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

/// 預かった閉包で `/snapshot` を 1 枚組む (**日付の変わり目と起動・停止のときだけ**呼ぶ)。
fn build(recent_max: Option<usize>, limit: u64) -> Option<String> {
    builder()
        .locked()
        .as_ref()
        .map(|f| f(recent_max, limit as usize))
}

/// 起動と停止の 1 枚の種類 (T.28.20)。名前は `<YYYY-MM-DDTHHMMSSZ>-<種類>.json`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// 起動から `memory.baseline` と同じ 60 秒 (締めの「0 時間」)
    Start,
    /// 停止シグナルで状態ファイルを書く前 (締めの「前」)
    Shutdown,
}

impl Moment {
    pub const ALL: [Moment; 2] = [Moment::Start, Moment::Shutdown];

    /// 名前の終わりと `/snapshots` の `kind` に出す綴り。
    pub fn name(self) -> &'static str {
        match self {
            Moment::Start => "start",
            Moment::Shutdown => "shutdown",
        }
    }
}

/// 撮った時刻と種類から名前 (拡張子なし) を作る: `2026-10-25T203134Z-shutdown`。
///
/// 時刻の書き方は `collect-deployed.sh` の雪像 (`<UTC 時刻>-snapshot.json`) と同じ。
pub fn moment_name(t: u64, m: Moment) -> String {
    let (y, mo, d, h, mi, s) = crate::log::civil_from_epoch(t);
    format!(
        "{:04}-{:02}-{:02}T{:02}{:02}{:02}Z-{}",
        y,
        mo,
        d,
        h,
        mi,
        s,
        m.name()
    )
}

/// `<YYYY-MM-DDTHHMMSSZ>-start|shutdown` として読めれば (撮った時刻, 種類)。
///
/// `/snapshots/<名前>` はこれ (か [`is_date`]) を通った文字列しかファイル名に使わない
/// (`..` や `/` を書かれても置き場所の外へ出ない)。
pub fn parse_moment(name: &str) -> Option<(u64, Moment)> {
    let (stamp, kind) = name.split_once("Z-")?;
    let m = Moment::ALL.into_iter().find(|m| m.name() == kind)?;
    let (date, time) = stamp.split_once('T')?;
    let day = day_of(date)?;
    if time.len() != 6 || !time.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (h, mi, s): (u64, u64, u64) = (
        time[0..2].parse().ok()?,
        time[2..4].parse().ok()?,
        time[4..6].parse().ok()?,
    );
    if h > 23 || mi > 59 || s > 59 {
        return None;
    }
    Some((day * SECS_PER_DAY + h * 3600 + mi * 60 + s, m))
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
    /// 起動と停止の 1 枚を書いた数 (T.28.20)
    moments: u64,
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
            moments: 0,
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
        if !self.put(&dir, &path, &body, DAILY_MAX_FILE, space) {
            return None;
        }
        self.last_written = finished;
        self.written += 1;
        let dropped = prune(&dir, self.days);
        log_info!(
            None,
            "daily snapshot {} ({} B, keeping {} days{})",
            path.display(),
            body.len(),
            self.days,
            if dropped > 0 {
                format!(", dropped {} older", dropped)
            } else {
                String::new()
            }
        );
        Some(path)
    }

    /// 起動と停止の 1 枚 (T.28.20) を `t` (撮った時刻) の名前で書く。戻り値は書いたファイル。
    ///
    /// 書いたら同じ種類の古いもの ([`prune_moments`]) を消す。書き先が無い設定
    /// (`PROXY_SNAPSHOT_DAYS=0` / `PROXY_STATS_PERSIST=off`) では `build` を呼ばない。
    pub fn write_moment(
        &mut self,
        t: u64,
        m: Moment,
        build: impl FnOnce() -> Option<String>,
        space: impl FnOnce(&Path) -> (Option<u64>, u64),
    ) -> Option<PathBuf> {
        let dir = self.dir.clone()?;
        let Some(body) = build() else {
            log_debug!(None, "{} snapshot: no snapshot builder", m.name());
            return None;
        };
        let path = dir.join(format!("{}{}", moment_name(t, m), EXT));
        if !self.put(&dir, &path, &body, MAX_FILE, space) {
            return None;
        }
        self.moments += 1;
        let dropped = prune_moments(&dir, self.days, m, t);
        log_info!(
            None,
            "{} snapshot {} ({} B, keeping {} days{})",
            m.name(),
            path.display(),
            body.len(),
            self.days,
            if dropped > 0 {
                format!(", dropped {} older", dropped)
            } else {
                String::new()
            }
        );
        Some(path)
    }

    /// 大きさとディスクの予算を確かめてから 1 ファイル書く (日次と起動・停止で共通)。
    fn put(
        &mut self,
        dir: &Path,
        path: &Path,
        body: &str,
        limit: u64,
        space: impl FnOnce(&Path) -> (Option<u64>, u64),
    ) -> bool {
        let need = body.len() as u64;
        if need > limit {
            // `/snapshot` 側が同じ上限で切っているので本来ここへは来ない (念のための歯止め)
            self.note_skip(path, &format!("{} B is over the {} B limit", need, limit));
            return false;
        }
        let (free, keep_free) = space(dir);
        if !fits(free, keep_free, need) {
            self.note_skip(
                path,
                &format!(
                    "only {} free and {} must stay free (needs {} B)",
                    free.map(mib).unwrap_or_else(|| "?".into()),
                    mib(keep_free),
                    need
                ),
            );
            return false;
        }
        match write_file(path, body) {
            Ok(()) => true,
            Err(e) => {
                self.errors += 1;
                if self.errors == 1 {
                    log_warn!(None, "snapshot {}: write failed: {}", path.display(), e);
                    // 出来事の時系列に 1 件 (**最初の 1 回だけ**。T14.11 と同じ口)
                    crate::events::note_state_file(path, "write", &e);
                }
                false
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

    /// 起動と停止の 1 枚を書いた数 (T.28.20)。
    pub fn moments_written(&self) -> u64 {
        self.moments
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

/// 置いてある起動と停止の 1 枚 (名前の時刻の古い順)。`(名前, 撮った時刻, 種類)`。
fn existing_moments(dir: &Path) -> Vec<(String, u64, Moment)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<(String, u64, Moment)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(EXT)?;
            let (t, m) = parse_moment(stem)?;
            Some((stem.to_string(), t, m))
        })
        .collect();
    v.sort_unstable_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    v
}

/// `m` の種類のうち、`now` から `days` 日より古いものと、新しい方から `days` 枚を越えたものを消す
/// (T.28.20)。消した数を返す。**別の種類と日次の 1 枚には触らない**。
fn prune_moments(dir: &Path, days: usize, m: Moment, now: u64) -> usize {
    let oldest = now.saturating_sub(days as u64 * SECS_PER_DAY);
    let mine: Vec<(String, u64)> = existing_moments(dir)
        .into_iter()
        .filter(|(_, _, k)| *k == m)
        .map(|(name, t, _)| (name, t))
        .collect();
    let over = mine.len().saturating_sub(days);
    let mut dropped = 0;
    for (i, (name, t)) in mine.into_iter().enumerate() {
        if i >= over && t >= oldest {
            continue;
        }
        let path = dir.join(format!("{}{}", name, EXT));
        match fs::remove_file(&path) {
            Ok(()) => dropped += 1,
            Err(e) => log_warn!(None, "snapshot {}: cannot drop: {}", path.display(), e),
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

/// `/snapshots` の `moments` の 1 行 (起動と停止の 1 枚。T.28.20)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedMoment {
    /// `<YYYY-MM-DDTHHMMSSZ>-<種類>` (`/snapshots/<名前>` で読める)
    pub name: String,
    pub kind: Moment,
    pub bytes: u64,
    /// 撮った時刻 (名前の時刻。epoch 秒)
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
    /// 起動と停止の 1 枚 (**古い順**。T.28.20)
    pub moments: Vec<SavedMoment>,
    /// その合計のバイト数
    pub moments_bytes: u64,
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
    writer().locked().add(
        t,
        // 日次だけ `recent` を 1 MiB まで持ち (T.28.46)、全体は 6 MiB まで (T.29.16)
        || build(Some(DAILY_RECENT_MAX), DAILY_MAX_FILE),
        |dir| {
            // T12.5 のディスクのマージン (プローブが決めた「空けておく量」)。
            // キャッシュを切っている設定では 0 = 空きがある限り書く
            (free_bytes(dir), cache.snapshot().disk_keep_free)
        },
    );
}

/// 起動の 1 枚を撮ったか (**起動ごとに 1 回**。T.28.20)。
static START_TAKEN: AtomicBool = AtomicBool::new(false);

/// **history スレッドの周期から呼ぶ** (`memory.baseline` を書くのと同じ所。T.28.20)。
///
/// `due` は「起動から `persist::baseline_after_secs` 秒たったか」(数えるのは呼ぶ側)。
/// たっていて、まだ撮っていなければ `-start` を 1 枚書く。ふだんは原子の読み 1〜2 回で戻る。
pub fn take_start_if_due(cache: &Cache, due: bool) -> Option<PathBuf> {
    if !due || !ENABLED.load(Ordering::Relaxed) || START_TAKEN.swap(true, Ordering::Relaxed) {
        return None;
    }
    take(cache, Moment::Start, None)
}

/// **停止シグナルの後始末から 1 回だけ呼ぶ** (状態ファイルを書く前。T.28.20)。
///
/// `-shutdown` を 1 枚書く。**`recent` は部の 256 KiB で切らずに全部入れる** (締めの「前」は
/// 個票の窓が判定の分母になるのに、`--from-server` で取り寄せた 1 枚は `offset=` で続きを
/// 追えないため)。書かない設定なら原子の読み 1 回で戻る。
pub fn take_shutdown(cache: &Cache) -> Option<PathBuf> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    take(cache, Moment::Shutdown, Some(MAX_FILE as usize))
}

fn take(cache: &Cache, m: Moment, recent_max: Option<usize>) -> Option<PathBuf> {
    let t = crate::cache::now_epoch();
    writer().locked().write_moment(
        t,
        m,
        || build(recent_max, MAX_FILE),
        |dir| (free_bytes(dir), cache.snapshot().disk_keep_free),
    )
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
            moments: Vec::new(),
            moments_bytes: 0,
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
    let moments: Vec<SavedMoment> = existing_moments(&dir)
        .into_iter()
        .map(|(name, t, kind)| SavedMoment {
            bytes: fs::metadata(dir.join(format!("{}{}", name, EXT)))
                .map(|m| m.len())
                .unwrap_or(0),
            name,
            kind,
            t,
        })
        .collect();
    let moments_bytes = moments.iter().map(|f| f.bytes).sum();
    Listing {
        dir: Some(dir),
        days,
        files,
        bytes,
        moments,
        moments_bytes,
    }
}

/// `/snapshots/<date>` と `/snapshots/<起動・停止の名前>` の中身 (そのままの JSON)。無ければ `None`。
///
/// 読むのは `<YYYY-MM-DD>.json` と `<YYYY-MM-DDTHHMMSSZ>-start|shutdown.json` (T.28.20) だけで、
/// 上限 (日次は [`DAILY_MAX_FILE`]、起動・停止は [`MAX_FILE`]) を超えるファイル (手で置かれたもの) は読まない。
pub fn read(date: &str) -> Option<String> {
    if !is_date(date) && parse_moment(date).is_none() {
        return None;
    }
    let dir = writer().locked().dir.clone()?;
    let path = dir.join(format!("{}{}", date, EXT));
    let len = fs::metadata(&path).ok()?.len();
    let limit = if is_date(date) {
        DAILY_MAX_FILE
    } else {
        MAX_FILE
    };
    if len > limit {
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

    /// 起動と停止の名前は往復でき、読めない名前は入れない (T.28.20)。
    #[test]
    fn moment_names_round_trip_and_bad_names_are_refused() {
        // 2026-10-25T20:31:34Z (tokyo の T.28 の雪像の時刻)
        let t = 20_751 * DAY + 20 * 3600 + 31 * 60 + 34;
        assert_eq!(
            moment_name(t, Moment::Shutdown),
            "2026-10-25T203134Z-shutdown"
        );
        assert_eq!(moment_name(t, Moment::Start), "2026-10-25T203134Z-start");
        for m in Moment::ALL {
            assert_eq!(parse_moment(&moment_name(t, m)), Some((t, m)));
        }
        for bad in [
            "",
            "2026-10-25",
            "2026-10-25T203134Z",
            "2026-10-25T203134Z-reboot",
            "2026-10-25T2031Z-shutdown",
            "2026-10-25T243134Z-shutdown",
            "2026-10-25T206034Z-start",
            "2026-13-25T203134Z-start",
            "../2026-10-25T203134Z-start",
            "2026-10-25T203134Z-start/../../x",
            "2026-10-25T20313xZ-start",
        ] {
            assert!(parse_moment(bad).is_none(), "{}", bad);
        }
        // 一覧に入るのは形の合う名前だけ (日次の 1 枚とは混ざらない)
        let dir = tmp("moment-names");
        fs::create_dir_all(&dir).unwrap();
        for name in [
            "2026-10-25T203134Z-shutdown.json",
            "2026-10-24T010203Z-start.json",
            "2026-10-25.json",
            "2026-10-25T203134Z-shutdown.json.tmp",
            "2026-10-25T203134Z-snapshot.json",
        ] {
            fs::write(dir.join(name), "{}").unwrap();
        }
        let names: Vec<String> = existing_moments(&dir).into_iter().map(|m| m.0).collect();
        assert_eq!(
            names,
            ["2026-10-24T010203Z-start", "2026-10-25T203134Z-shutdown"]
        );
        assert_eq!(dates(&dir), vec!["2026-10-25"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// 起動・停止の 1 枚を書き、種類ごとに日数と枚数で古いものを消す。日次の 1 枚には触らない。
    #[test]
    fn moments_are_written_and_pruned_by_kind() {
        let dir = tmp("moments");
        let mut w = Writer::new(Some(dir.clone()), 3);
        let base = 20_700 * DAY;
        // 日次の 1 枚を置いておく (消されないこと)
        w.add(base, body, roomy);
        w.add(base + DAY, body, roomy).expect("日次");
        // 停止を 1 日おきに 5 回 (3 日より古いものと、新しい方から 3 枚を越えたものが消える)
        let mut written = Vec::new();
        for i in 0..5 {
            let t = base + i * DAY + 100;
            let p = w
                .write_moment(t, Moment::Shutdown, body, roomy)
                .expect("停止の 1 枚");
            assert_eq!(
                p.file_name().unwrap().to_string_lossy(),
                format!("{}.json", moment_name(t, Moment::Shutdown))
            );
            written.push(moment_name(t, Moment::Shutdown));
        }
        // 起動を同じ時刻に 4 回 (日数では消えず、枚数の 3 で 1 枚消える)
        for i in 0..4 {
            w.write_moment(base + 4 * DAY + 200 + i, Moment::Start, body, roomy)
                .expect("起動の 1 枚");
        }
        let left: Vec<(String, Moment)> = existing_moments(&dir)
            .into_iter()
            .map(|(n, _, m)| (n, m))
            .collect();
        let shutdowns: Vec<&String> = left
            .iter()
            .filter(|(_, m)| *m == Moment::Shutdown)
            .map(|(n, _)| n)
            .collect();
        // 最後に書いた時刻 (base + 4 日) から 3 日以内 = 1・2・3・4 日目のうち新しい 3 枚
        assert_eq!(shutdowns, [&written[2], &written[3], &written[4]]);
        assert_eq!(left.iter().filter(|(_, m)| *m == Moment::Start).count(), 3);
        assert_eq!(dates(&dir), vec![day_name(20_700)], "日次の 1 枚はそのまま");
        assert_eq!(w.moments_written(), 9);
        // 予算を越えるなら書かない (日次と同じ判定)
        let _g = crate::events::TEST_LOCK.locked();
        let tight = |_: &Path| (Some(1024 * 1024), 1024 * 1024);
        assert_eq!(
            w.write_moment(base + 5 * DAY, Moment::Shutdown, body, tight),
            None
        );
        // 書き先が無い設定では組みもしない
        let mut none = Writer::new(None, 3);
        assert_eq!(
            none.write_moment(base, Moment::Start, || panic!("組まない"), roomy),
            None
        );
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
        // 組み方に渡った「`recent` の部の上限」を中身に写す (T.28.20・T.28.46。`null` = 256 KiB で切る)
        set_builder(Box::new(|max, _limit| {
            format!(
                "{{\"taken_at\":1,\"parts\":[\"status\"],\"recent_max\":{}}}",
                max.map_or("null".to_string(), |n| n.to_string())
            )
        }));
        // 書かない設定のうちは起動・停止の 1 枚も書かない (組みもしない)
        configure(None, DEFAULT_DAYS);
        assert_eq!(take_shutdown(&cache), None);
        assert_eq!(take_start_if_due(&cache, true), None);
        configure(Some(dir.clone()), DEFAULT_DAYS);
        // 起動の 1 枚: 待ち時間の前は書かず、たったら 1 回だけ (`recent` は切る方)
        assert_eq!(take_start_if_due(&cache, false), None);
        let start = take_start_if_due(&cache, true).expect("起動の 1 枚");
        assert_eq!(take_start_if_due(&cache, true), None, "起動ごとに 1 回");
        assert!(
            fs::read_to_string(&start)
                .unwrap()
                .contains("\"recent_max\":null")
        );
        // 停止の 1 枚は `recent` を全部 (4 MiB まで) 入れる方で組む
        let stop = take_shutdown(&cache).expect("停止の 1 枚");
        assert!(
            fs::read_to_string(&stop)
                .unwrap()
                .contains("\"recent_max\":4194304")
        );
        let l = list();
        assert!(l.files.is_empty(), "日次の一覧には混ぜない");
        let kinds: Vec<Moment> = l.moments.iter().map(|m| m.kind).collect();
        assert!(kinds.contains(&Moment::Start) && kinds.contains(&Moment::Shutdown));
        assert_eq!(l.moments_bytes, l.moments.iter().map(|m| m.bytes).sum());
        for m in &l.moments {
            assert!(parse_moment(&m.name).is_some(), "{}", m.name);
            assert!(read(&m.name).unwrap().contains("\"taken_at\""));
        }
        for p in [&start, &stop] {
            fs::remove_file(p).unwrap();
        }
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
        // 日次の 1 枚は `recent` を 1 MiB まで持つ方で組む (T.28.46)
        assert!(
            read(&day_name(20_600))
                .unwrap()
                .contains("\"recent_max\":1048576")
        );
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
