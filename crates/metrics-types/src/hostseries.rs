//! ホスト別の時系列 (上位 16 ホスト × 5 分 × 24 時間)。T14.22
//!
//! ホスト別の統計 ([`crate::metrics::HostStats`]) は**通算**しか無く (`/hosts` の
//! `avg_ms` は 3 日の平均)、`/history` は**全体**しか無い。「datadog が 15 時に遅かった」
//! 「mtalk だけ夜に再送が増えた」はどちらでも読めないので、その間を埋める:
//! **直近 1 時間の要求数で選んだ上位 16 ホスト**について、5 分の窓ごとに
//! 件数・確立の合計 ms・最大 ms・名前解決 ms・エラー数を 24 時間ぶん残す。
//!
//! **費用の置き方** (共通の決まり):
//!
//! - 要求の経路がするのは、[`crate::metrics::Metrics::record_host_detail`] が
//!   **既に取っている鍵の内側**で「上位に居るか」の旗 ([`crate::metrics::HostStats::series_slot`])
//!   を見て、居れば[配列 1 要素に 5 回足す][HostSeries::add]だけ。原子操作もシステムコールも増えない。
//! - **上位の入れ替えと窓送りは history スレッド**が 5 分ごとに行う ([`HostSeries::rotate`])。
//!   5 秒ごとに呼ばれるが、窓の境目でなければ比較 1 回で戻る。
//! - `--lite` は history スレッドそのものが立たない (`PROXY_STATS_PERSIST=off` と同じ) ので、
//!   旗が立つことが無く、下の配列も**確保しない** (費用 0)。
//! - `.rrd` (状態ファイル) には書かない (T14.2 (3): 標本のレコードの余白は 4 B しか無い)。
//!   再起動で消えてよい。旗もメモリだけの欄で、`HostStats::encode` / `decode` は 1 バイトも変えない。
//!
//! メモリは 16 ホスト × 288 標本 × 5 項目 × 8 B = **184,320 B (180 KiB)** で固定
//! (上位が 1 つ決まったときに 1 回だけ確保する)。ほかに順位付けの覚え書き
//! ([`Recent`]) がホストあたり 56 B + 名前 — 行はホスト表と同じで最大
//! [`crate::metrics::MAX_HOSTS`] (1,000) なので、多くても 100 KB ほど。
//!
//! **上位から落ちたホストの標本は捨てる** (再び上がれば 0 から)。ただし**枠が余っている
//! 間は今の住人を残す** (夜だけ動くホストの 24 時間が、1 時間の無音で消えないように)。
//! 24 時間の輪なので、標本を進めるときに飛んだぶんは 0 に戻す (1 日前の値が残らないように)。

use std::collections::HashMap;

use crate::metrics::HostStats;

/// 系列を持てるホストの数 (上位 16)。旗 (`Option<u8>`) に入る大きさ。
pub const SLOTS: usize = 16;
/// 1 ホストあたりの標本の数 (5 分 × 288 = 24 時間)。
pub const SAMPLES: usize = 288;
/// 1 標本の項目数 ([`FIELD_NAMES`] の順)。
pub const FIELDS: usize = 5;
/// 標本の項目名 (`/hosts/series` の `keys`)。
///
/// `ms_sum` / `ms_max` は CONNECT なら**確立まで**、forward なら**初バイトまで**の ms
/// (ホスト別統計の `avg_ms` と同じ値の入れ方)。`dns_ms` は名前解決にかかった ms の合計。
pub const FIELD_NAMES: [&str; FIELDS] = ["count", "ms_sum", "ms_max", "dns_ms", "errors"];
/// 窓の長さ (秒)。**本番は 5 分**で、短くするのは結合テストだけ ([`HostSeries::set_window`])。
pub const WINDOW_SECS: u64 = 300;
/// 順位付けに使う窓の数 (12 × 5 分 = **直近 1 時間**)。
const RANK_WINDOWS: usize = 12;

/// 順位付けの覚え書き (ホスト 1 行ぶん)。history スレッドしか触らない。
#[derive(Debug, Default, Clone, Copy)]
struct Recent {
    /// 前回の入れ替えのときの `HostStats::requests` (差分で「この 5 分の要求数」を出す)
    last_total: u64,
    /// 直近 12 窓の要求数。合計が「直近 1 時間の要求数」= 順位
    hits: [u32; RANK_WINDOWS],
}

impl Recent {
    fn score(&self) -> u64 {
        self.hits.iter().map(|&h| h as u64).sum()
    }
}

