//! 集計: 1 モードの結果 ([`Report`]) と、`conc` 本のスレッドで `seconds` 秒だけ回す骨組み ([`run_load`])。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) struct Report {
    pub(crate) ops: u64,
    pub(crate) bytes: u64,
    pub(crate) elapsed: Duration,
    pub(crate) latencies_us: Vec<u32>,
}

impl Report {
    pub(crate) fn percentile(&self, p: f64) -> f64 {
        if self.latencies_us.is_empty() {
            return 0.0;
        }
        let idx = ((self.latencies_us.len() - 1) as f64 * p).round() as usize;
        self.latencies_us[idx] as f64 / 1000.0
    }

    /// `--only connect-multi` 用の 1 行 (p50 / p95 / **max**)。
    ///
    /// この経路で読みたいのは「250 ms を払ったか」なので、p99 より **max** が効く
    /// (1 本目だけ払って以後は払わない、が正しい姿)。**`(N ops in X.Xs)` の形は
    /// [`Report::print`] と揃える** (`scripts/cpu-per-request.sh` がこの括弧から
    /// 操作数を読んで CPU/本 を出す)。
    pub(crate) fn print_multi(&self, label: &str) {
        let secs = self.elapsed.as_secs_f64().max(1e-9);
        println!(
            "{:<8} {:>9.0} op/s  p50 {:>8.3} ms  p95 {:>8.3} ms  max {:>8.3} ms  ({} ops in {:.1}s)",
            label,
            self.ops as f64 / secs,
            self.percentile(0.50),
            self.percentile(0.95),
            self.percentile(1.0),
            self.ops,
            secs,
        );
    }

    pub(crate) fn print(&self, label: &str) {
        let secs = self.elapsed.as_secs_f64().max(1e-9);
        println!(
            "{:<8} {:>9.0} op/s  {:>8.1} MiB/s  p50 {:>7.3} ms  p99 {:>7.3} ms  ({} ops in {:.1}s)",
            label,
            self.ops as f64 / secs,
            self.bytes as f64 / secs / (1024.0 * 1024.0),
            self.percentile(0.50),
            self.percentile(0.99),
            self.ops,
            secs,
        );
    }
}

/// `conc` 本のスレッドで `seconds` 秒だけ `work` を回す。`work` は 1 回の操作の
/// (バイト数) を返し、レイテンシは呼び出し側で測る。
pub(crate) fn run_load<F>(conc: usize, seconds: u64, work: F) -> Report
where
    F: Fn(&AtomicBool, &mut Vec<u32>, &mut u64) + Send + Sync + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let work = Arc::new(work);
    let started = Instant::now();
    let mut handles = Vec::with_capacity(conc);
    for _ in 0..conc {
        let stop = Arc::clone(&stop);
        let work = Arc::clone(&work);
        handles.push(thread::spawn(move || {
            let mut lat = Vec::new();
            let mut bytes = 0u64;
            work(&stop, &mut lat, &mut bytes);
            (lat, bytes)
        }));
    }
    thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::Relaxed);
    let mut latencies_us = Vec::new();
    let mut bytes = 0u64;
    for h in handles {
        if let Ok((lat, b)) = h.join() {
            latencies_us.extend(lat);
            bytes += b;
        }
    }
    let elapsed = started.elapsed();
    latencies_us.sort_unstable();
    Report {
        ops: latencies_us.len() as u64,
        bytes,
        elapsed,
        latencies_us,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--only connect-multi` の行は p50 / p95 / **max** を出す (T14.16)。
    /// max は `percentile(1.0)`。この経路で読みたいのは「250 ms を払った本があるか」なので、
    /// p99 より max が効く (1 本目だけ払って以後は払わない、が正しい姿)。
    #[test]
    fn percentile_covers_median_p95_and_max() {
        let r = Report {
            ops: 5,
            bytes: 0,
            elapsed: Duration::from_secs(1),
            latencies_us: vec![1_000, 2_000, 3_000, 4_000, 250_000],
        };
        assert_eq!(r.percentile(0.50), 3.0);
        assert_eq!(r.percentile(0.95), 250.0);
        assert_eq!(r.percentile(1.0), 250.0);
        // 1 本も張れなかったときも落ちない
        let empty = Report {
            ops: 0,
            bytes: 0,
            elapsed: Duration::from_secs(1),
            latencies_us: Vec::new(),
        };
        assert_eq!(empty.percentile(1.0), 0.0);
    }

    /// `run_load` は全スレッドの操作を 1 つにまとめ、レイテンシを昇順に並べる。
    #[test]
    fn run_load_gathers_every_thread_and_sorts_the_latencies() {
        let r = run_load(3, 1, |stop, lat, bytes| {
            // 1 スレッド 2 回ずつ (止められるまで待たず、すぐ返る)
            lat.push(30);
            lat.push(10);
            *bytes += 5;
            let _ = stop;
        });
        assert_eq!(r.ops, 6);
        assert_eq!(r.bytes, 15);
        assert_eq!(r.latencies_us, [10, 10, 10, 30, 30, 30]);
        assert!(r.elapsed >= Duration::from_secs(1));
    }
}
