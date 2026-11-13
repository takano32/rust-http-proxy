//! 判定そのもの ([`Detector`]): 増分の読み・窓の畳み・規則ごとの当たり・立つ / 解除の状態機械。

use std::collections::VecDeque;

use crate::history::History;
use crate::history::summary::{self, Params, Summary};
use crate::metrics::Metrics;
use crate::recent::{ConnTable, SpinningConn};

use super::text::{Facts, cleared_text, fired_text, new_client_text};
use super::*;

/// [`Detector`] が覚えておく CPU の 1 点 (5 分の増分を出すため。T15.0)。
#[derive(Clone, Copy, Debug, Default)]
struct CpuPoint {
    t: u64,
    throttled: u64,
    periods: u64,
    used_us: u64,
}

/// 5 分の窓ぶんの CPU の増分 (いちばん古い点と今の点の差)。
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CpuWindow {
    /// 窓の秒 (点が 1 つしか無ければ 0)
    pub(super) secs: u64,
    /// **5 分ぶん溜まったか** ([`cpu_window_is_full`]。窓の両端の差ではなく、
    /// いちばん最初に読めた標本からの経過で見る)
    pub(super) full: bool,
    pub(super) throttled: u64,
    pub(super) periods: u64,
    pub(super) used_us: u64,
    /// `cpu.max` の quota ÷ period (0 = 無制限か読めない)
    pub(super) quota_cores: f64,
    pub(super) top_tid: u32,
    pub(super) top_role: u8,
}

impl CpuWindow {
    /// 絞られた期間の割合 (0.0〜1.0)。期間が 1 つも過ぎていなければ `None`。
    pub(super) fn ratio(&self) -> Option<f64> {
        (self.periods > 0).then(|| self.throttled as f64 / self.periods as f64)
    }

    /// この窓で使った CPU のコア数 (1.0 = 1 コアを丸ごと)。
    pub(super) fn cores(&self) -> f64 {
        if self.secs == 0 {
            0.0
        } else {
            self.used_us as f64 / (self.secs as f64 * 1e6)
        }
    }

    /// 上位スレッドの「tid と役割」(標本が無ければ空文字列)。
    pub(super) fn top(&self) -> String {
        if self.top_tid == 0 {
            return String::new();
        }
        let role = crate::profile::ROLES
            .get(self.top_role as usize)
            .copied()
            .unwrap_or("other");
        format!(", tid {} {}", self.top_tid, role)
    }
}

/// 1 種類ぶんの状態。
#[derive(Clone, Copy, Debug, Default)]
struct KindState {
    /// いま立っているか (立っている間は同じ種類を二度書かない)
    firing: bool,
    /// 立った時刻
    since: u64,
    /// 条件を外れた最初の時刻 (外れたままここから [`CLEAR_SECS`] 秒で解除)
    calm_since: Option<u64>,
    /// 立った瞬間の基準値 (`connect_p95` だけ使う。下の [`Detector::observe`] を参照)
    base_p95: f64,
}

/// 判定そのもの。**時刻は [`Counters::t`] だけを見る** (テストから注入できるように)。
pub(crate) struct Detector {
    /// 同時接続数の上限 (`PROXY_MAX_CONNS`。0 なら (4) を判定しない)
    max_conns: usize,
    /// 山と見なす本数 (T14.6 の写真と同じ閾)
    threshold: usize,
    /// 1 本前の累計 (`None` = まだ 1 本も見ていない)
    last_totals: Option<[u64; 3]>,
    /// 1 本前の山の写真の通算
    last_shots: Option<u64>,
    state: [KindState; KINDS.len()],
    /// 規則 6 (初めて見た接続元。T14.54)
    new_clients: NewClients,
    /// 直近 5 分ぶんの CPU の累計 (T15.0)。61 点しか持たない
    cpu_hist: VecDeque<CpuPoint>,
    /// **いちばん最初に読めた** cgroup の標本の時刻 (T15.0)。
    /// 「窓が 5 分ぶん溜まったか」([`cpu_window_is_full`]) の起点
    cpu_since: Option<u64>,
    /// 回りっ放しのトンネルを最初に見つけた時刻 (T15.0)。
    /// [`KindState`] は「立つ / 解除」しか持たないので、「1 分続いたら」は
    /// **[`KINDS`] の外に状態を持つ規則** ([`NewClients`] と同じ形) で数える
    spin_since: Option<u64>,
}

