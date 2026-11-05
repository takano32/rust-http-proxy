//! ホスト別統計の表 ([`HostTable`]) と区間の合計 ([`Interval`])、その問い合わせ
//! (T.34.26 で `metrics.rs` から移した)。
//!
//! 行を動かしただけで中身は変えていない。外から見える名前は `metrics` の `pub use` でそのまま。

use super::*;

/// 「その区間の」合計 (T12.4 (3))。[`Metrics::take_interval`] が読んで 0 に戻す。
///
/// **ホスト別統計と同じ鍵の内側に置いてある**のがこの構造体の要点で、
/// 全体の合計を別の `AtomicU64` で持つと 1 要求あたり十数回の原子操作が増える
/// (熱い経路に測る側の費用を乗せない。T12.4 の注意)。
#[derive(Debug, Clone, Copy, Default)]
pub struct Interval {
    /// CONNECT の確立時間 (Phase 13 の主指標)
    pub connect: crate::history::Window,
    /// 転送した要求の初バイトまでの時間
    pub forward: crate::history::Window,
    pub errors: u64,
    /// 8 つの原因の件数。**DNS の内訳 (`nxdomain` / `dns_temp`) は持たない** (T25.0):
    /// この構造体は 5 秒の標本 ([`crate::history::Sample`]) と `/metrics` の元で、
    /// 列を増やすと `.rrd` の標本の並びが変わる (過去の標本が別の意味で読み戻る)。
    /// 内訳は**ホスト別の行** ([`HostStats::dns_by_cause`]) だけが持ち、`/status` の
    /// `errors_by_cause` の 9・10 番目に出る
    pub errors_by_cause: [u64; ERR_CAUSES],
    pub dns_misses: u64,
    pub dns_ms_sum: u64,
    /// **利用者が待つ時間** (`queue + client_read + dns + connect` の ms。CONNECT だけ。
    /// T15.0 (2))。`connect` の窓は `open()` の入口 (要求行を読んだ後) から測るので、
    /// accept してワーカーが動き出すまでの待ちも名前解決も入っていない。
    /// **`--lite` では書かない** (`queue` と `client_read` が 0 なので「4 段の和」を
    /// 名乗れない)。`/history` に列として出すのは T15.0 (10) の仕事で、ここは値を
    /// 用意するところまで。**欄は末尾に足す** (既存の並びを動かさない)
    pub wait: crate::history::Window,
    /// CONNECT 1 本ごとの「確立 (`dns + connect`) − オリジン側のカーネルの RTT」(ms。T.28.45)。
    /// 成功してカーネルの RTT が読めた本数だけ (`crate::history::Sample::excess` の元)
    pub excess: crate::history::Window,
    /// `dns_misses` のうち、名前解決に失敗した試み (原因 `dns`) が払ったぶん (T.28.45)
    pub dns_fail_misses: u64,
    /// CONNECT 1 本ごとの「確立 (`connect` だけ。**名前解決を除く**) − オリジン側のカーネルの RTT」
    /// (ms。T.29.12)。`excess` と同じ本数 (成功して RTT が読めたもの) で、`/slo` の確立の 2 つはこちら
    pub conn_excess: crate::history::Window,
    /// 名前解決に成功した試み (確立・転送の窓に入った成功と、原因 `dns` 以外のエラー) のうち、
    /// **宛先のポートが 443・80 以外**のもの (T.30.3。利用者の決定: `/slo` の `error_rate` は 443・80 だけ)。
    /// 鍵 (`scheme://host:port`) の末尾が `:443` か `:80` でなければここに数える
    pub other_port_attempts: u64,
    /// そのうちエラーで終わったもの (T.30.3。メール宛 `:993` の確立の時間切れなど)
    pub other_port_errors: u64,
    /// `dns_ms_sum` のうち、名前解決に失敗した試み (原因 `dns`) が払ったぶん (ms。T.32.16)。
    /// 異常検知の `dns_slow` は「名前解決に成功したミス」の ms (`dns_ms_sum − dns_fail_ms_sum`) で読む
    pub dns_fail_ms_sum: u64,
    /// 名前解決に成功した試みのミスのうち、**宛先のポートが 443・80** のもの (回数と ms の合計。T.33.3。
    /// 利用者の決定: 異常検知の `dns_slow` は 443・80 宛ての接続のための名前解決だけで判定する)。
    /// 鍵の末尾の見分けは `other_port_attempts` と同じ ([`web_port`])
    pub web_dns_misses: u64,
    pub web_dns_ms_sum: u64,
}

