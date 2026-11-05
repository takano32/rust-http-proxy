//! カーネルの標本の**窓そのもの** (I/O は無い): 前の生の値との差 ([`diff`])、5 秒の環と
//! 1 分への畳み ([`State`] / [`push`] / [`Sample::downsample`])、直近 5 分の値 ([`Health::of`])。
//! 源を読むのは `read`、JSON にするのは `json`。

use std::collections::VecDeque;

use super::{Health, Latest, RESOLUTIONS, SRC_CGROUP_CPU, SRC_NETSTAT, SRC_STATE_FILE, Sample};

#[derive(Default)]
pub(super) struct State {
    pub(super) rings: [VecDeque<Sample>; 2],
    /// 前回の生の値 (増分を出すため)。まだ 1 回も読んでいなければ `None`
    pub(super) last: Option<Latest>,
    /// **いちばん最初に読んだ生の値** (T15.0 (6))。`/status` の `since_start` は
    /// 「いまの累計 − これ」で出す。起動直後の雪像に `nr_throttled` 915 が
    /// 乗っていたのは、累計が**このプロセスより前から**動いている親の階層の値だから
    pub(super) first: Option<Latest>,
}

impl State {
    pub(super) const fn new() -> State {
        State {
            rings: [VecDeque::new(), VecDeque::new()],
            last: None,
            first: None,
        }
    }

    /// 読んだ生の値 1 本から標本を 1 本作り、環へ足す (`sample` の鍵の内側の仕事)。
    pub(super) fn record(&mut self, now: Latest) {
        let sample = match self.last {
            Some(prev) => diff(&prev, &now),
            // 1 本目は増分が出せない (前が無い) ので、値だけ入れて増分は 0
            None => diff(&now, &now),
        };
        // 起動からの増分を出すための最初の 1 本 (T15.0 (6))
        if self.first.is_none() {
            self.first = Some(now);
        }
        self.last = Some(now);
        push(self, sample);
    }
}

impl Sample {
    /// 窓の標本をひとつにまとめる: 増分は足し合わせ、値と PSI は**最大**
    /// (平均に畳むと山が消える。見たいのはバーストのときの姿)。
    pub(super) fn downsample(window: &[Sample], t: u64) -> Sample {
        let sum = |f: fn(&Sample) -> u64| window.iter().map(f).sum::<u64>();
        let max = |f: fn(&Sample) -> u64| window.iter().map(f).max().unwrap_or(0);
        let fmax = |f: fn(&Sample) -> f64| window.iter().map(f).fold(0.0f64, f64::max);
        let misses = sum(|s| s.dns_misses);
        Sample {
            t,
            // 源が読めた標本が 1 つでもあれば、その列は数で出す
            avail: window.iter().fold(0, |a, s| a | s.avail),
            listen_overflows: sum(|s| s.listen_overflows),
            listen_drops: sum(|s| s.listen_drops),
            tcp_timeouts: sum(|s| s.tcp_timeouts),
            syn_retrans: sum(|s| s.syn_retrans),
            abort_on_timeout: sum(|s| s.abort_on_timeout),
            retrans_segs: sum(|s| s.retrans_segs),
            curr_estab: max(|s| s.curr_estab),
            sockets_inuse: max(|s| s.sockets_inuse),
            time_wait: max(|s| s.time_wait),
            sockets_alloc: max(|s| s.sockets_alloc),
            sockets_mem: max(|s| s.sockets_mem),
            cpu_nr_throttled: sum(|s| s.cpu_nr_throttled),
            cpu_throttled_usec: sum(|s| s.cpu_throttled_usec),
            psi_cpu_some: fmax(|s| s.psi_cpu_some),
            psi_cpu_full: fmax(|s| s.psi_cpu_full),
            psi_mem_some: fmax(|s| s.psi_mem_some),
            psi_mem_full: fmax(|s| s.psi_mem_full),
            psi_io_some: fmax(|s| s.psi_io_some),
            psi_io_full: fmax(|s| s.psi_io_full),
            dns_misses: misses,
            // ミスの回数で重みを付けた平均 (窓ごとの「1 回の値段」を薄めない)。
            // ミスが 1 回も無い窓は `checked_div` が `None` を返すので 0
            dns_miss_ms: window
                .iter()
                .map(|s| s.dns_miss_ms * s.dns_misses)
                .sum::<u64>()
                .checked_div(misses)
                .unwrap_or(0),
            state_file_errors: sum(|s| s.state_file_errors),
            // 増分なので足し合わせ (T15.0 (6))
            cpu_nr_periods: sum(|s| s.cpu_nr_periods),
            // 同じく増分 (T16.0)
            cpu_user_usec: sum(|s| s.cpu_user_usec),
            cpu_system_usec: sum(|s| s.cpu_system_usec),
        }
    }
}