/// 上位 16 ホストの時系列。[`crate::metrics::Metrics`] のホスト表と**同じ鍵**の中にある
/// (要求の経路で鍵を 2 つ取らないため)。
#[derive(Debug)]
pub struct HostSeries {
    /// 窓の長さ (秒)
    win: u64,
    /// いまの窓の番号 (`epoch / win`)。`seeded` が立つまでは無効
    cur_win: u64,
    /// いまの標本の位置 (`cur_win % SAMPLES`)
    cur: usize,
    /// 起動直後の 1 回 (通算の基準を置くだけ) が済んだか
    seeded: bool,
    /// 窓を進めた回数 (順位付けの窓の位置も兼ねる)
    rotations: u64,
    /// `SLOTS × SAMPLES × FIELDS` の数値。上位が 1 つ決まるまで確保しない
    data: Option<Box<[u64]>>,
    /// 枠に居るホスト名 (空 = 空き)。`data` と一緒に 16 要素になる
    names: Vec<String>,
    /// 順位付けの覚え書き (ホスト表と同じ行数まで)
    recent: HashMap<String, Recent>,
}

impl Default for HostSeries {
    fn default() -> Self {
        Self {
            win: WINDOW_SECS,
            cur_win: 0,
            cur: 0,
            seeded: false,
            rotations: 0,
            data: None,
            names: Vec::new(),
            recent: HashMap::new(),
        }
    }
}

impl HostSeries {
    /// **要求の経路**。上位に居るホストの、いまの窓の標本に 1 件ぶん足す (T14.22)。
    ///
    /// 呼ぶのは旗 (`HostStats::series_slot`) が立っているときだけで、するのは
    /// 配列 1 要素 (5 項目) への加算。鍵は呼び出し側が既に持っている。
    #[inline]
    pub fn add(&mut self, slot: u8, ms: u64, dns_ms: u64, error: bool) {
        let Some(data) = self.data.as_mut() else {
            return;
        };
        let at = (slot as usize * SAMPLES + self.cur) * FIELDS;
        let s = &mut data[at..at + FIELDS];
        s[0] += 1;
        s[1] += ms;
        if ms > s[2] {
            s[2] = ms;
        }
        s[3] += dns_ms;
        s[4] += error as u64;
    }

    /// 窓を進め、直近 1 時間の要求数で上位 16 を入れ替える (**history スレッドが呼ぶ**)。
    ///
    /// 5 秒ごとに呼ばれるが、**窓の境目でなければ比較 1 回で戻る**。境目では
    /// (1) 標本を進めて飛んだぶんを 0 に戻し、(2) ホスト表の通算との差分で
    /// 直近 12 窓の要求数を数え直し、(3) 上位 16 を選び直して旗を付け替える。
    ///
    /// 起動直後の 1 回は**基準を置くだけ** (`.rrd` から読み戻した通算を
    /// 「直近 1 時間の要求数」と数えてしまわないように)。
    pub fn rotate(&mut self, now: u64, hosts: &mut HashMap<String, HostStats>) {
        let win = self.win.max(1);
        let w = now / win;
        if self.seeded && w == self.cur_win {
            return;
        }
        if !self.seeded {
            for (name, s) in hosts.iter() {
                self.recent.insert(
                    name.clone(),
                    Recent {
                        last_total: s.requests,
                        hits: [0; RANK_WINDOWS],
                    },
                );
            }
            self.seeded = true;
            self.cur_win = w;
            self.cur = (w % SAMPLES as u64) as usize;
            return;
        }
        // (1) 標本を進める。飛んだぶんも順に 0 に戻す (24 時間前の値を残さない)
        let steps = (w - self.cur_win).min(SAMPLES as u64);
        for _ in 0..steps {
            self.cur = (self.cur + 1) % SAMPLES;
            self.clear_sample(self.cur);
        }
        self.cur_win = w;
        // (2) 直近 12 窓の要求数 (ホスト表の通算との差分)
        let pos = (self.rotations % RANK_WINDOWS as u64) as usize;
        self.rotations += 1;
        for r in self.recent.values_mut() {
            r.hits[pos] = 0;
        }
        for (name, s) in hosts.iter() {
            let r = self.recent.entry(name.clone()).or_default();
            r.hits[pos] = s.requests.saturating_sub(r.last_total).min(u32::MAX as u64) as u32;
            r.last_total = s.requests;
        }
        // (3) 上位 16 (同点は名前で崩すので順序は 1 つに決まる)。1 件も無ければ何もしない
        let mut keep: Vec<String> = {
            let mut rank: Vec<(u64, &str)> = self
                .recent
                .iter()
                .map(|(k, r)| (r.score(), k.as_str()))
                .filter(|&(sc, _)| sc > 0)
                .collect();
            rank.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
            rank.truncate(SLOTS);
            rank.into_iter().map(|(_, n)| n.to_string()).collect()
        };
        // **枠が余っているなら今の住人はそのまま残す**: この 1 時間 静かだっただけの
        // ホスト (夜だけ動く相手) の 24 時間が、誰も欲しがっていない枠で消えないように。
        // 忙しいホストを押しのけることは無い (上位を全部置いたあとの余りだけ)
        if keep.len() < SLOTS {
            for n in self.names.iter() {
                if keep.len() >= SLOTS {
                    break;
                }
                if !n.is_empty() && !keep.iter().any(|h| h == n) {
                    keep.push(n.clone());
                }
            }
        }
        if keep.is_empty() && self.names.is_empty() {
            return;
        }
        self.alloc();
        // 落ちたホストの旗を下ろして枠を空ける
        for i in 0..SLOTS {
            if self.names[i].is_empty() || keep.iter().any(|h| *h == self.names[i]) {
                continue;
            }
            if let Some(s) = hosts.get_mut(self.names[i].as_str()) {
                s.series_slot = None;
            }
            self.names[i].clear();
        }
        // 上がったホストに空き枠を配る (**前の住人の標本は捨てる**)
        for h in &keep {
            if self.names.iter().any(|n| n == h) {
                continue;
            }
            let Some(i) = self.names.iter().position(|n| n.is_empty()) else {
                break;
            };
            self.clear_slot(i);
            self.names[i] = h.clone();
            if let Some(s) = hosts.get_mut(h.as_str()) {
                s.series_slot = Some(i as u8);
            }
        }
    }

