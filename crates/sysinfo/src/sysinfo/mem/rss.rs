//! 自プロセスの常駐の内訳 (`VmRSS` / `RssAnon` / `RssFile` / `RssShmem`) と、スレッドのスタックとして
//! 常駐しているページの合計 (T.32.4)。T.34.76 で `mem.rs` から移した (本体は変えていない)。

use super::{read_small, sized_field};
use std::path::Path;

/// 自プロセスの常駐の内訳 (バイト。`/proc/self/status` の**同じ 1 回の読み**から。T.32.4)。
///
/// `rss` は `VmRSS`、`anon` / `file` / `shmem` は `RssAnon` / `RssFile` / `RssShmem`
/// (`rss = anon + file + shmem`)。`Rss*` はカーネル 4.5 から (それより前は `None`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessRss {
    pub rss: u64,
    pub anon: Option<u64>,
    pub file: Option<u64>,
    pub shmem: Option<u64>,
}

/// 自プロセスの常駐の内訳を今読む (T.32.4)。
pub fn process_rss_detail() -> Option<ProcessRss> {
    parse_status_rss_detail(&read_small(Path::new("/proc/self/status"))?)
}

pub(crate) fn parse_status_rss_detail(text: &str) -> Option<ProcessRss> {
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(sized_field)
    };
    Some(ProcessRss {
        rss: field("VmRSS:")?,
        anon: field("RssAnon:"),
        file: field("RssFile:"),
        shmem: field("RssShmem:"),
    })
}

/// スレッドのスタックとして常駐しているページの合計 (バイト。T.32.4)。
///
/// `/status` の `memory.stacks_estimate` は予約 (接続スレッド 256 KiB・ほか 2 MiB) の合計で、
/// 実際に触ったのは 1 本 8〜28 KiB だった (tokyo の 22.0 MB に対し常駐は 1 MB 未満)。
/// `/proc/self/maps` でスタックの写像を見つけ、`mincore(2)` で常駐の頁を数える
/// (`/proc/self/smaps` は全部の写像のページ表を歩くので 1 回 0.7〜1 ms かかった)。
/// 読むのは `/status` に来たときだけ。Linux 以外は `None`。
pub fn stacks_resident() -> Option<u64> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    let mut total = 0u64;
    for (start, end) in parse_stack_ranges(&maps) {
        total += resident_bytes(start, end)?;
    }
    Some(total)
}

/// maps (smaps の見出しの行も同じ形) から、スタックの写像の `[始まり, 終わり)` を拾う。
///
/// スタックと見なすのは主スレッドの `[stack]` と、**すぐ下に `---p` の番兵が接している
/// 名前の無い `rw-p`** (glibc の `pthread_create` が取るスタックの形: 下端に番兵を置いて上へ使う)。
/// malloc のアリーナは逆に `rw-p` の上に `---p` の予約が続くので数えない。
pub(crate) fn parse_stack_ranges(text: &str) -> Vec<(u64, u64)> {
    // 直前の写像: (終わりの番地, 名前の無い `---p` で 64 KiB 以下か)
    let mut prev: Option<(u64, bool)> = None;
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(first) = it.next() else {
            continue;
        };
        let Some((a, b)) = first.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (u64::from_str_radix(a, 16), u64::from_str_radix(b, 16)) else {
            continue;
        };
        let perms = it.next().unwrap_or("");
        // offset・dev・inode の後ろが名前 (無ければ名前の無い写像)
        let name = it.nth(3).unwrap_or("");
        let guard_below = prev.is_some_and(|(e, guard)| guard && e == start);
        if name == "[stack]" || (name.is_empty() && perms == "rw-p" && guard_below) {
            out.push((start, end));
        }
        prev = Some((
            end,
            name.is_empty() && perms == "---p" && end.saturating_sub(start) <= 64 * 1024,
        ));
    }
    out
}

