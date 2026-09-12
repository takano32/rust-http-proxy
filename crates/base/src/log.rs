//! 依存クレートゼロの構造化ログモジュール。
//!
//! `PROXY_LOG_LEVEL` (error/warn/info/debug/trace) でレベルを制御する。
//! 出力形式: `2026-09-02T01:23:45.678Z INFO  [conn#12] message`
//! 出力先はレベルによらずすべて標準出力 (stdout)。
//!
//! **warn 以上は固定長のリングにも写す** (`/log`。T13.4)。動作環境 (Pterodactyl) の
//! コンソールは流れて消えるので、「さっき何を警告したか」を後から読む口が要る。
//! `info` のアクセスログは写さない — 熱い経路 (1 行 7.2 us/要求。T10.10) を重くしない。

use std::cell::RefCell;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::sync::LockExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" | "err" | "0" => Some(Level::Error),
            "warn" | "warning" | "1" => Some(Level::Warn),
            "info" | "2" => Some(Level::Info),
            "debug" | "3" => Some(Level::Debug),
            "trace" | "4" => Some(Level::Trace),
            _ => None,
        }
    }
}

static CURRENT_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

pub fn set_level(level: Level) {
    CURRENT_LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn current_level() -> Level {
    match CURRENT_LEVEL.load(Ordering::Relaxed) {
        0 => Level::Error,
        1 => Level::Warn,
        2 => Level::Info,
        3 => Level::Debug,
        _ => Level::Trace,
    }
}

pub fn enabled(level: Level) -> bool {
    level <= current_level()
}

/// `PROXY_LOG_LEVEL` からログレベルを初期化する。
pub fn init_from_env() {
    if let Some(v) = crate::envfile::var("PROXY_LOG_LEVEL") {
        if let Some(l) = Level::parse(&v) {
            set_level(l);
            return;
        }
        println!("Unknown PROXY_LOG_LEVEL '{}', falling back to 'info'", v);
    }
    // lite プロファイルの既定は warn (明示指定があれば上で返している)
    if crate::envfile::var("PROXY_PROFILE").is_some_and(|v| v.trim().eq_ignore_ascii_case("lite")) {
        set_level(Level::Warn);
        return;
    }
    set_level(Level::Info);
}

/// UTC のタイムスタンプ文字列 (`2026-09-02T01:23:45.678Z`) を生成する。
///
/// ログを 1 行出すだけなら [`log_line`] / [`access`] が使い回しのバッファへ直接書くので、
/// この関数は `String` が要る呼び出し元のためだけに残してある。
pub fn timestamp() -> String {
    let mut buf = Vec::with_capacity(24);
    push_timestamp(&mut buf);
    String::from_utf8(buf).unwrap_or_default()
}

/// UNIX epoch 秒を UTC の (年, 月, 日, 時, 分, 秒) へ変換する (Howard Hinnant のアルゴリズム)。
pub fn civil_from_epoch(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    );

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y };

    (y, mo, d, h, mi, s)
}

/// 10 進で書き足す (`{:0width$}` と同じ。桁が足りなければ先頭に `0` を詰める)。
/// `Display` を通さないので `String` を作らない。
fn push_padded(buf: &mut Vec<u8>, v: u64, width: usize) {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    let mut n = v;
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for _ in digits.len() - i..width {
        buf.push(b'0');
    }
    buf.extend_from_slice(&digits[i..]);
}