/// 規則 6 の覚えていること (T14.54)。
///
/// 窓は**前に見た時刻から今まで** (両端を含む)。両端を含むのは、周期のちょうどその秒に
/// 現れた接続元を落とさないため (`first_seen` は epoch 秒なので、標本を取った直後に
/// 来た接続元は同じ秒を持つ)。二度書かないのは [`NewClients::seen`] の役目。
#[derive(Debug, Default)]
struct NewClients {
    /// 前に見た時刻 (`None` = まだ 1 度も見ていない)。**1 本目は基準にするだけ**で
    /// 書かない (起動時に状態ファイルから読み戻した接続元を新しいと言わないため。
    /// 読み戻した側は `first_seen == 0` でも外れるので、これは二重の歯止め)
    last_t: Option<u64>,
    /// もう `/events` に書いた接続元 ([`NEW_CLIENT_REMEMBER`] 件まで)
    seen: std::collections::HashSet<String>,
}

/// 1 周期ぶんの読み ([`Detector::read`] が作り、[`Detector::hits`] と [`Detector::transition`] が読む)。
#[derive(Debug, Default)]
pub(super) struct Reading {
    /// いつ (epoch 秒。[`Counters::t`])
    pub(super) now: u64,
    /// 直近 [`WINDOW_SECS`] 秒と [`BASE_SECS`] 秒を畳んだもの
    pub(super) w5: Summary,
    pub(super) base: Summary,
    /// 断った / 追い出した数の増分と累計
    pub(super) delta: [u64; 3],
    pub(super) totals: [u64; 3],
    /// この周期に撮れた山の写真の番号
    pub(super) shot: Option<u64>,
    /// 直近 5 分の CPU の絞り (T15.0)
    pub(super) cpu: CpuWindow,
    /// 回っているトンネル (多くて 1 本) と、閾を越えた本数、続いている秒
    pub(super) spinning: Vec<SpinningConn>,
    pub(super) spinning_total: usize,
    pub(super) spin_secs: u64,
    /// `dns_slow` の閾 (ms。[`dns_miss_limit`])
    pub(super) dns_limit: f64,
    /// 起動からの秒 ([`Counters::uptime_secs`])
    pub(super) uptime_secs: u64,
}

impl Default for Detector {
    fn default() -> Self {
        Detector::new()
    }
}

impl Detector {
    pub(crate) fn new() -> Detector {
        Detector {
            max_conns: 0,
            threshold: 0,
            last_totals: None,
            last_shots: None,
            state: [KindState::default(); KINDS.len()],
            new_clients: NewClients::default(),
            cpu_hist: VecDeque::new(),
            cpu_since: None,
            spin_since: None,
        }
    }

    /// 同時接続の上限と、山と見なす本数を教える ([`configure`] が渡す値)。
    pub(crate) fn set_limits(&mut self, max_conns: usize, threshold: usize) {
        self.max_conns = max_conns;
        self.threshold = threshold;
    }

    /// 標本 1 本ぶんを入れて、**変わり目 (立った / 解除された) だけ**を返す。
    ///
    /// 窓を畳むのは [`summary::of`] (T14.24) で、`/history?summary=1` と同じ切り方
    /// (`since <= t <= until`、5 秒の解像度)。**本番では `h` に今の標本が積まれた
    /// あとに呼ぶ** (履歴スレッドの `push` の直後)。
    ///
    /// `conns` は「いま空回りしているトンネル」を引く先 (T15.0)。
    /// **[`ConnTable::update_rates`] のあと**に呼ぶこと (同じ周期の差分を読むため)。
    /// `None` なら `tunnel_spin` は判定しない。
    pub(crate) fn observe(
        &mut self,
        h: &History,
        conns: Option<&ConnTable>,
        c: Counters,
    ) -> Vec<Fired> {
        let r = self.read(h, conns, c);
        let hits = self.hits(&r);
        self.transition(&r, &hits)
    }