    /// 窓の長さを差し替える (**結合テスト用の口**。本番は [`WINDOW_SECS`])。
    ///
    /// 次の呼び出しで基準を置き直す (`seeded` を下ろす) ので、履歴スレッドを
    /// 起こす前でも後でも同じ動きになる。
    pub fn set_window(&mut self, secs: u64) {
        self.win = secs.max(1);
        self.seeded = false;
    }

    /// 窓の長さ (秒)。
    pub fn window_secs(&self) -> u64 {
        self.win
    }

    /// いま系列を持っているホストの数。
    pub fn tracked(&self) -> usize {
        self.names.iter().filter(|n| !n.is_empty()).count()
    }

    /// 窓を進めた回数 (テストが「境目を越えた」を待つのに使う)。
    pub fn rotations(&self) -> u64 {
        self.rotations
    }

    /// 読み出し用の写し (`/hosts/series`)。`want` を渡すとそのホストだけ、
    /// 渡さなければ**直近 1 時間の要求数の多い順に上位 `top` 件**。
    pub fn view(&self, want: Option<&str>, top: usize) -> View {
        let mut series: Vec<Series> = Vec::new();
        for i in 0..self.names.len() {
            if self.names[i].is_empty() || want.is_some_and(|w| w != self.names[i]) {
                continue;
            }
            let mut rows = Vec::with_capacity(SAMPLES);
            for k in 1..=SAMPLES {
                let idx = (self.cur + k) % SAMPLES;
                let at = (i * SAMPLES + idx) * FIELDS;
                let mut row = [0u64; FIELDS];
                if let Some(data) = self.data.as_deref() {
                    row.copy_from_slice(&data[at..at + FIELDS]);
                }
                rows.push(row);
            }
            series.push(Series {
                host: self.names[i].clone(),
                hour_requests: self.recent.get(&self.names[i]).map_or(0, |r| r.score()),
                rows,
            });
        }
        series.sort_by(|a, b| {
            b.hour_requests
                .cmp(&a.hour_requests)
                .then_with(|| a.host.cmp(&b.host))
        });
        series.truncate(top.min(SLOTS));
        View {
            window_secs: self.win,
            // いちばん古い標本の窓の始まり (i 番目の標本は `t0 + i * window_secs`)
            t0: self
                .cur_win
                .saturating_sub(SAMPLES as u64 - 1)
                .saturating_mul(self.win),
            rotations: self.rotations,
            tracked: self.tracked(),
            series,
        }
    }

    /// 16 枠ぶんの配列を用意する (上位が 1 つ決まったときに 1 回だけ)。
    fn alloc(&mut self) {
        if self.data.is_none() {
            self.data = Some(vec![0u64; SLOTS * SAMPLES * FIELDS].into_boxed_slice());
        }
        if self.names.len() < SLOTS {
            self.names.resize(SLOTS, String::new());
        }
    }