thread_local! {
    /// 直近に組み立てた「秒まで」のタイムスタンプ (`2026-09-02T01:23:45.`) と、その epoch 秒。
    /// 秒が変わらない限り作り直さない (`civil_from_epoch` の除算とゼロ詰めは要求ごとに効く)。
    static SECOND: RefCell<(u64, Vec<u8>)> = const { RefCell::new((u64::MAX, Vec::new())) };

    /// 1 行を組み立てるバッファ。書き出しは行ごとに 1 回なので、スレッドに 1 本で足りる。
    /// 伸びたままになるが、1 行の長さは要求行とヘッダーの上限で頭打ちになる。
    static LINE: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// 秒までの部分 (`2026-09-02T01:23:45.`) を書き足す。
fn push_second(buf: &mut Vec<u8>, secs: u64) {
    let (y, mo, d, h, mi, s) = civil_from_epoch(secs);
    // epoch 秒は u64 なので年は必ず 1970 以降 (負にはならない)
    push_padded(buf, y.max(0) as u64, 4);
    buf.push(b'-');
    push_padded(buf, mo as u64, 2);
    buf.push(b'-');
    push_padded(buf, d as u64, 2);
    buf.push(b'T');
    push_padded(buf, h as u64, 2);
    buf.push(b':');
    push_padded(buf, mi as u64, 2);
    buf.push(b':');
    push_padded(buf, s as u64, 2);
    buf.push(b'.');
}

/// `2026-09-02T01:23:45.678Z` を書き足す (秒までは使い回す)。
fn push_timestamp_at(buf: &mut Vec<u8>, secs: u64, millis: u32) {
    SECOND.with(|cell| match cell.try_borrow_mut() {
        Ok(mut c) => {
            if c.0 != secs || c.1.is_empty() {
                c.1.clear();
                push_second(&mut c.1, secs);
                c.0 = secs;
            }
            buf.extend_from_slice(&c.1);
        }
        // 借用に失敗するのはログの中からログを呼んだときだけ (通常は起きない)
        Err(_) => push_second(buf, secs),
    });
    push_padded(buf, millis as u64, 3);
    buf.push(b'Z');
}

/// 現在時刻の `2026-09-02T01:23:45.678Z` を書き足す。
fn push_timestamp(buf: &mut Vec<u8>) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    push_timestamp_at(buf, now.as_secs(), now.subsec_millis());
}

/// 1 行の頭 (`2026-09-02T01:23:45.678Z INFO  [conn#12] `) を書き足す。
fn push_prefix(buf: &mut Vec<u8>, level: Level, conn_id: Option<usize>) {
    push_timestamp(buf);
    buf.push(b' ');
    buf.extend_from_slice(level.as_str().as_bytes());
    match conn_id {
        Some(id) => {
            buf.extend_from_slice(b" [conn#");
            push_padded(buf, id as u64, 1);
            buf.extend_from_slice(b"] ");
        }
        None => buf.extend_from_slice(b" [main] "),
    }
}

/// 組み立てた 1 行を標準出力へ 1 回で書き出す。ロックは 1 回だけ取る
/// (`Stdout` の `write_all` と `flush` を別々に呼ぶと 2 回取ることになる)。
fn emit(line: &[u8]) {
    let out = std::io::stdout();
    let mut out = out.lock();
    let _ = out.write_all(line);
    let _ = out.flush();
}

/// 使い回しのバッファへ 1 行を組み立てて書き出す (`format!` の `String` を作らない)。
fn write_line(build: impl FnOnce(&mut Vec<u8>)) {
    // 借用に失敗するのはログの中からログを呼んだときだけ。そのときは使い捨ての置き場で出す
    let mut spare = Vec::new();
    LINE.with(|cell| {
        let mut held = cell.try_borrow_mut().ok();
        let buf = match held.as_deref_mut() {
            Some(buf) => {
                buf.clear();
                buf
            }
            None => &mut spare,
        };
        build(buf);
        emit(buf);
    });
}

/// `/log` に覚えておく行数 (固定)。1 行 [`MAX_LOG_LINE`] B なので最悪でも 256 KiB。
pub const MAX_LOG_LINES: usize = 1000;
/// 1 行に覚える長さ (バイト)。長い警告はここで切る。
pub const MAX_LOG_LINE: usize = 256;

/// リングに覚えている 1 行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// いつ (epoch 秒)
    pub at: u64,
    pub level: Level,
    /// ログの `[conn#N]` (無ければ `[main]`)
    pub conn: Option<usize>,
    pub msg: String,
}

#[derive(Default)]
struct LogRing {
    buf: Vec<Line>,
    next: usize,
    total: u64,
}

/// warn 以上の直近の行 (`/log`)。**書くのは警告とエラーのときだけ**なので、
/// 熱い経路 (info のアクセスログ) はこの鍵を 1 度も取らない。
static RECENT: Mutex<LogRing> = Mutex::new(LogRing {
    buf: Vec::new(),
    next: 0,
    total: 0,
});

/// 直近 `n` 行を**新しい順**で返す。2 つ目は起動からの通算 (捨てた分も含む)。
pub fn recent(n: usize) -> (Vec<Line>, u64) {
    let r = RECENT.locked();
    let len = r.buf.len();
    let n = n.min(len);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        // `next` の 1 つ手前が最新 (満杯になる前は `next == 0` なので末尾が最新)
        let start = if len < MAX_LOG_LINES { len } else { r.next };
        out.push(r.buf[(start + len - 1 - i) % len].clone());
    }
    (out, r.total)
}

