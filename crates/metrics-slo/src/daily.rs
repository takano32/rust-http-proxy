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

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::history::{Sample, Window};
use crate::metrics::Metrics;
use crate::sync::LockExt;
use crate::{log_debug, log_info, log_warn};

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
pub const FILE_NAME: &str = ".rust-http-proxy.daily.jsonl";

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

/// 標本から読む累計 (日の境目で差を取る)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Cum {
    requests: u64,
    bytes: u64,
    evicted_idle: u64,
    /// 山の写真の通し番号 (捨てた分も含む通算。T14.6)
    bursts: u64,
}

impl Cum {
    fn take(s: &Sample, bursts: u64) -> Cum {
        Cum {
            requests: s.requests,
            bytes: s.bytes,
            evicted_idle: s.evicted_idle,
            bursts,
        }
    }
}

/// 1 日ぶんの積み上げ。
#[derive(Debug, Clone, Default)]
struct Acc {
    /// UTC の日 (epoch 秒 / 86,400)
    day: u64,
    first_t: u64,
    last_t: u64,
    samples: u64,
    start: Cum,
    end: Cum,
    connect: Window,
    errors: u64,
    dns_misses: u64,
    dns_ms_sum: u64,
    /// `dns_misses` のうち名前解決に失敗した試みのぶん (標本の `dns_fail_misses`。T.32.17)
    dns_fail_misses: u64,
    active_max: u64,
    rss_max: u64,
    rss_sum: u64,
    rss_n: u64,
    /// 組み直した前のプロセスのぶん (T.32.18): 見ていた秒 (60 秒の行の数 × 60)、要求とバイト
    /// (行の `requests_delta` / `bytes_delta` の和)、追い出し (行どうしの通算の差の和)
    seed_secs: u64,
    seed_requests: u64,
    seed_bytes: u64,
    seed_evicted: u64,
    /// その日に動いた版の並び (組み直したときだけ。`None` なら書く側の今の版)
    versions: Option<String>,
}

impl Acc {
    fn start_at(day: u64, t: u64, cum: Cum) -> Acc {
        Acc {
            day,
            first_t: t,
            last_t: t,
            start: cum,
            end: cum,
            ..Acc::default()
        }
    }

    fn add(&mut self, s: &Sample, cum: Cum) {
        self.samples += 1;
        self.last_t = self.last_t.max(s.t);
        self.end = cum;
        self.connect.merge(&s.connect);
        self.errors += s.errors;
        self.dns_misses += s.dns_misses;
        self.dns_ms_sum += s.dns_ms_sum;
        self.dns_fail_misses += s.dns_fail_misses;
        self.active_max = self.active_max.max(s.active_max).max(s.active as u64);
        // RSS が読めない環境 (`/proc` が無い) では 0 が並ぶので平均に混ぜない
        if s.rss > 0 {
            self.rss_max = self.rss_max.max(s.rss);
            self.rss_sum = self.rss_sum.saturating_add(s.rss);
            self.rss_n += 1;
        }
    }

    /// 前のプロセスの 60 秒の行 1 本を足す (T.32.18)。区間の値は 5 秒の標本と同じく足し合わせ、
    /// 標本の数は行に畳んだ 5 秒の本数 (`gauge_n`。古い行は 1)、RSS の平均はその本数で重みを付ける。
    /// `prev_evicted` は 1 本前の行の `evicted_idle` (通算なので差を足す。減ったら再起動の段差で 0)
    fn add_seed(&mut self, s: &Sample, prev_evicted: Option<u64>) {
        let n = s.gauge_n.max(1);
        self.samples += n;
        self.seed_secs += crate::history::RESOLUTIONS[SEED_RES].0;
        self.seed_requests += s.requests_delta;
        self.seed_bytes += s.bytes_delta;
        if let Some(p) = prev_evicted {
            self.seed_evicted += s.evicted_idle.saturating_sub(p);
        }
        self.connect.merge(&s.connect);
        self.errors += s.errors;
        self.dns_misses += s.dns_misses;
        self.dns_ms_sum += s.dns_ms_sum;
        self.dns_fail_misses += s.dns_fail_misses;
        self.active_max = self.active_max.max(s.active_max).max(s.active as u64);
        if s.rss > 0 {
            self.rss_max = self.rss_max.max(s.rss);
            self.rss_sum = self.rss_sum.saturating_add(s.rss.saturating_mul(n));
            self.rss_n += n;
        }
    }