/// ホスト別統計の表と、区間の合計。1 つの鍵で守る。
#[derive(Default)]
pub(super) struct HostTable {
    pub(super) map: HashMap<String, HostStats>,
    /// 起動からの累計 (`/metrics` のヒストグラム用)
    pub(super) total: Interval,
    /// 直近の標本以降 (`take_interval` が読んで 0 に戻す)
    pub(super) interval: Interval,
    /// 直近の標本以降の段階 (`take_stages` が読んで 0 に戻す。T14.3 (1))。
    /// **同じ鍵の内側に置いてある**ので、段階を足しても原子操作は増えない
    pub(super) stages: crate::profile::Stages,
    /// 上位 16 ホストの時系列 (`/hosts/series`。T14.22)。**ホスト表と同じ鍵の中**に
    /// 置いて、要求の経路が鍵を 2 つ取らないようにしてある
    pub(super) series: crate::hostseries::HostSeries,
    /// 直近 1,024 本の標本そのもの (`/status` の `recent_quantiles`。T14.31)。
    /// ここも**同じ鍵の中**で、書くのは 8 バイト 1 回
    pub(super) quantiles: crate::quantiles::Quantiles,
    /// 443・80 宛てのエラーのいちばん多い宛先 (`/slo` の `error_rate` が外す。T.30.4)。
    /// 同じ鍵の中で、触るのはエラーの経路と履歴スレッドだけ
    pub(super) dests: web_dest::DestTable,
    /// 満杯の表から `last_seen` のいちばん古い行を捨てた数 (起動から。T.32.10)
    pub(super) evicted: u64,
}

/// 満杯のホスト表から `last_seen` のいちばん古い行を 1 つ捨て、通算は `other` の行に足す (T.32.10)。
///
/// `other` の行と、上位 16 の時系列の枠を持つ行 (`series_slot`) は捨てない。`other` の行を新しく
/// 作ったときはもう 1 行捨てて、表を [`MAX_HOSTS`] 未満にする。捨てられたら `true`。
/// 1,000 行を舐めるのは**新しい鍵が満杯の表に来たときだけ** (既にある鍵の要求は通らない)。
pub(super) fn evict_oldest_host(t: &mut HostTable) -> bool {
    while t.map.len() >= MAX_HOSTS {
        let Some(key) = t
            .map
            .iter()
            .filter(|(k, s)| k.as_str() != "other" && s.series_slot.is_none())
            .min_by_key(|(_, s)| s.last_seen)
            .map(|(k, _)| k.clone())
        else {
            return false;
        };
        if let Some(gone) = t.map.remove(&key) {
            t.map.entry("other".to_string()).or_default().absorb(&gone);
            t.evicted += 1;
        }
    }
    true
}

/// ホスト別統計の鍵 (`scheme://host:port`) の宛先のポートが 443 か 80 か (T.30.3。`/slo` の `error_rate` が数えるもの)。
///
/// 解かずに末尾を比べるだけ (`:8080` は `:80` で終わらず、`:1443` は `:443` で終わらない)。ポートの無い鍵
/// (`loop://` の `Host` にポートが無いとき) は 443・80 以外として数える。
#[inline]
pub(super) fn web_port(key: &str) -> bool {
    key.ends_with(":443") || key.ends_with(":80")
}