/// 覚えている行数。
pub fn recent_len() -> usize {
    RECENT.locked().buf.len()
}

/// リングを空にする (テスト用)。
pub fn clear_recent() {
    let mut r = RECENT.locked();
    r.buf.clear();
    r.next = 0;
    r.total = 0;
}

/// warn 以上の 1 行をリングに写す (満杯なら最も古いものを上書き)。
fn remember(level: Level, conn_id: Option<usize>, msg: &str) {
    let line = Line {
        at: crate::clock::now_epoch(),
        level,
        conn: conn_id,
        // 文字の途中で切らない
        msg: {
            let mut end = msg.len().min(MAX_LOG_LINE);
            while end > 0 && !msg.is_char_boundary(end) {
                end -= 1;
            }
            msg[..end].to_string()
        },
    };
    let mut r = RECENT.locked();
    r.total += 1;
    if r.buf.len() < MAX_LOG_LINES {
        r.buf.push(line);
        return;
    }
    let at = r.next;
    r.buf[at] = line;
    r.next = (at + 1) % MAX_LOG_LINES;
}

/// 1 行のログを出力する。レベルによらず、すべて標準出力へ書き出す。
pub fn log_line(level: Level, conn_id: Option<usize>, msg: &str) {
    if !enabled(level) {
        return;
    }
    // warn 以上は `/log` のリングにも写す (T13.4)。info / debug / trace は比較 1 回だけ
    if level <= Level::Warn {
        remember(level, conn_id, msg);
    }
    write_line(|buf| {
        push_prefix(buf, level, conn_id);
        buf.extend_from_slice(msg.as_bytes());
        buf.push(b'\n');
    });
}

/// 1 リクエスト分のアクセスログ (既定の INFO レベルで出力される)。
///
/// 例: `ACCESS 127.0.0.1 "GET http://example.com/ HTTP/1.1" 200 1234B 12.3ms cache=HIT(memory)`
#[derive(Clone, Copy)]
pub struct Access<'a> {
    pub client: &'a str,
    pub method: &'a str,
    pub target: &'a str,
    pub version: &'a str,
    pub status: &'a str,
    pub bytes: u64,
    pub duration_ms: f64,
    pub cache: &'a str,
}

