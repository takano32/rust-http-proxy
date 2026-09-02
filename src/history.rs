//! ダッシュボード用の履歴: 累計値の標本を 3 つの解像度の環状バッファに残し、`/history` で
//! JSON にして返す。ブラウザ側で差分からレート (req/s, MB/s) を求める。
//!
//! - 5 秒 × 720 (1 時間)、1 分 × 1440 (1 日)、1 時間 × 720 (30 日)
//! - 粗い解像度は細かい標本から作る: 累計カウンタは窓の最後の値、ゲージ (接続数・使用量) は平均
//! - 状態ファイル ([`crate::persist`]) があれば各解像度をそこにも書き、起動時に読み戻す

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::cache::{Cache, now_epoch};
use crate::metrics::Metrics;
use crate::rrd::{Dec, Enc};

/// 記録の間隔と本数 (5 秒 × 720 = 1 時間)。
pub const INTERVAL: Duration = Duration::from_secs(5);
pub const CAPACITY: usize = 720;

/// 解像度 (秒) と本数。
pub const RESOLUTIONS: [(u64, usize); 3] = [(5, 720), (60, 1440), (3600, 720)];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

    /// 状態ファイルのレコード (先頭が時刻)。
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u64(self.t)
            .u64(self.requests)
            .u64(self.bytes)
            .u64(self.active as u64)
            .u64(self.hits)
            .u64(self.misses)
            .u64(self.stores)
            .u64(self.evictions)
            .u64(self.mem_used)
            .u64(self.mem_limit)
            .u64(self.disk_used)
            .u64(self.disk_limit)
            .u64(self.rss);
        e.0
    }

    pub fn decode(payload: &[u8]) -> Option<Sample> {
        let mut d = Dec(payload);
        let t = d.u64();
        if t == 0 {
            return None;
        }
        Some(Sample {
            t,
            requests: d.u64(),
            bytes: d.u64(),
            active: d.u64() as usize,
            hits: d.u64(),
            misses: d.u64(),
            stores: d.u64(),
            evictions: d.u64(),
            mem_used: d.u64(),
            mem_limit: d.u64(),
            disk_used: d.u64(),
            disk_limit: d.u64(),
            rss: d.u64(),
        })
    }

    /// 窓の標本をひとつにまとめる: 累計は最後の値、ゲージは平均、時刻は窓の先頭。
    fn downsample(window: &[Sample], t: u64) -> Sample {
        let last = window.last().copied().unwrap_or_default();
        let n = window.len().max(1) as u64;
        let avg = |f: fn(&Sample) -> u64| window.iter().map(f).sum::<u64>() / n;
        Sample {
            t,
            requests: last.requests,
            bytes: last.bytes,
            active: avg(|s| s.active as u64) as usize,
            hits: last.hits,
            misses: last.misses,
            stores: last.stores,
            evictions: last.evictions,
            mem_used: avg(|s| s.mem_used),
            mem_limit: avg(|s| s.mem_limit),
            disk_used: avg(|s| s.disk_used),
            disk_limit: avg(|s| s.disk_limit),
            rss: avg(|s| s.rss),
        }
    }
}

/// 1 回の記録で各解像度に加わった標本 (状態ファイルへの書込用)。
#[derive(Debug, Default, Clone, Copy)]
pub struct Pushed {
    pub fine: Option<Sample>,
    pub minute: Option<Sample>,
    pub hour: Option<Sample>,
}

#[derive(Default)]
pub struct History {
    rings: [Mutex<VecDeque<Sample>>; 3],
}

impl History {
    /// 5 秒の標本を記録し、分・時の窓が閉じたらそれらも作る。
    pub fn push(&self, s: Sample) -> Pushed {
        let mut out = Pushed {
            fine: Some(s),
            ..Pushed::default()
        };
        Self::append(&self.rings[0], s, RESOLUTIONS[0].1);
        out.minute = self.roll(0, 1, s.t);
        if out.minute.is_some() {
            out.hour = self.roll(1, 2, s.t);
        }
        out
    }

