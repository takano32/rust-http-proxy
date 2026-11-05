//! `/profile?res=5|60` — 待ちの段階・スレッドの CPU と状態・ロックの取り合い (T14.3 (4))。
//!
//! `/history` と同じく**標本は配列の配列**で返す (キーは 1 回だけ)。1 標本の中身は
//!
//! ```text
//! [t, requests, cpu_us, [connect の 7 段], [forward の 6 段], [役割 9 つ], [ロック 4 つ],
//!  [待ち行列 3 つ], [上位スレッド 最大 8], [走れずに待った時間 9 つ],
//!  [ユーザー空間の CPU 9 つ], プロセスのユーザー空間の CPU]
//! ```
//!
//! で、段階 1 つは `[count, ms_sum, ms_max, [12 段 + 上限なし]]`、役割 1 つは
//! `[cpu_us, samples, [状態 22 枠]]`、上位スレッド 1 本は
//! `[tid, "comm", roles の添字, cpu_us, running]` (T15.0 (5))。
//! **件数 0 の段階と標本 0 の役割と上位スレッドの無い窓は `0` 1 文字**で書くので、
//! 静かな窓は 1 標本 60 バイト程度にしかならない。`run_delay_us` は
//! `/proc/<tid>/schedstat` が読めない環境では `null`。`user_us` (役割ごと) と `cpu_user_us`
//! (プロセス全体) は CPU のうちユーザー空間のぶん (us。T16.0)。カーネル側は `cpu_us` から引く。
//!
//! 応答は [`super::recent::MAX_BODY`] (256 KiB) 以下。入り切らないときは**新しい方を残して**
//! 古い標本から落とし、`"truncated":true` を出す (`/errors` などと同じ方針)。
//! 落ちたぶんは **`?offset=`** で続きが引ける (`?n=` は 1 枚に返す標本の数。T15.0 (11))。
//! **`?summary=1` は標本を返さず**、5 分 / 1 時間 / 全部の 3 段に畳んだ CPU/要求 だけを返す
//! (**重い口の扱いから外してある**ので、雪像を組んでいる最中でも 503 にならない)。
//!
//! `--lite` では段階の時計も読んでいないので `{"profile":"off"}` だけを返す。

use std::fmt::Write as _;

use super::{Endpoint, has_flag, next_offset, offset_param, param, parse_query};
use crate::metrics::{SCHEMA, SCHEMA_HEAD};
use crate::profile::{self, Profile};

/// 標本を書ける上限 (末尾のキーの並びぶんを空ける)。
const HEADER_ROOM: usize = 4096;

/// `?summary=1` が畳む 3 段 (名前と秒数。`all` = 環に残っている全部)。
const SPANS: [(&str, u64); 3] = [("5m", 300), ("1h", 3600), ("all", 0)];

/// 1 標本の並び (`/history` の `keys` と同じ役。新しい欄は末尾にだけ足す)。
const SAMPLE_KEYS: [&str; 12] = [
    "t",
    "requests",
    "cpu_us",
    "connect",
    "forward",
    "threads",
    "locks",
    "queue",
    "threads_top",
    "run_delay_us",
    "user_us",
    "cpu_user_us",
];

/// 1 枚の頁の位置 (`?n=` `?offset=` と、環の何本のうち何本を出したか)。
struct Page {
    total: usize,
    shown: usize,
    cut: bool,
    n: usize,
    offset: usize,
}

/// `/profile?res=5|60&n=&offset=&summary=1` を組み立てる。
pub fn profile(ep: &Endpoint<'_>, query: Option<&str>) -> (u16, &'static str, String) {
    if ep.lite {
        return (
            200,
            "application/json",
            format!("{{\"schema\":{},\"profile\":\"off\"}}", SCHEMA),
        );
    }
    let params = parse_query(query.unwrap_or(""));
    let num = |key: &str| param(&params, key).and_then(|v| v.parse::<u64>().ok());
    let res = num("res").map(Profile::index_for).unwrap_or(0);
    let p = &ep.metrics.profile;
    // 標本を返さない要約 (**重い口ではない**。T15.0 (11))
    if has_flag(query, "summary") {
        return (200, "application/json", summary(ep, res));
    }
    // `n=` と `offset=` の端はその解像度の環の大きさ (720 本 / 1,440 本)
    let cap = profile::RESOLUTIONS[res].1;
    let n = num("n").map_or(cap, |v| (v as usize).clamp(1, cap));
    // `offset=` の読み方は 3 つの口で 1 か所 (`super::offset_param`)
    let offset = offset_param(query, cap);
    let budget = super::recent::MAX_BODY.saturating_sub(HEADER_ROOM);
    let (rows, shown, total, cut) = p.rows_within_page(res, budget, n, offset);

    let mut out = String::with_capacity(rows.len() + HEADER_ROOM);
    write_head(&mut out, p, res);
    out.push_str(&rows);
    let page = Page {
        total,
        shown,
        cut,
        n,
        offset,
    };
    write_tail(&mut out, p, res, &page);
    (200, "application/json", out)
}

