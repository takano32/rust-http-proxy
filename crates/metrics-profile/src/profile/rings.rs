//! 段階とスレッドの窓の環状バッファ ([`Profile`])。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use crate::sync::LockExt;

use super::sample::Sample;
use super::sampler::SamplerState;
use super::{RESOLUTIONS, Stages, sample_bytes};

/// 段階とスレッドの窓の環状バッファ (5 秒 × 720 と 60 秒 × 1,440)。
#[derive(Default)]
pub struct Profile {
    rings: [Mutex<VecDeque<Sample>>; 2],
    /// **起動からの累計**の段階 (`/metrics` の `sorahost_stage_seconds`。T14.19)。
    ///
    /// Prometheus のヒストグラムは単調増加が前提なので、窓 (環状バッファ) の和ではなく
    /// ここに積む。足すのは 5 秒に 1 回 ([`Profile::push`]) だけで、熱い経路は通らない。
    stage_totals: Mutex<Stages>,
    /// スレッドの標本の状態 ([`SamplerState`])
    sampler: std::sync::atomic::AtomicU8,
    /// 表に無いシステムコール番号と回数 (`sys_N` で出す。最大 [`MAX_UNKNOWN`] 種)
    unknown: Mutex<Vec<(i64, u64)>>,
}

/// 覚えておく「表に無いシステムコール番号」の種類数。
pub(crate) const MAX_UNKNOWN: usize = 16;

impl Profile {
    /// 5 秒の標本を足し、1 分の窓が閉じていればそれも作る。
    pub fn push(&self, s: Sample) {
        self.stage_totals.locked().merge(&s.stages);
        Self::append(&self.rings[0], s, RESOLUTIONS[0].1);
        self.roll(s.t);
    }

    /// 起動からの段階の累計 (`/metrics` の `sorahost_stage_seconds`。T14.19)。
    ///
    /// 窓と違って 0 に戻らない。最後の 5 秒の標本まで (= [`TICK`](super::TICK) の遅れ) が入る。
    pub fn stage_totals(&self) -> Stages {
        *self.stage_totals.locked()
    }

    /// 直前の 1 分の窓がまだ無ければ、5 秒の標本から作る (`proxy_metrics_window::history::History` と同じ形)。
    fn roll(&self, now: u64) {
        let step = RESOLUTIONS[1].0;
        let window_start = (now / step) * step;
        let Some(prev) = window_start.checked_sub(step) else {
            return;
        };
        if self.rings[1].locked().back().is_some_and(|s| s.t >= prev) {
            return;
        }
        let src = self.rings[0].locked();
        let window: Vec<Sample> = src
            .iter()
            .filter(|s| s.t >= prev && s.t < window_start)
            .copied()
            .collect();
        drop(src);
        if window.is_empty() {
            return;
        }
        let mut agg = Sample {
            t: prev,
            ..Sample::default()
        };
        for s in &window {
            agg.merge_into(s);
        }
        Self::append(&self.rings[1], agg, RESOLUTIONS[1].1);
    }

    fn append(ring: &Mutex<VecDeque<Sample>>, s: Sample, cap: usize) {
        let mut q = ring.locked();
        if q.len() >= cap {
            q.pop_front();
        }
        q.push_back(s);
    }

    /// 2 つの環の置き場を満杯のぶん ([`capacity_bytes`](super::capacity_bytes)) 確保して触る
    /// (**起動時に 1 回だけ**。T17.8)。返すのは触ったバイト数。
    ///
    /// 件数は増えないので [`Profile::used_bytes`] (`memory.rings_used.profile`) は変わらない。
    pub fn prefault(&self) -> usize {
        (0..RESOLUTIONS.len())
            .map(|res| crate::prefault::deque(&mut self.rings[res].locked(), RESOLUTIONS[res].1))
            .sum()
    }

    /// スレッドの標本の状態を書く (`profile-sample` スレッドだけが呼ぶ)。
    pub(crate) fn set_sampler(&self, state: SamplerState) {
        self.sampler.store(state as u8, Ordering::Relaxed);
    }