    /// `from` の標本から、`to` の解像度で前の窓が閉じていればひとつ作る。
    fn roll(&self, from: usize, to: usize, now: u64) -> Option<Sample> {
        let step = RESOLUTIONS[to].0;
        let window_start = (now / step) * step;
        let last_to = self.rings[to]
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .back()
            .map(|s| s.t);
        // 直前の窓 [window_start - step, window_start) がまだ無ければ作る
        let prev = window_start.checked_sub(step)?;
        if last_to.is_some_and(|t| t >= prev) {
            return None;
        }
        let src = self.rings[from].lock().unwrap_or_else(|p| p.into_inner());
        let window: Vec<Sample> = src
            .iter()
            .filter(|s| s.t >= prev && s.t < window_start)
            .copied()
            .collect();
        drop(src);
        if window.is_empty() {
            return None;
        }
        let agg = Sample::downsample(&window, prev);
        Self::append(&self.rings[to], agg, RESOLUTIONS[to].1);
        Some(agg)
    }

    fn append(ring: &Mutex<VecDeque<Sample>>, s: Sample, cap: usize) {
        let mut q = ring.lock().unwrap_or_else(|p| p.into_inner());
        if q.len() >= cap {
            q.pop_front();
        }
        q.push_back(s);
    }

    /// 起動時に状態ファイルから読み戻す (`res` は 0=5 秒, 1=1 分, 2=1 時間)。
    pub fn restore(&self, res: usize, samples: impl IntoIterator<Item = Sample>) {
        let mut q = self.rings[res].lock().unwrap_or_else(|p| p.into_inner());
        q.clear();
        q.extend(samples);
        while q.len() > RESOLUTIONS[res].1 {
            q.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.rings[0]
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 解像度 (秒) から添字を引く。合わなければ 5 秒。
    pub fn index_for(secs: u64) -> usize {
        RESOLUTIONS
            .iter()
            .position(|(s, _)| *s == secs)
            .unwrap_or(0)
    }

    /// `{"interval_secs":N,"samples":[...]}` (古い順)。
    pub fn to_json(&self) -> String {
        self.to_json_res(0)
    }

    pub fn to_json_res(&self, res: usize) -> String {
        let res = res.min(RESOLUTIONS.len() - 1);
        let q = self.rings[res].lock().unwrap_or_else(|p| p.into_inner());
        let mut out = format!("{{\"interval_secs\":{},\"samples\":[", RESOLUTIONS[res].0);
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

/// 定期的に記録するスレッドを起動する。記録先は `metrics.history`、`store` があれば状態ファイルにも。
pub fn spawn(
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
    store: Option<Arc<crate::persist::Store>>,
) -> JoinHandle<()> {
    let record = move |metrics: &Metrics, cache: &Cache| {
        let pushed = metrics.history.push(Sample::take(metrics, cache));
        if let Some(st) = &store {
            st.write_samples(&pushed);
        }
    };
    record(&metrics, &cache);
    thread::Builder::new()
        .name("history".into())
        .spawn(move || {
            loop {
                thread::sleep(INTERVAL);
                record(&metrics, &cache);
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
            active: (t % 7) as usize,
            mem_used: 100,
            ..Sample::default()
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

    #[test]
    fn minute_and_hour_windows_are_rolled_up() {
        let h = History::default();
        let mut minutes = 0;
        let mut hours = 0;
        // 2 時間 + 少し、5 秒刻み
        for i in 0..(2 * 720 + 20) {
            let p = h.push(sample(1_000_000 + i * 5));
            if p.minute.is_some() {
                minutes += 1;
            }
            if p.hour.is_some() {
                hours += 1;
            }
        }
        assert!((119..=123).contains(&minutes), "minutes {}", minutes);
        assert!((1..=3).contains(&hours), "hours {}", hours);
        let m = h.to_json_res(1);
        assert!(m.starts_with("{\"interval_secs\":60,"));
        let q = h.rings[1].lock().unwrap();
        let first = q.front().unwrap();
        assert_eq!(first.t % 60, 0, "window start is aligned");
        // 累計は窓の最後の値 (t + 55 の standard)、ゲージは平均
        assert_eq!(first.requests, (first.t + 55) * 2);
        assert_eq!(first.mem_used, 100);
        let hq = h.rings[2].lock().unwrap();
        assert_eq!(hq.front().unwrap().t % 3600, 0);
    }

    #[test]
    fn sample_encoding_round_trips() {
        let s = Sample {
            t: 5,
            requests: 6,
            bytes: 7,
            active: 8,
            hits: 9,
            misses: 10,
            stores: 11,
            evictions: 12,
            mem_used: 13,
            mem_limit: 14,
            disk_used: 15,
            disk_limit: 16,
            rss: 17,
        };
        let enc = s.encode();
        assert!(enc.len() <= crate::rrd::SAMPLE_RECORD - 4);
        assert_eq!(Sample::decode(&enc), Some(s));
        assert_eq!(Sample::decode(&[0u8; 104]), None);
    }
}