/// 標本の前まで: 版・解像度・段階とロックの名前・1 標本の並び (`"samples":[` まで。T.34.95)。
fn write_head(out: &mut String, p: &Profile, res: usize) {
    // 応答の形の版は**いちばん先頭の鍵** (T14.49)
    out.push_str(SCHEMA_HEAD);
    let _ = write!(
        out,
        "\"interval_secs\":{},\"sample_ms\":{},\"sampler\":\"{}\",\"bounds_ms\":[",
        profile::RESOLUTIONS[res].0,
        profile::sample_ms(),
        p.sampler().name()
    );
    join(out, crate::history::WINDOW_BOUNDS_MS.iter(), |o, b| {
        let _ = write!(o, "{}", b);
    });
    out.push_str("],\"stages\":{\"connect\":[");
    join(out, profile::CONNECT_STAGES.iter(), quoted);
    out.push_str("],\"forward\":[");
    join(out, profile::FORWARD_STAGES.iter(), quoted);
    out.push_str("]},\"roles\":[");
    join(out, profile::ROLES.iter(), quoted);
    out.push_str("],\"states\":[");
    join(out, profile::state_names().iter(), quoted);
    out.push_str("],\"lock_names\":[");
    join(out, crate::sync::LOCK_NAMES.iter(), quoted);
    // 1 標本の並び (`/history` の `keys` と同じ役)
    out.push_str("],\"keys\":[");
    join(out, SAMPLE_KEYS.iter(), quoted);
    out.push_str("],\"samples\":[");
}

/// 標本の後ろ: 通算・直近 5 分の要約・頁の位置 (閉じ括弧まで。T.34.95)。
fn write_tail(out: &mut String, p: &Profile, res: usize, page: &Page) {
    out.push_str("],\"locks_total\":[");
    join(out, crate::sync::lock_contended().iter(), |o, v| {
        let _ = write!(o, "{}", v);
    });
    out.push_str("],\"queue_total\":[");
    join(out, crate::sync::queue_totals().iter(), |o, v| {
        let _ = write!(o, "{}", v);
    });
    out.push_str("],\"unknown_syscalls\":[");
    join(out, p.unknown_syscalls().iter(), |o, (nr, n)| {
        let _ = write!(o, "[\"sys_{}\",{}]", nr, n);
    });
    // 直近 5 分の要約 (**プロセスの CPU/要求**。§2 の loopback の 41 us/要求 と同じ物差し)
    let recent = p.recent_totals(res, 300 / profile::RESOLUTIONS[res].0 as usize);
    let _ = write!(
        out,
        "],\"recent\":{{\"secs\":{},\"requests\":{},\"cpu_us\":{},\"cpu_per_request_us\":{},\"cpu_user_us\":{}}}",
        300,
        recent.requests,
        recent.cpu_us,
        match recent.cpu_per_request_us() {
            Some(v) => format!("{:.2}", v),
            None => "null".to_string(),
        },
        recent.cpu_user_us
    );
    // 新しい欄は**末尾**に足す (古い読み手はそのまま動く)。`next_offset` が `null` なら
    // その並びはここで終わり = もう続きは無い
    let _ = write!(
        out,
        ",\"count\":{},\"shown\":{},\"truncated\":{},\"n\":{},\"offset\":{},\"next_offset\":{}}}",
        page.total,
        page.shown,
        page.cut,
        page.n,
        page.offset,
        next_offset(page.offset, page.shown, page.total)
    );
}

