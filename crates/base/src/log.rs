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
use std::sync::atomic::{AtomicU8, Ordering};

// 責務ごとに子のファイルへ分けてある (T.34.56)。公開の名前はここで出し直す (`proxy_base::log::…` のまま)。
// - `format`: 時刻と行の頭、アクセスログの本体の組み立て
// - `ring`: `/log` のリング (warn 以上の直近の行) と状態ファイルへの書き出し・読み戻し
// - `throttle`: 警告を絞る門 (`Throttle`) と、長い文字列を切って載せる `Clip`
mod format;
mod ring;
mod throttle;
pub use format::civil_from_epoch;
#[cfg(test)]
use format::push_timestamp_at;
#[cfg(test)]
use format::timestamp;
use format::{push_access, push_prefix};
use ring::remember;
pub use ring::{
    Line, MAX_LOG_LINE, MAX_LOG_LINES, clear_recent, prefault, recent, recent_len, restore,
    restored_count, take_unwritten,
};
pub use throttle::{Clip, Skipped, Throttle, WARN_EVERY_SECS};

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

    /// 符号 (`Level as u8`) から戻す。知らない値は `None` (T.34.73 で `from_u8` と
    /// [`current_level`] の 2 つの表をここ 1 つにまとめた。知らない値の行き先は呼ぶ側が決める)。
    #[inline]
    fn from_code(v: u8) -> Option<Level> {
        match v {
            0 => Some(Level::Error),
            1 => Some(Level::Warn),
            2 => Some(Level::Info),
            3 => Some(Level::Debug),
            4 => Some(Level::Trace),
            _ => None,
        }
    }

    /// 符号から戻す (状態ファイルの読み戻し。知らない値は [`Level::Warn`])。
    pub fn from_u8(v: u8) -> Level {
        Level::from_code(v).unwrap_or(Level::Warn)
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
    // 書くのは `set_level` だけなので知らない値は来ない。来たら前と同じく `Trace` に倒す
    Level::from_code(CURRENT_LEVEL.load(Ordering::Relaxed)).unwrap_or(Level::Trace)
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

thread_local! {
    /// 1 行を組み立てるバッファ。書き出しは行ごとに 1 回なので、スレッドに 1 本で足りる。
    /// 伸びたままになるが、1 行の長さは要求行とヘッダーの上限で頭打ちになる。
    static LINE: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
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

/// 1 行のログを出力する。レベルによらず、すべて標準出力へ書き出す。
pub fn log_line(level: Level, conn_id: Option<usize>, msg: &str) {
    if !enabled(level) {
        return;
    }
    // warn 以上は `/log` のリングにも写す (T13.4)。info / debug / trace は比較 1 回だけ。
    // `PROXY_RECORDS=off` はここも止める (T14.41)。**止まるのはリングだけ**で、
    // 標準出力への 1 行は今までどおり出す (コンテナのログは運用の側の道具で、
    // 認証なしで誰でも読める `/log` とは見える範囲が違う)
    if level <= Level::Warn && crate::records::recording() {
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
pub(crate) mod tests;
