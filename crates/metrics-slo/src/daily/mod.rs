//! 日次の要約を永久に残す (`$HOME/.rust-http-proxy.daily.jsonl`。T14.20)。
//!
//! `/history` は 30 日 (1 時間 × 720) で消える。1 日 1 行の要約なら 1 年で 100 KB に
//! 収まるので、**「いつから遅くなったか」「デプロイの前後で何が変わったか」**を
//! 年単位で追える。
//!
//! - 書くのは **history スレッドだけ** ([`tick`] は 5 秒に 1 回、加算が十数本)。
//!   利用者の要求の経路には 1 命令も足していない (費用 0)。
//! - **UTC の日付が変わった標本**で、終わった日の 1 行を追記する。
//!   1 行 ≤ [`MAX_LINE`] バイト、ファイルは**追記のみ**で上限 [`MAX_BYTES`]
//!   (越えたら古い行から捨てる。1 行 512 B なら 4,096 日 = 11 年ぶん入る)。
//! - 起動時に**最後の行の日付**を覚え ([`Writer::new`])、同じ日を 2 度書かない
//!   (同じ日に 2 回起動しても 1 行のまま)。
//! - `PROXY_STATS_PERSIST=off` では [`configure`] を呼ばないので 1 行も書かない。
//! - 読む口は `/daily?n=365` ([`recent`])。
//!
//! 累計 (要求数・バイト・追い出し・山の枚数) は**日の境目の値の差**、区間の値
//! (確立時間の分布・エラー・名前解決) は**その日の標本の足し合わせ**、ゲージ (同時接続数・
//! RSS) は**最大と平均**。プロセスが動いていた時間だけが入るので、`secs` と `samples` で
//! その日をどれだけ見ていたかが分かる。
//!
//! **再起動した日も前のプロセスのぶんを落とさない** (T.32.18): 積み上げはプロセスのメモリの中にしか
//! 無いので、前は再起動した日の行が「再起動のあとのぶん」だけになっていた (tokyo の 10-28 は 0.29.36 の
//! 19.3 時間が抜けて `secs` 16,819)。起動して最初の標本で、`.rrd` から読み戻した `history["60"]` の
//! **UTC 0 時から**の行で今日の積み上げを組み直す ([`Writer::seed`])。`version` は「その日に動いた版」を
//! 古い順に空白で並べる (`/events` の `start` から。1 つしか無ければ今までどおり 1 つ)。
//! 前のプロセスの最後の 1 分 (窓が閉じる前に止まったぶん) と、山の写真の枚数 (`bursts`) は戻らない。

mod acc;
mod file;
mod versions;
mod writer;

#[cfg(test)]
mod tests;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::history::Sample;
use crate::metrics::Metrics;
use crate::sync::LockExt;

pub use writer::Writer;

/// 1 行の上限 (バイト。改行を含まない)。T.29.33 で `slo_rate_hours` を足したので 512 → 640
/// (最悪の行が 491 B で、足す 80 B 弱が入らなかった。上限 2 MiB でも 3,276 日 = 9 年ぶん入る)。
/// T.32.17 の 2 欄と T.32.18 の版の並び ([`MAX_VERSIONS`]) で 640 → 672 (最悪 669 B。`/daily` の
/// 240 KiB に 1 年 = 365 行がまだ入る。2 MiB は 3,120 日 = 8 年ぶん)。
pub const MAX_LINE: usize = 672;
/// ファイルの上限 (バイト)。越えたら古い行から捨てる。
pub const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// `/daily?n=` の既定 (1 年) と上限。
pub const DEFAULT_DAYS: usize = 365;
pub const MAX_DAYS: usize = 4096;
/// 既定のファイル名 (`$HOME` に置く。状態ファイル `.rrd` と同じ場所)。
pub(crate) const FILE_NAME: &str = ".rust-http-proxy.daily.jsonl";