impl Health {
    /// 直近 5 分 (いちばん新しい標本から 300 秒ぶん) の値を 5 秒の環から出す
    /// ([`super::health`] の本体。環と最初の生の値を与えれば試験から直に呼べる)。
    pub(super) fn of(ring: &VecDeque<Sample>, first: Option<&Latest>) -> Health {
        let Some(newest) = ring.back().map(|s| s.t) else {
            return Health::default();
        };
        let since = newest.saturating_sub(300);
        let recent = ring.iter().filter(|s| s.t >= since);
        let mut h = Health {
            // 起動して最初に読んだ標本からの秒 (T15.0 (6))。窓が 5 分ぶん溜まったかの物差し
            sampled_secs: first.map(|f| newest.saturating_sub(f.at)),
            ..Health::default()
        };
        for s in recent {
            if s.has(SRC_NETSTAT) {
                h.listen_overflows_5m =
                    Some(h.listen_overflows_5m.unwrap_or(0) + s.listen_overflows);
            }
            if s.has(SRC_STATE_FILE) {
                h.state_file_errors_5m =
                    Some(h.state_file_errors_5m.unwrap_or(0) + s.state_file_errors);
            }
            // 絞られた期間と過ぎた期間の増分 (T15.0 (6))。割合は読む側が割る
            if s.has(SRC_CGROUP_CPU) {
                let (throttled, periods) = h.cpu_throttled_5m.unwrap_or((0, 0));
                h.cpu_throttled_5m =
                    Some((throttled + s.cpu_nr_throttled, periods + s.cpu_nr_periods));
            }
            // いちばん新しい「ミスのあった窓」の値 (古い順に見るので最後の 1 つが残る)
            if s.dns_misses > 0 {
                h.dns_miss_ms = Some(s.dns_miss_ms);
            }
        }
        h
    }
}

/// 前の生の値との差から 1 標本を作る (累計は増分、値はそのまま)。
///
/// カウンタが減っていたら (カーネルの折り返し・`/proc` の入れ替え) 0 にする。
pub(super) fn diff(prev: &Latest, now: &Latest) -> Sample {
    let d = |a: u64, b: u64| a.saturating_sub(b);
    let misses = d(now.dns_misses, prev.dns_misses);
    Sample {
        t: now.at,
        avail: now.avail,
        listen_overflows: d(now.listen_overflows, prev.listen_overflows),
        listen_drops: d(now.listen_drops, prev.listen_drops),
        tcp_timeouts: d(now.tcp_timeouts, prev.tcp_timeouts),
        syn_retrans: d(now.syn_retrans, prev.syn_retrans),
        abort_on_timeout: d(now.abort_on_timeout, prev.abort_on_timeout),
        retrans_segs: d(now.retrans_segs, prev.retrans_segs),
        curr_estab: now.curr_estab,
        sockets_inuse: now.sockets_inuse,
        time_wait: now.time_wait,
        sockets_alloc: now.sockets_alloc,
        sockets_mem: now.sockets_mem,
        cpu_nr_throttled: d(now.cpu_nr_throttled, prev.cpu_nr_throttled),
        cpu_throttled_usec: d(now.cpu_throttled_usec, prev.cpu_throttled_usec),
        cpu_nr_periods: d(now.cpu_nr_periods, prev.cpu_nr_periods),
        cpu_user_usec: d(now.cpu_user_usec, prev.cpu_user_usec),
        cpu_system_usec: d(now.cpu_system_usec, prev.cpu_system_usec),
        psi_cpu_some: now.psi_cpu_some,
        psi_cpu_full: now.psi_cpu_full,
        psi_mem_some: now.psi_mem_some,
        psi_mem_full: now.psi_mem_full,
        psi_io_some: now.psi_io_some,
        psi_io_full: now.psi_io_full,
        dns_misses: misses,
        // ミスのあった窓だけ「1 回の ms」が出る (0 回の窓は `checked_div` が `None`)
        dns_miss_ms: d(now.dns_us_sum, prev.dns_us_sum)
            .checked_div(misses)
            .map_or(0, |us| (us + 500) / 1000),
        state_file_errors: d(now.state_file_errors, prev.state_file_errors),
    }
}

/// 5 秒の環へ 1 本足し、1 分の窓が閉じていればそれも作る (`crate::history` と同じ形)。
pub(super) fn push(st: &mut State, s: Sample) {
    append(&mut st.rings[0], s, RESOLUTIONS[0].1);
    let step = RESOLUTIONS[1].0;
    let window_start = (s.t / step) * step;
    let Some(prev) = window_start.checked_sub(step) else {
        return;
    };
    if st.rings[1].back().is_some_and(|m| m.t >= prev) {
        return;
    }
    let window: Vec<Sample> = st.rings[0]
        .iter()
        .filter(|w| w.t >= prev && w.t < window_start)
        .copied()
        .collect();
    if !window.is_empty() {
        let agg = Sample::downsample(&window, prev);
        append(&mut st.rings[1], agg, RESOLUTIONS[1].1);
    }
}