    /// 1 周期ぶんの読み: 累計の増分・窓の畳み・CPU の窓・空回りの続いた秒 (T34.44)。
    ///
    /// `last_totals` → `last_shots` → (窓を畳む) → `cpu_window` → `spin_since` の順に進める
    /// (割る前の [`Detector::observe`] と同じ順)。規則の当たりはここでは決めない ([`Detector::hits`])。
    pub(super) fn read(&mut self, h: &History, conns: Option<&ConnTable>, c: Counters) -> Reading {
        let now = c.t;
        let totals = [c.rejected_overload, c.evicted_idle, c.rejected_client_acl];
        // 1 本目は比べる相手が無いので増分 0 (途中から見始めても古い数で立てない)
        let delta = match self.last_totals.replace(totals) {
            Some(prev) => [
                totals[0].saturating_sub(prev[0]),
                totals[1].saturating_sub(prev[1]),
                totals[2].saturating_sub(prev[2]),
            ],
            None => [0; 3],
        };
        let shot = match self.last_shots.replace(c.shots) {
            Some(prev) if c.shots > prev => Some(c.shots),
            _ => None,
        };
        let w5 = fold(h, now, WINDOW_SECS);
        let base = fold(h, now, BASE_SECS);
        // (7) CPU の絞り: 5 分の窓の**増分どうしの比** (累計は親の階層の値のことがある)
        let cpu = self.cpu_window(c);
        // (8) 回っているトンネル。**1 分続いたら**立てる (回り始めた瞬間ではない)
        let (spinning, spinning_total) = match conns {
            Some(t) => t.spinning(TUNNEL_SPIN_DELTA, 1),
            None => (Vec::new(), 0),
        };
        let spin_secs = if spinning_total > 0 {
            now.saturating_sub(*self.spin_since.get_or_insert(now))
        } else {
            self.spin_since = None;
            0
        };
        Reading {
            now,
            w5,
            base,
            delta,
            totals,
            shot,
            cpu,
            spinning,
            spinning_total,
            spin_secs,
            // (2) ミスの平均の閾。canary の基準線の 3 倍と 100 ms の大きい方 (T17.1)
            dns_limit: dns_miss_limit(c.canary_dns_p50_ms),
            uptime_secs: c.uptime_secs,
        }
    }

    /// 8 規則の当たり ([`KINDS`] の順。T34.44)。**状態は読むだけで変えない**
    /// (立っている間の閾の切り替えに `firing` と `base_p95` を見る)。
    pub(super) fn hits(&self, r: &Reading) -> [bool; KINDS.len()] {
        let (w5, base) = (&r.w5, &r.base);
        // (3) `/slo` の `error_rate` と同じ数え方 (T32.36。利用者の決定 (a)): 名前解決の失敗
        // (原因 `dns` の全部。T28.17 の `nxdomain` もここに入る) と 443・80 以外の宛先のエラー
        // (メール宛の確立の時間切れ) は数えない。どちらも標本の列なので窓を畳むだけで出る
        let errors = w5.web_errors();

        // (1) 確立の p95 は「確立 − RTT」(名前解決を除く) の窓で読む (T29.11)
        let p95 = w5.conn_excess.quantile_ms(0.95);
        let p95_base = base.conn_excess.quantile_ms(0.95);
        let peak = w5.active_max;
        // **立っている間は「立った瞬間の基準値」と比べる。** 基準値の窓 (1 時間) は
        // 直近の窓を含むので、山が 1 時間続くと基準値そのものが山になって倍率が 1 に戻り、
        // 山の最中に解除を書いてしまう (T14.0 の 17〜23 時のような 6 時間の山がこれ)。
        // 「収まるまで 1 回だけ」の「収まる」は**元の基準値に戻ること**にする
        let p95_ref = if self.state[I_CONNECT_P95].firing {
            self.state[I_CONNECT_P95].base_p95
        } else {
            p95_base
        };
        let cpu_limit = if self.state[I_CPU_THROTTLED].firing {
            CPU_THROTTLED_CLEAR
        } else {
            CPU_THROTTLED
        };
        // (9) ミスの**率**。窓は 1 時間 (`base`) で、立っている間は `CLEAR` の閾を使う。
        // 名前解決に失敗した試みのミスは数えない (`/slo` と同じ。T32.36)
        let miss_rate = base.resolved_dns_miss_per_connect();
        let miss_limit = if self.state[I_DNS_MISS_RATE].firing {
            DNS_MISS_RATE_CLEAR
        } else {
            DNS_MISS_RATE
        };
        // 添字は [`KINDS`] の並び (直値で引かず、`I_*` で置く)
        let mut hits = [false; KINDS.len()];
        // 直近の窓は基準値の中にも入っているので、起動直後 (1 時間ぶんが全部この
        // 5 分) は倍率がちょうど 1 になり、立たない
        hits[I_CONNECT_P95] = p95 >= CONNECT_MIN_MS
            && base.conn_excess.count > 0
            && p95 >= CONNECT_RATIO * p95_ref
            && w5.conn_excess.count >= CONNECT_MIN_SAMPLES;
        // (2) 名前解決に**成功した**ミスだけで読む (T32.16)。0.30.4 の 62 件は nxdomain の
        // 2 秒級の失敗 (`imap.froggy.com.au` の 2,151 ms など) で鳴っていた。失敗の遅さは
        // 客の頼んだ名前の都合で、利用者の待ちではない (`errors` と同じく `/slo` の数え方)。
        // さらに 443・80 宛ての接続のための名前解決だけ (T33.3。利用者の決定)。0.32.39 の 48 時間の
        // 15 件は NAT の接続元が初めて引いたメールのホスト (`:993`・`:995`・`:143`) の遠い名前で鳴っていた
        hits[I_DNS_SLOW] = w5.web_dns_misses >= DNS_MIN_MISSES
            && w5.web_dns_miss_avg_ms() >= r.dns_limit
            && w5.web_dns_ms_sum >= DNS_MIN_SUM_MS;
        hits[I_ERRORS] = errors >= ERRORS_MIN;
        hits[I_ACTIVE_HIGH] =
            (self.threshold > 0 && peak >= self.threshold as u64) || r.shot.is_some();
        hits[I_REJECTED] = r.delta.iter().any(|&d| d > 0);
        // 窓が 5 分ぶん溜まるまでは判定しない (「5 分のあいだ絞られ続けた」が条件)
        hits[I_CPU_THROTTLED] = r.cpu.full && r.cpu.ratio().is_some_and(|x| x >= cpu_limit);
        hits[I_TUNNEL_SPIN] = r.spinning_total > 0 && r.spin_secs >= TUNNEL_SPIN_SECS;
        hits[I_DNS_MISS_RATE] = r.uptime_secs >= DNS_MISS_RATE_WARMUP_SECS
            && base.connects() >= DNS_MISS_RATE_MIN_CONNECTS
            && miss_rate >= miss_limit;
        hits
    }