/// `/daily` の本体に載せる上限。個票の 256 KiB
/// (`endpoints::recent::MAX_BODY`) に包みのぶんを残した値。
const MAX_JSON: usize = 240 * 1024;
/// 版の文字列をこの長さで切る (1 行の上限を必ず守るため)。
const MAX_VERSION: usize = 40;
/// その日に動いた版の並び (空白で区切る。T.32.18) の長さの上限。越えたら古い版から落とす
/// (今の版は必ず残る)。ふつうの版 (`0.30.4+ba512ad`) なら 5 つ入る。
const MAX_VERSIONS: usize = 2 * MAX_VERSION;
/// 組み直しに使う解像度 (`history["60"]`。T.32.18)。
const SEED_RES: usize = 1;
const SECS_PER_DAY: u64 = 86_400;
/// 数を 15 桁で頭打ちにする (1 行 ≤ [`MAX_LINE`] を必ず守るため。現実の値は 10 桁以内)。
const MAX_NUM: u64 = 999_999_999_999_999;
/// 小数も同じ理由で頭打ちにする。
const MAX_FLOAT: f64 = 999_999.9;

/// 既定の場所 (`$HOME/.rust-http-proxy.daily.jsonl`)。
pub fn default_path() -> Option<PathBuf> {
    crate::envfile::env_path().map(|p| p.with_file_name(FILE_NAME))
}

/// `/daily` に返す中身。
pub struct Daily {
    /// 1 日 1 行の JSON (**古い順**。そのまま配列に並べられる)
    pub lines: Vec<String>,
    /// ファイルにある全行数
    pub count: usize,
    /// ファイルの大きさ (バイト)
    pub bytes: u64,
    /// 件数か大きさで古い行を落としたか
    pub truncated: bool,
    /// 書いている場所 (`PROXY_STATS_PERSIST=off` なら `None`)
    pub path: Option<PathBuf>,
}

static WRITER: OnceLock<Mutex<Writer>> = OnceLock::new();
/// [`configure`] 済みか。**[`tick`] を原子の読み 1 回で素通しさせるための旗**
/// (`PROXY_STATS_PERSIST=off` と、履歴スレッドを回す試験で鍵を取らない)。
static ENABLED: AtomicBool = AtomicBool::new(false);

fn writer() -> &'static Mutex<Writer> {
    WRITER.get_or_init(|| Mutex::new(Writer::new(None, "")))
}

/// 書く場所と版を決める (起動時に 1 回。`PROXY_STATS_PERSIST=off` では呼ばない)。
pub fn configure(path: Option<PathBuf>, version: &str) {
    let w = Writer::new(path, version);
    let on = w.path().is_some();
    *writer().locked() = w;
    ENABLED.store(on, Ordering::Relaxed);
}

/// **history スレッドの周期から 1 回だけ呼ぶ** (5 秒に 1 回)。
///
/// [`configure`] していなければ原子の読み 1 回で戻る。
pub fn tick(metrics: &Metrics, s: &Sample) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let bursts = metrics.bursts.next_seq().saturating_sub(1);
    let mut w = writer().locked();
    if !w.seeded() {
        // 起動して最初の標本: 読み戻した `history["60"]` で今日を組み直す (T.32.18。1 回だけ)。
        // `/events` (512 件まで) を写すのもこの 1 回だけ
        let (events, _) = crate::events::select(0, usize::MAX);
        w.seed(&metrics.history, s, bursts, &events);
    }
    w.add(s, bursts);
}

/// `/daily?n=` の中身 (新しい方から `n` 日ぶん、並びは古い順)。
pub fn recent(n: usize) -> Daily {
    let Some(path) = writer().locked().path().map(Path::to_path_buf) else {
        return Daily {
            lines: Vec::new(),
            count: 0,
            bytes: 0,
            truncated: false,
            path: None,
        };
    };
    let all = file::read_lines(&path);
    let count = all.len();
    let (lines, truncated) = file::newest_within(all, n, MAX_JSON);
    Daily {
        lines,
        count,
        bytes: fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
        truncated,
        path: Some(path),
    }
}
