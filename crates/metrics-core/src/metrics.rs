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
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::cache::Cache;

// 接続元の個票と、内部エンドポイントを引いた接続元は下の層
// (`proxy-metrics-window`) に置いてある (T14.55)。**今までの名前でここから引ける**
// (`crate::metrics::ClientStats` / `Reader` …)。
pub use crate::clients::{
    ClientStats, MAX_READER_PATH, MAX_READERS, Reader, STATUS_READERS, fingerprint, target_parts,
};

// `/status` の組み立ては `metrics/status.rs` に割った (T.28.27)。**今までの名前でここから引ける**
// (`crate::metrics::set_canary_status` / `CANARY_STATUS_UNSET`)。
mod status;

pub use status::{CANARY_STATUS_UNSET, set_canary_status};

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
    /// **前回の標本からの同時接続の真の山** (`/history` の `active_peak`。T15.0 (10))。
    ///
    /// `active_max` は 5 秒ごとの瞬間値の最大なので、5 秒より短い山を取りこぼす
    /// (実測で取りこぼしが分かっていたので、この列を足した)。ここは
    /// [`Metrics::inc_active_conn`] が数え上げるたびに見るので、**5 秒より短い山も拾う**。
    ///
    /// 厳密に「1 本も落とさない」ではない: 5 秒に 1 回の読み出し
    /// ([`Metrics::take_active_peak`]) の「今の本数を読む」と「山を取り出して書き戻す」の
    /// 間 (命令 2 つぶん) に接続が 1 本増えると、書き戻す値がその 1 本を含まない。
    /// 読み出し側が `max(今の本数)` を取って補正しているので、**張ったままの接続では
    /// 下振れしない**が、その 1 本が次の読み出しまでに閉じた場合だけ 1 本ぶん低く出る。
    ///
    /// **要求の経路に足す原子操作は読み 1 回と比較 1 回だけ** (越えたときだけ `fetch_max`)。
    /// T15.0 の決まり「要求の経路は増やさない」の**例外その 2** (もう 1 つは
    /// `dns` の `warm_requests`)。システムコールも確保も増えない。
    /// 読み出しは [`Metrics::take_active_peak`] (5 秒に 1 回)
    active_peak: AtomicUsize,
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
    /// 断った要求の WARN を理由ごとに 1 分に 1 回へ絞る門 (T.28.9)。本数は上の
    /// `rejected_requests` が持つので、ログには 1 分に 1 行と抑えた本数だけを出す
    pub rejected_warn: [crate::log::Throttle; BAD_REQUEST_REASONS],
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
    /// 起動時にリングの置き場を確保して触ったか (`/status` の `memory.rings_touched`。T17.8)。
    /// 立てるのは [`Metrics::prefault_rings`] だけ
    rings_touched: AtomicBool,
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
            active_peak: AtomicUsize::new(0),
            parked_connections: AtomicUsize::new(0),
            parked_tunnels: AtomicUsize::new(0),
            park_watcher_alive: AtomicBool::new(false),
            rejected_overload: AtomicU64::new(0),
            evicted_idle: AtomicU64::new(0),
            rejected_client_acl: AtomicU64::new(0),
            rejected_per_client: AtomicU64::new(0),
            rejected_requests: [const { AtomicU64::new(0) }; BAD_REQUEST_REASONS],
            rejected_warn: [const { crate::log::Throttle::new() }; BAD_REQUEST_REASONS],
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
            rings_touched: AtomicBool::new(false),
            hosts: Mutex::new(HostTable::default()),
            clients: Mutex::new(HashMap::new()),
            readers: Mutex::new(HashMap::new()),
        }
    }

    /// 記録のリングの置き場を満杯のぶん確保して、1 ページに 1 バイトずつ触る (T17.8)。
    ///
    /// **起動時に 1 回だけ、既定のプロファイルの Linux から**呼ぶ (`--lite` は呼ばない)。
    /// リングは満ちるまで伸びるので、呼ばなければ RSS は起動から 1 日かけて天井まで上がる。
    /// 先に触っておけば起動直後から天井の近くに居て、そこから増えたらそのまま信号になる。
    ///
    /// - `/profile` の窓は必ず触る (`--lite` 以外では `profile` スレッドが 5 秒ごとに積む)
    /// - `history`: `/history` の標本と `closed` / `transfer` の窓 (履歴スレッドが回らない
    ///   `PROXY_STATS_PERSIST=off` では `false` を渡す。1 本も積まないリングの置き場は持たない)
    /// - `/recent` `/errors` `/bursts` `/events` `/log` の固定部は `PROXY_RECORDS=off` なら
    ///   触らない (1 件も書かないリングの置き場を持たない)。`/trace` は追跡する接続元を
    ///   決めたときだけ書くので触らない。`hostseries` と `quantiles` は最初の 1 件で
    ///   1 回だけ確保する固定長 (合わせて 0.2 MB) なので触らない
    ///
    /// 触っても件数は増えないので `memory.rings_used` は 1 バイトも変わらない。
    /// 返すのは触ったバイト数 (起動のログに出す)。
    pub fn prefault_rings(&self, history: bool) -> usize {
        let mut bytes = self.profile.prefault();
        if history {
            bytes += self.history.prefault();
        }
        if crate::records::recording() {
            bytes += self.closed.prefault()
                + self.errors.prefault()
                + self.bursts.prefault()
                + crate::events::prefault()
                + crate::log::prefault();
        }
        self.rings_touched.store(true, Ordering::Relaxed);
        bytes
    }

    /// [`Metrics::prefault_rings`] が済んでいるか (`/status` の `memory.rings_touched`)。
    pub fn rings_touched(&self) -> bool {
        self.rings_touched.load(Ordering::Relaxed)
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
        // 名前解決の失敗は内訳 (`nxdomain` / `dns_temp`) で個票に出す (T25.0)。
        // `detail.cause` が `Dns` でも `dns_cause` が無い (古い経路・canary) ときは
        // 今までどおり `dns` のまま
        let cause = match (detail.cause, detail.dns_cause) {
            (Some(ErrCause::Dns), Some(d)) => crate::recent::EntryCause::ErrorDns(d),
            (Some(c), _) => crate::recent::EntryCause::Error(c),
            (None, _) => return,
        };
        self.errors.push(crate::recent::ErrorEntry::new(
            crate::recent::EntryKind::from_connect(connect),
            target,
            client,
            status,
            cause,
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
    ///
    /// **戻り値は WARN を 1 行出してよいか** (T.28.9)。不正な要求の本数は攻撃者が決めるので、
    /// 理由ごとに 1 分に 1 回 (`log::WARN_EVERY_SECS`) だけ `Some` を返し、中身は前の行から
    /// 抑えた本数 (行の終わりにそのまま `{}` で付ける)。呼ぶ側は
    /// `if let Some(more) = metrics.record_bad_request(..) { log_warn!(.., "…{}", more) }` の形で出す
    #[must_use = "WARN を出すかどうかを返す (T.28.9)"]
    pub fn record_bad_request(
        &self,
        reason: BadRequestReason,
        client: &str,
        status: u16,
    ) -> Option<crate::log::Skipped> {
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
        self.rejected_warn[reason as usize].pass(crate::log::WARN_EVERY_SECS)
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
        // **エラーで終わった 1 件は、全体の確立 / 初バイトの窓・直近の標本・`wait`・段階の窓に
        // 入れない** (T20.4)。失敗した CONNECT が渡してくるのは「失敗するまでの時間」で、
        // 繋がらない宛先への再試行が続くと (2026-10-08 の実測: 接続の 17.2% が 1 本 約 1.3 秒)
        // `recent_quantiles.connect` の p90 も `/history` の確立の p95 も秒になって、
        // 速さの物差しにならなかった。これで `connects` / `forwards` は「エラーにならなかった
        // 本数」になり、`/slo` の `attempts = connects + forwards + errors` が 2 重に数えない。
        // 失敗は `errors` / `errors_by_cause` / `/errors` の個票 / **ホスト別の行**
        // (下の `stats.count`。「その相手に掛かった時間」なので失敗も入れたまま) に残る。
        // 比較はここで 1 回だけ (下の `errors` の分岐が元から見ていたものを使い回す)
        let failed = outcome == HostOutcome::Error;
        // 全体の合計も同じ鍵の内側で足す (原子操作を増やさない)
        for iv in [&mut hosts.total, &mut hosts.interval] {
            iv.dns_misses += detail.dns_misses;
            iv.dns_ms_sum += detail.dns_ms;
            if let Some(c) = detail.cause {
                iv.errors_by_cause[c as usize] += 1;
            }
            // 成功の経路の分岐は今までと同じ数 (`errors` を足すかどうかの 1 回を、
            // 窓に入れるかどうかと兼ねている)
            if failed {
                iv.errors += 1;
            } else if let Some(ms) = ms {
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
        // 原子操作も鍵の取り直しも増えない。**`failed` は最後に見る** (`--lite` は
        // `profile::on()` で抜けるので、そこまでの命令は今までと同じ。T20.4)
        if let Some(d) = took
            && counted
            && crate::profile::on()
            && !failed
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
                // **利用者が待つ時間** = `queue + client_read + dns + connect` (T15.0 (2))。
                // 4 つがそろうのはここだけ。`--lite` はこの分岐に入らない
                let total_ms = ms.unwrap_or(0);
                // `connect_ms` は「全体 − 窓の内側の名前解決」なので、差が窓の内側のぶん
                let dns_inside = total_ms.saturating_sub(detail.connect_ms);
                let head_ms = (detail.stages.queue as u64 + detail.stages.client_read as u64)
                    .saturating_add(detail.dns_ms)
                    .saturating_sub(dns_inside);
                // 窓 (ms) は素直に 4 段の和
                let wait_ms = head_ms.saturating_add(total_ms);
                // 環は **us のまま**: 確立の段だけ `d` の us を使う (ms に丸めると
                // 1 ms 未満の確立が 0 に潰れて `connect` の環と比べられなくなる)。
                // これで標本ごとに必ず `wait ≥ connect` になる
                let wait_us = head_ms
                    .saturating_mul(1_000)
                    .saturating_add(us as u64)
                    .min(u32::MAX as u64) as u32;
                hosts.quantiles.wait.observe(wait_us, now);
                hosts.total.wait.observe(wait_ms);
                hosts.interval.wait.observe(wait_ms);
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
                hosts
                    .series
                    .add(slot, ms.unwrap_or(0), detail.dns_ms, failed);
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
    /// **呼ぶのは接続の最初の要求のときだけ** (`crates/server/src/lib.rs`)。要求ごとに見ると
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

    /// `hosts[]` の通算の始まり = いちばん古い `last_seen` (`/status` の `restored_since`)。
    /// `0` は「表が空」。
    ///
    /// **[`Self::hosts_sorted_by`] を通さない**のが大事: `?hosts=0` の軽い口は 50 行を
    /// 組まないので、ここも写しも並べ替えも作らずに表を 1 回舐めるだけにする
    /// (値は同じ。T25.1 段 2)。
    fn restored_since(&self) -> u64 {
        let hosts = self.hosts.locked();
        hosts
            .map
            .values()
            .map(|s| s.last_seen)
            .filter(|&t| t > 0)
            .min()
            .unwrap_or(0)
    }

    pub fn inc_requests(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_active_conn(&self) {
        // 戻り + 1 = いまの本数。5 秒ごとの瞬間値では取りこぼす短い山を、ここで拾う
        // (T15.0 (10))。**足すのは原子の読み 1 回と比較 1 回**で、越えたときだけ書く
        let now = self.active_connections.fetch_add(1, Ordering::Relaxed) + 1;
        if now > self.active_peak.load(Ordering::Relaxed) {
            self.active_peak.fetch_max(now, Ordering::Relaxed);
        }
    }

    pub fn dec_active_conn(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    /// 前回からの同時接続の山を読み、**いまの本数に**戻す (`/history` の `active_peak`)。
    ///
    /// 0 ではなく今の本数に戻すのは、次の区間の山が「いま張っている本数」より小さく
    /// 出ないようにするため (山は「その区間に何本まで居たか」であって
    /// 「その区間に何本増えたか」ではない)。**呼ぶのは 5 秒の標本のときだけ**。
    ///
    /// 読みと `swap` の間に接続が増えると、書き戻す `now` がその 1 本を含まない
    /// ([`Metrics::active_peak`] の但し書き)。**戻り値だけは `now` で下から押さえて**
    /// おくと、張ったままの接続が次の区間で 1 本ぶん消えることはない。
    pub fn take_active_peak(&self) -> usize {
        let now = self.active_connections.load(Ordering::Relaxed);
        let peak = self.active_peak.swap(now, Ordering::Relaxed);
        peak.max(now)
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
mod tests;

#[cfg(test)]
mod latency_tests;