    /// 要約の 1 行 (改行は付けない)。**必ず [`MAX_LINE`] バイト以下**になる
    /// (数は 15 桁・小数は 6 桁 + 1 位・版は [`MAX_VERSION`] 文字で頭打ち)。
    ///
    /// `rate_hours` は `/slo` の比の 2 つを 1 時間の合計で判定した `[満たした時間, 外した時間]`
    /// (T.29.33。その日のうちこのプロセスが動いていた時間だけ)。
    fn line(&self, version: &str, rate_hours: [[u32; 2]; crate::slo::RATES]) -> String {
        let (y, mo, d, ..) = crate::log::civil_from_epoch(self.day.saturating_mul(SECS_PER_DAY));
        let connects = self.connect.count;
        let dns_per = if connects == 0 {
            0.0
        } else {
            self.dns_misses as f64 / connects as f64
        };
        // 名前解決に成功したミス ÷ 確立 (`/slo` の `dns_miss_per_connect` と同じ数え方。T.32.17)。
        // `dns_per_connect` は今までどおり失敗した試みのミスも入れた値で、読む側の互換のため残す
        let resolved_per = if connects == 0 {
            0.0
        } else {
            self.dns_misses.saturating_sub(self.dns_fail_misses) as f64 / connects as f64
        };
        let dns_ms = if self.dns_misses == 0 {
            0.0
        } else {
            self.dns_ms_sum as f64 / self.dns_misses as f64
        };
        let rss_avg = self.rss_sum.checked_div(self.rss_n).unwrap_or(0);
        let mut out = String::with_capacity(MAX_LINE);
        let _ = write!(
            out,
            "{{\"day\":\"{:04}-{:02}-{:02}\",\"t\":{},\"secs\":{},\"samples\":{},\
             \"requests\":{},\"bytes\":{},\"connects\":{},\
             \"connect_p50_ms\":{:.1},\"connect_p95_ms\":{:.1},\
             \"dns_misses\":{},\"dns_per_connect\":{:.3},\"dns_miss_ms\":{:.1},\
             \"errors\":{},\"bursts\":{},\"active_max\":{},\"evicted_idle\":{},\
             \"rss_max\":{},\"rss_avg\":{},\"version\":\"{}\",\
             \"slo_rate_hours\":{{\"error_rate\":[{},{}],\"dns_miss_per_connect\":[{},{}]}},\
             \"dns_fail_misses\":{},\"dns_resolved_per_connect\":{:.3}}}",
            y,
            mo,
            d,
            num(self.day.saturating_mul(SECS_PER_DAY)),
            num(self
                .seed_secs
                .saturating_add(self.last_t.saturating_sub(self.first_t))),
            num(self.samples),
            num(self
                .seed_requests
                .saturating_add(self.end.requests.saturating_sub(self.start.requests))),
            num(self
                .seed_bytes
                .saturating_add(self.end.bytes.saturating_sub(self.start.bytes))),
            num(connects),
            flt(self.connect.quantile_ms(0.5)),
            flt(self.connect.quantile_ms(0.95)),
            num(self.dns_misses),
            flt(dns_per),
            flt(dns_ms),
            num(self.errors),
            num(self.end.bursts.saturating_sub(self.start.bursts)),
            num(self.active_max),
            num(self.seed_evicted.saturating_add(
                self.end
                    .evicted_idle
                    .saturating_sub(self.start.evicted_idle)
            )),
            num(self.rss_max),
            num(rss_avg),
            self.versions.as_deref().unwrap_or(version),
            rate_hours[0][0].min(99),
            rate_hours[0][1].min(99),
            rate_hours[1][0].min(99),
            rate_hours[1][1].min(99),
            // 末尾に足した欄 (T.32.17)。無い古い行は読む側が `dns_per_connect` に落ちる
            num(self.dns_fail_misses),
            flt(resolved_per),
        );
        out
    }
}

fn num(v: u64) -> u64 {
    v.min(MAX_NUM)
}

fn flt(v: f64) -> f64 {
    if v.is_finite() {
        v.clamp(0.0, MAX_FLOAT)
    } else {
        0.0
    }
}

/// 版の文字列を 1 行に入れられる形に直す (印字できる ASCII だけ、[`MAX_VERSION`] 文字)。
fn clean_version(v: &str) -> String {
    v.chars()
        .filter(|c| c.is_ascii_graphic() && *c != '"' && *c != '\\')
        .take(MAX_VERSION)
        .collect()
}

/// その日に動いた版の並び (T.32.18)。`/events` の `start` (`version <版> on port …`) から、
/// 0 時に動いていた版 (`with_midnight` のとき。0 時より前のいちばん新しい `start`) と、0 時から
/// `until` までに起動した版を古い順に並べ、最後に今の版 `current` を置く (続けて同じ版は 1 つに)。
/// 長さが [`MAX_VERSIONS`] を越えたら古い方から落とす。
fn versions_of_day(
    events: &[crate::events::Event],
    day_start: u64,
    until: u64,
    with_midnight: bool,
    current: &str,
) -> String {
    let mut starts: Vec<(u64, String)> = events
        .iter()
        .filter(|e| e.kind == crate::events::EventKind::Start)
        .filter_map(|e| {
            let v = e.text.strip_prefix("version ")?.split(' ').next()?;
            Some((e.at, clean_version(v)))
        })
        .filter(|(at, v)| *at <= until && !v.is_empty())
        .collect();
    starts.sort_by_key(|(at, _)| *at);
    let mut list: Vec<String> = Vec::new();
    if with_midnight && let Some((_, v)) = starts.iter().rev().find(|(at, _)| *at < day_start) {
        list.push(v.clone());
    }
    list.extend(
        starts
            .iter()
            .filter(|(at, _)| *at >= day_start)
            .map(|(_, v)| v.clone()),
    );
    list.push(current.to_string());
    list.dedup();
    // 今の版は必ず残す (古い方から落とす)
    while list.len() > 1 && list.iter().map(|v| v.len() + 1).sum::<usize>() - 1 > MAX_VERSIONS {
        list.remove(0);
    }
    list.join(" ")
}