    /// 立つ / 解除の状態機械 (T34.44)。**変わり目だけ**を返す。
    ///
    /// 立つのは収まるまで 1 回で、そのとき基準値の p95 を `base_p95` に控える。外れたまま
    /// [`CLEAR_SECS`] 秒続いたら解除を 1 件書く。
    pub(super) fn transition(&mut self, r: &Reading, hits: &[bool; KINDS.len()]) -> Vec<Fired> {
        let now = r.now;
        let p95_base = r.base.conn_excess.quantile_ms(0.95);
        // 説明を組むのに要るだけ (下で `self.state` を可変に借りるので先に写しておく)
        let facts = Facts {
            limits: (self.max_conns, self.threshold),
            delta: r.delta,
            totals: r.totals,
            shot: r.shot,
            cpu: r.cpu,
            spinning: &r.spinning,
            spinning_total: r.spinning_total,
            spin_secs: r.spin_secs,
            dns_limit: r.dns_limit,
        };
        let (w5, base) = (&r.w5, &r.base);
        let mut out = Vec::new();
        for (i, &hit) in hits.iter().enumerate() {
            let kind = KINDS[i];
            let st = &mut self.state[i];
            if hit {
                st.calm_since = None;
                if !st.firing {
                    st.firing = true;
                    st.since = now;
                    st.base_p95 = p95_base;
                    out.push(Fired {
                        kind,
                        cleared: false,
                        text: fired_text(kind, w5, base, &facts),
                    });
                }
                continue;
            }
            if !st.firing {
                continue;
            }
            let calm = *st.calm_since.get_or_insert(now);
            if now.saturating_sub(calm) >= CLEAR_SECS {
                let mins = now.saturating_sub(st.since).div_ceil(60);
                st.firing = false;
                st.calm_since = None;
                out.push(Fired {
                    kind,
                    cleared: true,
                    text: cleared_text(kind, w5, base, &facts, mins),
                });
            }
        }
        out
    }