/// ホスト別統計の鍵が、確立 / 初バイトの窓・段階の窓に入る鍵か (T.34.40。`Shape::of` と
/// `/slo` の宛先 (`web_dest`) の 2 か所の判定を 1 つにした)。
///
/// CONNECT (`connect://`) はいつも入る。forward のうち `blocked://` (拒否) と `loop://` (自分宛て) は入れない。
/// 前綴りを見るだけで、解かない。
#[inline]
pub(super) fn counted_key(key: &str) -> bool {
    key.starts_with("connect://") || !(key.starts_with("blocked://") || key.starts_with("loop://"))
}

impl Metrics {
    /// 満杯の表から行を捨てた数 (`/status` の `hosts_evicted`。起動から。T.32.10)。
    pub fn hosts_evicted(&self) -> u64 {
        self.hosts.locked().evicted
    }

    /// 443・80 宛てのエラーのいちばん多い宛先の、確定した前の時間といまの時間 (T.30.4)。
    ///
    /// **呼ぶのは履歴スレッドだけ** (5 秒ごと。`/slo` に渡す)。`now` の時間に切り替わっていれば
    /// ここで切り替える (エラーが 1 件も来ない時間も確定させるため)。切り替えない回は鍵 1 回と比較 1 回、
    /// いちばん多い宛先が居れば名前の写し 1 回。
    pub fn web_error_hours(&self, now: u64) -> (Option<DestHour>, DestHour) {
        let mut hosts = self.hosts.locked();
        let t = &mut *hosts;
        t.dests.roll(&mut t.map, now);
        (t.dests.prev(), t.dests.current(&t.map))
    }

    /// ホスト別の時系列の窓を進め、上位 16 を入れ替える (T14.22)。
    ///
    /// **呼ぶのは history スレッドだけ** (5 秒ごと)。窓の境目 (既定 5 分) でなければ
    /// ホスト表の鍵 1 回と比較 1 回で戻る。`--lite` は履歴スレッドそのものが立たない
    /// (`PROXY_STATS_PERSIST=off` と同じ) ので、旗が立つことも配列を確保することも無い。
    pub fn roll_host_series(&self) {
        let mut hosts = self.hosts.locked();
        let t = &mut *hosts;
        t.series.rotate(crate::cache::now_epoch(), &mut t.map);
    }

    /// 上位ホストの時系列の写し (`/hosts/series`。T14.22)。
    ///
    /// `host` を渡すとそのホストだけ、渡さなければ直近 1 時間の要求数の多い順に `top` 件。
    pub fn host_series(&self, host: Option<&str>, top: usize) -> crate::hostseries::View {
        self.hosts.locked().series.view(host, top)
    }

    /// 時系列の窓を差し替える (**結合テスト用の口**。本番は 5 分。T14.22)。
    pub fn set_host_series_window(&self, secs: u64) {
        self.hosts.locked().series.set_window(secs);
    }

    /// 直近 1,024 本の正確な分位点 (`/status` の `recent_quantiles`。T14.31)。
    ///
    /// **鍵の内側でするのは値の写しだけ**で、`select_nth_unstable` は鍵を放してから
    /// 回す (1,024 本で数 us だが、要求の経路が待つ鍵をその間握らない)。
    /// 呼ぶのは `/status` に来たときだけ。
    pub fn recent_quantiles(
        &self,
    ) -> (
        crate::quantiles::Stats,
        crate::quantiles::Stats,
        crate::quantiles::Stats,
    ) {
        let (c, f, w) = self.hosts.locked().quantiles.copy();
        let now = crate::cache::now_epoch();
        (c.stats(now), f.stats(now), w.stats(now))
    }

    /// 上の 3 つを `{"connect":{..},"forward":{..},"wait":{..}}` にしたもの
    /// (`wait` は T15.0 (2) で**末尾に**足した)。
    pub fn recent_quantiles_json(&self) -> String {
        let (c, f, w) = self.recent_quantiles();
        crate::quantiles::to_json(&c, &f, &w)
    }

