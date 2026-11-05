//! 表の中身とカウンタを外に見せる所: `/dns` の 1 行 ([`TableRow`]) と並べ替え、`/status` の `"dns"` 要素、
//! Prometheus 用のカウンタ。T21.7 で `dns.rs` から移した (本体は変えていない)。公開の道は親の
//! `dns` が出し直すので `proxy_net_dns::dns::…` のまま。

use super::{
    CHANGES, FAILURES, HITS, MISS_COLD, MISS_EXPIRED, MISS_NEGATIVE, MISS_WARM_STALE, MISSES,
    NEGATIVE_SECS, REFRESH_FAILURES, REFRESH_LATE, REFRESH_US_MAX, REFRESH_US_SUM, REFRESHES,
    STALE, STALE_HITS, STALE_RETRIES, TABLE, TTL_SECS, WARM_EVICTED, WARM_SECS,
    misses_by_kind_json, resolve_cost_total, ttl, warm_count,
};
use crate::sync::LockExt;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// `/dns` の並べ替えの鍵 (T13.4)。知らない値は既定 (`age`) に倒す
/// (`/status?sort=` と同じ方針で、綴り違いで 400 を返すより今までどおり返す方が安全)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DnsSort {
    /// 解決してからの経過が長い順 (既定。期限に近いものが上に来る)
    #[default]
    Age,
    /// ホスト名の順
    Host,
    /// OS に問い合わせた回数の多い順 (先回りが効いていない名前が上に来る)
    Misses,
}

impl DnsSort {
    pub fn from_param(v: &str) -> DnsSort {
        match v {
            "host" => DnsSort::Host,
            "misses" => DnsSort::Misses,
            _ => DnsSort::Age,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            DnsSort::Age => "age",
            DnsSort::Host => "host",
            DnsSort::Misses => "misses",
        }
    }
}

/// 1 行に出すアドレスの上限 (CDN は A / AAAA を 10 本以上返すことがある)。
pub const MAX_ROW_ADDRS: usize = 8;

/// `/dns` の 1 行 (T13.4)。表の中身をそのまま写したもの。
#[derive(Debug, Clone)]
pub struct TableRow {
    pub host: String,
    /// 覚えているアドレス (多いときは [`MAX_ROW_ADDRS`] 本で切る)
    pub addrs: Vec<IpAddr>,
    /// 覚えている本数 (切る前)
    pub addr_count: usize,
    /// 解決してからの秒
    pub age_secs: u64,
    /// 残り TTL (秒。0 = 期限切れ)
    pub ttl_left: u64,
    /// 最後に引いてからの秒
    pub idle_secs: u64,
    /// このホストで最後に勝った族 (`Some(true)` = IPv6。T12.1)
    pub win_v6: Option<bool>,
    /// 負のキャッシュ: (何秒前に失敗したか, 理由)
    pub failed: Option<(u64, String)>,
    /// 裏で引き直している最中か (T13.1)
    pub refreshing: bool,
    /// warm な名前か = 使われなくても 3/4 TTL ごとに引き直す名前か (T14.1)
    pub warm: bool,
    /// 次に裏で引き直すまでの秒 (warm でなければ `None`。T14.1)
    pub next_refresh_secs: Option<u64>,
    /// OS に問い合わせた回数と、期限前に裏で引き直した回数
    pub misses: u64,
    pub refreshes: u64,
    /// 引き直しで答えの集合が変わった回数 (T14.37)。`refreshes + misses` のうち
    /// どれだけ答えが動いたかが、TTL の長さを決める材料になる
    pub changes: u64,
    /// warm な状態でこの名前が引かれた回数 (通算。T15.0 (7))
    pub warm_requests: u64,
    /// ミスの種類別の回数 (通算。並びは cold / expired / warm_stale / negative。
    /// T15.0 (7))。**和は `misses` と一致する**
    pub misses_by_kind: [u64; 4],
}

