//! 1 つの窓 (5 秒 または 60 秒) の標本と、その JSON の 1 行。

use std::fmt::Write as _;

use crate::window::Window;

use super::Stages;
use super::threads::{ROLES, TOP_THREADS, Threads, TopThread, merge_top};

/// 1 つの窓 (5 秒 または 60 秒) の中身。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sample {
    /// 窓の先頭 (epoch 秒)
    pub t: u64,
    /// その窓に処理した要求数 (`total_requests` の増分)
    pub requests: u64,
    /// その窓にプロセスが使った CPU (us。`/proc/self/stat` の utime + stime の増分)
    pub cpu_us: u64,
    pub stages: Stages,
    /// 役割ごとの CPU と状態 (T14.3)
    pub threads: Threads,
    /// その窓にロックが取り合いになった回数 ([`crate::sync::LOCK_NAMES`] の順。T14.3)
    pub locks: [u64; crate::sync::LOCK_NAMES.len()],
    /// その窓にワーカーの待ち行列で待った仕事の数・合計 ms・最大 ms
    pub queue_waited: u64,
    pub queue_ms_sum: u64,
    pub queue_ms_max: u64,
    /// その窓に CPU を多く使ったスレッド (多い順。空の枠は `tid == 0`。T15.0)
    pub threads_top: [TopThread; TOP_THREADS],
    /// **走れるのに走れなかった時間**の増分 (us。[`ROLES`] の順)。
    /// `None` = `schedstat` が読めない環境 (`/profile` では `null`)
    pub run_delay_us: Option<[u64; ROLES.len()]>,
    /// その窓にプロセスが使った CPU のうち**ユーザー空間** (us。`/proc/self/stat` の utime の増分。T16.0)。
    /// カーネル側は `cpu_us - cpu_user_us` (sys は持たない)。役割ごとは [`RoleWindow::user_us`](super::RoleWindow::user_us)
    pub cpu_user_us: u64,
}

impl Sample {
    /// **プロセスの CPU/要求** (us)。`docs/status.md` の loopback の 41 us/要求 と同じ物差し。
    pub fn cpu_per_request_us(&self) -> Option<f64> {
        (self.requests > 0).then(|| self.cpu_us as f64 / self.requests as f64)
    }

    pub(super) fn merge_into(&mut self, o: &Sample) {
        self.requests += o.requests;
        self.cpu_us += o.cpu_us;
        self.cpu_user_us += o.cpu_user_us;
        self.stages.merge(&o.stages);
        for (a, b) in self.threads.iter_mut().zip(o.threads.iter()) {
            a.merge(b);
        }
        for (a, b) in self.locks.iter_mut().zip(o.locks.iter()) {
            *a += *b;
        }
        self.queue_waited += o.queue_waited;
        self.queue_ms_sum += o.queue_ms_sum;
        self.queue_ms_max = self.queue_ms_max.max(o.queue_ms_max);
        self.threads_top = merge_top(&self.threads_top, &o.threads_top);
        // 片方でも読めていれば足す (読めない窓は無かったことにする)
        self.run_delay_us = match (self.run_delay_us, o.run_delay_us) {
            (Some(mut a), Some(b)) => {
                for (x, y) in a.iter_mut().zip(b.iter()) {
                    *x += *y;
                }
                Some(a)
            }
            (a, b) => a.or(b),
        };
    }

    /// `[t,requests,cpu_us,[connect...],[forward...],[roles...],[locks...],[queue...],`
    /// `[threads_top...],[run_delay_us...],[user_us...],cpu_user_us]`。
    /// **件数 0 の段階と標本 0 の役割は `0` 1 文字**で書く (静かな窓を小さくするため)。
    /// 上位のスレッドが 1 本も無い窓も `0`、`schedstat` が読めなければ `run_delay_us` は `null`。
    /// `user_us` (役割ごとのユーザー空間。T16.0) は `roles` と同じ長さの配列で、いつも数を書く。
    pub(super) fn push_row(&self, out: &mut String) {
        let _ = write!(out, "[{},{},{},[", self.t, self.requests, self.cpu_us);
        push_windows(out, &self.stages.folded_connect());
        out.push_str("],[");
        push_windows(out, &self.stages.folded_forward());
        out.push_str("],[");
        self.push_roles(out);
        out.push_str("],[");
        for (i, l) in self.locks.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", l);
        }
        let _ = write!(
            out,
            "],[{},{},{}],",
            self.queue_waited, self.queue_ms_sum, self.queue_ms_max
        );
        self.push_top(out);
        self.push_run_delay(out);
        // 新しい列は末尾に足す (T16.0): 役割ごとのユーザー空間と、プロセス全体のユーザー空間
        out.push_str(",[");
        for (i, r) in self.threads.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", r.user_us);
        }
        let _ = write!(out, "],{}]", self.cpu_user_us);
    }

    /// 役割ごとの `[cpu_us,samples,[states...]]` (標本 0 の役割は `0`)。
    fn push_roles(&self, out: &mut String) {
        for (i, r) in self.threads.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            if r.samples == 0 {
                out.push('0');
                continue;
            }
            let _ = write!(out, "[{},{},[", r.cpu_us, r.samples);
            for (j, c) in r.states.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{}", c);
            }
            out.push_str("]]");
        }
    }

    /// 上位のスレッド: `[[tid,"comm",role,cpu_us,running],…]` (空の窓は `0`)。
    fn push_top(&self, out: &mut String) {
        if self.threads_top[0].tid == 0 {
            out.push('0');
        } else {
            out.push('[');
            for (i, t) in self
                .threads_top
                .iter()
                .take_while(|t| t.tid != 0)
                .enumerate()
            {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(
                    out,
                    "[{},\"{}\",{},{},{}]",
                    t.tid,
                    t.comm_str(),
                    t.role,
                    t.cpu_us,
                    t.running
                );
            }
            out.push(']');
        }
    }

    /// 走れずに待った時間 (`schedstat` が読めなければ `,null`)。
    fn push_run_delay(&self, out: &mut String) {
        match &self.run_delay_us {
            None => out.push_str(",null"),
            Some(v) => {
                out.push_str(",[");
                for (i, d) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    let _ = write!(out, "{}", d);
                }
                out.push(']');
            }
        }
    }
}

/// 窓の並びを JSON に書く (件数 0 は `0`、それ以外は `[count,sum,max,[buckets]]`)。
pub(super) fn push_windows(out: &mut String, ws: &[Window]) {
    for (i, w) in ws.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if w.count == 0 {
            out.push('0');
            continue;
        }
        let _ = write!(out, "[{},{},{},[", w.count, w.ms_sum, w.ms_max);
        for (j, b) in w.buckets.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", b);
        }
        out.push_str("]]");
    }
}