    /// 直近の標本以降の合計を読み、0 に戻す ([`crate::history::Sample::take`] だけが呼ぶ)。
    pub fn take_interval(&self) -> Interval {
        let mut hosts = self.hosts.locked();
        std::mem::take(&mut hosts.interval)
    }

    /// 直近の標本以降の段階を読み、0 に戻す (`profile-sample` スレッドだけが呼ぶ。T14.3)。
    pub fn take_stages(&self) -> crate::profile::Stages {
        let mut hosts = self.hosts.locked();
        std::mem::take(&mut hosts.stages)
    }

    /// 起動からの累計 (`/metrics` の全体のヒストグラム用)。
    pub fn totals(&self) -> Interval {
        self.hosts.locked().total
    }

    /// DNS の失敗の内訳 (`nxdomain` / `dns_temp`) の通算 (T26.1)。
    ///
    /// `/metrics` の `errors_total{cause="nxdomain"|"dns_temp"}` と、`/status` の
    /// `errors_by_cause` の 9・10 番目が**同じ値**になるように、ホスト別の表を 1 回
    /// 舐めて足す。区間の合計 ([`Interval`]) に内訳を**持たせていない**ので
    /// (標本 (`.rrd`) の列を増やさない。T25.0 の判断はそのまま)、ここで数え直す。
    ///
    /// **足し切れる**のは、表が [`MAX_HOSTS`] で溢れた回も `other` の行に入るため
    /// (`errors_total{cause="dns"}` と 9・10 番目の合計が食い違わない)。
    pub fn dns_cause_totals(&self) -> [u64; DNS_CAUSES] {
        let hosts = self.hosts.locked();
        let mut acc = [0u64; DNS_CAUSES];
        for s in hosts.map.values() {
            for (a, b) in acc.iter_mut().zip(s.dns_by_cause) {
                *a = a.saturating_add(b);
            }
        }
        acc
    }

    /// ホスト (オリジン側) のカーネルの RTT と再送を 1 標本足す (`/hosts`。T14.5)。
    ///
    /// **呼ぶのは接続の終わりだけ** (トンネルを閉じるとき、プールのオリジン接続を
    /// 捨てるとき)。要求ごとには呼ばない。読めなかった (`rtt_us == 0`) ときは
    /// 鍵も取らずに戻る = Linux 以外と `--lite` は 1 命令も払わない。
    pub fn record_host_rtt(&self, host: &str, rtt_us: u32, retrans: u32) {
        if rtt_us == 0 {
            return;
        }
        let mut hosts = self.hosts.locked();
        // **行は作らない**: ここへ来る接続は直前に必ず数えられているので、無いのは
        // 表が [`MAX_HOSTS`] で溢れて `other` に畳まれたときだけ (その 1 標本は捨てる)
        if let Some(s) = hosts.map.get_mut(host) {
            s.observe_rtt(rtt_us, retrans);
        }
    }

    /// 起動時に状態ファイルから読み戻す (今の値が空のときだけ)。
    ///
    /// **ホスト別はそのまま、接続元別は記録の形に直してから入れる** (T14.41)。
    /// `off` なら接続元の表は 1 行も戻さず (`.rrd` は消さないので `on` に戻せばまた読める)、
    /// `hashed` なら前の起動が `on` で書いた生の IP をここで 16 進に直す — 直さないと
    /// `hashed` で起こし直した直後の `/clients` に生の IP が並ぶ。
    pub fn restore(&self, hosts: Vec<(String, HostStats)>, clients: Vec<(String, HostStats)>) {
        let mut h = self.hosts.locked();
        if h.map.is_empty() {
            h.map.extend(hosts);
        }
        if !crate::records::recording() {
            return;
        }
        let mut c = self.clients.locked();
        if c.is_empty() {
            // 読み戻した接続元は「いつから居るか」が分からない (`first_seen` は
            // `.rrd` に書いていない欄なので 0 のまま = この起動より前から)
            c.extend(clients.into_iter().map(|(k, s)| {
                (
                    crate::records::client_key(&k).into_owned(),
                    ClientStats::restored(s),
                )
            }));
        }
    }

