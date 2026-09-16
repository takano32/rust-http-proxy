//! 計測の本体 ([`Metrics`])。ホスト別・接続元別の統計、`/status` の組み立て。
//!
//! 型と定数は下の層 (`proxy-metrics-types`) にあり、**ここで丸ごと出し直している**ので
//! `crate::metrics::ErrCause` のような書き方は割る前と同じに通る (T14.55)。

// 下の層の型と定数をこのモジュールの名前空間にも出す (呼ぶ側の書き方を変えないため)。
pub use proxy_metrics_window::metrics::*;

use crate::sync::LockExt;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::cache::Cache;

// 接続元の個票と、内部エンドポイントを引いた接続元は下の層
// (`proxy-metrics-window`) に置いてある (T14.55)。**今までの名前でここから引ける**
// (`crate::metrics::ClientStats` / `Reader` …)。
pub use crate::clients::{
    ClientStats, MAX_READER_PATH, MAX_READERS, Reader, STATUS_READERS, fingerprint, target_parts,
};

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
    pub errors_by_cause: [u64; ERR_CAUSES],
    pub dns_misses: u64,
    pub dns_ms_sum: u64,
}

/// ホスト別統計の表と、区間の合計。1 つの鍵で守る。
#[derive(Default)]
struct HostTable {
    map: HashMap<String, HostStats>,
    /// 起動からの累計 (`/metrics` のヒストグラム用)
    total: Interval,
    /// 直近の標本以降 (`take_interval` が読んで 0 に戻す)
    interval: Interval,
    /// 直近の標本以降の段階 (`take_stages` が読んで 0 に戻す。T14.3 (1))。
    /// **同じ鍵の内側に置いてある**ので、段階を足しても原子操作は増えない
    stages: crate::profile::Stages,
    /// 上位 16 ホストの時系列 (`/hosts/series`。T14.22)。**ホスト表と同じ鍵の中**に
    /// 置いて、要求の経路が鍵を 2 つ取らないようにしてある
    series: crate::hostseries::HostSeries,
    /// 直近 1,024 本の標本そのもの (`/status` の `recent_quantiles`。T14.31)。
    /// ここも**同じ鍵の中**で、書くのは 8 バイト 1 回
    quantiles: crate::quantiles::Quantiles,
}

