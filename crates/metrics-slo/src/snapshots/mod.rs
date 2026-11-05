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

//!
//! **T.34.2 で 4 ファイルに割った**: 名前の往復 (`YYYY-MM-DD` / `<時刻>-start|shutdown`) は
//! `snapshots/names.rs`、書くもの ([`Writer`]) は `snapshots/writer.rs`、置き場所の一覧と間引きは
//! `snapshots/store.rs`。ここには大域の口 ([`configure`] / [`tick`] / [`list`] / [`read`] …) と
//! `/snapshot` を組む閉包の預かり ([`set_builder`]) だけを置く。行を動かしただけで中身は変えていない。
//! `pub use` で出し直すので、外からの `snapshots::…` の経路は変わらない。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::cache::Cache;
use crate::history::Sample;
use crate::log_warn;
use crate::sync::LockExt;

mod names;
mod store;
mod writer;

pub use names::{Moment, day_name};
pub(crate) use names::{is_date, parse_moment};
pub use store::{Saved, SavedMoment};
pub use writer::Writer;

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
pub(super) const EXT: &str = ".json";
pub(super) const SECS_PER_DAY: u64 = 86_400;

/// 既定の置き場所 (`$HOME/.rust-http-proxy/snapshots/`)。`$HOME` が無ければ `None`。
pub fn default_dir() -> Option<PathBuf> {
    crate::envfile::env_path().map(|p| p.with_file_name(DIR_NAME).join(SUB_DIR))
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
    let on = w.dir().is_some();
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

/// `/snapshots` の一覧 (組み立ては `store.rs`。T.34.51)。
pub fn list() -> Listing {
    let w = writer().locked();
    let (dir, days) = (w.dir().map(Path::to_path_buf), w.days());
    drop(w);
    let (files, moments) = match &dir {
        Some(d) => (store::saved(d), store::saved_moments(d)),
        None => (Vec::new(), Vec::new()),
    };
    Listing {
        bytes: files.iter().map(|f| f.bytes).sum(),
        moments_bytes: moments.iter().map(|f| f.bytes).sum(),
        dir,
        days,
        files,
        moments,
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
    let dir = writer().locked().dir().map(Path::to_path_buf)?;
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

/// `snapshots` の単体試験 (T.34.2 で別のファイルに移した)。
#[cfg(test)]
mod tests;