    /// スレッドの標本の状態。
    pub fn sampler(&self) -> SamplerState {
        SamplerState::from_u8(self.sampler.load(Ordering::Relaxed))
    }

    /// 表に無いシステムコール番号を 1 回数える (`/profile` に `sys_N` で出す)。
    pub(crate) fn note_unknown_syscall(&self, nr: i64) {
        let mut v = self.unknown.locked();
        if let Some(e) = v.iter_mut().find(|e| e.0 == nr) {
            e.1 += 1;
        } else if v.len() < MAX_UNKNOWN {
            v.push((nr, 1));
        }
    }

    /// 表に無いシステムコール番号の控え (多い順)。
    pub fn unknown_syscalls(&self) -> Vec<(i64, u64)> {
        let mut v = self.unknown.locked().clone();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// 解像度 (秒) から添字を引く。合わなければ 5 秒。
    pub fn index_for(secs: u64) -> usize {
        RESOLUTIONS
            .iter()
            .position(|(s, _)| *s == secs)
            .unwrap_or(0)
    }

    /// その解像度の標本数。
    pub fn len(&self, res: usize) -> usize {
        self.rings[res.min(1)].locked().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len(0) == 0
    }

    /// **いま環に入っているぶん**のバイト (`/status` の `memory.rings_used.profile`。T15.0 (13))。
    ///
    /// `--lite` では標本を 1 本も作らないので 0。満杯のときの見積もりは
    /// [`capacity_bytes`](super::capacity_bytes)。
    pub fn used_bytes(&self) -> usize {
        (self.len(0) + self.len(1)) * sample_bytes()
    }

    /// 直近 `n` 標本を 1 つに足し合わせる (画面の積み上げと CPU/要求 の要約用)。
    pub fn recent_totals(&self, res: usize, n: usize) -> Sample {
        let q = self.rings[res.min(1)].locked();
        let mut out = Sample::default();
        for s in q.iter().skip(q.len().saturating_sub(n)) {
            out.t = s.t;
            out.merge_into(s);
        }
        out
    }

    /// **新しい順に** `budget` バイトまで書けるだけ集め、古い順に並べて返す。
    /// 返すのは (JSON の並び, 書けた件数, 全体の件数, 打ち切ったか)。
    pub fn rows_within(&self, res: usize, budget: usize) -> (String, usize, usize, bool) {
        self.rows_within_page(res, budget, usize::MAX, 0)
    }

    /// [`Profile::rows_within`] の**ページ送りつき** (`/profile?n=&offset=`。T15.0 (11))。
    ///
    /// 環は時刻で引ける作りになっていない (新しい順に詰めるだけ) ので、`since=` /
    /// `until=` ではなく「新しい方から何本飛ばして、何本返すか」で切る。`offset` は
    /// **新しい順の並び**を飛ばす本数で、返す並びは今までどおり**古い順**。
    /// `budget` はページ 1 枚ぶんに効く (飛ばしたぶんは数えない)。
    ///
    /// 打ち切り (4 つ目の戻り) は**バイト数で切れたときだけ**真で、`n` で切れたぶんは
    /// 「続きがある」= 呼ぶ側が `offset + 書けた件数` で次を引く。
    pub fn rows_within_page(
        &self,
        res: usize,
        budget: usize,
        n: usize,
        offset: usize,
    ) -> (String, usize, usize, bool) {
        let q = self.rings[res.min(1)].locked();
        let total = q.len();
        let mut rows: Vec<String> = Vec::new();
        let mut used = 0usize;
        let mut cut = false;
        for s in q.iter().rev().skip(offset) {
            if rows.len() >= n {
                break;
            }
            let mut row = String::with_capacity(160);
            s.push_row(&mut row);
            if used + row.len() + 1 > budget {
                cut = true;
                break;
            }
            used += row.len() + 1;
            rows.push(row);
        }
        drop(q);
        rows.reverse();
        let shown = rows.len();
        (rows.join(","), shown, total, cut)
    }
}
