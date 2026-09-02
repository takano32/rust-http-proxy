//! ダッシュボード用の履歴: 数秒ごとの累計値を環状バッファに残し、`/history` で JSON にして返す。
//! ブラウザ側で差分からレート (req/s, MB/s) を求める。

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::cache::{Cache, now_epoch};
use crate::metrics::Metrics;

/// 記録の間隔と本数 (5 秒 × 720 = 1 時間)。
pub const INTERVAL: Duration = Duration::from_secs(5);
pub const CAPACITY: usize = 720;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub t: u64,
    pub requests: u64,
    pub bytes: u64,
    pub active: usize,
    pub hits: u64,
    pub misses: u64,
    pub stores: u64,
    pub evictions: u64,
    pub mem_used: u64,
    pub mem_limit: u64,
    pub disk_used: u64,
    pub disk_limit: u64,
    pub rss: u64,
}

impl Sample {
    pub fn take(metrics: &Metrics, cache: &Cache) -> Self {
        let (mem_used, _) = cache.mem_usage();
        let (disk_used, _) = cache.disk_usage();
        Self {
            t: now_epoch(),
            requests: metrics.total_requests.load(Ordering::Relaxed),
            bytes: metrics.bytes_forwarded.load(Ordering::Relaxed),
            active: metrics.active_connections.load(Ordering::Relaxed),
            hits: metrics.cache_hits.load(Ordering::Relaxed),
            misses: metrics.cache_misses.load(Ordering::Relaxed),
            stores: cache.stores.load(Ordering::Relaxed),
            evictions: cache.evictions.load(Ordering::Relaxed),
            mem_used,
            mem_limit: cache.mem_capacity(),
            disk_used,
            disk_limit: cache.disk_capacity(),
            rss: cache.snapshot().rss.unwrap_or(0),
        }
    }

    fn json(&self) -> String {
        format!(
            "{{\"t\":{},\"requests\":{},\"bytes\":{},\"active\":{},\"hits\":{},\"misses\":{},\"stores\":{},\"evictions\":{},\"mem_used\":{},\"mem_limit\":{},\"disk_used\":{},\"disk_limit\":{},\"rss\":{}}}",
            self.t,
            self.requests,
            self.bytes,
            self.active,
            self.hits,
            self.misses,
            self.stores,
            self.evictions,
            self.mem_used,
            self.mem_limit,
            self.disk_used,
            self.disk_limit,
            self.rss
        )
    }
}

#[derive(Default)]
pub struct History {
    samples: Mutex<VecDeque<Sample>>,
}

impl History {
    pub fn push(&self, s: Sample) {
        let mut q = self.samples.lock().unwrap_or_else(|p| p.into_inner());
        if q.len() >= CAPACITY {
            q.pop_front();
        }
        q.push_back(s);
    }

    pub fn len(&self) -> usize {
        self.samples.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `{"interval_secs":5,"samples":[...]}` (古い順)。
    pub fn to_json(&self) -> String {
        let q = self.samples.lock().unwrap_or_else(|p| p.into_inner());
        let mut out = format!("{{\"interval_secs\":{},\"samples\":[", INTERVAL.as_secs());
        for (i, s) in q.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&s.json());
        }
        out.push_str("]}");
        out
    }
}

/// 定期的に記録するスレッドを起動する。記録先は `metrics.history`。
pub fn spawn(metrics: Arc<Metrics>, cache: Arc<Cache>) -> JoinHandle<()> {
    metrics.history.push(Sample::take(&metrics, &cache));
    thread::Builder::new()
        .name("history".into())
        .spawn(move || {
            loop {
                thread::sleep(INTERVAL);
                metrics.history.push(Sample::take(&metrics, &cache));
            }
        })
        .expect("spawn history thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t: u64) -> Sample {
        Sample {
            t,
            requests: t * 2,
            bytes: 0,
            active: 0,
            hits: 0,
            misses: 0,
            stores: 0,
            evictions: 0,
            mem_used: 0,
            mem_limit: 0,
            disk_used: 0,
            disk_limit: 0,
            rss: 0,
        }
    }

    #[test]
    fn keeps_the_newest_samples_in_order() {
        let h = History::default();
        for t in 0..(CAPACITY as u64 + 5) {
            h.push(sample(t));
        }
        assert_eq!(h.len(), CAPACITY);
        let json = h.to_json();
        assert!(json.starts_with("{\"interval_secs\":5,\"samples\":[{\"t\":5,"));
        assert!(json.ends_with("\"rss\":0}]}"));
    }
}
