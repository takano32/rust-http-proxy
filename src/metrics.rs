use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::cache::Cache;

/// ホスト別に数える結果の分類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOutcome {
    Hit,
    Miss,
    Bypass,
    Error,
    /// ACL / ブロックリストで拒否した (403)
    Blocked,
}

impl HostOutcome {
    /// アクセスログの `cache=` の値とステータスから分類する。
    pub fn from_access(cache_state: &str, status: u16) -> Self {
        if status >= 500 {
            return HostOutcome::Error;
        }
        if cache_state.starts_with("HIT")
            || cache_state.starts_with("REVALIDATED")
            || cache_state.starts_with("REFRESHING")
            || cache_state.starts_with("COALESCED")
            || cache_state.starts_with("STALE")
        {
            HostOutcome::Hit
        } else if cache_state.starts_with("MISS") {
            HostOutcome::Miss
        } else {
            HostOutcome::Bypass
        }
    }
}

/// 接続元 IP ごとの統計を持つ上限。
pub const MAX_CLIENTS: usize = 1000;

/// 応答時間ヒストグラムの上限 (ms)。最後の区間は上限なし。
pub const LATENCY_BOUNDS_MS: [u64; 9] = [10, 25, 50, 100, 250, 500, 1000, 2500, 5000];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HostStats {
    pub requests: u64,
    pub hits: u64,
    pub misses: u64,
    pub bypass: u64,
    pub errors: u64,
    /// ACL / ブロックリストで拒否した数
    pub blocked: u64,
    pub bytes: u64,
    /// 応答時間を記録した要求数と、その合計・最大 (ms)。CONNECT は接続確立までの時間
    pub timed: u64,
    pub duration_ms_sum: u64,
    pub duration_ms_max: u64,
    /// `LATENCY_BOUNDS_MS` の区間ごとの件数 (+ 上限なしの区間)
    pub buckets: [u64; LATENCY_BOUNDS_MS.len() + 1],
    /// 最後に要求を受けた時刻 (epoch 秒)
    pub last_seen: u64,
}

impl HostStats {
    fn count(&mut self, outcome: HostOutcome, bytes: u64, took: Option<Duration>) {
        self.requests += 1;
        self.bytes += bytes;
        self.last_seen = crate::cache::now_epoch();
        match outcome {
            HostOutcome::Hit => self.hits += 1,
            HostOutcome::Miss => self.misses += 1,
            HostOutcome::Bypass => self.bypass += 1,
            HostOutcome::Error => self.errors += 1,
            HostOutcome::Blocked => self.blocked += 1,
        }
        if let Some(d) = took {
            self.observe(d);
        }
    }

    /// 状態ファイルのレコード (名前 128 バイト + 数値)。
    pub fn encode(&self, name: &str) -> Vec<u8> {
        let mut e = crate::rrd::Enc::new();
        e.str(name, 128)
            .u64(self.last_seen)
            .u64(self.requests)
            .u64(self.hits)
            .u64(self.misses)
            .u64(self.bypass)
            .u64(self.errors)
            .u64(self.blocked)
            .u64(self.bytes)
            .u64(self.timed)
            .u64(self.duration_ms_sum)
            .u64(self.duration_ms_max);
        for b in self.buckets {
            e.u64(b);
        }
        e.0
    }

    pub fn decode(payload: &[u8]) -> Option<(String, HostStats)> {
        let mut d = crate::rrd::Dec(payload);
        let name = d.str(128);
        if name.is_empty() {
            return None;
        }
        let mut s = HostStats {
            last_seen: d.u64(),
            requests: d.u64(),
            hits: d.u64(),
            misses: d.u64(),
            bypass: d.u64(),
            errors: d.u64(),
            blocked: d.u64(),
            bytes: d.u64(),
            timed: d.u64(),
            duration_ms_sum: d.u64(),
            duration_ms_max: d.u64(),
            ..HostStats::default()
        };
        for b in s.buckets.iter_mut() {
            *b = d.u64();
        }
        Some((name, s))
    }

    fn observe(&mut self, d: Duration) {
        let ms = d.as_millis().min(u64::MAX as u128) as u64;
        self.timed += 1;
        self.duration_ms_sum += ms;
        self.duration_ms_max = self.duration_ms_max.max(ms);
        let idx = LATENCY_BOUNDS_MS
            .iter()
            .position(|&b| ms <= b)
            .unwrap_or(LATENCY_BOUNDS_MS.len());
        self.buckets[idx] += 1;
    }

    pub fn avg_ms(&self) -> f64 {
        if self.timed == 0 {
            0.0
        } else {
            self.duration_ms_sum as f64 / self.timed as f64
        }
    }