/// 日次の要約を積んで、日が変わったら 1 行書くもの。
///
/// 大域の状態 ([`configure`] / [`tick`]) はこれを 1 つ持っているだけで、
/// 中身は全部ここにある (単体テストは大域に触らずこれを直に動かせる)。
pub struct Writer {
    path: Option<PathBuf>,
    version: String,
    /// 最後にファイルへ書いた日 (起動時に最後の行から読む)
    last_written: u64,
    acc: Option<Acc>,
    /// この起動で書いた行数と、書けなかった回数
    written: u64,
    errors: u64,
    /// 起動して最初の標本で組み直したか (T.32.18。1 回だけ)
    seeded: bool,
}

impl Writer {
    /// 書く場所と版を決める。**ファイルの最後の行の日付を読む** (同じ日を 2 度書かないため)。
    pub fn new(path: Option<PathBuf>, version: &str) -> Writer {
        let last_written = path.as_deref().map(last_day).unwrap_or(0);
        Writer {
            path,
            version: clean_version(version),
            last_written,
            acc: None,
            written: 0,
            errors: 0,
            seeded: false,
        }
    }

    /// 起動して最初の標本 `first` の前に、読み戻した `history["60"]` の**今日の UTC 0 時から**の行で
    /// 今日の積み上げを組み直す (T.32.18)。1 回だけ (2 回目からは何もしない)。
    ///
    /// 使う行は `0 時 <= t < first.t - 5` (`first` が持つ 5 秒の手前まで)。60 秒の行の `t` は窓の頭で、
    /// 起動してから最初の行が閉じるのは 1 分あと = 組み直す時点の行は全部前のプロセスのもの。
    /// `events` は `/events` の中身 (版の並びを作る)。行が 1 本も無ければ今までどおり (今の版だけ)。
    pub fn seed(
        &mut self,
        h: &crate::history::History,
        first: &Sample,
        bursts: u64,
        events: &[crate::events::Event],
    ) {
        if std::mem::replace(&mut self.seeded, true) || self.path.is_none() || self.acc.is_some() {
            return;
        }
        let day = first.t / SECS_PER_DAY;
        let day_start = day * SECS_PER_DAY;
        // 今のプロセスの累計の起点は最初の標本 (`add` の `None` の枝と同じ)
        let mut acc = Acc::start_at(day, first.t, Cum::take(first, bursts));
        let mut prev_evicted = None;
        let mut first_row = None;
        h.for_each_in(SEED_RES, day_start, first.t.saturating_sub(5), |s| {
            first_row.get_or_insert(s.t);
            acc.add_seed(s, prev_evicted);
            prev_evicted = Some(s.evicted_idle);
        });
        let Some(first_row) = first_row else {
            return;
        };
        // 0 時に動いていた版を入れるのは、組み直した行が今日の最初の `start` より前からあるとき
        let first_start = events
            .iter()
            .filter(|e| e.kind == crate::events::EventKind::Start && e.at >= day_start)
            .map(|e| e.at)
            .min();
        let with_midnight = first_start.is_none_or(|at| first_row < at);
        acc.versions = Some(versions_of_day(
            events,
            day_start,
            first.t,
            with_midnight,
            &self.version,
        ));
        log_info!(
            None,
            "daily summary: rebuilt today from {} s of restored history (versions {})",
            acc.seed_secs,
            acc.versions.as_deref().unwrap_or("")
        );
        self.acc = Some(acc);
    }

    /// 履歴の標本を 1 本足す。日が変わっていたら終わった日の 1 行を書く。
    ///
    /// `bursts` は山の写真の通算 (`BurstRing::next_seq() - 1`)。
    pub fn add(&mut self, s: &Sample, bursts: u64) {
        if self.path.is_none() {
            return;
        }
        let day = s.t / SECS_PER_DAY;
        let cum = Cum::take(s, bursts);
        let finished = match self.acc.as_mut() {
            Some(acc) if acc.day == day => {
                acc.add(s, cum);
                None
            }
            Some(acc) => {
                // 境目の標本の窓 (5 秒) はほとんどが前の日なので、**終わる日に足す**。
                // 累計の差もここで閉じるので、次の日は同じ値から数え始める
                acc.add(s, cum);
                self.acc.replace(Acc::start_at(day, s.t, cum))
            }
            None => {
                let mut acc = Acc::start_at(day, s.t, cum);
                acc.add(s, cum);
                self.acc = Some(acc);
                None
            }
        };
        if let Some(done) = finished {
            self.finish(&done);
        }
    }

