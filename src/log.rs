//! 依存クレートゼロの構造化ログモジュール。
//!
//! `PROXY_LOG_LEVEL` (error/warn/info/debug/trace) でレベルを制御する。
//! 出力形式: `2026-09-02T01:23:45.678Z INFO  [conn#12] message`
//! 出力先はレベルによらずすべて標準出力 (stdout)。

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
    if let Ok(v) = std::env::var("PROXY_LOG_LEVEL") {
        if let Some(l) = Level::parse(&v) {
            set_level(l);
            return;
        }
        println!("Unknown PROXY_LOG_LEVEL '{}', falling back to 'info'", v);
    }
    set_level(Level::Info);
}

/// UTC のタイムスタンプ文字列 (`2026-09-02T01:23:45.678Z`) を生成する。
pub fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = now.subsec_millis();
    let (y, mo, d, h, mi, s) = civil_from_epoch(now.as_secs());
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, d, h, mi, s, millis
    )
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

/// 1 行のログを出力する。レベルによらず、すべて標準出力へ書き出す。
pub fn log_line(level: Level, conn_id: Option<usize>, msg: &str) {
    if !enabled(level) {
        return;
    }
    let line = match conn_id {
        Some(id) => format!("{} {} [conn#{}] {}\n", timestamp(), level.as_str(), id, msg),
        None => format!("{} {} [main] {}\n", timestamp(), level.as_str(), msg),
    };
    let mut out = std::io::stdout();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

/// 1 リクエスト分のアクセスログ (既定の INFO レベルで出力される)。
///
/// 例: `ACCESS 127.0.0.1 "GET http://example.com/ HTTP/1.1" 200 1234B 12.3ms cache=HIT(memory)`
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

pub fn access(conn_id: usize, rec: &Access<'_>) {
    if !enabled(Level::Info) {
        return;
    }
    log_line(
        Level::Info,
        Some(conn_id),
        &format!(
            "ACCESS {} \"{} {} {}\" {} {}B {:.1}ms cache={}",
            rec.client,
            rec.method,
            rec.target,
            rec.version,
            rec.status,
            rec.bytes,
            rec.duration_ms,
            rec.cache
        ),
    );
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

    #[test]
    fn test_timestamp_format() {
        let ts = timestamp();
        assert_eq!(ts.len(), 24, "unexpected timestamp: {}", ts);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }
}