/// アクセスログの本体 (`ACCESS ...`) を書き足す。行の頭と改行は付けない。
fn push_access(buf: &mut Vec<u8>, rec: &Access<'_>) {
    buf.extend_from_slice(b"ACCESS ");
    buf.extend_from_slice(rec.client.as_bytes());
    buf.extend_from_slice(b" \"");
    buf.extend_from_slice(rec.method.as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(rec.target.as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(rec.version.as_bytes());
    buf.extend_from_slice(b"\" ");
    buf.extend_from_slice(rec.status.as_bytes());
    buf.push(b' ');
    push_padded(buf, rec.bytes, 1);
    buf.extend_from_slice(b"B ");
    // `{:.1}` の丸め (最近接・偶数) を自前で真似ると 1 バイト変わることがあるので、
    // 小数だけは std に任せる (`Vec<u8>` への `write!` は確保しない)
    let _ = write!(buf, "{:.1}", rec.duration_ms);
    buf.extend_from_slice(b"ms cache=");
    buf.extend_from_slice(rec.cache.as_bytes());
}

pub fn access(conn_id: usize, rec: &Access<'_>) {
    if !enabled(Level::Info) {
        return;
    }
    write_line(|buf| {
        push_prefix(buf, Level::Info, Some(conn_id));
        push_access(buf, rec);
        buf.push(b'\n');
    });
}

/// 内部マクロ: `log!(Level::Info, conn_id, "fmt", args...)`
#[macro_export]
macro_rules! log_at {
    ($level:expr, $conn:expr, $($arg:tt)*) => {{
        if $crate::log::enabled($level) {
            $crate::log::log_line($level, $conn, &format!($($arg)*));
        }
    }};
}

#[macro_export]
macro_rules! log_error {
    ($conn:expr, $($arg:tt)*) => { $crate::log_at!($crate::log::Level::Error, $conn, $($arg)*) };
}

#[macro_export]
macro_rules! log_warn {
    ($conn:expr, $($arg:tt)*) => { $crate::log_at!($crate::log::Level::Warn, $conn, $($arg)*) };
}

#[macro_export]
macro_rules! log_info {
    ($conn:expr, $($arg:tt)*) => { $crate::log_at!($crate::log::Level::Info, $conn, $($arg)*) };
}

#[macro_export]
macro_rules! log_debug {
    ($conn:expr, $($arg:tt)*) => { $crate::log_at!($crate::log::Level::Debug, $conn, $($arg)*) };
}

#[macro_export]
macro_rules! log_trace {
    ($conn:expr, $($arg:tt)*) => { $crate::log_at!($crate::log::Level::Trace, $conn, $($arg)*) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_level_parse() {
        assert_eq!(Level::parse("TRACE"), Some(Level::Trace));
        assert_eq!(Level::parse(" debug "), Some(Level::Debug));
        assert_eq!(Level::parse("nonsense"), None);
    }

    #[test]
    fn test_level_ordering() {
        assert!(Level::Error < Level::Info);
        assert!(Level::Trace > Level::Debug);
    }

    #[test]
    fn test_civil_from_epoch() {
        // 2026-09-02T01:23:45Z
        assert_eq!(civil_from_epoch(1_788_312_225), (2026, 9, 2, 1, 23, 45));
        // UNIX epoch
        assert_eq!(civil_from_epoch(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn test_access_log_is_info_level() {
        // ログ水準はプロセスに 1 つなので、水準を触るテストは直列に回す
        let _guard = crate::sync::LockExt::locked(&RING_TEST_LOCK);
        set_level(Level::Info);
        assert!(enabled(Level::Info));
        assert!(!enabled(Level::Debug));
        // 既定レベルでアクセスログが出ること (パニックしないこと) を確認する
        access(
            1,
            &Access {
                client: "127.0.0.1",
                method: "GET",
                target: "http://example.com/",
                version: "HTTP/1.1",
                status: "200",
                bytes: 42,
                duration_ms: 1.5,
                cache: "MISS",
            },
        );
    }

    /// タイムスタンプが以前の `format!("{:04}-{:02}-...{:03}Z")` と 1 バイトも変わらないこと。
    /// 秒を使い回すので、同じ秒を続けたり戻したりする並びも通す。
    #[test]
    fn test_timestamp_matches_the_old_format() {
        let cases = [
            (0u64, 0u32),
            (1_788_312_225, 678),
            (1_788_312_225, 7),
            (1_788_312_225, 70),
            (1_788_312_226, 0),
            (1_788_312_225, 999),
            (951_782_400, 1),       // 2000-02-29 (うるう年)
            (253_402_300_799, 999), // 9999-12-31T23:59:59
            (253_402_300_800, 0),   // 10000-01-01 (年が 5 桁でも `{:04}` は詰めない)
        ];
        for (secs, millis) in cases {
            let mut buf = Vec::new();
            push_timestamp_at(&mut buf, secs, millis);
            let (y, mo, d, h, mi, s) = civil_from_epoch(secs);
            let expected = format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                y, mo, d, h, mi, s, millis
            );
            assert_eq!(
                String::from_utf8(buf).unwrap(),
                expected,
                "{} {}",
                secs,
                millis
            );
        }
    }

    /// 行の頭が以前の `format!("{} {} [conn#{}] {}\n")` と同じ並びであること。
    #[test]
    fn test_prefix_matches_the_old_format() {
        let mut buf = Vec::new();
        push_prefix(&mut buf, Level::Info, Some(12));
        let line = String::from_utf8(buf).unwrap();
        assert_eq!(line.len(), 24 + " INFO  [conn#12] ".len());
        assert_eq!(&line[24..], " INFO  [conn#12] ");

        let mut buf = Vec::new();
        push_prefix(&mut buf, Level::Error, None);
        let line = String::from_utf8(buf).unwrap();
        assert_eq!(&line[24..], " ERROR [main] ");
    }

    /// アクセスログの本体が以前の `format!` と 1 バイトも変わらないこと。
    /// 経過時間の `{:.1}` は丸めが効く値 (0.05 / 0.25 / 0.35) も通す。
    #[test]
    fn test_access_line_matches_the_old_format() {
        let base = Access {
            client: "127.0.0.1",
            method: "GET",
            target: "http://example.com/",
            version: "HTTP/1.1",
            status: "200",
            bytes: 1234,
            duration_ms: 12.34,
            cache: "HIT(memory)",
        };
        let cases = [
            base,
            Access {
                client: "::1",
                method: "CONNECT",
                target: "example.com:443",
                status: "200",
                bytes: 0,
                duration_ms: 0.0,
                cache: "BYPASS(tunnel)",
                ..base
            },
            Access {
                method: "HEAD",
                version: "HTTP/1.0",
                status: "304",
                bytes: 1,
                duration_ms: 0.05,
                cache: "HIT(disk,304) age=3s",
                ..base
            },
            Access {
                status: "206",
                bytes: u64::MAX,
                duration_ms: 0.25,
                cache: "HIT(memory) age=0s ttl_left=59s range=0-99",
                ..base
            },
            Access {
                method: "POST",
                status: "502",
                bytes: 9,
                duration_ms: 0.35,
                cache: "MISS truncated",
                ..base
            },
            Access {
                bytes: 7,
                duration_ms: 1234.5678,
                cache: "MISS stored ttl=60s",
                ..base
            },
        ];
        for rec in cases {
            let mut buf = Vec::new();
            push_access(&mut buf, &rec);
            let expected = format!(
                "ACCESS {} \"{} {} {}\" {} {}B {:.1}ms cache={}",
                rec.client,
                rec.method,
                rec.target,
                rec.version,
                rec.status,
                rec.bytes,
                rec.duration_ms,
                rec.cache
            );
            assert_eq!(String::from_utf8(buf).unwrap(), expected);
        }
    }

    /// warn 以上だけがリングに入り、`info` のアクセスログは入らない (T13.4)。
    #[test]
    fn only_warnings_and_errors_reach_the_ring() {
        let _guard = crate::sync::LockExt::locked(&RING_TEST_LOCK);
        clear_recent();
        set_level(Level::Info);
        log_line(Level::Info, Some(1), "this is info");
        access(
            1,
            &Access {
                client: "127.0.0.1",
                method: "GET",
                target: "http://example.com/",
                version: "HTTP/1.1",
                status: "200",
                bytes: 42,
                duration_ms: 1.5,
                cache: "MISS",
            },
        );
        assert_eq!(recent_len(), 0, "info もアクセスログも写さない");
        log_line(Level::Warn, Some(7), "502 Bad Gateway");
        log_line(Level::Error, None, "accept failed");
        let (got, total) = recent(10);
        assert_eq!(total, 2);
        assert_eq!(got.len(), 2);
        // 新しい順
        assert_eq!(got[0].level, Level::Error);
        assert_eq!(got[0].conn, None);
        assert_eq!(got[0].msg, "accept failed");
        assert_eq!(got[1].level, Level::Warn);
        assert_eq!(got[1].conn, Some(7));
        assert!(got[1].at > 1_700_000_000, "{}", got[1].at);
        clear_recent();
    }

    /// レベルを error に下げたら warn は出ないので、リングにも入らない。
    #[test]
    fn the_ring_follows_the_log_level() {
        let _guard = crate::sync::LockExt::locked(&RING_TEST_LOCK);
        clear_recent();
        set_level(Level::Error);
        log_line(Level::Warn, None, "not logged");
        assert_eq!(recent_len(), 0);
        log_line(Level::Error, None, "logged");
        assert_eq!(recent_len(), 1);
        set_level(Level::Info);
        clear_recent();
    }

    /// 1 行は 256 B までで切り、1,000 行で頭打ち (古いものを上書き)。
    #[test]
    fn the_ring_clips_long_lines_and_wraps_at_1000() {
        let _guard = crate::sync::LockExt::locked(&RING_TEST_LOCK);
        clear_recent();
        set_level(Level::Info);
        log_line(Level::Warn, None, &"x".repeat(1000));
        assert_eq!(recent(1).0[0].msg.len(), MAX_LOG_LINE);
        clear_recent();
        for i in 0..(MAX_LOG_LINES + 5) {
            log_line(Level::Warn, None, &format!("line {}", i));
        }
        let (got, total) = recent(MAX_LOG_LINES + 100);
        assert_eq!(total as usize, MAX_LOG_LINES + 5);
        assert_eq!(got.len(), MAX_LOG_LINES);
        assert_eq!(got[0].msg, format!("line {}", MAX_LOG_LINES + 4));
        assert_eq!(got[MAX_LOG_LINES - 1].msg, "line 5");
        clear_recent();
    }

    /// リングもログ水準もプロセスに 1 組なので、この束は直列に回す。
    static RING_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_timestamp_format() {
        let ts = timestamp();
        assert_eq!(ts.len(), 24, "unexpected timestamp: {}", ts);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }
}
