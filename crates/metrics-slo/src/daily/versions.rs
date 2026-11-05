//! その日に動いた版の並び (T.32.18)。

use super::{MAX_VERSION, MAX_VERSIONS};

/// 版の文字列を 1 行に入れられる形に直す (印字できる ASCII だけ、[`MAX_VERSION`] 文字)。
pub(super) fn clean_version(v: &str) -> String {
    v.chars()
        .filter(|c| c.is_ascii_graphic() && *c != '"' && *c != '\\')
        .take(MAX_VERSION)
        .collect()
}

/// その日に動いた版の並び (T.32.18)。`/events` の `start` (`version <版> on port …`) から、
/// 0 時に動いていた版 (`with_midnight` のとき。0 時より前のいちばん新しい `start`) と、0 時から
/// `until` までに起動した版を古い順に並べ、最後に今の版 `current` を置く (続けて同じ版は 1 つに)。
/// 長さが [`MAX_VERSIONS`] を越えたら古い方から落とす。
pub(super) fn versions_of_day(
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
