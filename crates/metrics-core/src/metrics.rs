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

/// オリジン側の失敗の WARN を絞る理由の数 ([`ErrCause`] の 8 つ + 名前解決の内訳 2 つ。T.29.15)。
pub const ORIGIN_WARN_REASONS: usize = ERR_CAUSES + DNS_CAUSES;

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

// `/slo` の `error_rate` が外す「1 つの宛先に偏ったエラー」の数 (T.30.4)
mod web_dest;

pub use web_dest::DestHour;

// `impl Metrics` は仕事ごとに割ってある (T.34.26)。ここに残すのは構造体・`new`・原子の増減と
// `impl profile::Source` だけ。**行を動かしただけ**で、公開 API は変えていない。
// ホスト別統計の表と区間の合計 ([`Interval`])、その問い合わせ
mod host_table;
// 要求ごとの記録 (`record_host*`)
mod record;
// 個票のリング (`/errors` `/recent` `/bursts`) と、断った要求・オリジン側の失敗の WARN
mod records;
// 接続元 IP ごとの個票 (`/clients`)
mod client_table;
// 内部エンドポイントを引いた接続元 (`/readers`)
mod readers;

use host_table::HostTable;
pub use host_table::Interval;

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
    /// オリジン側の失敗 (502 / 時間切れ) の WARN を**理由ごとに 1 分に 1 回**へ絞る門 (T.29.15)。
    /// 並びは [`ErrCause`] の 8 つ + 名前解決の内訳 2 つ ([`Metrics::origin_warn`])。本数は
    /// `/errors`・`/hosts`・`errors_by_cause` が持つので、ログには 1 分に 1 行と抑えた本数だけを出す
    pub origin_warn: [crate::log::Throttle; ORIGIN_WARN_REASONS],
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
    /// 1 要求目の 1 バイト目も来ないまま `PROXY_HEADER_TIMEOUT_SECS` を過ぎ、408 を返さずに黙って
    /// 閉じた接続の数 (`/status` の `silent_closes`。T.33.0)。ブラウザやクライアントの先張りの接続
    /// (応答を読まない) が来るたびに増える。`rejected_requests` にも `/errors` にも入れない
    pub silent_closes: AtomicU64,
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
            origin_warn: [const { crate::log::Throttle::new() }; ORIGIN_WARN_REASONS],
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
            silent_closes: AtomicU64::new(0),
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

#[cfg(test)]
mod window_tests;

#[cfg(test)]
mod status_tests;

#[cfg(test)]
mod client_tests;

#[cfg(test)]
mod dest_tests;

#[cfg(test)]
mod record_tests;