impl TableRow {
    /// `/dns` の 1 要素。
    pub fn to_json(&self) -> String {
        let addrs: Vec<String> = self.addrs.iter().map(|a| format!("\"{}\"", a)).collect();
        let failed = match &self.failed {
            Some((secs, msg)) => format!(
                "{{\"secs_ago\":{},\"error\":\"{}\"}}",
                secs,
                // 理由は `getaddrinfo` の文言なので短い。念のため 120 B で切る
                crate::json::escape(&clip(msg, 120))
            ),
            None => "null".to_string(),
        };
        format!(
            "{{\"host\":\"{}\",\"addrs\":[{}],\"addr_count\":{},\"age_secs\":{},\"ttl_left\":{},\"idle_secs\":{},\"win_v6\":{},\"failed\":{},\"refreshing\":{},\"warm\":{},\"next_refresh_secs\":{},\"misses\":{},\"refreshes\":{},\"changes\":{},\"warm_requests\":{},\"misses_by_kind\":{}}}",
            crate::json::escape(&self.host),
            addrs.join(","),
            self.addr_count,
            self.age_secs,
            self.ttl_left,
            self.idle_secs,
            match self.win_v6 {
                Some(true) => "true",
                Some(false) => "false",
                None => "null",
            },
            failed,
            self.refreshing,
            self.warm,
            match self.next_refresh_secs {
                Some(s) => s.to_string(),
                None => "null".to_string(),
            },
            self.misses,
            self.refreshes,
            self.changes,
            self.warm_requests,
            misses_by_kind_json(&self.misses_by_kind),
        )
    }
}

/// 文字列を `max` バイト以内に切る (文字の途中で切らない)。
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// 名前解決の表の中身 (`/dns`。T13.4)。**鍵の内側では複製だけして、並べ替えも
/// 組み立ても外でやる** (表は要求の経路が取る鍵なので、長く握らない)。
pub fn table(sort: DnsSort) -> Vec<TableRow> {
    let now = Instant::now();
    let ttl = ttl();
    let mut rows: Vec<TableRow> = {
        let guard = TABLE.locked();
        let Some(t) = guard.as_ref() else {
            return Vec::new();
        };
        t.iter()
            .map(|(host, e)| {
                let age = now.saturating_duration_since(e.resolved_at);
                TableRow {
                    host: host.clone(),
                    addrs: e.addrs.iter().take(MAX_ROW_ADDRS).copied().collect(),
                    addr_count: e.addrs.len(),
                    age_secs: age.as_secs(),
                    ttl_left: ttl.saturating_sub(age).as_secs(),
                    idle_secs: now.saturating_duration_since(e.last_used).as_secs(),
                    win_v6: e.last_win_v6,
                    failed: e.failed_at.as_ref().map(|(at, _, msg)| {
                        (now.saturating_duration_since(*at).as_secs(), msg.clone())
                    }),
                    refreshing: e.refreshing,
                    warm: e.warm,
                    next_refresh_secs: e
                        .next_refresh
                        .map(|at| at.saturating_duration_since(now).as_secs()),
                    misses: e.misses,
                    refreshes: e.refreshes,
                    changes: e.changes,
                    warm_requests: e.warm_requests,
                    misses_by_kind: e.misses_by_kind,
                }
            })
            .collect()
    };
    // 同点は名前で崩す (どの鍵でも順序が 1 つに決まる。`/status?sort=` と同じ方針)
    rows.sort_by(|a, b| match sort {
        DnsSort::Age => b
            .age_secs
            .cmp(&a.age_secs)
            .then_with(|| a.host.cmp(&b.host)),
        DnsSort::Host => a.host.cmp(&b.host),
        DnsSort::Misses => b.misses.cmp(&a.misses).then_with(|| a.host.cmp(&b.host)),
    });
    rows
}