    /// 直近 [`WINDOW_SECS`] 秒ぶんの CPU の増分 (T15.0)。
    ///
    /// 累計そのものは**このプロセスより前から**動いている (自分の階層に cpu
    /// コントローラが無ければ親の値を読む) ので、生の値では何も言えない。
    /// 覚えておくのは 5 分ぶん = 61 点だけ。
    ///
    /// **「5 分ぶん溜まったか」は窓の両端の差では測らない** ([`cpu_window_is_full`]。
    /// 差は枝打ちの側で [`WINDOW_SECS`] 以下に固定されるので、刻みが 1 秒ずれると
    /// 二度と届かない)。起点はこの [`Detector`] が**最初に読めた**標本の時刻。
    fn cpu_window(&mut self, c: Counters) -> CpuWindow {
        // 読めない標本 (cgroup v1 / Linux 以外 / 上限なし) は窓に入れない。0 を混ぜると
        // 「読めるようになった瞬間」に**累計そのもの** (親の階層の値) が増分に化ける
        if c.cpu_nr_periods == 0 {
            return CpuWindow::default();
        }
        let since = *self.cpu_since.get_or_insert(c.t);
        self.cpu_hist.push_back(CpuPoint {
            t: c.t,
            throttled: c.cpu_nr_throttled,
            periods: c.cpu_nr_periods,
            used_us: c.cpu_used_us,
        });
        while self.cpu_hist.len() > 2
            && self
                .cpu_hist
                .front()
                .is_some_and(|p| c.t.saturating_sub(p.t) > WINDOW_SECS)
        {
            self.cpu_hist.pop_front();
        }
        let (Some(first), Some(last)) = (self.cpu_hist.front(), self.cpu_hist.back()) else {
            return CpuWindow::default();
        };
        CpuWindow {
            secs: last.t.saturating_sub(first.t),
            full: cpu_window_is_full(c.t.saturating_sub(since)),
            throttled: last.throttled.saturating_sub(first.throttled),
            periods: last.periods.saturating_sub(first.periods),
            used_us: last.used_us.saturating_sub(first.used_us),
            quota_cores: c.cpu_quota_cores,
            top_tid: c.cpu_top_tid,
            top_role: c.cpu_top_role,
        }
    }

    /// **規則 6**: この周期に初めて見た接続元を `/events` の 1 行にする (T14.54)。
    ///
    /// 上の 5 種と違って「立つ / 収まる」を持たない (**接続元ごとに 1 回**)。
    /// 返すのは説明だけで、書くのは呼んだ側 ([`check`]。鍵を握ったまま
    /// 出来事のリングに触らないため)。
    ///
    /// 見るのは [`Metrics::clients_first_seen_in`] だけ = **鍵の内側でやるのは
    /// `first_seen` の比較**で、標本のリングは 1 度も畳まない。
    pub(crate) fn observe_clients(&mut self, metrics: &Metrics, now: u64) -> Vec<String> {
        // 1 本目は「前に見た時刻」を置くだけ (それ以前から居た接続元は新しくない)
        let Some(from) = self.new_clients.last_t.replace(now) else {
            return Vec::new();
        };
        let (found, mut over) = metrics.clients_first_seen_in(from, now, NEW_CLIENT_SCAN);
        let mut out = Vec::new();
        for c in found {
            if self.new_clients.seen.contains(&c.client) {
                continue; // 窓の境目の秒。同じ接続元は 1 回だけ
            }
            if self.new_clients.seen.len() < NEW_CLIENT_REMEMBER {
                self.new_clients.seen.insert(c.client.clone());
            }
            if out.len() >= NEW_CLIENT_MAX {
                over += 1;
                continue;
            }
            out.push(new_client_text(&c));
        }
        if over > 0 {
            // 走査を受けたとき。出来事のリング (512 件) を 1 回で流さないための 1 行
            out.push(format!(
                "new_client: {} more clients first seen in the same {}s window (wrote {})",
                over,
                now.saturating_sub(from).max(1),
                out.len()
            ));
        }
        out
    }

    /// いま立っている種類 (テストと、途中経過を見るため)。
    #[cfg(test)]
    pub(crate) fn firing(&self) -> Vec<Kind> {
        KINDS
            .iter()
            .enumerate()
            .filter(|(i, _)| self.state[*i].firing)
            .map(|(_, k)| *k)
            .collect()
    }
}

/// 直近 `secs` 秒を畳む (T14.24 の関数をそのまま使う。5 秒の標本のリングは
/// 6 時間ぶん = 4,320 本 (T14.32) だが、足し込むのは `secs` に入る標本だけ)。
pub(super) fn fold(h: &History, now: u64, secs: u64) -> Summary {
    summary::of(
        h,
        &Params {
            since: now.saturating_sub(secs),
            until: now,
            res: Some(5),
            // バーストを外すのは「平常時どうしを比べる」ための切り方 (T14.0)。
            // ここは**バーストそのものを見つける**のが仕事なので外さない
            normal_hours_only: false,
        },
    )
}

/// `dns_slow` の閾 (ms): [`DNS_MISS_MS`] と canary の基準線の [`DNS_CANARY_RATIO`] 倍の
/// 大きい方 (T17.1)。基準線が無ければ [`DNS_MISS_MS`]。
pub(super) fn dns_miss_limit(canary_p50_ms: Option<u64>) -> f64 {
    match canary_p50_ms {
        Some(b) => DNS_MISS_MS.max(DNS_CANARY_RATIO * b as f64),
        None => DNS_MISS_MS,
    }
}