fn append(ring: &mut VecDeque<Sample>, s: Sample, cap: usize) {
    if ring.len() >= cap {
        ring.pop_front();
    }
    ring.push_back(s);
}

#[cfg(test)]
mod tests {
    use super::super::{SRC_PSI_CPU, SRC_SNMP, SRC_SOCKSTAT};
    use super::*;

    fn raw(t: u64, overflows: u64, misses: u64) -> Latest {
        Latest {
            at: t,
            avail: SRC_NETSTAT
                | SRC_SNMP
                | SRC_SOCKSTAT
                | SRC_CGROUP_CPU
                | SRC_PSI_CPU
                | SRC_STATE_FILE,
            listen_overflows: overflows,
            cpu_nr_periods: 8_000 + t,
            cpu_nr_throttled: 900 + t / 2,
            dns_misses: misses,
            dns_us_sum: misses * 12_600,
            state_file_errors: 1,
            ..Latest::default()
        }
    }

    /// 生の値を順に受けると、1 本目は基準 (増分 0) になり、2 本目から増分が出る。
    /// `first` は最初の 1 本のまま、`last` は最新。
    #[test]
    fn the_state_takes_the_first_raw_value_as_the_baseline_for_deltas() {
        let mut st = State::new();
        st.record(raw(1_000_000, 100, 10));
        assert_eq!(st.rings[0].len(), 1);
        assert_eq!(st.rings[0][0].listen_overflows, 0, "1 本目は増分が出せない");
        assert_eq!(st.first.map(|f| f.at), Some(1_000_000));
        st.record(raw(1_000_005, 103, 12));
        st.record(raw(1_000_010, 103, 12));
        assert_eq!(st.rings[0].len(), 3);
        assert_eq!(st.rings[0][1].listen_overflows, 3);
        assert_eq!(st.rings[0][1].dns_misses, 2);
        assert_eq!(st.rings[0][1].dns_miss_ms, 13, "12.6 ms は 13 に丸まる");
        assert_eq!(st.rings[0][2].listen_overflows, 0);
        assert_eq!(
            st.first.map(|f| f.at),
            Some(1_000_000),
            "最初の 1 本は動かない"
        );
        assert_eq!(st.last.map(|l| l.at), Some(1_000_010));
    }

    /// 直近 5 分は「いちばん新しい標本から 300 秒」の標本だけを足し、ミスの ms はその中のいちばん新しい
    /// 「ミスのあった窓」の値。読めない源の欄は `None`、環が空なら全部 `None`。
    #[test]
    fn health_of_sums_only_the_last_five_minutes() {
        assert_eq!(Health::of(&VecDeque::new(), None), Health::default());
        let mut ring = VecDeque::new();
        // 0〜400 秒を 5 秒刻み: 300 秒より前の溢れ (1 本 7) は窓に入らない
        for i in 0..=80u64 {
            let t = 1_000_000 + i * 5;
            let mut s = diff(
                &raw(t - 5, 0, 0),
                &raw(t, if t < 1_000_100 { 7 } else { 1 }, 0),
            );
            s.cpu_nr_periods = 1;
            s.cpu_nr_throttled = u64::from(i % 2 == 0);
            if i == 10 || i == 70 {
                s.dns_misses = 1;
                s.dns_miss_ms = i;
            }
            ring.push_back(s);
        }
        let first = raw(1_000_000 - 95, 0, 0);
        let h = Health::of(&ring, Some(&first));
        // 新しい方の 61 本 (t = 1,000,100 〜 1,000,400) が窓に入る
        assert_eq!(h.listen_overflows_5m, Some(61));
        assert_eq!(h.state_file_errors_5m, Some(0));
        assert_eq!(h.cpu_throttled_5m, Some((31, 61)));
        assert_eq!(h.dns_miss_ms, Some(70), "窓の中でいちばん新しいミスの窓");
        assert_eq!(h.sampled_secs, Some(495), "最初の生の値からの秒");
        // 源が読めていない標本は欄を `None` のままにする
        let mut dark = ring.clone();
        for s in dark.iter_mut() {
            s.avail = 0;
        }
        let h = Health::of(&dark, None);
        assert_eq!(
            (
                h.listen_overflows_5m,
                h.state_file_errors_5m,
                h.cpu_throttled_5m,
                h.sampled_secs
            ),
            (None, None, None, None)
        );
    }
}