    fn finish(&mut self, done: &Acc) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if done.day <= self.last_written {
            // 同じ日の行が既にある (同じ日に 2 回起動した / 別のプロセスが書いた)
            log_debug!(
                None,
                "daily summary {}: day {} is already there, not writing it twice",
                path.display(),
                done.day
            );
            return;
        }
        // `/slo` の比を 1 時間の合計で判定した時間の数 (T.29.33)。集計はメモリの大域にある
        let rate_hours = crate::slo::rate_hours_of_day(done.day.saturating_mul(SECS_PER_DAY));
        let line = done.line(&self.version, rate_hours);
        match append(&path, &line) {
            Ok(()) => {
                self.last_written = done.day;
                self.written += 1;
                log_info!(None, "daily summary {}: {}", path.display(), line);
            }
            Err(e) => {
                self.errors += 1;
                if self.errors == 1 {
                    log_warn!(
                        None,
                        "daily summary {}: write failed: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
    }

    /// 書いた行数 (この起動で) と、書けなかった回数。
    pub fn counts(&self) -> (u64, u64) {
        (self.written, self.errors)
    }
}

/// 1 行追記し、上限を越えたら古い行から捨てる。
fn append(path: &Path, line: &str) -> std::io::Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    let mut buf = String::with_capacity(line.len() + 1);
    buf.push_str(line);
    buf.push('\n');
    f.write_all(buf.as_bytes())?;
    f.flush()?;
    let len = f.metadata()?.len();
    drop(f);
    if len > MAX_BYTES {
        trim(path)?;
    }
    Ok(())
}

/// 上限を越えたぶんを古い行から捨てる (一時ファイルへ書いて差し替える)。
fn trim(path: &Path) -> std::io::Result<()> {
    let lines = read_lines(path);
    let mut total: u64 = lines.iter().map(|l| l.len() as u64 + 1).sum();
    let mut from = 0usize;
    while from < lines.len() && total > MAX_BYTES {
        total -= lines[from].len() as u64 + 1;
        from += 1;
    }
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        for l in &lines[from..] {
            f.write_all(l.as_bytes())?;
            f.write_all(b"\n")?;
        }
        f.flush()?;
    }
    fs::rename(&tmp, path)
}

/// 行として読めるか (外から手を入れられても壊れた JSON を返さないための歯止め)。
fn is_line(l: &str) -> bool {
    l.len() <= MAX_LINE * 2
        && l.starts_with('{')
        && l.ends_with('}')
        && !l.chars().any(|c| (c as u32) < 0x20)
}

/// ファイルの行 (古い順)。読めない行は落とす。
fn read_lines(path: &Path) -> Vec<String> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    // 外から大きくされても効かないよう、上限の 2 倍までしか見ない
    let cap = (MAX_BYTES * 2) as usize;
    let bytes = if bytes.len() > cap {
        &bytes[bytes.len() - cap..]
    } else {
        &bytes[..]
    };
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| is_line(l))
        .collect()
}

/// `"key":value` の value を切り出す (この形式の行だけを相手にする小さな読み取り)。
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{}\":", key);
    let rest = &line[line.find(&pat)? + pat.len()..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim().trim_matches('"'))
}

/// ファイルの最後の行の日 (無ければ 0)。
fn last_day(path: &Path) -> u64 {
    read_lines(path)
        .last()
        .and_then(|l| field(l, "t"))
        .and_then(|v| v.parse::<u64>().ok())
        .map(|t| t / SECS_PER_DAY)
        .unwrap_or(0)
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
    let on = w.path.is_some();
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
    if !w.seeded {
        // 起動して最初の標本: 読み戻した `history["60"]` で今日を組み直す (T.32.18。1 回だけ)。
        // `/events` (512 件まで) を写すのもこの 1 回だけ
        let (events, _) = crate::events::select(0, usize::MAX);
        w.seed(&metrics.history, s, bursts, &events);
    }
    w.add(s, bursts);
}