    /// 区間内を線形に補間した分位点 (ms)。最後の区間は最大値で頭打ち。
    pub fn quantile_ms(&self, q: f64) -> f64 {
        if self.timed == 0 {
            return 0.0;
        }
        let rank = (q.clamp(0.0, 1.0) * self.timed as f64).max(1.0);
        let mut seen = 0u64;
        for (i, &n) in self.buckets.iter().enumerate() {
            if n == 0 {
                continue;
            }
            if (seen + n) as f64 >= rank {
                let lo = if i == 0 {
                    0.0
                } else {
                    LATENCY_BOUNDS_MS[i - 1] as f64
                };
                let hi = if i < LATENCY_BOUNDS_MS.len() {
                    LATENCY_BOUNDS_MS[i] as f64
                } else {
                    (self.duration_ms_max as f64).max(lo)
                };
                let frac = (rank - seen as f64) / n as f64;
                // 観測した最大値は超えない (件数が少ないとき区間の上端が出ないように)
                return (lo + (hi - lo) * frac).min(self.duration_ms_max as f64);
            }
            seen += n;
        }
        self.duration_ms_max as f64
    }

    pub fn error_rate(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.errors as f64 / self.requests as f64
        }
    }
}

/// ホスト別統計の上限。超えた分は `other` にまとめる。
pub const MAX_HOSTS: usize = 1000;

pub struct Metrics {
    pub start_time: Instant,
    pub total_requests: AtomicU64,
    pub active_connections: AtomicUsize,
    pub bytes_forwarded: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    /// オリジンへ新規に張った接続数と、プールから再利用した回数
    pub origin_new: AtomicU64,
    pub origin_reused: AtomicU64,
    /// ダッシュボード用の履歴 (`history::spawn` が記録)
    pub history: crate::history::History,
    /// ホスト (`scheme://host:port`) ごとの統計
    hosts: Mutex<HashMap<String, HostStats>>,
    /// 接続元 IP ごとの統計 (上位 `MAX_CLIENTS`、あふれた分は "other")
    clients: Mutex<HashMap<String, HostStats>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            total_requests: AtomicU64::new(0),
            active_connections: AtomicUsize::new(0),
            bytes_forwarded: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            origin_new: AtomicU64::new(0),
            origin_reused: AtomicU64::new(0),
            history: crate::history::History::default(),
            hosts: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// ホスト別に 1 要求を数える (応答時間なし)。
    pub fn record_host(&self, host: &str, outcome: HostOutcome, bytes: u64) {
        self.record(host, outcome, bytes, None);
    }

    /// ホスト別に 1 要求と応答時間を数える。
    pub fn record_host_timed(&self, host: &str, outcome: HostOutcome, bytes: u64, took: Duration) {
        self.record(host, outcome, bytes, Some(took));
    }

    fn record(&self, host: &str, outcome: HostOutcome, bytes: u64, took: Option<Duration>) {
        let mut hosts = self.hosts.lock().unwrap_or_else(|p| p.into_inner());
        let key = if hosts.len() >= MAX_HOSTS && !hosts.contains_key(host) {
            "other"
        } else {
            host
        };
        hosts
            .entry(key.to_string())
            .or_default()
            .count(outcome, bytes, took);
    }

    /// 接続元 IP ごとに 1 要求を数える。
    pub fn record_client(
        &self,
        client: &str,
        outcome: HostOutcome,
        bytes: u64,
        took: Option<Duration>,
    ) {
        let mut clients = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        let key = if clients.len() >= MAX_CLIENTS && !clients.contains_key(client) {
            "other"
        } else {
            client
        };
        clients
            .entry(key.to_string())
            .or_default()
            .count(outcome, bytes, took);
    }

