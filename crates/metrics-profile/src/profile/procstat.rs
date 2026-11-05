//! `/proc/<pid>/stat` の読みと解析。

/// `/proc/self/stat` の utime + stime (us)。読めなければ `None`。
pub fn process_cpu_us() -> Option<u64> {
    let text = crate::sysinfo::proc::read_small(std::path::Path::new("/proc/self/stat"))?;
    parse_stat_cpu_us(&text)
}

/// `/proc/<pid>/stat` の utime (14) + stime (15) を us で返す。
///
/// **comm は括弧で囲まれていて空白も括弧も含みうる**ので、最後の `)` から後ろを数える。
/// (異常の見張りと `tunnel_spin_test` が使うので残す。`/profile` は [`parse_stat_cpu_split_us`])
pub(crate) fn parse_stat_cpu_us(text: &str) -> Option<u64> {
    parse_stat_cpu_split_us(text).map(|(total, _)| total)
}

/// `/proc/self/stat` の (utime + stime, utime) (us)。読めなければ `None` (T16.0)。
pub(crate) fn process_cpu_split_us() -> Option<(u64, u64)> {
    let text = crate::sysinfo::proc::read_small(std::path::Path::new("/proc/self/stat"))?;
    parse_stat_cpu_split_us(&text)
}

/// `/proc/<pid>/stat` の (スレッド数 (項目 20), utime + stime (tick)) (T.32.37)。
pub(crate) fn parse_stat_threads_ticks(text: &str) -> Option<(u64, u64)> {
    let rest = &text[text.rfind(')')? + 1..];
    let mut it = rest.split_whitespace();
    // 最後の ')' の次は state (項目 3)。utime は項目 14、stime 15、num_threads 20
    let utime: u64 = it.nth(11)?.parse().ok()?;
    let stime: u64 = it.next()?.parse().ok()?;
    let threads: u64 = it.nth(4)?.parse().ok()?;
    Some((threads, utime + stime))
}

/// `/proc/<pid>/stat` の (utime + stime, utime) を us で返す (T16.0)。
///
/// **持つのはユーザー空間だけ** (カーネル側は合計から引けば出る)。
pub(crate) fn parse_stat_cpu_split_us(text: &str) -> Option<(u64, u64)> {
    let rest = &text[text.rfind(')')? + 1..];
    let mut it = rest.split_whitespace();
    // 最後の ')' の次は state (3 番目の項目) なので、utime は 11 個先
    let utime: u64 = it.nth(11)?.parse().ok()?;
    let stime: u64 = it.next()?.parse().ok()?;
    let tick_us = 1_000_000 / clock_tick();
    Some(((utime + stime) * tick_us, utime * tick_us))
}

/// `sysconf(_SC_CLK_TCK)`。Linux では実質いつも 100。
pub(super) fn clock_tick() -> u64 {
    #[cfg(target_os = "linux")]
    {
        const SC_CLK_TCK: i32 = 2;
        unsafe extern "C" {
            fn sysconf(name: i32) -> i64;
        }
        // SAFETY: 引数も戻り値も整数だけの読み取り専用の問い合わせ
        let v = unsafe { sysconf(SC_CLK_TCK) };
        if v > 0 {
            return v as u64;
        }
    }
    100
}