/// `/daily?n=` の中身 (新しい方から `n` 日ぶん、並びは古い順)。
pub fn recent(n: usize) -> Daily {
    let Some(path) = writer().locked().path.clone() else {
        return Daily {
            lines: Vec::new(),
            count: 0,
            bytes: 0,
            truncated: false,
            path: None,
        };
    };
    let all = read_lines(&path);
    let count = all.len();
    let take = n.min(count);
    let mut lines = Vec::with_capacity(take);
    let mut used = 0usize;
    let mut truncated = count > take;
    for l in all.into_iter().rev().take(take) {
        if used + l.len() + 1 > MAX_JSON {
            truncated = true;
            break;
        }
        used += l.len() + 1;
        lines.push(l);
    }
    lines.reverse();
    Daily {
        lines,
        count,
        bytes: fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
        truncated,
        path: Some(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = SECS_PER_DAY;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rhp-t1420-{}-{}-{:?}.jsonl",
            name,
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_file(&p);
        p
    }

    /// `t` 秒の標本 1 本 (要求は 5 秒で 10 本、CONNECT を 1 本確立した形)。
    fn sample(t: u64, requests: u64) -> Sample {
        let mut connect = Window::default();
        connect.observe(9);
        Sample {
            t,
            requests,
            bytes: requests * 1024,
            active: 3,
            active_max: 3,
            rss: 50 * 1024 * 1024,
            connect,
            errors: 1,
            dns_misses: 2,
            dns_ms_sum: 30,
            ..Sample::default()
        }
    }

    fn lines(path: &Path) -> Vec<String> {
        read_lines(path)
    }

    #[test]
    fn a_line_is_written_when_the_utc_day_changes() {
        let path = tmp("rollover");
        let mut w = Writer::new(Some(path.clone()), "0.1.0+test");
        // 1 日目 (day 20000) の 4 本
        let base = 20_000 * DAY;
        for i in 0..4 {
            w.add(&sample(base + 86_000 + i * 5, 100 + i * 10), 0);
        }
        assert!(lines(&path).is_empty(), "日が変わるまでは 1 行も書かない");
        // 日が変わった最初の標本で 1 行
        w.add(&sample(base + DAY + 5, 200), 2);
        let rows = lines(&path);
        assert_eq!(rows.len(), 1, "{:?}", rows);
        assert_eq!(field(&rows[0], "day"), Some("2024-10-04"));
        assert_eq!(
            field(&rows[0], "t"),
            Some((20_000 * DAY).to_string().as_str())
        );
        // 累計の差 (100 → 200) と、区間の足し合わせ (5 本 × errors 1)
        assert_eq!(field(&rows[0], "requests"), Some("100"));
        assert_eq!(field(&rows[0], "errors"), Some("5"));
        assert_eq!(field(&rows[0], "connects"), Some("5"));
        assert_eq!(field(&rows[0], "dns_misses"), Some("10"));
        assert_eq!(field(&rows[0], "bursts"), Some("2"));
        assert_eq!(field(&rows[0], "samples"), Some("5"));
        assert_eq!(field(&rows[0], "version"), Some("0.1.0+test"));
        assert!(rows[0].len() <= MAX_LINE, "{} B", rows[0].len());
        // 次の日も同じように 1 行増える (2 日目 → 3 日目)
        w.add(&sample(base + DAY + 10, 210), 2);
        w.add(&sample(base + 2 * DAY, 300), 2);
        assert_eq!(lines(&path).len(), 2);
        let _ = fs::remove_file(&path);
    }

    /// T.32.17: 名前解決に失敗した試みのミスを除いた率 (`dns_resolved_per_connect`) を末尾の欄に書く。
    /// `dns_per_connect` は今までどおり失敗も入れた値 (読む側の互換)。
    #[test]
    fn a_line_carries_the_miss_rate_without_failed_lookups() {
        let path = tmp("resolved");
        let mut w = Writer::new(Some(path.clone()), "0.1.0+test");
        let base = 20_400 * DAY;
        for i in 0..4 {
            // 1 本に CONNECT 1 本・ミス 2 回 (うち失敗 1 回は 1 本おき)
            let mut s = sample(base + 100 + i * 5, 10);
            s.dns_fail_misses = (i % 2 == 0) as u64;
            w.add(&s, 0);
        }
        w.add(&sample(base + DAY + 5, 20), 0);
        let rows = lines(&path);
        assert_eq!(rows.len(), 1, "{:?}", rows);
        // 5 本 (境目の 1 本も終わる日に足す): ミス 10・失敗 2・CONNECT 5
        assert_eq!(field(&rows[0], "dns_per_connect"), Some("2.000"));
        assert_eq!(field(&rows[0], "dns_fail_misses"), Some("2"));
        assert_eq!(field(&rows[0], "dns_resolved_per_connect"), Some("1.600"));
        let _ = fs::remove_file(&path);
    }

    /// `version <v> on port …` の `start` 1 件 (T.32.18)。
    fn start_event(at: u64, v: &str) -> crate::events::Event {
        crate::events::Event {
            at,
            kind: crate::events::EventKind::Start,
            text: format!("version {} on port 8080 boot x", v),
        }
    }

    /// プロセス 1 本ぶん: `from`〜`until` (5 秒ごと) の標本を `h` に積みながら `w` に足す (本番と同じ順:
    /// `daily` が先、`push` が後)。要求は 5 秒に 10 本 (`requests` は**このプロセスの通算**で 0 から)、
    /// CONNECT 1 本・ミス 2 回 (うち失敗 1 回)。返すのはこのプロセスが通した要求の数
    fn run_process(
        h: &crate::history::History,
        w: &mut Writer,
        from: u64,
        until: u64,
        events: &[crate::events::Event],
    ) -> u64 {
        let mut total = 0;
        let mut t = from;
        while t <= until {
            let mut s = sample(t, total);
            // 最初の 1 本は起点 (区間の値は 0。`tick.rs` の `totals` が `None` の 1 本目と同じ)
            s.requests_delta = if t == from { 0 } else { 10 };
            s.bytes_delta = s.requests_delta * 1024;
            s.dns_fail_misses = 1;
            s.gauge_n = 1;
            if !w.seeded {
                w.seed(h, &s, 0, events);
            }
            w.add(&s, 0);
            h.push(s);
            t += 5;
            total += 10;
        }
        // 最後の 1 本の `requests` が通算 (= 足した `requests_delta` の和)
        total - 10
    }

    /// T.32.18 の受け入れ基準: 日の途中で再起動しても、その日の `secs` が 86,400 前後になり、要求の和が
    /// 前後のプロセスの和と一致する。前のプロセスのぶんは `.rrd` から読み戻した `history["60"]`
    /// (ここでは前の `History` の 60 秒の行をそのまま `restore`) から組み直す。`version` はその日に動いた版の並び。
    #[test]
    fn a_restart_in_the_middle_of_the_day_keeps_the_earlier_process() {
        let path = tmp("restart");
        let day = 20_500 * DAY;
        // 前のプロセス (0.1.0+aaa): 前の日の 23:00 から 12:00:00 まで。11:59 の行は 12:00:00 の標本で閉じる
        let h1 = crate::history::History::default();
        let mut w1 = Writer::new(Some(path.clone()), "0.1.0+aaa");
        let ev1 = [start_event(day - 3600, "0.1.0+aaa")];
        run_process(&h1, &mut w1, day - 3600, day + 12 * 3600, &ev1);
        assert_eq!(lines(&path).len(), 1, "前の日の行は前のプロセスが書いた");
        // 0 時からの 12 時間ぶんの要求 (12:00:00 の標本は窓が閉じる前に止まったので戻らない)
        let before: u64 = 12 * 3600 / 5 * 10;
        // 次のプロセス (0.1.0+bbb): 12:00:30 に起動。`.rrd` から 60 秒の行を読み戻す
        let h2 = crate::history::History::default();
        let mut rows = Vec::new();
        h1.for_each_in(SEED_RES, 0, u64::MAX, |s| rows.push(*s));
        h2.restore(SEED_RES, rows);
        let mut w2 = Writer::new(Some(path.clone()), "0.1.0+bbb");
        let ev2 = [
            ev1[0].clone(),
            start_event(day + 12 * 3600 + 30, "0.1.0+bbb"),
        ];
        // 翌日 0:00:00 の標本 (日をまたいだ最初の 1 本) は終わる日に足して、そこで 1 行書く
        let after = run_process(&h2, &mut w2, day + 12 * 3600 + 30, day + DAY, &ev2);
        let rows = lines(&path);
        assert_eq!(rows.len(), 2, "{:?}", rows);
        let today = &rows[1];
        let secs: u64 = field(today, "secs").unwrap().parse().unwrap();
        assert!(
            (86_000..=86_400).contains(&secs),
            "secs {}: {}",
            secs,
            today
        );
        let requests: u64 = field(today, "requests").unwrap().parse().unwrap();
        assert_eq!(requests, before + after, "{}", today);
        assert_eq!(field(today, "version"), Some("0.1.0+aaa 0.1.0+bbb"));
        // 区間の値も前のプロセスのぶんが入る (CONNECT は 5 秒に 1 本)
        let connects: u64 = field(today, "connects").unwrap().parse().unwrap();
        assert!(connects > 17_000, "{}", today);
        assert_eq!(field(today, "dns_resolved_per_connect"), Some("1.000"));
        assert!(today.len() <= MAX_LINE);
        // 組み直すのは起動して最初の 1 回だけ
        assert!(w2.seeded);
        let _ = fs::remove_file(&path);
    }

    /// 読み戻した行が無ければ (`.rrd` が無い・前の日のぶんしか無い) 今までどおり今のプロセスのぶんだけ。
    #[test]
    fn without_restored_rows_the_day_is_this_process_only() {
        let path = tmp("norows");
        let day = 20_600 * DAY;
        let h = crate::history::History::default();
        let mut w = Writer::new(Some(path.clone()), "0.1.0+ccc");
        let ev = [
            start_event(day - 50, "0.1.0+old"),
            start_event(day + 600, "0.1.0+ccc"),
        ];
        run_process(&h, &mut w, day + 600, day + DAY + 5, &ev);
        let rows = lines(&path);
        assert_eq!(rows.len(), 1);
        assert_eq!(field(&rows[0], "version"), Some("0.1.0+ccc"));
        let secs: u64 = field(&rows[0], "secs").unwrap().parse().unwrap();
        assert_eq!(
            secs,
            DAY - 600,
            "日をまたいだ最初の標本 (翌日 0:00:00) までの秒"
        );
        let _ = fs::remove_file(&path);
    }

    /// 版の並び: 0 時に動いていた版 + 0 時からの `start` + 今の版 (続けて同じ版は 1 つ)。長すぎれば古い方から落とす。
    #[test]
    fn the_versions_of_the_day_are_listed_oldest_first() {
        let day = 20_700 * DAY;
        let ev = [
            start_event(day - 7200, "0.1.0+old"),
            start_event(day - 100, "0.1.0+a"),
            start_event(day + 100, "0.1.0+b"),
            start_event(day + 200, "0.1.0+b"),
            crate::events::Event {
                at: day + 300,
                kind: crate::events::EventKind::Start,
                text: "self-bench: 3 runs".to_string(),
            },
            start_event(day + 400, "0.1.0+c"),
        ];
        assert_eq!(
            versions_of_day(&ev, day, day + 500, true, "0.1.0+c"),
            "0.1.0+a 0.1.0+b 0.1.0+c"
        );
        assert_eq!(
            versions_of_day(&ev, day, day + 500, false, "0.1.0+c"),
            "0.1.0+b 0.1.0+c"
        );
        let many: Vec<_> = (0..20)
            .map(|i| start_event(day + i, &format!("0.{}.0+abcdef0", i)))
            .collect();
        let v = versions_of_day(&many, day, day + 100, false, "9.9.9+fffffff");
        assert!(v.len() <= MAX_VERSIONS, "{}", v);
        assert!(v.ends_with("0.19.0+abcdef0 9.9.9+fffffff"), "{}", v);
    }

    #[test]
    fn starting_twice_on_the_same_day_keeps_one_line() {
        let path = tmp("twice");
        let base = 20_100 * DAY;
        let mut first = Writer::new(Some(path.clone()), "0.1.0+test");
        first.add(&sample(base + 100, 10), 0);
        first.add(&sample(base + DAY + 100, 20), 0);
        assert_eq!(lines(&path).len(), 1);
        // 同じ日をもう一度またぐ起動 (最後の行の日付を見て書かない)
        let mut second = Writer::new(Some(path.clone()), "0.1.0+test");
        second.add(&sample(base + 200, 10), 0);
        second.add(&sample(base + DAY + 200, 40), 0);
        let rows = lines(&path);
        assert_eq!(rows.len(), 1, "{:?}", rows);
        assert_eq!(second.counts(), (0, 0));
        // その次の日はちゃんと増える
        second.add(&sample(base + 2 * DAY, 60), 0);
        assert_eq!(lines(&path).len(), 2);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn nothing_is_written_without_a_path() {
        let mut w = Writer::new(None, "0.1.0+test");
        let base = 20_200 * DAY;
        w.add(&sample(base, 1), 0);
        w.add(&sample(base + DAY, 2), 0);
        assert_eq!(w.counts(), (0, 0));
    }

    /// 1 行は必ず [`MAX_LINE`] 以下 (現実離れした値でも)。
    #[test]
    fn a_line_never_exceeds_the_limit() {
        let mut connect = Window::default();
        for _ in 0..3 {
            connect.observe(u64::MAX / 4);
        }
        let acc = Acc {
            day: 20_000,
            first_t: 0,
            last_t: u64::MAX,
            samples: u64::MAX,
            start: Cum::default(),
            end: Cum {
                requests: u64::MAX,
                bytes: u64::MAX,
                evicted_idle: u64::MAX,
                bursts: u64::MAX,
            },
            connect,
            errors: u64::MAX,
            dns_misses: u64::MAX,
            dns_ms_sum: u64::MAX,
            dns_fail_misses: u64::MAX,
            active_max: u64::MAX,
            rss_max: u64::MAX,
            rss_sum: u64::MAX,
            rss_n: 1,
            seed_secs: u64::MAX,
            seed_requests: u64::MAX,
            seed_bytes: u64::MAX,
            seed_evicted: u64::MAX,
            // その日に動いた版の並びも上限まで (T.32.18)
            versions: Some("9".repeat(MAX_VERSIONS)),
        };
        let worst = acc.line(&clean_version(&"9".repeat(200)), [[u32::MAX; 2]; 2]);
        println!("daily line: worst {} B (limit {})", worst.len(), MAX_LINE);
        assert!(worst.len() <= MAX_LINE, "{} B: {}", worst.len(), worst);
        assert!(is_line(&worst));
        // ありふれた 1 行も測っておく (1 年 = 365 行の大きさの根拠)
        let mut w = Window::default();
        for ms in [3, 8, 9, 12, 45] {
            w.observe(ms);
        }
        let typical = Acc {
            day: 20_713,
            first_t: 20_713 * DAY,
            last_t: 20_713 * DAY + 86_395,
            samples: 17_279,
            start: Cum::default(),
            end: Cum {
                requests: 12_345,
                bytes: 9_876_543_210,
                evicted_idle: 3,
                bursts: 2,
            },
            connect: w,
            errors: 5,
            dns_misses: 123,
            dns_ms_sum: 3_988,
            dns_fail_misses: 100,
            active_max: 42,
            rss_max: 58_720_256,
            rss_sum: 52_428_800,
            rss_n: 1,
            ..Acc::default()
        }
        .line("0.1.0+fffb795", [[23, 1], [24, 0]]);
        println!("daily line: typical {} B -> {}", typical.len(), typical);
        assert!(typical.len() <= MAX_LINE);
    }

    /// 上限 2 MiB を越えたら古い行から捨てる。
    #[test]
    fn the_file_is_capped_and_drops_the_oldest_lines() {
        let path = tmp("cap");
        // 512 B ちょうどの行で埋める (2 MiB = 4,096 行)
        let filler = format!("{{\"t\":1,\"x\":\"{}\"}}", "y".repeat(MAX_LINE - 14));
        assert_eq!(filler.len(), MAX_LINE);
        {
            let mut f = fs::File::create(&path).unwrap();
            for _ in 0..4_200 {
                f.write_all(filler.as_bytes()).unwrap();
                f.write_all(b"\n").unwrap();
            }
        }
        append(&path, "{\"t\":2}").unwrap();
        let size = fs::metadata(&path).unwrap().len();
        assert!(size <= MAX_BYTES, "{} B", size);
        let rows = lines(&path);
        assert_eq!(rows.last().map(String::as_str), Some("{\"t\":2}"));
        println!("daily file: {} lines in {} B", rows.len(), size);
        let _ = fs::remove_file(&path);
    }

    /// `/daily` の中身は 256 KiB に収まる (行が何行あっても)。
    #[test]
    fn the_endpoint_body_stays_under_the_limit() {
        let path = tmp("read");
        let filler = format!("{{\"t\":1,\"x\":\"{}\"}}", "y".repeat(MAX_LINE - 14));
        {
            let mut f = fs::File::create(&path).unwrap();
            for _ in 0..4_096 {
                f.write_all(filler.as_bytes()).unwrap();
                f.write_all(b"\n").unwrap();
            }
        }
        // 大域の口を使わずに読む側だけ確かめる (recent は WRITER を見るため)
        let all = read_lines(&path);
        assert_eq!(all.len(), 4_096);
        let mut used = 0usize;
        let mut taken = 0usize;
        for l in all.iter().rev().take(MAX_DAYS) {
            if used + l.len() + 1 > MAX_JSON {
                break;
            }
            used += l.len() + 1;
            taken += 1;
        }
        assert!(used <= MAX_JSON && used + 1024 <= 256 * 1024, "{} B", used);
        println!("/daily: {} 行 {} B (上限 {} B)", taken, used, MAX_JSON);
        // 1 年ぶん (365 行) は切られない
        assert!(taken >= DEFAULT_DAYS);
        let _ = fs::remove_file(&path);
    }

    /// 大域の口 ([`configure`] / [`tick`] / [`recent`]) も 1 度通しておく
    /// (history スレッドが呼ぶのはこちら。**この単体テストだけが大域に触る**)。
    #[test]
    fn the_global_writer_records_and_reads_back() {
        let path = tmp("global");
        let metrics = Metrics::new();
        configure(Some(path.clone()), "0.1.0+global");
        let base = 20_300 * DAY;
        tick(&metrics, &sample(base + 10, 1));
        assert!(recent(DEFAULT_DAYS).lines.is_empty(), "日が変わるまでは空");
        tick(&metrics, &sample(base + DAY + 10, 9));
        let d = recent(DEFAULT_DAYS);
        assert_eq!(d.count, 1);
        assert_eq!(d.lines.len(), 1);
        assert!(!d.truncated);
        assert_eq!(d.path.as_deref(), Some(path.as_path()));
        assert!(d.bytes > 0 && d.bytes <= MAX_BYTES);
        assert_eq!(field(&d.lines[0], "requests"), Some("8"));
        assert_eq!(field(&d.lines[0], "version"), Some("0.1.0+global"));
        // 書かない設定に戻すと `tick` は原子の読み 1 回で戻り、`recent` は空
        configure(None, "");
        tick(&metrics, &sample(base + 2 * DAY, 99));
        assert!(recent(DEFAULT_DAYS).path.is_none());
        assert_eq!(read_lines(&path).len(), 1);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn broken_lines_are_ignored() {
        let path = tmp("broken");
        fs::write(
            &path,
            "not json\n{\"t\":1728000000}\n\n{\"half\":\n{\"t\":1728086400}\n",
        )
        .unwrap();
        let rows = lines(&path);
        assert_eq!(rows.len(), 2, "{:?}", rows);
        assert_eq!(last_day(&path), 1_728_086_400 / DAY);
        assert_eq!(last_day(Path::new("/nonexistent/rhp-t1420")), 0);
        let _ = fs::remove_file(&path);
    }
}