    /// 全部の枠の、この位置の標本を 0 に戻す (窓を進めるとき)。
    fn clear_sample(&mut self, idx: usize) {
        let Some(data) = self.data.as_mut() else {
            return;
        };
        for slot in 0..SLOTS {
            let at = (slot * SAMPLES + idx) * FIELDS;
            data[at..at + FIELDS].fill(0);
        }
    }

    /// 枠 1 つの 24 時間ぶんを 0 に戻す (住人が替わるとき)。
    fn clear_slot(&mut self, slot: usize) {
        let Some(data) = self.data.as_mut() else {
            return;
        };
        let at = slot * SAMPLES * FIELDS;
        data[at..at + SAMPLES * FIELDS].fill(0);
    }
}

/// 1 ホストぶんの系列 ([`HostSeries::view`] の結果)。
#[derive(Debug, Clone)]
pub struct Series {
    pub host: String,
    /// 直近 1 時間の要求数 (順位そのもの)
    pub hour_requests: u64,
    /// 古い順に [`SAMPLES`] 個。1 つが [`FIELD_NAMES`] の順の 5 項目
    pub rows: Vec<[u64; FIELDS]>,
}

impl Series {
    /// この系列の合計 (`/hosts/series` の `total`)。`ms_max` だけは最大。
    ///
    /// 288 標本を足すので、読む口が落ちないように飽和で足す (デバッグ版の
    /// 溢れの検査も含めて、値がどうであれ応答は返す)。
    pub fn totals(&self) -> [u64; FIELDS] {
        let mut t = [0u64; FIELDS];
        for r in &self.rows {
            t[0] = t[0].saturating_add(r[0]);
            t[1] = t[1].saturating_add(r[1]);
            t[2] = t[2].max(r[2]);
            t[3] = t[3].saturating_add(r[3]);
            t[4] = t[4].saturating_add(r[4]);
        }
        t
    }
}

/// 読み出し用の写し。
#[derive(Debug, Clone)]
pub struct View {
    pub window_secs: u64,
    /// いちばん古い標本の窓の始まり (epoch 秒)
    pub t0: u64,
    pub rotations: u64,
    /// いま系列を持っているホストの数
    pub tracked: usize,
    pub series: Vec<Series>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `requests` が `n` のホスト表を作る。
    fn table(rows: &[(&str, u64)]) -> HashMap<String, HostStats> {
        rows.iter()
            .map(|(h, n)| {
                (
                    h.to_string(),
                    HostStats {
                        requests: *n,
                        ..HostStats::default()
                    },
                )
            })
            .collect()
    }

    /// 最初の 1 回は基準を置くだけ (`.rrd` から読み戻した通算で上位を決めない)。
    #[test]
    fn the_first_rotation_only_takes_a_baseline() {
        let mut s = HostSeries::default();
        let mut hosts = table(&[("a", 1_000_000)]);
        s.rotate(0, &mut hosts);
        assert_eq!(s.tracked(), 0, "読み戻した通算では上位にしない");
        assert_eq!(hosts["a"].series_slot, None);
        // 次の窓では「この 5 分の差分」だけが効く (差分 0 なので上がらない)
        s.rotate(WINDOW_SECS, &mut hosts);
        assert_eq!(s.tracked(), 0);
    }

    /// 要求のあったホストが上位に入り、旗が立ち、標本に足せること。
    #[test]
    fn a_busy_host_gets_a_slot_and_samples() {
        let mut s = HostSeries::default();
        let mut hosts = table(&[]);
        s.rotate(0, &mut hosts);
        hosts = table(&[("a", 5), ("b", 3)]);
        s.rotate(WINDOW_SECS, &mut hosts);
        assert_eq!(hosts["a"].series_slot, Some(0));
        assert_eq!(hosts["b"].series_slot, Some(1));
        s.add(0, 12, 3, false);
        s.add(0, 20, 0, true);
        let v = s.view(None, SLOTS);
        assert_eq!(v.series.len(), 2);
        assert_eq!(v.series[0].host, "a");
        assert_eq!(v.series[0].totals(), [2, 32, 20, 3, 1]);
        assert_eq!(v.series[1].totals(), [0, 0, 0, 0, 0]);
        // いちばん新しい標本に入っている
        assert_eq!(*v.series[0].rows.last().unwrap(), [2, 32, 20, 3, 1]);
    }