    /// 要求数の多い順に並べた接続元別統計。
    pub fn clients_sorted(&self) -> Vec<(String, HostStats)> {
        let clients = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        let mut v: Vec<(String, HostStats)> = clients
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        v.sort_by(|a, b| b.1.requests.cmp(&a.1.requests).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// 起動時に状態ファイルから読み戻す (今の値が空のときだけ)。
    pub fn restore(&self, hosts: Vec<(String, HostStats)>, clients: Vec<(String, HostStats)>) {
        let mut h = self.hosts.lock().unwrap_or_else(|p| p.into_inner());
        if h.is_empty() {
            h.extend(hosts);
        }
        let mut c = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        if c.is_empty() {
            c.extend(clients);
        }
    }

    /// 要求数の多い順に並べたホスト別統計。
    pub fn hosts_sorted(&self) -> Vec<(String, HostStats)> {
        let hosts = self.hosts.lock().unwrap_or_else(|p| p.into_inner());
        let mut v: Vec<(String, HostStats)> =
            hosts.iter().map(|(k, s)| (k.clone(), s.clone())).collect();
        v.sort_by(|a, b| b.1.requests.cmp(&a.1.requests).then_with(|| a.0.cmp(&b.0)));
        v
    }

    pub fn inc_requests(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_active_conn(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_active_conn(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn add_bytes(&self, bytes: u64) {
        self.bytes_forwarded.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn inc_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_origin_conn(&self, reused: bool) {
        if reused {
            self.origin_reused.fetch_add(1, Ordering::Relaxed);
        } else {
            self.origin_new.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn to_json(&self) -> String {
        self.to_json_with_cache(None)
    }

    /// キャッシュ統計を含めた `/status` 用 JSON を生成する。
    pub fn to_json_with_cache(&self, cache: Option<&Cache>) -> String {
        let uptime = self.start_time.elapsed().as_secs();
        let requests = self.total_requests.load(Ordering::Relaxed);
        let active = self.active_connections.load(Ordering::Relaxed);
        let bytes = self.bytes_forwarded.load(Ordering::Relaxed);

        let cache_json = match cache {
            Some(c) => c.to_json(),
            None => "null".to_string(),
        };

        let hosts_json: Vec<String> = self
            .hosts_sorted()
            .into_iter()
            .take(50)
            .map(|(h, s)| format!("{{\"host\":\"{}\",{}}}", json_str(&h), stats_json(&s)))
            .collect();
        let clients_json: Vec<String> = self
            .clients_sorted()
            .into_iter()
            .take(50)
            .map(|(c, s)| format!("{{\"client\":\"{}\",{}}}", json_str(&c), stats_json(&s)))
            .collect();
        format!(
            concat!(
                "{{\"status\":\"ok\",\"uptime_secs\":{},\"total_requests\":{},",
                "\"active_connections\":{},\"bytes_forwarded\":{},",
                "\"cache_hits\":{},\"cache_misses\":{},",
                "\"origin_connections\":{{\"new\":{},\"reused\":{}}},",
                "\"hosts\":[{}],\"clients\":[{}],",
                "\"log_level\":\"{}\",\"settings\":{},\"dns\":{},\"blocklist\":{},\"state_file\":{},\"cache\":{}}}"
            ),
            uptime,
            requests,
            active,
            bytes,
            self.cache_hits.load(Ordering::Relaxed),
            self.cache_misses.load(Ordering::Relaxed),
            self.origin_new.load(Ordering::Relaxed),
            self.origin_reused.load(Ordering::Relaxed),
            hosts_json.join(","),
            clients_json.join(","),
            crate::log::current_level().as_str().trim(),
            crate::reload::status_json(),
            crate::dns::status_json(),
            crate::blocklist::status_json(),
            crate::persist::status_json(),
            cache_json
        )
    }
}

fn json_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// ホスト別 / 接続元別に共通の統計フィールド (先頭・末尾の波括弧なし)。
fn stats_json(s: &HostStats) -> String {
    format!(
        "\"requests\":{},\"hits\":{},\"misses\":{},\"bypass\":{},\"errors\":{},\"blocked\":{},\"bytes\":{},\"timed\":{},\"avg_ms\":{:.1},\"p50_ms\":{:.1},\"p95_ms\":{:.1},\"max_ms\":{},\"last_seen\":{}",
        s.requests,
        s.hits,
        s.misses,
        s.bypass,
        s.errors,
        s.blocked,
        s.bytes,
        s.timed,
        s.avg_ms(),
        s.quantile_ms(0.5),
        s.quantile_ms(0.95),
        s.duration_ms_max,
        s.last_seen
    )
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics() {
        let metrics = Metrics::new();
        metrics.inc_requests();
        metrics.inc_active_conn();
        metrics.add_bytes(1024);

        let json = metrics.to_json();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"total_requests\":1"));
        assert!(json.contains("\"active_connections\":1"));
        assert!(json.contains("\"bytes_forwarded\":1024"));

        metrics.inc_cache_hit();
        metrics.inc_cache_miss();
        let json_c = metrics.to_json();
        assert!(json_c.contains("\"cache_hits\":1"));
        assert!(json_c.contains("\"cache_misses\":1"));
        assert!(json_c.contains("\"cache\":null"));

        metrics.dec_active_conn();
        let json2 = metrics.to_json();
        assert!(json2.contains("\"active_connections\":0"));
    }

    #[test]
    fn clients_and_blocked_are_counted() {
        let m = Metrics::new();
        m.record_client(
            "10.0.0.1",
            HostOutcome::Hit,
            100,
            Some(Duration::from_millis(30)),
        );
        m.record_client("10.0.0.1", HostOutcome::Blocked, 0, None);
        m.record_client("10.0.0.2", HostOutcome::Bypass, 5, None);
        m.record_host("blocked://ads.example", HostOutcome::Blocked, 0);
        let clients = m.clients_sorted();
        assert_eq!(clients[0].0, "10.0.0.1");
        assert_eq!(clients[0].1.requests, 2);
        assert_eq!(clients[0].1.blocked, 1);
        assert_eq!(clients[0].1.timed, 1);
        assert!(clients[0].1.last_seen > 0);
        let enc = clients[0].1.encode("10.0.0.1");
        assert!(enc.len() <= crate::rrd::STATS_RECORD - 4);
        let (name, back) = HostStats::decode(&enc).unwrap();
        assert_eq!(name, "10.0.0.1");
        assert_eq!(back, clients[0].1);
        let json = m.to_json();
        assert!(json.contains("\"clients\":[{\"client\":\"10.0.0.1\",\"requests\":2"));
        assert!(json.contains("\"blocked\":1"));
        assert!(json.contains("\"host\":\"blocked://ads.example\",\"requests\":1,\"hits\":0,\"misses\":0,\"bypass\":0,\"errors\":0,\"blocked\":1"));
    }

    #[test]
    fn test_host_stats() {
        let m = Metrics::new();
        m.record_host(
            "http://a:80",
            HostOutcome::from_access("HIT(memory) age=1s", 200),
            10,
        );
        m.record_host(
            "http://a:80",
            HostOutcome::from_access("MISS stored ttl=1s", 200),
            20,
        );
        m.record_host("http://b:80", HostOutcome::from_access("BYPASS", 200), 5);
        m.record_host("http://b:80", HostOutcome::from_access("MISS", 502), 0);
        let hosts = m.hosts_sorted();
        assert_eq!(hosts[0].0, "http://a:80");
        assert!(hosts[0].1.last_seen > 0);
        assert_eq!(
            HostStats {
                last_seen: 0,
                ..hosts[0].1.clone()
            },
            HostStats {
                requests: 2,
                hits: 1,
                misses: 1,
                bypass: 0,
                errors: 0,
                bytes: 30,
                ..Default::default()
            }
        );
        assert_eq!(hosts[1].1.errors, 1);
        assert_eq!(hosts[1].1.bypass, 1);
        let json = m.to_json();
        assert!(
            json.contains("\"hosts\":[{\"host\":\"http://a:80\",\"requests\":2"),
            "{}",
            json
        );
        for i in 0..(MAX_HOSTS + 5) {
            m.record_host(&format!("http://h{}:80", i), HostOutcome::Hit, 1);
        }
        let hosts = m.hosts_sorted();
        assert!(hosts.len() <= MAX_HOSTS + 1);
        assert!(hosts.iter().any(|(h, _)| h == "other"));
    }
}

#[cfg(test)]
mod latency_tests {
    use super::*;

    #[test]
    fn quantiles_interpolate_within_buckets() {
        let m = Metrics::new();
        for ms in [5, 20, 40, 80, 200, 400, 800, 2000, 4000, 9000] {
            m.record_host_timed(
                "http://a:80",
                HostOutcome::Miss,
                0,
                Duration::from_millis(ms),
            );
        }
        let (_, s) = &m.hosts_sorted()[0];
        assert_eq!(s.timed, 10);
        assert_eq!(s.duration_ms_max, 9000);
        assert!((s.avg_ms() - 1654.5).abs() < 0.01);
        // 各区間に 1 件ずつ: p50 は 5 番目の区間 (100..250] の上端、p95 は最後の区間の途中
        assert!(
            (s.quantile_ms(0.5) - 250.0).abs() < 1e-6,
            "{}",
            s.quantile_ms(0.5)
        );
        let p95 = s.quantile_ms(0.95);
        assert!(p95 > 5000.0 && p95 <= 9000.0, "{}", p95);
        assert_eq!(s.quantile_ms(1.0), 9000.0);
        assert_eq!(HostStats::default().quantile_ms(0.5), 0.0);
    }

    #[test]
    fn error_rate_counts_all_requests() {
        let m = Metrics::new();
        m.record_host("http://a:80", HostOutcome::Error, 0);
        m.record_host("http://a:80", HostOutcome::Hit, 0);
        m.record_host("http://a:80", HostOutcome::Hit, 0);
        m.record_host("http://a:80", HostOutcome::Hit, 0);
        let (_, s) = &m.hosts_sorted()[0];
        assert!((s.error_rate() - 0.25).abs() < 1e-9);
        assert_eq!(s.timed, 0);
    }
}