pub struct Metrics {
    pub start_time: Instant,
    pub total_requests: AtomicU64,
    pub active_connections: AtomicUsize,
    /// アイドルなまま監視スレッド (epoll) に預けている接続数と、その監視が生きているか
    pub parked_connections: AtomicUsize,
    /// そのうち CONNECT トンネルの数 (両方向とも暇なもの。T8.1)
    pub parked_tunnels: AtomicUsize,
    pub park_watcher_alive: AtomicBool,
    /// 同時接続数の上限に当たって 503 で断った数
    pub rejected_overload: AtomicU64,
    /// 上限に当たったときに、席を作るために閉じた暇なトンネルの数 (T13.2)
    pub evicted_idle: AtomicU64,
    /// `PROXY_ALLOW_CLIENTS` に無い接続元として accept 直後に閉じた数 (T14.18)
    pub rejected_client_acl: AtomicU64,
    /// 接続元ごとの同時接続の上限 (`PROXY_MAX_CONNS_PER_CLIENT`) に当たって 503 で断った数 (T14.13)
    pub rejected_per_client: AtomicU64,
    /// 要求を読めずに断った数 (理由別。`/status` の `rejected_requests`。T14.28)。
    /// **書くのは断る経路だけ**なので、通した要求はこの配列を 1 度も触らない
    pub rejected_requests: [AtomicU64; BAD_REQUEST_REASONS],
    pub bytes_forwarded: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    /// オリジンへ新規に張った接続数と、プールから再利用した回数
    pub origin_new: AtomicU64,
    pub origin_reused: AtomicU64,
    /// ダッシュボード用の履歴 (`history::spawn` が記録)
    pub history: crate::history::History,
    /// 直近のエラーの個票 (`/errors`。T13.4)。**書くのはエラーの経路だけ**なので、
    /// 成功の熱い経路はこのリングを 1 度も触らない
    pub errors: crate::recent::ErrorRing,
    /// いま開いている接続の一覧 (`/connections`。T13.4)。登録と抹消は接続の開始と
    /// 終了で 1 回ずつだけ (`--lite` では登録しない)
    pub conns: crate::recent::ConnTable,
    /// 段階・スレッド・ロックの窓 (`/profile`。T14.3)。`.rrd` には書かない
    pub profile: crate::profile::Profile,
    /// 閉じた接続の個票 (`/recent`。T14.4)。**書くのは接続の終了で 1 回だけ**で、
    /// 要求ごとにも中継のバイトごとにも触らない
    pub closed: crate::recent::RecentRing,
    /// 山の写真 (`/bursts`。T14.6)。accept の経路は閾を越えた瞬間に旗を立てるだけで、
    /// **撮るのは history スレッド** ([`Metrics::take_burst_shot`])
    pub bursts: crate::recent::BurstRing,
    /// 上の 4 本のリング (`/recent` `/errors` `/bursts` `/log`) を
    /// `$HOME/.rust-http-proxy.recent` に残しているか (T14.9)。
    /// `PROXY_STATS_PERSIST=off` と、ファイルが開けなかったときは `false`
    pub recent_persisted: AtomicBool,
    /// CONNECT のホストと SNI が食い違った本数の合計 (`/status` の `sni_mismatches`。T14.38)。
    /// **メモリだけ** (`.rrd` には書かない)。ホスト別は [`HostStats::sni_mismatch`]
    pub sni_mismatches: AtomicU64,
    /// 確立までに SYN を送り直した回数の合計 (`/status` の `syn_retrans_total`。T14.46)。
    /// **メモリだけ** (`.rrd` には書かない)。ホスト別は [`HostStats::syn_retrans`]
    pub syn_retrans_total: AtomicU64,
    /// 重い口 (`/snapshot` `/profile` …) が既に 1 本走っていたので 503 で断った数 (T14.51)。
    /// 足すのは内部エンドポイントの経路だけで、プロキシとしての要求は 1 度も触らない
    pub heavy_rejected: AtomicU64,
    /// ホスト (`scheme://host:port`) ごとの統計と、区間の合計
    hosts: Mutex<HostTable>,
    /// 接続元 IP ごとの個票 (上位 `MAX_CLIENTS`、あふれた分は "other")
    clients: Mutex<HashMap<String, ClientStats>>,
    /// **内部エンドポイントを引いた**接続元の表 (`/status` の `readers` と `/readers`。
    /// 最大 [`MAX_READERS`]。T14.53)。上の `clients` とは**別の表**で、
    /// プロキシとして通した要求は 1 件も入らない ([`Metrics::record_reader`])
    readers: Mutex<HashMap<String, Reader>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            total_requests: AtomicU64::new(0),
            active_connections: AtomicUsize::new(0),
            parked_connections: AtomicUsize::new(0),
            parked_tunnels: AtomicUsize::new(0),
            park_watcher_alive: AtomicBool::new(false),
            rejected_overload: AtomicU64::new(0),
            evicted_idle: AtomicU64::new(0),
            rejected_client_acl: AtomicU64::new(0),
            rejected_per_client: AtomicU64::new(0),
            rejected_requests: [const { AtomicU64::new(0) }; BAD_REQUEST_REASONS],
            bytes_forwarded: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            origin_new: AtomicU64::new(0),
            origin_reused: AtomicU64::new(0),
            history: crate::history::History::default(),
            errors: crate::recent::ErrorRing::new(),
            conns: crate::recent::ConnTable::new(),
            profile: crate::profile::Profile::default(),
            closed: crate::recent::RecentRing::new(),
            bursts: crate::recent::BurstRing::new(),
            recent_persisted: AtomicBool::new(false),
            sni_mismatches: AtomicU64::new(0),
            syn_retrans_total: AtomicU64::new(0),
            heavy_rejected: AtomicU64::new(0),
            hosts: Mutex::new(HostTable::default()),
            clients: Mutex::new(HashMap::new()),
            readers: Mutex::new(HashMap::new()),
        }
    }

    /// ホスト別に 1 要求を数える (応答時間なし)。
    pub fn record_host(&self, host: &str, outcome: HostOutcome, bytes: u64) {
        self.record(host, outcome, bytes, None, &Detail::default());
    }

    /// ホスト別に 1 要求と応答時間を数える。
    pub fn record_host_timed(&self, host: &str, outcome: HostOutcome, bytes: u64, took: Duration) {
        self.record(host, outcome, bytes, Some(took), &Detail::default());
    }

    /// [`record_host_timed`](Self::record_host_timed) に内訳を添えた版 (T12.4 (2))。
    /// 内訳は**この関数が取る鍵の内側**でしか触らないので、原子操作は増えない。
    pub fn record_host_detail(
        &self,
        host: &str,
        outcome: HostOutcome,
        bytes: u64,
        took: Option<Duration>,
        detail: Detail,
    ) {
        self.record(host, outcome, bytes, took, &detail);
    }

    /// エラー 1 件を個票のリングに写す (`/errors`。T13.4)。
    ///
    /// **エラーを返す経路からだけ呼ぶこと。** 集計 (`/status`) では「どのホストで何件」
    /// までしか分からず、デプロイ先で 2 秒かかって失敗した名前解決の**相手と時刻**が
    /// 読めなかった (T13.0)。原因の分からないエラー (`detail.cause` が `None`) は
    /// 書かない — 原因なしの行が並んでも読む人の手が増えないため。
    pub fn record_error(
        &self,
        connect: bool,
        target: &str,
        client: &str,
        status: u16,
        detail: &Detail,
    ) {
        let Some(cause) = detail.cause else {
            return;
        };
        self.errors.push(crate::recent::ErrorEntry::new(
            crate::recent::EntryKind::from_connect(connect),
            target,
            client,
            status,
            crate::recent::EntryCause::Error(cause),
            detail.dns_ms,
            detail.connect_ms,
        ));
    }

    /// canary (T14.10) の失敗を個票のリングに 1 件だけ残す (`/errors` の `kind: "canary"`)。
    ///
    /// **集計 (`errors` / `errors_by_cause`) には足さない**: canary は利用者の要求では
    /// ないので、「利用者に返したエラー」の数に混ざると `/status` が読めなくなる。
    /// 接続元は空 (自分) で、返した状態コードも無い (0)。
    pub fn record_canary_error(&self, target: &str, cause: ErrCause, dns_ms: u64, connect_ms: u64) {
        self.errors.push(crate::recent::ErrorEntry::new(
            crate::recent::EntryKind::Canary,
            target,
            "",
            0,
            crate::recent::EntryCause::Error(cause),
            dns_ms,
            connect_ms,
        ));
    }

    /// 閉じた接続を `/connections` から外し、個票を 1 件残す (`/recent`。T14.4)。
    ///
    /// **接続の終了で 1 回だけ呼ぶこと** (`ActiveGuard::drop` = 接続の寿命そのもの)。
    /// 取る鍵は表の鍵 1 回 (元からある抹消のぶん) とリングの鍵 1 回だけで、
    /// 何を書くかは枠 ([`crate::recent::ConnSlot`]) に既に載っている。
    pub fn record_closed(&self, id: u64) {
        if let Some(slot) = self.conns.unregister(id)
            && let Some(entry) = slot.closed_entry(std::time::Instant::now())
        {
            // 閉じた理由・寿命・上り下りのバイト・預けられていた秒を窓に畳む (T14.6)。
            // **2,000 件のリングを読み直さない**: 1 件を作ったこの場で、区間の値として
            // 足しておく (どちらも鍵 1 回、原子操作もシステムコールも増えない)
            self.history.closed.observe(&entry);
            self.closed.push(entry);
        }
    }

    /// 山の写真を 1 枚撮る (**history スレッドが 5 秒ごとに呼ぶ**。T14.6)。
    ///
    /// 頼まれていなければ、山が引いたかどうかだけ見て戻る (原子の読み 2 回)。
    /// 頼まれていたら `/connections` の表から 1 枚作る (**表の鍵 1 回**)。
    /// 越えた接続を受けたスレッドは旗を立てるだけなので、accept の経路に鍵は増えない。
    pub fn take_burst_shot(&self) {
        let active = self.active_connections.load(Ordering::Relaxed);
        let Some(trigger) = self.bursts.take_pending() else {
            self.bursts.rearm_if_calm(active);
            return;
        };
        let rows = self.conns.snapshot();
        let shot = crate::recent::BurstShot::take(
            &rows,
            self.bursts.next_seq(),
            active,
            trigger,
            self.bursts.max_conns(),
            self.bursts.threshold(),
            self.evicted_idle.load(Ordering::Relaxed),
            self.rejected_overload.load(Ordering::Relaxed),
        );
        self.bursts.push(shot);
    }

    /// 403 で拒否した 1 件を個票のリングに写す (`/errors`。T14.2 (4))。
    ///
    /// 集計 (`errors_by_cause`) は**変えない** ([`BlockCause`] の説明のとおり、
    /// 配列を伸ばすと `.rrd` の版が上がる)。ここも**拒否した経路からだけ**通るので、
    /// 通した要求には 1 命令も足さない。
    pub fn record_blocked(&self, connect: bool, target: &str, client: &str, cause: BlockCause) {
        self.errors.push(crate::recent::ErrorEntry::new(
            crate::recent::EntryKind::from_connect(connect),
            target,
            client,
            403,
            crate::recent::EntryCause::Blocked(cause),
            0,
            0,
        ));
    }

    /// 読めなかった要求を 1 件数え、個票のリングにも写す (`/errors`。T14.28)。
    ///
    /// **断る経路からだけ呼ぶこと** (400 / 414 / 431 を返して閉じる場所)。
    /// `status` は**実際にクライアントへ返した状態コード**で、要求行が壊れていた
    /// ときと絶対 URI / `Host` が無いときは 400、要求行が長すぎたときは 414、
    /// ヘッダーが長すぎたときは 431 になる (`/errors` に嘘を書かないため)。
    ///
    /// **個票に要求行そのものは入れない** (個票の決まり: 入れてよいのは接続元 IP・
    /// 宛先 `host:port`・時刻・数字だけ。壊れた要求行には URL もヘッダーも載っている)。
    /// 宛先はまだ解けていないので空のままにする。集計 (`errors_by_cause`) にも
    /// 足さない: 4xx はエラー (5xx) ではないので、混ぜると `/status` が読めなくなる。
    pub fn record_bad_request(&self, reason: BadRequestReason, client: &str, status: u16) {
        self.rejected_requests[reason as usize].fetch_add(1, Ordering::Relaxed);
        self.errors.push(crate::recent::ErrorEntry::new(
            // まだメソッドを解いていない (CONNECT か転送か分からない) 段階なので
            // `forward` で揃える
            crate::recent::EntryKind::Forward,
            "",
            client,
            status,
            crate::recent::EntryCause::BadRequest(reason),
            0,
            0,
        ));
    }

    /// `/status` の `rejected_requests` (理由別と合計)。
    pub fn rejected_requests_json(&self) -> String {
        let mut out = String::with_capacity(160);
        let mut total = 0u64;
        out.push('{');
        for (i, name) in BAD_REQUEST_REASON_NAMES.iter().enumerate() {
            let n = self.rejected_requests[i].load(Ordering::Relaxed);
            total += n;
            let _ = write!(out, "\"{}\":{},", name, n);
        }
        let _ = write!(out, "\"total\":{}}}", total);
        out
    }

    fn record(
        &self,
        host: &str,
        outcome: HostOutcome,
        bytes: u64,
        took: Option<Duration>,
        detail: &Detail,
    ) {
        // 起動時の自己ベンチ (T14.43) が自分で打った要求は `/hosts` にも窓にも入れない。
        // 入れると 20,000 要求ぶんの行・分位点・段階が実トラフィックの統計に混ざり、
        // デプロイ先の `/status` と `/history` が起動直後の 3 秒に支配される。
        // 費用は自己ベンチが回っていないときの原子の読み 1 回 (鍵も時計も触る前)
        if crate::selfbench::is_target(host) {
            return;
        }
        // 壁時計はここで 1 回だけ読む (ホスト別の `last_seen` と直近の標本 (T14.31) で
        // 使い回す。**読む回数は今までと同じ 1 回**)
        let now = crate::cache::now_epoch();
        // 取り合いを数える (T14.3 (3))。空いていれば `locked` と同じ費用
        let mut hosts = self
            .hosts
            .locked_counted(&crate::sync::LOCK_CONTENDED[crate::sync::LOCK_STATS]);
        let hosts = &mut *hosts;
        // 鍵の種類は 1 回だけ見る (CONNECT のホスト別統計の鍵は `connect://` で始まる。
        // `tunnel::report`。前綴りを見るだけで済むので、呼び出し側に旗を持たせない)
        let connect = host.starts_with("connect://");
        let counted = connect || !(host.starts_with("blocked://") || host.starts_with("loop://"));
        // 窓と時系列に入れる値 (ms)。**1 回だけ作る** (以前は Interval 2 つで 2 回作っていた。T14.22)
        let ms = took.map(|d| {
            detail
                .first_byte_ms
                .unwrap_or_else(|| d.as_millis().min(u64::MAX as u128) as u64)
        });
        // 全体の合計も同じ鍵の内側で足す (原子操作を増やさない)
        for iv in [&mut hosts.total, &mut hosts.interval] {
            iv.dns_misses += detail.dns_misses;
            iv.dns_ms_sum += detail.dns_ms;
            if outcome == HostOutcome::Error {
                iv.errors += 1;
            }
            if let Some(c) = detail.cause {
                iv.errors_by_cause[c as usize] += 1;
            }
            if let Some(ms) = ms {
                if connect {
                    iv.connect.observe(ms);
                } else if counted {
                    iv.forward.observe(ms);
                }
            }
        }
        // 段階の窓 (T14.3 (1)) と、直近 1,024 本の標本そのもの (T14.31)。どちらも
        // `--lite` では時計を読んでいないので触らない。**旗も分岐も 1 つにまとめてある**
        // ので、`--lite` の経路には 1 命令も足していない。同じ鍵の内側なので、
        // 原子操作も鍵の取り直しも増えない
        if let Some(d) = took
            && counted
            && crate::profile::on()
        {
            // 直近の標本は **us のまま**入れる (12 段の区間では 1 ms 単位で読めない)。
            // forward の初バイトは元が ms 刻みなので ×1,000 するだけ、CONNECT の確立は
            // `Duration` のまま来るので us が出る (ベンチの p50 と ±0.05 ms で比べる値)
            let us = match detail.first_byte_ms {
                Some(fb) => fb.saturating_mul(1_000).min(u32::MAX as u64) as u32,
                None => d.as_micros().min(u32::MAX as u128) as u32,
            };
            if connect {
                hosts.stages.observe_connect(detail);
                hosts.quantiles.connect.observe(us, now);
            } else {
                hosts.stages.observe_forward(detail);
                hosts.quantiles.forward.observe(us, now);
            }
        }
        // CONNECT のホストと SNI が食い違った本数の合計 (`/status`。T14.38)。
        // 旗が立つのはトンネルの終わりだけなので、ここは分岐 1 回 (原子は触らない)
        if detail.sni_mismatch {
            self.sni_mismatches.fetch_add(1, Ordering::Relaxed);
        }
        // 確立までの SYN の再送の合計 (`/status`。T14.46)。再送は稀なので、
        // 普通の接続はこの分岐 1 回で終わる (原子は触らない)
        if detail.syn_retrans != 0 {
            self.syn_retrans_total
                .fetch_add(detail.syn_retrans as u64, Ordering::Relaxed);
        }
        // 既にある行はキーを作り直さない (毎要求の String 確保をなくす)
        if let Some(stats) = hosts.map.get_mut(host) {
            stats.count(now, outcome, bytes, took, detail);
            // 上位 16 ホストなら時系列にも 1 標本ぶん (T14.22)。旗が無ければ分岐 1 回で終わり
            if let Some(slot) = stats.series_slot {
                hosts.series.add(
                    slot,
                    ms.unwrap_or(0),
                    detail.dns_ms,
                    outcome == HostOutcome::Error,
                );
            }
            return;
        }
        let key = if hosts.map.len() >= MAX_HOSTS {
            "other".to_string()
        } else {
            host.to_string()
        };
        hosts
            .map
            .entry(key)
            .or_default()
            .count(now, outcome, bytes, took, detail);
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
    pub fn recent_quantiles(&self) -> (crate::quantiles::Stats, crate::quantiles::Stats) {
        let (c, f) = self.hosts.locked().quantiles.copy();
        let now = crate::cache::now_epoch();
        (c.stats(now), f.stats(now))
    }

    /// 上の 2 つを `{"connect":{..},"forward":{..}}` にしたもの。
    pub fn recent_quantiles_json(&self) -> String {
        let (c, f) = self.recent_quantiles();
        crate::quantiles::to_json(&c, &f)
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

    /// 接続元 IP ごとに 1 要求を数える。
    ///
    /// `target` はこの要求の宛先 (`scheme://host:port` / `host:port`。分からなければ
    /// `None`)。宛先の種類・ポート・IP リテラルは**この関数が既に取っている鍵の内側**で
    /// 数える (原子操作もシステムコールも増やさない。T14.7)。
    pub fn record_client(
        &self,
        client: &str,
        outcome: HostOutcome,
        bytes: u64,
        dir: (u64, u64),
        took: Option<Duration>,
        target: Option<&str>,
    ) {
        // ホスト別と同じ理由で、自己ベンチのぶんは `/clients` にも入れない (T14.43)
        if target.is_some_and(crate::selfbench::is_target) {
            return;
        }
        // 記録の一括 off とハッシュ化 (T14.41)。`off` は鍵も取らずに戻る
        // (ホスト別の統計 `/hosts` はこの上の `record_host*` なので残る)
        if !crate::records::recording() {
            return;
        }
        let client = &crate::records::client_key(client);
        let mut clients = self.clients.locked();
        if let Some(stats) = clients.get_mut(client.as_ref()) {
            stats.count(outcome, bytes, dir, took, target);
            return;
        }
        let key = if clients.len() >= MAX_CLIENTS {
            "other".to_string()
        } else {
            client.to_string()
        };
        clients
            .entry(key)
            .or_insert_with(ClientStats::now)
            .count(outcome, bytes, dir, took, target);
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

    /// 接続元 (クライアント側) のカーネルの RTT と再送を 1 標本足す (`/clients`。T14.5)。
    ///
    /// 利用者 → プロキシの往復がここで初めて数字になる。[`record_host_rtt`](Self::record_host_rtt)
    /// と同じく**接続の終わりだけ**で、鍵は `record_client` のものと同じ 1 つ。
    pub fn record_client_rtt(&self, client: &str, rtt_us: u32, retrans: u32) {
        if rtt_us == 0 || !crate::records::recording() {
            return;
        }
        let client = crate::records::client_key(client);
        let mut clients = self.clients.locked();
        if let Some(c) = clients.get_mut(client.as_ref()) {
            c.stats.observe_rtt(rtt_us, retrans);
        }
    }

    /// 全体の RTT の合計 (`/metrics` の `sorahost_rtt_seconds`。T14.5)。
    ///
    /// 返すのは `[(us 合計, 標本数); 2]` で、添字は
    /// [`crate::recent::CLIENT_SIDE`] / [`crate::recent::ORIGIN_SIDE`]。
    /// **ホスト別は出さない** (系列が増えすぎる) ので、ここで畳んでから渡す。
    /// 読むのは `/metrics` に来たときだけなので、表を一度なめてよい。
    pub fn rtt_totals(&self) -> [(u64, u64); 2] {
        let fold = |acc: (u64, u64), s: &HostStats| {
            (
                acc.0.saturating_add(s.rtt_us_sum),
                acc.1.saturating_add(s.rtt_samples),
            )
        };
        let client = self
            .clients
            .locked()
            .values()
            .fold((0, 0), |a, c| fold(a, &c.stats));
        let origin = self.hosts.locked().map.values().fold((0, 0), fold);
        [client, origin]
    }

    /// 接続元の `User-Agent` を 1 つ覚える (`/clients`。T14.7)。
    ///
    /// **呼ぶのは接続の最初の要求のときだけ** (`src/lib.rs`)。要求ごとに見ると
    /// ヘッダーの走査も鍵も要求ごとに増えるので、接続 1 本につき 1 回に決めてある
    /// (2 要求目からは呼び出し側の旗で飛ばす)。
    pub fn record_client_agent(&self, client: &str, agent: &str) {
        if !crate::records::recording() {
            return;
        }
        let client = &crate::records::client_key(client);
        let mut clients = self.clients.locked();
        if let Some(stats) = clients.get_mut(client.as_ref()) {
            stats.note_agent(agent);
            return;
        }
        let key = if clients.len() >= MAX_CLIENTS {
            "other".to_string()
        } else {
            client.to_string()
        };
        clients
            .entry(key)
            .or_insert_with(ClientStats::now)
            .note_agent(agent);
    }

    /// 接続元ごとの同時接続の上限に当たって断った 1 本を数える (`/clients` の `rejected`。T14.13)。
    ///
    /// **通るのは断った経路だけ**なので、通した接続には 1 命令も足さない。全体の合計
    /// (`rejected_per_client`) も同じ場所で足す (鍵は接続元の表の 1 回)。
    pub fn record_client_rejected(&self, client: &str) {
        // **合計 (`/status` の `rejected_per_client`) は `off` でも数える**
        // (「誰が」を含まない数字なので、止めるのは接続元別の表だけ。T14.41)
        self.rejected_per_client
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if !crate::records::recording() {
            return;
        }
        let client = &crate::records::client_key(client);
        let mut clients = self.clients.locked();
        if let Some(stats) = clients.get_mut(client.as_ref()) {
            stats.rejected += 1;
            return;
        }
        let key = if clients.len() >= MAX_CLIENTS {
            "other".to_string()
        } else {
            client.to_string()
        };
        clients.entry(key).or_insert_with(ClientStats::now).rejected += 1;
    }

    /// 内部エンドポイント (`/status` `/clients` …) を 1 回引かれたことを数える (T14.53)。
    ///
    /// **呼ぶのは `endpoints::handle` の入口で 1 回だけ** (要求が自分宛てだと決まった
    /// あと)。プロキシとして通す要求 (CONNECT / forward) はこの関数を 1 度も通らないので、
    /// 熱い経路の費用は 0 で、T14.7 の `clients[]` (自分宛てを数えない表) とも混ざらない。
    ///
    /// `path` は**問い合わせ文字列を外したパス**を渡すこと (`?` 以降は記録しない)。
    /// 表が [`MAX_READERS`] で溢れたら、最後に引いたのがいちばん古い行を 1 つ捨てる。
    pub fn record_reader(&self, client: &str, path: &str) {
        // 記録の一括 off とハッシュ化 (T14.41)。読み手の表も接続元 1 人ずつの記録なので、
        // `off` なら鍵も取らずに戻り、`hashed` なら他の個票と同じ 1 関数を通す
        if !crate::records::recording() {
            return;
        }
        let client = &crate::records::client_key(client);
        let now = crate::cache::now_epoch();
        let mut readers = self.readers.locked();
        if let Some(r) = readers.get_mut(client.as_ref()) {
            r.count += 1;
            r.last_at = now;
            // 同じパスが続く間は確保しない (監視は同じ口を叩き続ける)。
            // [`MAX_READER_PATH`] より長いパスだけは毎回切り直すことになるが、
            // そういう相手は走査なので惜しまない (どのみち自分宛ての経路だけ)
            if r.last_path != path {
                r.last_path = crate::recent::clip(path, MAX_READER_PATH);
            }
            return;
        }
        if readers.len() >= MAX_READERS
            // 捨てるのは**最後に引いたのがいちばん古い行**。同着は引いた数の少ない方 →
            // 名前の順で崩すので、どの環境でも捨てる 1 行は同じに決まる
            && let Some(old) = readers
                .iter()
                .min_by(|a, b| {
                    a.1.last_at
                        .cmp(&b.1.last_at)
                        .then_with(|| a.1.count.cmp(&b.1.count))
                        .then_with(|| a.0.cmp(b.0))
                })
                .map(|(k, _)| k.clone())
        {
            readers.remove(&old);
        }
        readers.insert(
            client.to_string(),
            Reader {
                count: 1,
                last_at: now,
                last_path: crate::recent::clip(path, MAX_READER_PATH),
            },
        );
    }

    /// 内部エンドポイントを引いた接続元を、引いた回数の多い順に (T14.53)。
    ///
    /// **同点は最後に引いた時刻の新しい順 → 名前**で崩すので、順序は 1 つに決まる
    /// ([`Metrics::clients_sorted_by`] と同じ作法)。
    pub fn readers_sorted(&self) -> Vec<(String, Reader)> {
        let readers = self.readers.locked();
        let mut v: Vec<(String, Reader)> = readers
            .iter()
            .map(|(k, r)| (k.clone(), r.clone()))
            .collect();
        v.sort_by(|a, b| {
            b.1.count
                .cmp(&a.1.count)
                .then_with(|| b.1.last_at.cmp(&a.1.last_at))
                .then_with(|| a.0.cmp(&b.0))
        });
        v
    }

    /// `/status` の `readers` (上位 [`STATUS_READERS`] 件の配列だけ)。全部は `/readers`。
    pub fn readers_json(&self) -> String {
        let rows: Vec<String> = self
            .readers_sorted()
            .into_iter()
            .take(STATUS_READERS)
            .map(|(c, r)| r.to_json(&c))
            .collect();
        format!("[{}]", rows.join(","))
    }

    /// `/readers` の応答 (**全部**。`budget` バイトに収まるところまで。T14.53)。
    ///
    /// 上限のバイト数は呼ぶ側 (`crates/endpoints` の `recent::MAX_BODY` = 256 KiB) が
    /// 持っている数で、個票の口と同じ扱いにするために引数で受け取る (この層に写しを
    /// 置くと 2 か所で食い違う)。満杯 (256 行) でも 30 KB ほどなので普通は切れない。
    pub fn readers_body(&self, budget: usize) -> String {
        let all = self.readers_sorted();
        let count = all.len();
        // 末尾 (`],"count":...}`) のために空けておくぶん
        let room = budget.saturating_sub(256);
        let mut out = String::with_capacity(4096);
        out.push_str(SCHEMA_HEAD);
        out.push_str("\"readers\":[");
        let (mut shown, mut cut) = (0usize, false);
        for (c, r) in &all {
            let item = r.to_json(c);
            if out.len() + item.len() + 2 > room {
                cut = true;
                break;
            }
            if shown > 0 {
                out.push(',');
            }
            out.push_str(&item);
            shown += 1;
        }
        let _ = write!(
            out,
            "],\"count\":{},\"shown\":{},\"truncated\":{},\"max_readers\":{},\"max_path\":{},\"persisted\":false,\"uptime_secs\":{}}}",
            count,
            shown,
            cut,
            MAX_READERS,
            MAX_READER_PATH,
            self.start_time.elapsed().as_secs()
        );
        out
    }

    /// 要求数の多い順に並べた接続元別統計 (`.rrd` と `/metrics` が使う欄だけ)。
    pub fn clients_sorted(&self) -> Vec<(String, HostStats)> {
        self.clients_sorted_by(ClientSort::Requests)
            .into_iter()
            .map(|(k, s)| (k, s.stats))
            .collect()
    }

    /// 鍵を選んで並べた接続元の個票 (`/clients` と `/status` の `clients[]`。T14.7)。
    ///
    /// **同点は要求数 → 名前で崩す**ので、どの鍵でも順序は 1 つに決まる
    /// ([`Metrics::hosts_sorted_by`] と同じ作法。テストが順序で書ける)。
    pub fn clients_sorted_by(&self, sort: ClientSort) -> Vec<(String, ClientStats)> {
        let clients = self.clients.locked();
        let mut v: Vec<(String, ClientStats)> = clients
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        v.sort_by(|a, b| {
            let tie =
                b.1.stats
                    .requests
                    .cmp(&a.1.stats.requests)
                    .then_with(|| a.0.cmp(&b.0));
            match sort {
                ClientSort::Requests => tie,
                ClientSort::Recent => b.1.stats.last_seen.cmp(&a.1.stats.last_seen).then(tie),
                ClientSort::Targets => {
                    b.1.distinct_targets()
                        .cmp(&a.1.distinct_targets())
                        .then(tie)
                }
                ClientSort::Literal => b.1.literal_targets.cmp(&a.1.literal_targets).then(tie),
            }
        });
        v
    }

    /// **初めて見た時刻がこの窓の中にある接続元だけ**を拾う (T14.54 の規則 6)。
    ///
    /// 窓は `[from, to]` (両端を含む。呼ぶ側は「前に見た時刻」から「いまの時刻」を
    /// 渡し、同じ秒を二度見ても書かないよう自分で覚えておく)。`first_seen` が `0` の
    /// 接続元 (状態ファイルから読み戻した = この起動より前から居る) は**新しくない**
    /// ので外す。並びは `first_seen` の古い順 (同じ秒は名前順) で、`max` 件まで。
    /// 2 つ目は `max` に入り切らなかった件数。
    ///
    /// **鍵の内側でやるのは `first_seen` の比較だけ**で、写すのは拾った数件だけ。
    /// [`Metrics::clients_sorted_by`] で表を丸ごと写すと、1,000 接続元 × 宛先 256 件の
    /// clone を 5 秒ごとに鍵を握ったまま行うことになり、要求の経路
    /// ([`Metrics::record_client`]) が待たされる。
    pub fn clients_first_seen_in(&self, from: u64, to: u64, max: usize) -> (Vec<NewClient>, usize) {
        let mut found: Vec<NewClient> = {
            let clients = self.clients.locked();
            clients
                .iter()
                .filter(|(_, s)| s.first_seen > 0 && s.first_seen >= from && s.first_seen <= to)
                .map(|(k, s)| NewClient {
                    client: k.clone(),
                    first_seen: s.first_seen,
                    agent: s.agent().map(|a| a.to_string()),
                    requests: s.stats.requests,
                    // `ports` は**出た順**に伸びるので、先頭が最初の宛先のポート
                    port: s.ports.first().map(|(p, _)| *p),
                    literal: s.literal_targets > 0,
                })
                .collect()
        };
        found.sort_by(|a, b| {
            a.first_seen
                .cmp(&b.first_seen)
                .then(a.client.cmp(&b.client))
        });
        let over = found.len().saturating_sub(max);
        found.truncate(max);
        (found, over)
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

    /// プールから再利用できた割合 (`reused / (new + reused)`)。
    pub fn pool_hit_ratio(&self) -> f64 {
        let new = self.origin_new.load(Ordering::Relaxed);
        let reused = self.origin_reused.load(Ordering::Relaxed);
        match new + reused {
            0 => 0.0,
            total => reused as f64 / total as f64,
        }
    }

    pub fn inc_origin_conn(&self, reused: bool) {
        if reused {
            self.origin_reused.fetch_add(1, Ordering::Relaxed);
        } else {
            self.origin_new.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn to_json(&self) -> String {
        self.to_json_with_cache(None, StatusExtras::default())
    }

    /// キャッシュ統計と、上の層が用意した部品を含めた `/status` 用 JSON を生成する。
    ///
    /// `settings` / `blocklist` / `state_file` を**呼び出し側から受け取る**のは、
    /// ここで `reload::status_json()` のように直接呼ぶと、下の層 (指標) が上の層を
    /// 呼ぶ形になって依存が輪になるため。`/status` を組み立てるのは `proxy-endpoints`
    /// の仕事で、ここはその部品を並べるだけにする。
    pub fn to_json_with_cache(&self, cache: Option<&Cache>, extra: StatusExtras<'_>) -> String {
        let uptime = self.start_time.elapsed().as_secs();
        let requests = self.total_requests.load(Ordering::Relaxed);
        let active = self.active_connections.load(Ordering::Relaxed);
        let bytes = self.bytes_forwarded.load(Ordering::Relaxed);

        let cache_json = match cache {
            Some(c) => c.to_json(),
            None => "null".to_string(),
        };

        // 上位 50 を切り出す鍵だけが `?sort=` で変わる (JSON の形は変わらない。T13.3)
        let all_hosts = self.hosts_sorted_by(extra.sort);
        // ホスト別統計は `.rrd` で再起動をまたいで通算されるので、**いつからの通算か**を出す
        // (`total_requests` は起動から、`hosts[]` は通算という窓の混在が読めなかった)
        let restored_since = all_hosts
            .iter()
            .map(|(_, s)| s.last_seen)
            .filter(|&t| t > 0)
            .min()
            .unwrap_or(0);
        // `/proc` を読むのはこのパスに来たときだけ (要求ごとには読まない)
        let threads = crate::sysinfo::process_threads().unwrap_or(0);
        let (fds, max_fds) = crate::sysinfo::process_fds().unwrap_or((0, 0));
        // RSS は、キャッシュのプローブ (既定 1 秒ごと) が回っているならその値を使う。
        // **同じ `/status` の `cache.system.process_rss_bytes` と 1 バイトも違わない**ように
        // するため (1 枚の中に食い違う RSS が 2 つ並ぶと、どちらを信じるかが分からない)。
        // プローブが止まっている (`PROXY_CACHE_PROBE_SECS=0` / キャッシュ無効 / `--lite`)
        // ときは古い値しか無いので、その場で `/proc/self/status` を読む
        let rss = cache
            .filter(|c| !c.config().probe_interval.is_zero())
            .and_then(|c| c.snapshot().rss)
            .or_else(crate::sysinfo::process_rss);
        let hosts_json: Vec<String> = all_hosts
            .into_iter()
            .take(50)
            .map(|(h, s)| {
                format!(
                    "{{\"host\":\"{}\",{}}}",
                    crate::json::escape(&h),
                    stats_json(&s, true)
                )
            })
            .collect();
        let clients_json: Vec<String> = self
            .clients_sorted_by(ClientSort::Requests)
            .into_iter()
            .take(50)
            .map(|(c, s)| {
                format!(
                    "{{\"client\":\"{}\",{}{}}}",
                    crate::json::escape(&c),
                    stats_json(&s.stats, false),
                    // T14.7 の 4 つは**末尾に足す** (既存の鍵の順は変えない)
                    s.status_json()
                )
            })
            .collect();
        // **部ごとに分けて書く** (T14.55)。1 つの巨大な `format!` (50 引数) は
        // `rustc` がこのクレートを抱える量をひとりで押し上げていたため。
        // **出す JSON は 1 バイトも変えていない**: 書式の並びも引数の順もそのままで、
        // 区切りが `,` のところで切っただけ。
        let mut out = String::with_capacity(4096);
        push_head(&mut out, &extra, uptime, requests, restored_since);
        self.push_capacity(&mut out, &extra, threads, fds, max_fds, active);
        self.push_io(&mut out, bytes);
        push_tables(&mut out, &hosts_json, &clients_json);
        push_env(&mut out, &extra, &cache_json);
        self.push_tail(
            &mut out,
            rss,
            threads,
            extra.concurrency.live_threads as u64,
            cache,
        );
        out
    }

    /// `/status` の `threads` 〜 `rejected_per_client` (席と、断った数)。
    fn push_capacity(
        &self,
        out: &mut String,
        extra: &StatusExtras<'_>,
        threads: u64,
        fds: u64,
        max_fds: u64,
        active: usize,
    ) {
        let _ = write!(
            out,
            concat!(
                "\"threads\":{},\"fds\":{},\"max_fds\":{},",
                "\"active_connections\":{},\"max_conns\":{},",
                "\"parked_connections\":{},\"parked_tunnels\":{},",
                "\"parking\":{},",
                "\"live_threads\":{},\"idle_threads\":{},\"queued_jobs\":{},\"max_threads\":{},",
                "\"rejected_overload\":{},\"evicted_idle\":{},\"rejected_client_acl\":{},",
                "\"rejected_per_client\":{},"
            ),
            threads,
            fds,
            max_fds,
            active,
            extra.concurrency.max_conns,
            self.parked_connections.load(Ordering::Relaxed),
            self.parked_tunnels.load(Ordering::Relaxed),
            self.park_watcher_alive.load(Ordering::Relaxed),
            extra.concurrency.live_threads,
            extra.concurrency.idle_threads,
            extra.concurrency.queued_jobs,
            extra.concurrency.max_threads,
            self.rejected_overload.load(Ordering::Relaxed),
            self.evicted_idle.load(Ordering::Relaxed),
            self.rejected_client_acl.load(Ordering::Relaxed),
            self.rejected_per_client.load(Ordering::Relaxed)
        );
    }

    /// `/status` の `bytes_forwarded` 〜 `origin_connections` (流した量)。
    fn push_io(&self, out: &mut String, bytes: u64) {
        let _ = write!(
            out,
            concat!(
                "\"bytes_forwarded\":{},",
                "\"cache_hits\":{},\"cache_misses\":{},",
                "\"origin_connections\":{{\"new\":{},\"reused\":{},\"pool_hit_ratio\":{:.4}}},"
            ),
            bytes,
            self.cache_hits.load(Ordering::Relaxed),
            self.cache_misses.load(Ordering::Relaxed),
            self.origin_new.load(Ordering::Relaxed),
            self.origin_reused.load(Ordering::Relaxed),
            self.pool_hit_ratio()
        );
    }

    /// `/status` の末尾 (**鍵を足すときはここの末尾に足す**。既存の鍵の順は変えない)。
    ///
    /// `kernel` は T14.12、`memory` は T14.21、`recent_quantiles` は T14.31、
    /// `rate_bps_total` は T14.39、`rejected_requests` は T14.28、`sni_mismatches` は
    /// T14.38、`self_bench` は T14.43、`readers` は T14.53、`syn_retrans_total` は
    /// T14.46、`records` は T14.41、`heavy_rejected` は T14.51 で末尾に足したもの。
    fn push_tail(
        &self,
        out: &mut String,
        rss: Option<u64>,
        threads: u64,
        conn_threads: u64,
        cache: Option<&Cache>,
    ) {
        let _ = write!(
            out,
            concat!(
                "\"kernel\":{},\"memory\":{},\"recent_quantiles\":{},\"rate_bps_total\":{},",
                "\"rejected_requests\":{},\"sni_mismatches\":{},\"self_bench\":{},",
                "\"readers\":{},\"syn_retrans_total\":{},\"records\":\"{}\",",
                "\"heavy_rejected\":{}}}"
            ),
            // カーネルと cgroup の統計 (5 秒の標本で読んだ最新の値。T14.12)。
            // ここから直に呼べるのは `dns` / `ipv6` と同じ**下の層**だから
            // (`settings` のような上の層の部品は `extra` で受け取る)
            crate::kernel::status_json(),
            // RSS の内訳 (T14.21)。`mallinfo2` を読むのはこの経路だけ
            memory_json(rss, threads, conn_threads, cache),
            // 直近 1,024 本の正確な分位点 (T14.31)。区間の補間ではない実測の並び
            self.recent_quantiles_json(),
            // いま流れているバイト/秒の合計 (T14.39)。history スレッドが 5 秒ごとに
            // 書いた値を原子 1 回読むだけ (`/connections` の `rate_bps` の和)
            self.conns.rate_bps_total(),
            // 読めずに断った要求の理由別 (T14.28)。原子 6 本を読むだけ
            self.rejected_requests_json(),
            // CONNECT のホストと SNI が食い違った本数 (T14.38)
            self.sni_mismatches.load(Ordering::Relaxed),
            // 起動直後に loopback だけで測った CPU/要求 と CPU/本 (T14.43)。
            // `PROXY_SELF_BENCH=off` (既定) なら `null` (覚えている結果が無い)
            crate::selfbench::status_json(),
            // 内部エンドポイントを引いた接続元の上位 20 (T14.53)。全部は `/readers`。
            // **プロキシとして通した要求は入らない** (`clients[]` とは別の表)
            self.readers_json(),
            // 確立までに SYN を送り直した回数の合計 (T14.46)
            self.syn_retrans_total.load(Ordering::Relaxed),
            // 記録の一括 off とハッシュ化 (T14.41)。旗を原子 1 回読むだけ
            crate::records::mode().name(),
            // 重い口が 1 本走っている最中に来て 503 で断った数 (T14.51)
            self.heavy_rejected.load(Ordering::Relaxed)
        );
    }
}

/// `/status` の先頭 (版と、起動からの窓の目印)。
///
/// **応答の形の版は先頭の鍵** (T14.49)。読む道具が先頭 64 バイトで分岐できる。
/// 窓の目印 (T12.4 (4)) は、`since_start_secs` から下が起動から、`restored_since` が
/// `hosts[]` / `clients[]` の通算の始まり (epoch 秒、`0` = 無し)。
fn push_head(
    out: &mut String,
    extra: &StatusExtras<'_>,
    uptime: u64,
    requests: u64,
    restored_since: u64,
) {
    let _ = write!(
        out,
        concat!(
            "{{\"schema\":{},\"status\":\"ok\",\"version\":\"{}\",\"uptime_secs\":{},\"total_requests\":{},",
            "\"since_start_secs\":{},\"restored_since\":{},"
        ),
        SCHEMA,
        crate::json::escape(extra.version),
        uptime,
        requests,
        uptime,
        restored_since
    );
}

/// `/status` の `hosts[]` と `clients[]` (どちらも上位 50 を組み終えたもの)。
fn push_tables(out: &mut String, hosts_json: &[String], clients_json: &[String]) {
    let _ = write!(
        out,
        "\"hosts\":[{}],\"clients\":[{}],",
        hosts_json.join(","),
        clients_json.join(",")
    );
}

/// `/status` の「この環境で何が読めるか」(T14.15) と canary (T14.10)。
///
/// `settings` / `blocklist` / `state_file` は**上の層が組んだものを受け取る**
/// ([`StatusExtras`])。ここから直に呼べるのは下の層だけ。
fn push_env(out: &mut String, extra: &StatusExtras<'_>, cache_json: &str) {
    let _ = write!(
        out,
        "\"log_level\":\"{}\",\"settings\":{},\"dns\":{},\"canary\":{},\"ipv6\":{},\"blocklist\":{},\"state_file\":{},\"capabilities\":{},\"cache\":{},",
        crate::log::current_level().as_str().trim(),
        extra.settings,
        crate::dns::status_json(),
        // 利用者の要求が無い時間帯の名前解決と TCP 接続 (最後の 1 回。T14.10)
        crate::canary::status_json(),
        crate::net::ipv6_status_json(),
        extra.blocklist,
        extra.state_file,
        crate::sysinfo::capabilities::status_json(),
        cache_json
    );
}

/// `/status` の `memory` (RSS が何でできているか。T14.21)。
///
/// 256 MiB のコンテナで「RSS の内訳」を `/status` 1 枚から読むためのもの。読むのは
/// **`/status` に来たときだけ**で、要求の経路には 1 命令も足さない (`mallinfo2` は
/// アリーナの鍵を順に取るので数 us)。
///
/// **足して RSS になる形ではない** (README の `/status` の節にも同じ注意を書いてある):
///
/// - `rss` は呼び出し側が渡す 1 つの値 (キャッシュのプローブが読んだもの、または今読んだもの)。
///   同じ `/status` の `cache.system.process_rss_bytes` と食い違わせない
/// - `heap_used` / `heap_free` / `mmap` は `mallinfo2(3)` の `uordblks` / `fordblks` /
///   `hblkhd`。`heap_free` は「返していないだけ」で、`MADV_DONTNEED` 済みなら常駐していない。
///   `mmap` も確保しただけで触っていないページは常駐しない (`calloc` の大きな塊など)
/// - `stacks_estimate` は**予約**の合計 (実際に触ったページとの差は出せない)。接続スレッド
///   (`conn`) は 256 KiB、それ以外は Rust の既定 2 MiB
/// - `cache_memory` はキャッシュの本体 (`cache.memory.used_bytes`) と先行確保
///   (`cache.memory.reserved_bytes`) の合計 = キャッシュがヒープに持っている量
/// - `rings` は記録のリングが**満杯のときの見積もり** (固定部 + 文字列の上限。T13.4 / T14.4 /
///   T14.6 / T14.11 / T14.22 / T14.25 / T14.27 / T14.31)。いま何件入っているかは `/recent` や `/errors` の `total` を見る。
///   `readers` だけは環状ではなく表 (最大 256 行。T14.53) だが、同じ「満杯のとき」の見積もりで並べてある
/// - `arenas` は `PROXY_MALLOC_ARENAS` で掛けた上限 (`0` = glibc の既定のまま。T5.6)
///
/// `mallinfo2` が無い環境 (musl / glibc 2.32 以下 / Linux 以外) では 3 つとも `null`。
fn memory_json(rss: Option<u64>, threads: u64, conn_threads: u64, cache: Option<&Cache>) -> String {
    use crate::events::{Event, MAX_EVENTS, MAX_TEXT};
    use crate::history::{History, RESOLUTIONS, Sample};
    use crate::log::{Line, MAX_LOG_LINE, MAX_LOG_LINES};
    use crate::recent::{
        BurstShot, ClosedCounts, ErrorEntry, MAX_BURSTS, MAX_CLIENT, MAX_ERRORS, MAX_RECENT,
        MAX_RECENT_TARGET, MAX_SHOT_CLIENTS, MAX_SHOT_TARGETS, MAX_TARGET, RecentEntry,
    };
    use crate::transfer::TransferCounts;

    /// 接続スレッドのスタック (`crates/workers` の `STACK_SIZE` と同じ値)。
    /// あちらは private なので写してある (変えるときは両方)。
    const CONN_STACK: u64 = 256 * 1024;
    /// それ以外のスレッド (`std::thread` の既定)。
    const THREAD_STACK: u64 = 2 * 1024 * 1024;

    let name = size_of::<(String, u32)>();
    let recent = (MAX_RECENT * (size_of::<RecentEntry>() + MAX_RECENT_TARGET + MAX_CLIENT)) as u64;
    let errors = (MAX_ERRORS * (size_of::<ErrorEntry>() + MAX_TARGET + MAX_CLIENT)) as u64;
    let bursts = (MAX_BURSTS
        * (size_of::<BurstShot>()
            + MAX_SHOT_CLIENTS * (name + MAX_CLIENT)
            + MAX_SHOT_TARGETS * (name + MAX_TARGET))) as u64;
    let log = (MAX_LOG_LINES * (size_of::<Line>() + MAX_LOG_LINE)) as u64;
    let events = (MAX_EVENTS * (size_of::<Event>() + MAX_TEXT)) as u64;
    // 接続元 1 つの追跡 (T14.27)。**追跡していなければ 1 バイトも確保していない**ので、
    // これも他と同じ「満杯のとき」の見積もり
    let trace = (crate::trace::MAX_TRACE * crate::trace::MAX_LINE_ESTIMATE) as u64;
    // ホスト別の時系列は固定長 (上位 16 ホスト × 288 標本 × 5 項目 × 8 B。T14.22)。
    // **上位が 1 つ決まるまでは確保しない**ので、これも「満杯のとき」の見積もり
    let hostseries = (crate::hostseries::SLOTS
        * crate::hostseries::SAMPLES
        * crate::hostseries::FIELDS
        * size_of::<u64>()) as u64;
    // 内部エンドポイントを引いた接続元の表 (T14.53)。環状ではないが、同じ「満杯のとき」の
    // 見積もり (最大 256 行 × (鍵 + パス))。**1 行も引かれていなければ 1 バイトも確保しない**
    let readers =
        (MAX_READERS * (size_of::<(String, Reader)>() + MAX_CLIENT + MAX_READER_PATH)) as u64;
    // 直近の標本の環状は固定長 (2 系統 × 1,024 本 × 8 B = 16 KiB。T14.31)。
    // 1 本目を書くまで確保しないので、これも「満杯のとき」の見積もり
    let quantiles = crate::quantiles::BYTES as u64;
    // 履歴は 3 解像度の標本 (T12.4。**5 秒はメモリだけ 6 時間 = 4,320 本**。T14.32) と、
    // 閉じた接続の分布の窓 2 つ (T14.6)、速さと半閉じの窓 2 つ (T14.25)。
    // 窓の方は 5 秒 × 720 のままなので [`RESOLUTIONS`] を使う
    let samples: usize = (0..RESOLUTIONS.len()).map(History::capacity).sum();
    let windows = RESOLUTIONS[0].1 + RESOLUTIONS[1].1;
    let history = (samples * size_of::<Sample>()
        + windows * size_of::<(u64, ClosedCounts)>()
        + windows * size_of::<(u64, TransferCounts)>()) as u64;

    let conn = conn_threads.min(threads);
    let other = threads.saturating_sub(conn_threads);
    let stacks = (threads > 0).then(|| conn * CONN_STACK + other * THREAD_STACK);
    let heap = crate::sysinfo::malloc_info();
    let opt = |v: Option<u64>| v.map_or_else(|| "null".to_string(), |x| x.to_string());

    format!(
        concat!(
            "{{\"rss\":{},\"heap_used\":{},\"heap_free\":{},\"mmap\":{},",
            "\"stacks_estimate\":{},\"cache_memory\":{},",
            "\"rings\":{{\"recent\":{},\"errors\":{},\"bursts\":{},\"log\":{},",
            "\"events\":{},\"trace\":{},\"history\":{},\"hostseries\":{},\"quantiles\":{},",
            "\"readers\":{},\"total\":{}}},\"arenas\":{}}}"
        ),
        opt(rss),
        opt(heap.map(|h| h.used)),
        opt(heap.map(|h| h.free)),
        opt(heap.map(|h| h.mmap)),
        opt(stacks),
        cache.map_or(0, |c| c.mem_usage().0.saturating_add(c.mem_reserved())),
        recent,
        errors,
        bursts,
        log,
        events,
        trace,
        history,
        hostseries,
        quantiles,
        readers,
        recent
            + errors
            + bursts
            + log
            + events
            + trace
            + history
            + hostseries
            + quantiles
            + readers,
        crate::sysinfo::arena_max(),
    )
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// 段階とスレッドの標本を取るスレッド ([`crate::profile::spawn`]) が読む 3 つ。
///
/// **下の層 (`proxy-metrics-window`) から `Metrics` を呼べない**ので、あちらが決めた
/// 口をここで埋める (T14.55 でクレートを割ったときの形。中身は今までと同じ読み方)。
impl crate::profile::Source for Metrics {
    fn total_requests(&self) -> u64 {
        self.total_requests.load(Ordering::Relaxed)
    }

    fn take_stages(&self) -> crate::profile::Stages {
        Metrics::take_stages(self)
    }

    fn profile(&self) -> &crate::profile::Profile {
        &self.profile
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
        // 上の層が渡さないときは 0 (= 無制限・数えていない) で出る
        assert!(json2.contains("\"max_conns\":0"));
        assert!(
            json2.contains(
                "\"live_threads\":0,\"idle_threads\":0,\"queued_jobs\":0,\"max_threads\":0"
            )
        );
    }

    /// 版は上の層から渡ったものがそのまま `/status` に出ること (T12.6)。
    #[test]
    fn the_version_from_the_upper_layer_lands_in_the_status_json() {
        let m = Metrics::new();
        let json = m.to_json_with_cache(
            None,
            StatusExtras {
                version: "0.1.0+deadbee",
                ..StatusExtras::default()
            },
        );
        assert!(json.contains("\"version\":\"0.1.0+deadbee\""), "{}", json);
        // 渡されなければ "unknown" (git の無い環境でビルドしたときと同じ見え方)
        assert!(
            m.to_json().contains("\"version\":\"unknown\""),
            "{}",
            m.to_json()
        );
    }

    /// 上限といまのスレッド数は上の層から渡ったものがそのまま出ること (T10.7)。
    #[test]
    fn capacity_from_the_upper_layer_lands_in_the_status_json() {
        let m = Metrics::new();
        let json = m.to_json_with_cache(
            None,
            StatusExtras {
                concurrency: Concurrency {
                    max_conns: 48,
                    max_threads: 256,
                    live_threads: 7,
                    idle_threads: 3,
                    queued_jobs: 2,
                },
                ..StatusExtras::default()
            },
        );
        assert!(json.contains("\"max_conns\":48"), "{}", json);
        assert!(
            json.contains(
                "\"live_threads\":7,\"idle_threads\":3,\"queued_jobs\":2,\"max_threads\":256"
            ),
            "{}",
            json
        );
    }

    #[test]
    fn clients_and_blocked_are_counted() {
        let m = Metrics::new();
        m.record_client(
            "10.0.0.1",
            HostOutcome::Hit,
            100,
            // 向き別 (T14.26)。ここでは合計と辻褄が合う値にしてある
            (30, 70),
            Some(Duration::from_millis(30)),
            Some("http://a.example:80"),
        );
        m.record_client(
            "10.0.0.1",
            HostOutcome::Blocked,
            0,
            (0, 0),
            None,
            Some("ads.example:443"),
        );
        m.record_client("10.0.0.2", HostOutcome::Bypass, 5, (5, 0), None, None);
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

    /// カーネルの RTT の 4 欄が `.rrd` の余白に入り、**古いファイルは 0 で読み戻る**こと (T14.5)。
    #[test]
    fn the_rtt_columns_fit_in_the_slot_and_old_records_read_back_as_zero() {
        let m = Metrics::new();
        m.record_host("connect://a:443", HostOutcome::Bypass, 10);
        m.record_client("10.0.0.1", HostOutcome::Bypass, 10, (0, 0), None, None);
        // 標本が無い間は「無い」(`/hosts` では `null`)
        let none = m.hosts_sorted()[0].1.clone();
        assert_eq!(none.rtt_samples, 0);
        assert_eq!(none.rtt_avg_ms(), None);
        assert!(stats_json(&none, false).contains("\"rtt_ms\":null,\"retrans\":0"));

        // 接続の終わりに 2 本ぶん (30.1 ms と 20.0 ms)
        m.record_host_rtt("connect://a:443", 30_100, 2);
        m.record_host_rtt("connect://a:443", 20_000, 0);
        // 知らないホストは作らない (数えられていない相手の行が生えない)
        m.record_host_rtt("connect://never-seen:443", 1_000, 0);
        // 読めなかった側 (0) は標本にしない
        m.record_client_rtt("10.0.0.1", 0, 0);
        m.record_client_rtt("10.0.0.1", 48_300, 0);
        assert_eq!(m.hosts_sorted().len(), 1, "行は増えない");

        let s = m.hosts_sorted()[0].1.clone();
        assert_eq!(s.rtt_samples, 2);
        assert_eq!(s.rtt_us_sum, 50_100);
        assert_eq!(s.rtt_us_min, 20_000);
        assert_eq!(s.retrans, 2);
        assert_eq!(s.rtt_avg_ms(), Some(25.05));
        assert_eq!(s.rtt_min_ms(), Some(20.0));
        assert!(
            stats_json(&s, false)
                .contains("\"rtt_ms\":{\"avg\":25.050,\"min\":20.000,\"samples\":2},\"retrans\":2")
        );
        // 全体の合計 (`/metrics`)。添字は [クライアント側, オリジン側]
        let totals = m.rtt_totals();
        assert_eq!(totals[crate::recent::CLIENT_SIDE], (48_300, 1));
        assert_eq!(totals[crate::recent::ORIGIN_SIDE], (50_100, 2));

        // `.rrd` の 1 スロット: 名前 128 B + 55 項目 × 8 B = 568 B (T14.26 の 2 欄まで)。
        // 版 3 (T14.14) でペイロードが 636 B になったので**余白は 68 B**
        // (版 2 では 4 B しか残っていなかった)。欄を足すとここが減る: 64 B を割ったら
        // 「予備を使い切りかけている」ので、版を上げる算段をすること
        let enc = s.encode("connect://a:443");
        assert_eq!(enc.len(), 128 + 55 * 8);
        let spare = crate::rrd::STATS_RECORD - 4 - enc.len();
        assert!(
            (32..=68).contains(&spare),
            "スロットの余白が {} B (版 3 で足した 64 B を使い切りかけている)",
            spare
        );
        assert_eq!(HostStats::decode(&enc).unwrap().1, s);

        // T14.5 より前に書かれたレコード (末尾 6 欄が無い) は 0 で読み戻る
        let old = &enc[..enc.len() - 48];
        let (name, back) = HostStats::decode(old).unwrap();
        assert_eq!(name, "connect://a:443");
        assert_eq!(
            (
                back.rtt_us_sum,
                back.rtt_us_min,
                back.rtt_samples,
                back.retrans
            ),
            (0, 0, 0, 0)
        );
        assert_eq!(back.requests, s.requests, "前の欄はそのまま読める");
    }

    /// 向き別のバイトが `.rrd` の残りの余白に入り、**古いファイルは 0 で読み戻る**こと (T14.26)。
    ///
    /// `bytes` (合計) は今までどおりで、新しいのは「その内訳」だけ。
    #[test]
    fn the_direction_columns_fit_in_the_slot_and_old_records_read_back_as_zero() {
        let m = Metrics::new();
        // CONNECT 1 本: 1 KiB 上げて 2 KiB 下ろした (合計 3 KiB)
        m.record_host_detail(
            "connect://a:443",
            HostOutcome::Bypass,
            3072,
            Some(Duration::from_millis(12)),
            Detail {
                bytes_in: 1024,
                bytes_out: 2048,
                ..Detail::default()
            },
        );
        m.record_client(
            "10.0.0.1",
            HostOutcome::Bypass,
            3072,
            (1024, 2048),
            None,
            None,
        );
        // もう 1 本 (足し込まれること)
        m.record_host_detail(
            "connect://a:443",
            HostOutcome::Bypass,
            30,
            None,
            Detail {
                bytes_in: 10,
                bytes_out: 20,
                ..Detail::default()
            },
        );

        let s = m.hosts_sorted()[0].1.clone();
        assert_eq!(s.bytes, 3102, "合計は今までどおり");
        assert_eq!(s.bytes_in, 1034);
        assert_eq!(s.bytes_out, 2068);
        assert_eq!(
            s.bytes_in + s.bytes_out,
            s.bytes,
            "CONNECT は合計と一致する"
        );
        assert!(
            stats_json(&s, false).contains("\"bytes\":3102,\"bytes_in\":1034,\"bytes_out\":2068"),
            "{}",
            stats_json(&s, false)
        );
        // 接続元別にも同じ 2 欄が出る (`/status` の `clients[]` と `/clients`)
        let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
        assert_eq!((c.stats.bytes_in, c.stats.bytes_out), (1024, 2048));
        assert!(
            c.to_json("10.0.0.1")
                .contains("\"bytes_in\":1024,\"bytes_out\":2048"),
            "{}",
            c.to_json("10.0.0.1")
        );

        // `.rrd` の 1 スロット: 名前 128 B + 55 項目 × 8 B = 568 B。版 2 では余白 4 B だったが、
        // T14.14 の版 3 (640 B) で予備 68 B = 8 項目になった (T14.26 の時点の値は 4 B)
        let enc = s.encode("connect://a:443");
        assert_eq!(enc.len(), 128 + 55 * 8);
        assert_eq!(crate::rrd::STATS_RECORD - 4 - enc.len(), 68, "残りの余白");
        assert_eq!(HostStats::decode(&enc).unwrap().1, s);

        // T14.26 より前に書かれたレコード (末尾 2 欄が無い) は 0 で読み戻り、
        // それより前の欄 (T14.5 の RTT も含めて) はそのまま読める
        let (name, back) = HostStats::decode(&enc[..enc.len() - 16]).unwrap();
        assert_eq!(name, "connect://a:443");
        assert_eq!((back.bytes_in, back.bytes_out), (0, 0));
        assert_eq!(back.bytes, s.bytes, "合計は昔のファイルにも入っている");
        assert_eq!(back.requests, s.requests);
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
        // 24 段では 1 件ずつ別の区間に入る: p50 は 200 ms の入る (140, 210] の上端
        assert!(
            (s.quantile_ms(0.5) - 210.0).abs() < 1e-6,
            "{}",
            s.quantile_ms(0.5)
        );
        let p95 = s.quantile_ms(0.95);
        assert!(p95 > 5000.0 && p95 <= 9000.0, "{}", p95);
        assert_eq!(s.quantile_ms(1.0), 9000.0);
        assert_eq!(HostStats::default().quantile_ms(0.5), 0.0);
    }

    /// T12.4 (1) の受け入れ基準: **区間が 10 段だった頃は 257 ms が (250, 500] に全部入り、
    /// 補間しても観測した最大値で頭打ちになって p50 = p95 = max = 290 になっていた**。
    /// 24 段では 257 ms と 290 ms が (210, 315] に入り、区間内の補間が意味を持つ。
    #[test]
    fn the_deployed_shape_of_257ms_gets_a_real_median() {
        let m = Metrics::new();
        for _ in 0..90 {
            m.record_host_timed(
                "connect://www.dlsite.com:443",
                HostOutcome::Bypass,
                0,
                Duration::from_millis(257),
            );
        }
        for _ in 0..10 {
            m.record_host_timed(
                "connect://www.dlsite.com:443",
                HostOutcome::Bypass,
                0,
                Duration::from_millis(290),
            );
        }
        let (_, s) = &m.hosts_sorted()[0];
        let (p50, p95) = (s.quantile_ms(0.5), s.quantile_ms(0.95));
        assert!((250.0..=265.0).contains(&p50), "p50 {}", p50);
        assert!(p95 >= 280.0, "p95 {}", p95);
        assert!(p50 < p95, "p50 {} p95 {}", p50, p95);
    }

    /// 同じく T12.4 (1): AAAA の無いホスト (デプロイ先の p50 5.1 ms) が潰れないこと。
    #[test]
    fn a_five_millisecond_host_keeps_a_five_millisecond_median() {
        let m = Metrics::new();
        for _ in 0..100 {
            m.record_host_timed(
                "connect://discord.com:443",
                HostOutcome::Bypass,
                0,
                Duration::from_millis(5),
            );
        }
        let (_, s) = &m.hosts_sorted()[0];
        let p50 = s.quantile_ms(0.5);
        assert!((4.0..=6.0).contains(&p50), "p50 {}", p50);
    }

    /// 区間は 1 ms から 10 s まで単調増加で、公比はおよそ 1.5。
    #[test]
    fn the_bounds_are_a_geometric_ladder() {
        assert_eq!(LATENCY_BOUNDS_MS.len(), 24);
        assert_eq!(LATENCY_BOUNDS_MS[0], 1);
        assert_eq!(LATENCY_BOUNDS_MS[23], 10_000);
        for w in LATENCY_BOUNDS_MS.windows(2) {
            assert!(w[0] < w[1], "{:?}", w);
        }
        // 5 ms より上は公比 1.33〜1.55 に収まっている (下の端はもっと細かい)
        for w in LATENCY_BOUNDS_MS.windows(2).skip(4) {
            let r = w[1] as f64 / w[0] as f64;
            assert!((1.3..=1.6).contains(&r), "{:?} -> {}", w, r);
        }
    }

    /// `io::Error` から 8 つの原因へ畳めること (T12.4 (2))。
    #[test]
    fn io_errors_fold_into_eight_causes() {
        use std::io::{Error, ErrorKind};
        let c = |k: ErrorKind| ErrCause::from_io(&Error::new(k, "x"));
        assert_eq!(c(ErrorKind::ConnectionRefused), ErrCause::Refused);
        assert_eq!(c(ErrorKind::NetworkUnreachable), ErrCause::Unreachable);
        assert_eq!(c(ErrorKind::HostUnreachable), ErrCause::Unreachable);
        assert_eq!(c(ErrorKind::TimedOut), ErrCause::Timeout);
        assert_eq!(c(ErrorKind::ConnectionReset), ErrCause::Reset);
        assert_eq!(c(ErrorKind::BrokenPipe), ErrCause::Reset);
        assert_eq!(c(ErrorKind::NotFound), ErrCause::Dns);
        assert_eq!(c(ErrorKind::PermissionDenied), ErrCause::Other);
        // `getaddrinfo` の失敗は種別が付かないことがあるので文言も見る
        assert_eq!(
            ErrCause::from_io(&Error::other("failed to lookup address information: x")),
            ErrCause::Dns
        );
        assert_eq!(
            ErrCause::from_io(&Error::other("TLS handshake failed")),
            ErrCause::Tls
        );
        assert_eq!(ErrCause::Loop.name(), "loop");
        assert_eq!(ERR_CAUSE_NAMES.len(), ERR_CAUSES);
    }

    /// 内訳はホスト別の行と区間の合計の両方に乗る (T12.4 (2) / (3))。
    #[test]
    fn the_breakdown_lands_on_the_host_row_and_the_interval() {
        let m = Metrics::new();
        m.record_host_detail(
            "connect://a:443",
            HostOutcome::Bypass,
            0,
            Some(Duration::from_millis(257)),
            Detail {
                dns_ms: 12,
                dns_misses: 1,
                connect_ms: 245,
                family_v6: Some(false),
                cause: None,
                first_byte_ms: None,
                ..Detail::default()
            },
        );
        m.record_host_detail(
            "connect://a:443",
            HostOutcome::Error,
            0,
            Some(Duration::from_millis(300)),
            Detail {
                family_v6: Some(true),
                cause: Some(ErrCause::Refused),
                ..Detail::default()
            },
        );
        let (host, s) = &m.hosts_sorted()[0];
        assert_eq!(host, "connect://a:443");
        assert_eq!((s.dns_ms_sum, s.dns_misses, s.connect_ms_sum), (12, 1, 245));
        assert_eq!((s.v4_wins, s.v6_wins), (1, 1));
        assert_eq!(s.errors_by_cause[ErrCause::Refused as usize], 1);
        // `connect://` の鍵は CONNECT の窓へ入る
        let iv = m.totals();
        assert_eq!(iv.connect.count, 2);
        assert_eq!(iv.forward.count, 0);
        assert_eq!(iv.errors, 1);
        assert_eq!(iv.errors_by_cause[ErrCause::Refused as usize], 1);
        assert_eq!((iv.dns_misses, iv.dns_ms_sum), (1, 12));
        // 区間は読むと 0 に戻る
        assert_eq!(m.take_interval().connect.count, 2);
        assert_eq!(m.take_interval().connect.count, 0);
        assert_eq!(m.totals().connect.count, 2, "累計は残る");
        // forward は初バイトの値が窓に入る (応答全体の時間ではない)
        m.record_host_detail(
            "http://b:80",
            HostOutcome::Miss,
            0,
            Some(Duration::from_millis(900)),
            Detail {
                first_byte_ms: Some(7),
                ..Detail::default()
            },
        );
        let iv = m.take_interval();
        assert_eq!(iv.forward.count, 1);
        assert_eq!(iv.forward.ms_max, 7);
        // ホスト別の応答時間はこれまでどおり応答全体
        let b = m
            .hosts_sorted()
            .into_iter()
            .find(|(h, _)| h == "http://b:80")
            .unwrap()
            .1;
        assert_eq!(b.duration_ms_max, 900);
        // `/status` にはホスト別だけ内訳が出る (接続元別には出ない)
        let json = m.to_json();
        assert!(json.contains("\"v6_wins\":1"), "{}", json);
        assert!(json.contains("\"errors_by_cause\":["), "{}", json);
    }

    /// `/status?sort=` が上位を切り出す鍵だけを変えること (T13.3)。
    ///
    /// 鍵ごとに先頭が入れ替わり、**JSON の形は変わらない** (件数もキーもそのまま)。
    #[test]
    fn the_sort_key_only_changes_which_hosts_are_cut_out() {
        let m = Metrics::new();
        // 要求は多いが健全なホスト
        for _ in 0..10 {
            m.record_host_detail(
                "connect://busy:443",
                HostOutcome::Bypass,
                0,
                Some(Duration::from_millis(5)),
                Detail::default(),
            );
        }
        // 名前解決に時間を払っているホスト (要求は 2 件)
        for _ in 0..2 {
            m.record_host_detail(
                "connect://slow-dns:443",
                HostOutcome::Bypass,
                0,
                Some(Duration::from_millis(60)),
                Detail {
                    dns_ms: 50,
                    dns_misses: 1,
                    connect_ms: 10,
                    ..Detail::default()
                },
            );
        }
        // エラーだけのホスト (要求 1 件)
        m.record_host_detail(
            "connect://broken:443",
            HostOutcome::Error,
            0,
            None,
            Detail {
                cause: Some(ErrCause::Dns),
                dns_misses: 1,
                ..Detail::default()
            },
        );
        // 遠いホスト (1 件だけだが平均が飛び抜けて遅い)
        m.record_host_detail(
            "connect://far-away:443",
            HostOutcome::Bypass,
            0,
            Some(Duration::from_millis(900)),
            Detail {
                connect_ms: 900,
                ..Detail::default()
            },
        );
        let first = |sort| m.hosts_sorted_by(sort)[0].0.clone();
        assert_eq!(first(HostSort::Requests), "connect://busy:443");
        assert_eq!(first(HostSort::Errors), "connect://broken:443");
        assert_eq!(first(HostSort::Dns), "connect://slow-dns:443");
        assert_eq!(first(HostSort::Slow), "connect://far-away:443");
        // 知らない値と空は既定 (要求数順) に倒れる
        for v in ["", "requests", "REQUESTS", "errors?", "なにか"] {
            assert_eq!(
                m.hosts_sorted_by(HostSort::from_param(v))[0].0,
                "connect://busy:443",
                "{}",
                v
            );
        }
        // 同点は要求数 → 名前で崩すので、どの鍵でも順序は 1 つに決まる
        assert_eq!(
            m.hosts_sorted_by(HostSort::Errors),
            m.hosts_sorted_by(HostSort::Errors)
        );
        // JSON の形は鍵で変わらない (件数もキーも同じ。先頭のホストだけが違う)
        let of = |sort| {
            m.to_json_with_cache(
                None,
                StatusExtras {
                    sort,
                    ..StatusExtras::default()
                },
            )
        };
        let (a, b) = (of(HostSort::Requests), of(HostSort::Errors));
        // 数えるのは `hosts[]` の要素 (`{"host":` で始まる) だけ。`canary` にも
        // `host` の欄がある (T14.10) ので、鍵の名前だけで数えると 1 件多くなる
        assert_eq!(a.matches("{\"host\":").count(), 4);
        assert_eq!(
            a.matches("{\"host\":").count(),
            b.matches("{\"host\":").count()
        );
        assert_eq!(
            a.matches("\"errors_by_cause\":[").count(),
            b.matches("\"errors_by_cause\":[").count()
        );
        assert!(
            a.contains("\"hosts\":[{\"host\":\"connect://busy:443\""),
            "{}",
            a
        );
        assert!(
            b.contains("\"hosts\":[{\"host\":\"connect://broken:443\""),
            "{}",
            b
        );
    }

    /// `/status` の応答は上位 50 ホスト + 上位 50 接続元でも 64 KiB に収まること (T13.3)。
    ///
    /// 監視が 5 秒ごとに引く口なので、太らせない。長い名前 (RFC の上限に近い 200 バイト) を
    /// 並べた最悪に近い形で測る。
    #[test]
    fn the_status_json_stays_under_64_kib() {
        let m = Metrics::new();
        for i in 0..200 {
            let host = format!("connect://{}{:03}.example.net:443", "n".repeat(180), i);
            m.record_host_detail(
                &host,
                HostOutcome::Error,
                u64::MAX / 2,
                Some(Duration::from_millis(1234)),
                Detail {
                    dns_ms: 9999,
                    dns_misses: 7,
                    connect_ms: 8888,
                    family_v6: Some(true),
                    cause: Some(ErrCause::Dns),
                    first_byte_ms: None,
                    ..Detail::default()
                },
            );
            m.record_client(
                &format!("2001:db8:{:04x}:{:04x}::{:04x}", i, i, i),
                HostOutcome::Error,
                u64::MAX / 2,
                // 向き別も桁を振り切らせる (1 行の JSON を最悪にする。T14.26)
                (u64::MAX / 2, u64::MAX / 2),
                Some(Duration::from_millis(1234)),
                Some("connect://very-long-host-name.example.com:443"),
            );
        }
        for sort in [
            HostSort::Requests,
            HostSort::Errors,
            HostSort::Dns,
            HostSort::Slow,
        ] {
            let json = m.to_json_with_cache(
                None,
                StatusExtras {
                    sort,
                    ..StatusExtras::default()
                },
            );
            assert!(json.len() <= 64 * 1024, "{} バイト", json.len());
        }
    }

    /// 接続元の個票 (T14.7): `User-Agent`・宛先の種類・ポート・IP リテラル宛て。
    #[test]
    fn the_client_card_counts_agents_targets_ports_and_literals() {
        let m = Metrics::new();
        // `User-Agent` は接続の最初の要求で 1 回だけ渡る (前後の空白は落ちる)
        m.record_client_agent("10.0.0.1", " t147/1.0 ");
        for t in [
            "connect://example.com:443",
            "http://example.com:80",
            "192.0.2.7:8443",
        ] {
            m.record_client("10.0.0.1", HostOutcome::Bypass, 1, (0, 1), None, Some(t));
        }
        let v = m.clients_sorted_by(ClientSort::Requests);
        assert_eq!(v[0].0, "10.0.0.1");
        let c = &v[0].1;
        assert_eq!(c.stats.requests, 3);
        assert_eq!(c.agents, vec!["t147/1.0".to_string()]);
        assert_eq!(c.agent(), Some("t147/1.0"));
        assert_eq!(c.agents_dropped, 0);
        // 宛先は example.com と 192.0.2.7 の 2 種 (ポートが違っても同じホスト)
        assert_eq!(c.distinct_targets(), 2);
        assert!(!c.targets_capped);
        assert_eq!(c.literal_targets, 1);
        assert_eq!(c.nonstandard_ports, 1);
        // 同数のポートは番号の小さい順 (順序は 1 つに決まる)
        assert_eq!(c.ports_sorted(), vec![(80, 1), (443, 1), (8443, 1)]);
        assert_eq!(c.ports_other, 0);
        assert!(c.first_seen > 1_700_000_000);
        // 宛先の指紋は大小を同一視する
        m.record_client(
            "10.0.0.1",
            HostOutcome::Bypass,
            0,
            (0, 0),
            None,
            Some("EXAMPLE.COM:443"),
        );
        let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
        assert_eq!(c.distinct_targets(), 2);
        assert_eq!(c.ports_sorted()[0], (443, 2));
        // `/status` の欄と `/clients` の行
        assert!(
            c.status_json().starts_with(",\"first_seen\":"),
            "{}",
            c.status_json()
        );
        let row = c.to_json("10.0.0.1");
        assert!(
            row.starts_with("{\"client\":\"10.0.0.1\",\"requests\":4"),
            "{}",
            row
        );
        assert!(row.contains("\"agent\":\"t147/1.0\""), "{}", row);
        assert!(row.contains("\"agents\":[\"t147/1.0\"]"), "{}", row);
        assert!(
            row.contains("\"ports\":[{\"port\":443,\"requests\":2}"),
            "{}",
            row
        );
    }

    /// `User-Agent` は 4 種まで覚え、5 種類目からは数えるだけ。長いものは 128 B で切る。
    #[test]
    fn the_client_card_keeps_four_agents() {
        let m = Metrics::new();
        for i in 0..6 {
            m.record_client_agent("10.0.0.1", &format!("ua/{}", i));
        }
        m.record_client("10.0.0.1", HostOutcome::Bypass, 0, (0, 0), None, None);
        let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
        assert_eq!(c.agents.len(), MAX_CLIENT_AGENTS);
        assert_eq!(c.agents_dropped, 2);
        // 覚えた中で最後に見たものが `/status` に出る 1 つ
        assert_eq!(c.agent(), Some("ua/3"));
        m.record_client_agent("10.0.0.1", "ua/0");
        let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
        assert_eq!(c.agent(), Some("ua/0"));
        assert_eq!(c.agents.len(), MAX_CLIENT_AGENTS, "並びは変えない");

        // 同じ長い `User-Agent` は 1 種のまま (切った形で見比べるため)
        let long = "x".repeat(300);
        let m2 = Metrics::new();
        m2.record_client_agent("10.0.0.2", &long);
        m2.record_client_agent("10.0.0.2", &long);
        m2.record_client("10.0.0.2", HostOutcome::Bypass, 0, (0, 0), None, None);
        let c2 = m2.clients_sorted_by(ClientSort::Requests).remove(0).1;
        assert_eq!(c2.agents.len(), 1);
        assert_eq!(c2.agents_dropped, 0);
        assert!(
            c2.agents[0].len() <= MAX_AGENT_BYTES,
            "{}",
            c2.agents[0].len()
        );
    }

    /// 宛先は 256 種、ポートは 8 種で頭打ち (あふれた分は旗と `ports_other`)。
    #[test]
    fn the_client_card_caps_targets_and_ports() {
        let m = Metrics::new();
        for i in 0..(MAX_CLIENT_TARGETS + 10) {
            m.record_client(
                "10.0.0.1",
                HostOutcome::Bypass,
                0,
                (0, 0),
                None,
                Some(&format!("h{}.example:443", i)),
            );
        }
        let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
        assert_eq!(c.distinct_targets(), MAX_CLIENT_TARGETS);
        assert!(c.targets_capped);
        assert_eq!(
            c.ports_sorted(),
            vec![(443, MAX_CLIENT_TARGETS as u64 + 10)]
        );

        for p in 0..10u16 {
            m.record_client(
                "10.0.0.2",
                HostOutcome::Bypass,
                0,
                (0, 0),
                None,
                Some(&format!("a.example:{}", 1000 + p)),
            );
        }
        let c2 = m
            .clients_sorted_by(ClientSort::Requests)
            .into_iter()
            .find(|(k, _)| k == "10.0.0.2")
            .expect("10.0.0.2 の行")
            .1;
        assert_eq!(c2.ports.len(), MAX_CLIENT_PORTS);
        assert_eq!(c2.ports_other, 2);
        assert_eq!(c2.nonstandard_ports, 10);
        assert_eq!(c2.distinct_targets(), 1);
    }

    /// 接続元ごとの上限で断った数は、全体の合計と個票の両方に残る (T14.13)。
    #[test]
    fn rejections_are_counted_for_the_total_and_for_the_client() {
        let m = Metrics::new();
        m.record_client_rejected("10.0.0.1");
        m.record_client_rejected("10.0.0.1");
        m.record_client_rejected("10.0.0.2");
        assert_eq!(m.rejected_per_client.load(Ordering::Relaxed), 3);

        // 要求を 1 本も通していない接続元でも `/clients` に出る (断られただけの相手)
        let all = m.clients_sorted_by(ClientSort::Requests);
        let one = all
            .iter()
            .find(|(k, _)| k == "10.0.0.1")
            .expect("10.0.0.1 の行")
            .1
            .to_json("10.0.0.1");
        assert!(one.contains("\"rejected\":2"), "{}", one);
        assert!(m.to_json().contains("\"rejected_per_client\":3"));
    }

    /// `?sort=` の鍵ごとに先頭が入れ替わること (T14.7)。
    #[test]
    fn clients_can_be_sorted_by_requests_recent_targets_or_literals() {
        // (1) 要求数と「最後に見た時刻」は向きが逆になるように読み戻す
        let m = Metrics::new();
        m.restore(
            vec![],
            vec![
                (
                    "10.0.0.1".to_string(),
                    HostStats {
                        requests: 10,
                        last_seen: 100,
                        ..HostStats::default()
                    },
                ),
                (
                    "10.0.0.2".to_string(),
                    HostStats {
                        requests: 5,
                        last_seen: 900,
                        ..HostStats::default()
                    },
                ),
            ],
        );
        let first = |s: ClientSort| m.clients_sorted_by(s).remove(0).0;
        assert_eq!(first(ClientSort::Requests), "10.0.0.1");
        assert_eq!(first(ClientSort::Recent), "10.0.0.2");
        // 読み戻した接続元は「いつから居るか」が分からない
        assert_eq!(m.clients_sorted_by(ClientSort::Requests)[0].1.first_seen, 0);

        // (2) 宛先の種類と IP リテラル宛ても向きを逆にする
        let m2 = Metrics::new();
        for i in 0..3 {
            m2.record_client(
                "10.0.0.3",
                HostOutcome::Bypass,
                0,
                (0, 0),
                None,
                Some(&format!("h{}.example:443", i)),
            );
        }
        for _ in 0..5 {
            m2.record_client(
                "10.0.0.4",
                HostOutcome::Bypass,
                0,
                (0, 0),
                None,
                Some("192.0.2.9:443"),
            );
        }
        let first2 = |s: ClientSort| m2.clients_sorted_by(s).remove(0).0;
        assert_eq!(first2(ClientSort::Requests), "10.0.0.4");
        assert_eq!(first2(ClientSort::Targets), "10.0.0.3");
        assert_eq!(first2(ClientSort::Literal), "10.0.0.4");
        // 知らない値は既定に倒す
        assert_eq!(ClientSort::from_param("nonsense"), ClientSort::Requests);
        assert_eq!(ClientSort::from_param("targets"), ClientSort::Targets);
        assert_eq!(ClientSort::from_param("literal").name(), "literal");
    }

    /// 宛先の鍵は呼び出し側でまちまちなので、ホストとポートの取り出しを 1 か所で見る。
    #[test]
    fn the_target_key_is_split_the_same_way_everywhere() {
        assert_eq!(
            target_parts("connect://example.com:443"),
            ("example.com", Some(443))
        );
        assert_eq!(
            target_parts("http://example.com:80"),
            ("example.com", Some(80))
        );
        assert_eq!(
            target_parts("http://example.com:80/a/b"),
            ("example.com", Some(80))
        );
        assert_eq!(target_parts("example.com"), ("example.com", None));
        assert_eq!(
            target_parts("[2001:db8::1]:8443"),
            ("2001:db8::1", Some(8443))
        );
        assert_eq!(target_parts("2001:db8::1"), ("2001:db8::1", None));
        // ポートを持たない宛先はポートを数えない (ホストの種類だけ)
        let m = Metrics::new();
        m.record_client(
            "10.0.0.1",
            HostOutcome::Blocked,
            0,
            (0, 0),
            None,
            Some("ads.example"),
        );
        let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
        assert_eq!(c.distinct_targets(), 1);
        assert!(c.ports.is_empty());
        assert_eq!(c.nonstandard_ports, 0);
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