#[cfg(target_os = "linux")]
fn resident_bytes(start: u64, end: u64) -> Option<u64> {
    use std::ffi::{c_int, c_long, c_void};
    unsafe extern "C" {
        fn mincore(addr: *mut c_void, length: usize, vec: *mut u8) -> c_int;
        fn sysconf(name: c_int) -> c_long;
    }
    /// glibc / musl の Linux の `_SC_PAGESIZE`
    const SC_PAGESIZE: c_int = 30;
    // SAFETY: 引数を取らない問い合わせ。
    let page = unsafe { sysconf(SC_PAGESIZE) };
    let page = u64::try_from(page).ok().filter(|p| *p > 0)?;
    let len = end.checked_sub(start)?;
    let mut vec = vec![0u8; len.div_ceil(page) as usize];
    // SAFETY: `start` は maps に出ていた写像の頁境界、`vec` は頁の数だけある。写像が
    // その間に外れていれば ENOMEM で -1 が返るだけ (その写像は数えない)。
    let r = unsafe { mincore(start as *mut c_void, len as usize, vec.as_mut_ptr()) };
    if r != 0 {
        return Some(0);
    }
    Some(vec.iter().filter(|v| *v & 1 == 1).count() as u64 * page)
}

#[cfg(not(target_os = "linux"))]
fn resident_bytes(_: u64, _: u64) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_rss_detail_from_one_read() {
        let text = "Name:\tx\nVmRSS:\t   26000 kB\nRssAnon:\t   17000 kB\nRssFile:\t    9000 kB\nRssShmem:\t       0 kB\n";
        let m = parse_status_rss_detail(text).unwrap();
        assert_eq!(m.rss, 26_000 * 1024);
        assert_eq!(m.anon, Some(17_000 * 1024));
        assert_eq!(m.file, Some(9_000 * 1024));
        assert_eq!(m.shmem, Some(0));
        let old = parse_status_rss_detail("VmRSS: 10 kB\n").unwrap();
        assert_eq!((old.rss, old.anon, old.file), (10 * 1024, None, None));
        assert!(parse_status_rss_detail("RssAnon: 1 kB\n").is_none());
    }

    /// 番兵が下に接した `rw-p` と `[stack]` だけを拾い、アリーナ (`rw-p` の上に `---p`) と
    /// 名前のある写像は拾わない (T.32.4。smaps の見出しの行も同じ形)。
    #[test]
    fn finds_only_thread_stacks_in_maps() {
        let text = "\
7f0000000000-7f0000001000 ---p 00000000 00:00 0 
Rss:                   0 kB
7f0000001000-7f0000041000 rw-p 00000000 00:00 0 
Size:                256 kB
Rss:                  12 kB
7f1000000000-7f1000100000 rw-p 00000000 00:00 0 
Rss:                 900 kB
7f1000100000-7f1004000000 ---p 00000000 00:00 0 
Rss:                   0 kB
7f1004000000-7f1004200000 rw-p 00000000 00:00 0 
Rss:                 500 kB
7f2000000000-7f2000010000 r-xp 00000000 08:01 1234                       /usr/lib/libc.so.6
Rss:                  64 kB
7ffd00000000-7ffd00021000 rw-p 00000000 00:00 0                          [stack]
Rss:                  20 kB
";
        assert_eq!(
            parse_stack_ranges(text),
            vec![
                (0x7f0000001000, 0x7f0000041000),
                (0x7ffd00000000, 0x7ffd00021000)
            ]
        );
    }

    /// この試験のプロセス自身で: 内訳の和が VmRSS に合い、スタックの常駐が見つかる。
    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_rss_detail_adds_up() {
        let m = process_rss_detail().unwrap();
        if let (Some(a), Some(f), Some(s)) = (m.anon, m.file, m.shmem) {
            assert!((a + f + s).abs_diff(m.rss) <= 4096, "{:?}", m);
        }
        let stacks = std::thread::spawn(|| stacks_resident().unwrap())
            .join()
            .unwrap();
        assert!(stacks > 0 && stacks < m.rss, "{} / {:?}", stacks, m);
    }
}