    /// 窓の境目で標本が進む (前の窓の値は前の標本に残る)。
    #[test]
    fn samples_move_on_at_the_window_boundary() {
        let mut s = HostSeries::default();
        let mut hosts = table(&[]);
        s.rotate(0, &mut hosts);
        hosts = table(&[("a", 1)]);
        s.rotate(WINDOW_SECS, &mut hosts);
        s.add(0, 10, 0, false);
        hosts.get_mut("a").unwrap().requests += 1;
        s.rotate(WINDOW_SECS * 2, &mut hosts);
        s.add(0, 40, 0, false);
        s.add(0, 50, 0, false);
        let v = s.view(None, SLOTS);
        let rows = &v.series[0].rows;
        assert_eq!(rows[SAMPLES - 2], [1, 10, 10, 0, 0], "前の窓");
        assert_eq!(rows[SAMPLES - 1], [2, 90, 50, 0, 0], "いまの窓");
        assert_eq!(v.series[0].totals(), [3, 100, 50, 0, 0]);
    }

    /// 上位から落ちたホストの標本は捨てる (再び上がれば 0 から)。
    #[test]
    fn a_host_that_drops_out_loses_its_samples() {
        let mut s = HostSeries::default();
        let mut hosts = table(&[]);
        s.rotate(0, &mut hosts);
        // 17 番目のホストが入ると、いちばん低いものが押し出される
        let mut rows: Vec<(&str, u64)> = Vec::new();
        let names: Vec<String> = (0..SLOTS).map(|i| format!("h{:02}", i)).collect();
        for (i, n) in names.iter().enumerate() {
            rows.push((n.as_str(), (SLOTS - i) as u64 * 10));
        }
        hosts = table(&rows);
        s.rotate(WINDOW_SECS, &mut hosts);
        assert_eq!(s.tracked(), SLOTS);
        let last = hosts["h15"].series_slot.unwrap();
        s.add(last, 99, 0, false);
        assert_eq!(s.view(Some("h15"), SLOTS).series[0].totals()[0], 1);
        // 押し出す (要求が 1 件も無かった `h15` は落ちる)
        for (i, n) in names.iter().enumerate() {
            hosts.get_mut(n.as_str()).unwrap().requests += (SLOTS - i) as u64 * 10;
        }
        hosts.insert(
            "newcomer".to_string(),
            HostStats {
                requests: 1_000,
                ..HostStats::default()
            },
        );
        // `h15` だけ増やさない
        hosts.get_mut("h15").unwrap().requests -= 10;
        s.rotate(WINDOW_SECS * 2, &mut hosts);
        assert_eq!(hosts["newcomer"].series_slot, Some(last), "空いた枠に入る");
        assert_eq!(hosts["h15"].series_slot, None, "落ちたら旗は下りる");
        assert_eq!(
            s.view(Some("newcomer"), SLOTS).series[0].totals(),
            [0, 0, 0, 0, 0],
            "前の住人の標本は残さない"
        );
    }

    /// 24 時間より長く空けたら、1 日前の値は残らない。
    #[test]
    fn a_long_gap_clears_the_whole_ring() {
        let mut s = HostSeries::default();
        let mut hosts = table(&[]);
        s.rotate(0, &mut hosts);
        hosts = table(&[("a", 1)]);
        s.rotate(WINDOW_SECS, &mut hosts);
        s.add(0, 10, 0, false);
        hosts.get_mut("a").unwrap().requests += 1;
        s.rotate(WINDOW_SECS * (SAMPLES as u64 + 5), &mut hosts);
        assert_eq!(s.view(None, SLOTS).series[0].totals(), [0, 0, 0, 0, 0]);
    }

    /// 旗が立っていなければ何も確保しない (`--lite` と、まだ誰も上位に居ないとき)。
    #[test]
    fn nothing_is_allocated_before_the_first_slot() {
        let mut s = HostSeries::default();
        let mut hosts = table(&[]);
        s.rotate(0, &mut hosts);
        s.rotate(WINDOW_SECS, &mut hosts);
        assert!(s.data.is_none());
        assert!(s.view(None, SLOTS).series.is_empty());
    }

    /// 窓はテストから差し替えられる。
    #[test]
    fn the_window_can_be_shortened_for_tests() {
        let mut s = HostSeries::default();
        s.set_window(1);
        assert_eq!(s.window_secs(), 1);
        let mut hosts = table(&[]);
        s.rotate(100, &mut hosts);
        hosts = table(&[("a", 2)]);
        s.rotate(101, &mut hosts);
        assert_eq!(hosts["a"].series_slot, Some(0));
        assert_eq!(s.view(None, SLOTS).window_secs, 1);
    }
}