/// 覚えている名前の数 (`/dns` の `count`)。
pub fn entries() -> usize {
    TABLE.locked().as_ref().map_or(0, HashMap::len)
}

/// `/status` の `"dns"` 要素。
pub fn status_json() -> String {
    let entries = entries();
    let (us, misses) = resolve_cost_total();
    let timeout = super::timeout();
    let (timeouts, busy, late, in_flight) = super::resolver_counters();
    // ミスの内訳と引き直しの様子 (T15.0 (7))。`misses_by_kind` の 4 つの和は `misses` と
    // 一致する。`refresh_*` は `dns-refresh` スレッド 1 本ぶん
    let by_kind = [
        MISS_COLD.load(Ordering::Relaxed),
        MISS_EXPIRED.load(Ordering::Relaxed),
        MISS_WARM_STALE.load(Ordering::Relaxed),
        MISS_NEGATIVE.load(Ordering::Relaxed),
    ];
    format!(
        "{{\"ttl_secs\":{},\"negative_ttl_secs\":{},\"warm_secs\":{},\"entries\":{},\"warm\":{},\"hits\":{},\"misses\":{},\"stale_served\":{},\"negative_hits\":{},\"refreshes\":{},\"changes\":{},\"miss_ms_sum\":{:.1},\"miss_avg_ms\":{:.2},\"misses_by_kind\":{},\"refresh_failures\":{},\"refresh_ms_sum\":{:.1},\"refresh_ms_max\":{:.1},\"refresh_late\":{},\"warm_evicted\":{},\"stale_hits\":{},\"stale_retries\":{},\"timeout_secs\":{},\"timeouts\":{},\"resolver_busy\":{},\"late_answers\":{},\"resolvers\":{},\"resolver_limit\":{}}}",
        TTL_SECS.load(Ordering::Relaxed),
        NEGATIVE_SECS.load(Ordering::Relaxed),
        WARM_SECS.load(Ordering::Relaxed),
        entries,
        warm_count(),
        HITS.load(Ordering::Relaxed),
        MISSES.load(Ordering::Relaxed),
        STALE.load(Ordering::Relaxed),
        FAILURES.load(Ordering::Relaxed),
        REFRESHES.load(Ordering::Relaxed),
        CHANGES.load(Ordering::Relaxed),
        us as f64 / 1000.0,
        if misses == 0 {
            0.0
        } else {
            us as f64 / 1000.0 / misses as f64
        },
        misses_by_kind_json(&by_kind),
        REFRESH_FAILURES.load(Ordering::Relaxed),
        REFRESH_US_SUM.load(Ordering::Relaxed) as f64 / 1000.0,
        REFRESH_US_MAX.load(Ordering::Relaxed) as f64 / 1000.0,
        REFRESH_LATE.load(Ordering::Relaxed),
        // T16.0 で末尾に足したもの (起動からの通算)
        WARM_EVICTED.load(Ordering::Relaxed),
        // T21.3 で末尾に足したもの (起動からの通算)。`stale_served` とは意味が違う
        STALE_HITS.load(Ordering::Relaxed),
        STALE_RETRIES.load(Ordering::Relaxed),
        // T.32.11 で末尾に足したもの: 締め切り (秒)・締め切りで諦めた回数・上限で待たずに
        // 断った回数・諦めたあとに答えが出て表に書いた回数 (起動からの通算)、いま走っている
        // `getaddrinfo` の数とその上限
        timeout.as_secs(),
        timeouts,
        busy,
        late,
        in_flight,
        super::resolver_limit(),
    )
}

/// Prometheus 用のカウンタ (hits, misses, stale, negative, refresh)。
pub fn counters() -> [u64; 5] {
    [
        HITS.load(Ordering::Relaxed),
        MISSES.load(Ordering::Relaxed),
        STALE.load(Ordering::Relaxed),
        FAILURES.load(Ordering::Relaxed),
        REFRESHES.load(Ordering::Relaxed),
    ]
}
