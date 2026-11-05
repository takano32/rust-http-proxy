//! 時刻と行の組み立て: UTC のタイムスタンプ (秒までは使い回す)、行の頭 (`… INFO  [conn#12] `)、
//! アクセスログの本体 (`ACCESS …`)。`String` を作らずに使い回しのバッファへ直に書く。
//! T.34.56 で `log.rs` から移した (本体は変えていない)。

use super::{Access, Level};
use std::cell::RefCell;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

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
pub(super) fn push_padded(buf: &mut Vec<u8>, v: u64, width: usize) {
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
}

/// 秒までの部分 (`2026-09-02T01:23:45.`) を書き足す。
pub(super) fn push_second(buf: &mut Vec<u8>, secs: u64) {
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
pub(super) fn push_timestamp_at(buf: &mut Vec<u8>, secs: u64, millis: u32) {
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
pub(super) fn push_timestamp(buf: &mut Vec<u8>) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    push_timestamp_at(buf, now.as_secs(), now.subsec_millis());
}

/// 1 行の頭 (`2026-09-02T01:23:45.678Z INFO  [conn#12] `) を書き足す。
pub(super) fn push_prefix(buf: &mut Vec<u8>, level: Level, conn_id: Option<usize>) {
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

/// アクセスログの本体 (`ACCESS ...`) を書き足す。行の頭と改行は付けない。
pub(super) fn push_access(buf: &mut Vec<u8>, rec: &Access<'_>) {
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