/// `?summary=1` — 標本を返さず、5 分 / 1 時間 / 全部の 3 段に畳んだ CPU/要求 だけ (T15.0 (11))。
///
/// 雪像 1 枚に入り切らないほど標本があるときでも、**どの解像度が何本残っていて、
/// この機械が 1 要求に何 us 使っているか**はこれ 1 本で読める。標本を組まないので
/// [`super::is_heavy`] からも外してあり、`/snapshot` を組んでいる最中でも 200 で返る。
fn summary(ep: &Endpoint<'_>, res: usize) -> String {
    let p = &ep.metrics.profile;
    let interval = profile::RESOLUTIONS[res].0;
    let count = p.len(res);
    let mut out = String::with_capacity(1024);
    out.push_str(SCHEMA_HEAD);
    let _ = write!(
        out,
        "\"summary\":true,\"interval_secs\":{},\"sample_ms\":{},\"sampler\":\"{}\",\"spans\":[",
        interval,
        profile::sample_ms(),
        p.sampler().name()
    );
    join(&mut out, SPANS.iter(), |o, (name, secs)| {
        // `all` (秒数 0) は環に残っている全部。ほかは「その秒数ぶんの標本数」で切る
        let want = match *secs {
            0 => count,
            s => (s / interval.max(1)) as usize,
        };
        let folded = want.min(count);
        let s = p.recent_totals(res, want);
        let _ = write!(
            o,
            "{{\"name\":\"{}\",\"secs\":{},\"samples\":{},\"requests\":{},\"cpu_us\":{},\"cpu_per_request_us\":{},\"cpu_user_us\":{}}}",
            name,
            folded as u64 * interval,
            folded,
            s.requests,
            s.cpu_us,
            match s.cpu_per_request_us() {
                Some(v) => format!("{:.2}", v),
                None => "null".to_string(),
            },
            s.cpu_user_us
        );
    });
    let _ = write!(
        out,
        "],\"count\":{},\"capacity\":{}}}",
        count,
        profile::RESOLUTIONS[res].1
    );
    out
}

/// `,` 区切りで並べる小道具 (`json` の配列を組み立てるだけ)。
fn join<T>(
    out: &mut String,
    items: impl Iterator<Item = T>,
    mut write: impl FnMut(&mut String, T),
) {
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.push(',');
        }
        write(out, item);
    }
}

fn quoted(out: &mut String, s: &&str) {
    let _ = write!(out, "\"{}\"", s);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 標本の並び 12 個は、この順のまま (読む側は添字で引く。T.34.95)。
    #[test]
    fn the_sample_keys_keep_their_order() {
        assert_eq!(
            SAMPLE_KEYS,
            [
                "t",
                "requests",
                "cpu_us",
                "connect",
                "forward",
                "threads",
                "locks",
                "queue",
                "threads_top",
                "run_delay_us",
                "user_us",
                "cpu_user_us"
            ]
        );
        let m = crate::metrics::Metrics::new();
        let mut out = String::new();
        write_head(&mut out, &m.profile, 0);
        assert!(out.starts_with(SCHEMA_HEAD), "{}", out);
        // 前は 1 本の文字列で書いていた並びと 1 バイトも違わない
        assert!(
            out.ends_with(
                "],\"keys\":[\"t\",\"requests\",\"cpu_us\",\"connect\",\"forward\",\"threads\",\
                 \"locks\",\"queue\",\"threads_top\",\"run_delay_us\",\"user_us\",\"cpu_user_us\"],\
                 \"samples\":["
            ),
            "{}",
            out
        );
        for key in [
            "\"interval_secs\":",
            "\"bounds_ms\":[",
            "\"stages\":{\"connect\":[",
            "],\"forward\":[",
            "]},\"roles\":[",
            "],\"states\":[",
            "],\"lock_names\":[",
        ] {
            assert!(out.contains(key), "{} が無い: {}", key, out);
        }
    }

    /// 尾は通算・直近 5 分・頁の位置の順で、頁の位置は末尾 (T.34.95)。
    #[test]
    fn the_tail_ends_with_the_page_position() {
        let m = crate::metrics::Metrics::new();
        let mut out = String::new();
        let page = Page {
            total: 3,
            shown: 2,
            cut: true,
            n: 5,
            offset: 0,
        };
        write_tail(&mut out, &m.profile, 0, &page);
        assert!(out.starts_with("],\"locks_total\":["), "{}", out);
        let order = [
            "],\"queue_total\":[",
            "],\"unknown_syscalls\":[",
            "],\"recent\":{\"secs\":300,\"requests\":0,\"cpu_us\":0,\"cpu_per_request_us\":null,",
        ];
        let mut at = 0;
        for key in order {
            let i = out[at..]
                .find(key)
                .unwrap_or_else(|| panic!("{} が無い: {}", key, out));
            at += i + key.len();
        }
        assert!(
            out.ends_with(
                ",\"count\":3,\"shown\":2,\"truncated\":true,\"n\":5,\"offset\":0,\"next_offset\":2}"
            ),
            "{}",
            out
        );
        // 続きが無ければ `null`
        let mut out = String::new();
        let last = Page {
            total: 3,
            shown: 1,
            cut: false,
            n: 1,
            offset: 2,
        };
        write_tail(&mut out, &m.profile, 0, &last);
        assert!(
            out.ends_with("\"offset\":2,\"next_offset\":null}"),
            "{}",
            out
        );
    }
}