    /// 要求数の多い順に並べたホスト別統計。
    pub fn hosts_sorted(&self) -> Vec<(String, HostStats)> {
        self.hosts_sorted_by(HostSort::Requests)
    }

    /// 要求数の多い上位 `n` 件 ([`Self::hosts_sorted`] の先頭 `n` 件と同じ顔ぶれ) を、
    /// **写さず並べずに順不同で**見る (状態ファイルの 60 秒ごとの書き直し。T.32.22)。
    ///
    /// 表が `n` 件以下なら全部をそのまま、越えていれば `select_nth_unstable` で上位を選ぶ。
    /// **鍵を握ったまま `f` を呼ぶ**ので、`f` は軽く (システムコールはしない) すること。
    pub fn visit_top_hosts(&self, n: usize, mut f: impl FnMut(&str, &HostStats)) {
        let hosts = self.hosts.locked();
        if hosts.map.len() <= n {
            for (k, s) in &hosts.map {
                f(k, s);
            }
            return;
        }
        if n == 0 {
            return;
        }
        let mut v: Vec<(&String, &HostStats)> = hosts.map.iter().collect();
        v.select_nth_unstable_by(n - 1, |a, b| {
            b.1.requests.cmp(&a.1.requests).then_with(|| a.0.cmp(b.0))
        });
        for (k, s) in &v[..n] {
            f(k, s);
        }
    }

    /// 鍵を選んで並べたホスト別統計 (T13.3)。`/status?sort=` が上位 50 を切り出すのに使う。
    ///
    /// **同点は要求数 → 名前で崩す**ので、どの鍵でも順序は 1 つに決まる (テストが順序で書ける)。
    /// 名前解決だけは合計が同じときに問い合わせた回数を先に見る: 手元の loopback では
    /// ミス 1 回が 1 ms 未満で `dns_ms_sum` が 0 に丸まる (`dns::take_resolve_cost`) ため、
    /// 合計だけでは「名前で引いているホスト」と「IP リテラル」の区別が付かない。
    pub fn hosts_sorted_by(&self, sort: HostSort) -> Vec<(String, HostStats)> {
        let hosts = self.hosts.locked();
        let mut v: Vec<(String, HostStats)> = hosts
            .map
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        v.sort_by(|a, b| {
            let tie = b.1.requests.cmp(&a.1.requests).then_with(|| a.0.cmp(&b.0));
            match sort {
                HostSort::Requests => tie,
                HostSort::Errors => b.1.errors.cmp(&a.1.errors).then(tie),
                HostSort::Dns => {
                    b.1.dns_ms_sum
                        .cmp(&a.1.dns_ms_sum)
                        .then_with(|| b.1.dns_misses.cmp(&a.1.dns_misses))
                        .then(tie)
                }
                HostSort::Slow => b.1.avg_ms().total_cmp(&a.1.avg_ms()).then(tie),
            }
        });
        v
    }

    /// `hosts[]` の通算の始まり = いちばん古い `last_seen` (`/status` の `restored_since`)。
    /// `0` は「表が空」。
    ///
    /// **[`Self::hosts_sorted_by`] を通さない**のが大事: `?hosts=0` の軽い口は 50 行を
    /// 組まないので、ここも写しも並べ替えも作らずに表を 1 回舐めるだけにする
    /// (値は同じ。T25.1 段 2)。
    pub(super) fn restored_since(&self) -> u64 {
        let hosts = self.hosts.locked();
        hosts
            .map
            .values()
            .map(|s| s.last_seen)
            .filter(|&t| t > 0)
            .min()
            .unwrap_or(0)
    }
}
