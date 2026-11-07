//! 試験用プロキシをこのプロセスの中で起こす口と、試験用の設定。
//!
//! # 時計に頼る試験の一覧 (T35.1)
//!
//! キャッシュの「いま」(鮮度・`Age`・延命) は `Cache` に注入した時計 (`proxy_base::clock::Clock`)
//! から読む。TTL の切れを待つ試験は [`start_test_proxy_clocked`] に [`ManualClock`] を渡し、
//! `thread::sleep` の代わりに `clock.advance(..)` で時間を進める。
//!
//! 移したもの (実時間の待ち 1.2〜3 秒 → 0):
//! - `cache_revalidate_test`: grace の REFRESHING・裏の再検証の置き場・置き場が満杯で捨てる
//! - `revalidate_4xx_test` (3 本)・`revalidate_cookie_test` (裏の 304)・
//!   `revalidate_etag_mismatch_test` (裏の 304)・`revalidate_headers_test` (裏の 304・`Age` の 3 本)・
//!   `range_revalidate_test`
//!
//! 実時間のまま残すもの (時計を偽っても、待っているのはカーネルか相手のスレッド):
//! - `header_timeout_test`・`min_transfer_test`・`stall_test`: 締め切りは `SO_RCVTIMEO`/`poll`
//!   の待ちと組 (読む `Instant` だけ偽っても `read` は返らない)
//! - `idle_sleep_test`: 監視スレッドが実際に眠り続けることそのものを見る
//! - `canary_test`: 周期で起きる見張りのスレッドが繋がない (起きる回数) ことを見る
//! - `cache_revalidate_test` の遅いオリジン (`stale_wait`)・`cache_coalesce_test`: オリジンの
//!   応答の遅れ (相手のスレッドの `sleep`) と、合流の `Condvar` の待ち

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rust_http_proxy::Upstream;
use rust_http_proxy::cache::{Cache, CacheConfig, MIB};
/// TTL の切れを手で作る時計 (T35.1)。[`start_test_proxy_clocked`] に渡す
pub use rust_http_proxy::clock::ManualClock;
use rust_http_proxy::clock::{self, SharedClock};
use rust_http_proxy::config::Config;
use rust_http_proxy::metrics::Metrics;
use rust_http_proxy::pool::Pool;
use rust_http_proxy::tls::TlsClient;

pub fn start_test_proxy(config: Config) -> u16 {
    start_test_proxy_with_cache(config, CacheConfig::disabled())
}

pub fn start_test_proxy_with_cache(config: Config, cache_cfg: CacheConfig) -> u16 {
    start_test_proxy_full(config, cache_cfg, None)
}

/// 接続スレッドの置き場も返す版 (スレッドの上限を見るテスト用)。
pub fn start_test_proxy_with_workers(
    config: Config,
    cache_cfg: CacheConfig,
) -> (u16, Arc<rust_http_proxy::workers::Workers>) {
    let (port, workers, _) = start_test_proxy_parts(config, cache_cfg, None);
    (port, workers)
}

/// 指標も返す版 (上限に当たっている最中を見るテスト用)。
///
/// **上限のテストは `/status` で待ってはいけない**: `/status` 自体が 1 本の接続なので、
/// 上限に当たっていると「暇なトンネルを 1 本閉じて受ける」(T13.2) が動いて、見たかった
/// 状態を測るその行為が状態を変えてしまう。指標を直に読めば接続を増やさずに待てる。
pub fn start_test_proxy_with_metrics(config: Config) -> (u16, Arc<Metrics>) {
    let (port, _, metrics) = start_test_proxy_parts(config, CacheConfig::disabled(), None);
    (port, metrics)
}

/// `ca_file` を渡すと、その証明書だけを信頼する TLS クライアント付きで起動する。
pub fn start_test_proxy_full(
    config: Config,
    cache_cfg: CacheConfig,
    ca_file: Option<std::path::PathBuf>,
) -> u16 {
    start_test_proxy_parts(config, cache_cfg, ca_file).0
}

/// キャッシュの時計を注入して起動する版 (T35.1)。接続スレッドの置き場も返す。
///
/// `clock` に `ManualClock` を渡すと、キャッシュの鮮度・`Age`・延命・掃除が読む「いま」は
/// その時計だけになる。TTL の切れを `thread::sleep` で待たず、`clock.advance(..)` で作れる。
/// 時計はこのプロキシの `Cache` だけのもの (大域ではない) なので、同じバイナリで並んで走る
/// 隣の試験には漏れない。
///
/// **実時間のまま残るもの**: 締め切り・最低転送速度・合流の待ち (`Instant` とカーネルの待ち)。
/// これらに頼る試験 (header_timeout・min_transfer・stall・idle_sleep・canary・遅いオリジンの
/// `stale_wait`) は実時間で待つ (理由は `proxy_base::clock` の doc)。
pub fn start_test_proxy_clocked(
    config: Config,
    cache_cfg: CacheConfig,
    clock: SharedClock,
) -> (u16, Arc<rust_http_proxy::workers::Workers>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let cfg = Arc::new(config);
    let first = Arc::clone(&cfg);
    let (port, workers, _) = wire_and_serve(
        listener,
        &first,
        move || Arc::clone(&cfg),
        cache_cfg,
        None,
        clock,
    );
    (port, workers)
}

/// `serve` を呼ぶ前に `early` 本の接続の握手を済ませておく版 (起動直後の接続のテスト用。T28.30)。
///
/// 握手は待ち受けの accept 待ちの列に載ったまま、受け付けのループが待ち受けに
/// オプションを当てるより前に終わっている (= 待ち受けの設定を継承していない)。
#[allow(dead_code)]
pub fn start_test_proxy_with_early(config: Config, early: usize) -> (u16, Vec<TcpStream>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let conns = (0..early)
        .map(|_| TcpStream::connect(("127.0.0.1", port)).unwrap())
        .collect();
    start_test_proxy_on(listener, config, CacheConfig::disabled(), None);
    (port, conns)
}

fn start_test_proxy_parts(
    config: Config,
    cache_cfg: CacheConfig,
    ca_file: Option<std::path::PathBuf>,
) -> (u16, Arc<rust_http_proxy::workers::Workers>, Arc<Metrics>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    start_test_proxy_on(listener, config, cache_cfg, ca_file)
}

fn start_test_proxy_on(
    listener: TcpListener,
    config: Config,
    cache_cfg: CacheConfig,
    ca_file: Option<std::path::PathBuf>,
) -> (u16, Arc<rust_http_proxy::workers::Workers>, Arc<Metrics>) {
    let cfg = Arc::new(config);
    let first = Arc::clone(&cfg);
    wire_and_serve(
        listener,
        &first,
        move || Arc::clone(&cfg),
        cache_cfg,
        ca_file,
        clock::system(),
    )
}

/// 試験用プロキシの配線 (T34.105 で 1 本にした)。`cfg` は起動のときの設定で、
/// 接続ごとの設定は `config_of` から引く (固定の設定も、差し替えられる設定も同じ配線を通る)。
///
/// 前は差し替えられる版が別に配線を書いていて、`attach_origin_rtt`・`conns.set_enabled`・
/// `sni::set_peek`・`TlsClient::deferred` が無かった (本番と違う配線で動いていた)。
fn wire_and_serve(
    listener: TcpListener,
    cfg: &Config,
    config_of: impl Fn() -> Arc<Config> + Send + 'static,
    cache_cfg: CacheConfig,
    ca_file: Option<std::path::PathBuf>,
    clock: SharedClock,
) -> (u16, Arc<rust_http_proxy::workers::Workers>, Arc<Metrics>) {
    let port = listener.local_addr().unwrap().port();
    let metrics = Arc::new(Metrics::new());
    // 時計はここ (組み立ての根) で 1 回だけ決めて `Cache` に注入する (T35.1)
    let cache = Arc::new(Cache::with_clock(cache_cfg, clock));
    // 本番 (`crates/run`) と同じく、OpenSSL は最初の https:// オリジンで読む (T32.5)。
    // libssl が無ければ、そのときの接続が今までどおり `Unsupported` の誤り (502) になる
    let tls = Some(TlsClient::deferred(true, ca_file.as_deref(), None));
    let mut origin_pool = Pool::new(cfg.pool_per_host, Duration::from_secs(30));
    // 捨てるオリジン接続のカーネルの RTT をホスト別統計に足す (本番の main.rs と同じ配線。T14.5)
    rust_http_proxy::attach_origin_rtt(&mut origin_pool, Arc::clone(&metrics));
    let pool = Arc::new(Upstream {
        pool: origin_pool,
        tls,
    });

    // `--lite` では `/connections` に登録しない (本番の main.rs と同じ配線。T13.4)
    metrics.conns.set_enabled(!cfg.lite);
    // CONNECT の最初のバイトから SNI を覗くか (本番の main.rs と同じ配線。T14.38)
    rust_http_proxy::sni::set_peek(cfg.peek_sni);
    // 上限は設定から (テストでは既定のまま = コア数から決まる値)
    let workers = Arc::new(rust_http_proxy::workers::Workers::new(cfg.max_threads));
    // park_idle が立っている設定なら、アイドル接続を預ける監視スレッドも起こす
    let park = cfg.park_idle.then(|| {
        rust_http_proxy::idle::IdleWatch::start(Arc::clone(&workers), Arc::clone(&metrics))
            .expect("idle watcher")
    });

    let handle = Arc::clone(&workers);
    let seen = Arc::clone(&metrics);
    thread::spawn(move || {
        rust_http_proxy::serve(
            listener,
            config_of,
            rust_http_proxy::Limiter::new(),
            workers,
            metrics,
            cache,
            pool,
            park,
        )
    });

    (port, handle, seen)
}

/// 履歴スレッド付きで起こす版 (T14.6)。
///
/// 山の写真を撮るのは history スレッドなので、写真を見るテストはこれで起こす。
/// **周期を指定できる**ので 5 秒待たずに済む (`history::spawn_every`)。
/// 状態ファイルには書かない (`store` は `None`)。
pub fn start_test_proxy_with_history(config: Config, interval: Duration) -> (u16, Arc<Metrics>) {
    let (port, _, metrics) = start_test_proxy_parts(config, CacheConfig::disabled(), None);
    // 履歴の標本はキャッシュの使用量も読むので 1 つ渡す (無効のままでよい)
    let cache = Arc::new(Cache::new(CacheConfig::disabled()));
    rust_http_proxy::history::spawn_every(Arc::clone(&metrics), cache, None, interval);
    (port, metrics)
}

/// 履歴スレッドと**状態ファイル**付きで起こす版 (T14.9)。
///
/// 個票の永続化 (`$HOME/.rust-http-proxy.recent`) は状態ファイルの `Store` が持つので、
/// 「止めて同じ `$HOME` で起こし直す」を 1 つのプロセスで再現するための口。
/// `rrd_path` は `<dir>/.rust-http-proxy.rrd` (個票はその隣に置かれる)。
pub fn start_test_proxy_with_store(
    config: Config,
    interval: Duration,
    rrd_path: std::path::PathBuf,
) -> (
    u16,
    Arc<Metrics>,
    Option<Arc<rust_http_proxy::persist::Store>>,
) {
    let (port, _, metrics) = start_test_proxy_parts(config, CacheConfig::disabled(), None);
    let store = rust_http_proxy::persist::start(rrd_path, &metrics).map(|(s, _)| s);
    let cache = Arc::new(Cache::new(CacheConfig::disabled()));
    rust_http_proxy::history::spawn_every(Arc::clone(&metrics), cache, store.clone(), interval);
    (port, metrics, store)
}

/// `.env` の再読込のように**設定を差し替えられる**テスト用プロキシ (T11.6)。
///
/// 返した `RwLock` の中身を入れ替えると、`serve` が次に受ける接続から新しい設定を引く
/// (本番の `reload::Live::config()` と同じ形)。配線は固定の設定の版と同じ (T34.105)。
pub fn start_test_proxy_with_live_config(
    config: Config,
) -> (u16, Arc<std::sync::RwLock<Arc<Config>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let live = Arc::new(std::sync::RwLock::new(Arc::new(config)));
    let first = live.read().unwrap().clone();
    let shared = Arc::clone(&live);
    let (port, _, _) = wire_and_serve(
        listener,
        &first,
        move || Arc::clone(&shared.read().unwrap()),
        CacheConfig::disabled(),
        None,
        clock::system(),
    );
    (port, live)
}

pub fn proxy_config() -> Config {
    let mut cfg = Config::new("0", None, None, Duration::from_secs(5)).unwrap();
    cfg.keepalive = Duration::from_secs(2);
    // テストのオリジンは 127.0.0.1 なので、ローカル宛て (と私設アドレス宛て。T28.41) の既定の拒否を外す
    cfg.allow_local = rust_http_proxy::acl::AllowLocal::On;
    cfg
}

/// 固定上限 (メモリ 200 MiB / ディスク 2048 MiB) のテスト用キャッシュ設定。
pub fn cache_cfg(dir: &str) -> CacheConfig {
    let dir = super::fresh_dir(dir);
    CacheConfig {
        default_ttl: Duration::from_secs(60),
        ..CacheConfig::fixed(200 * MIB, 2048 * MIB, dir)
    }
}

/// アイドル接続を監視スレッド (epoll) に預ける設定 (既定なので `proxy_config` と
/// 同じだが、そのテストが何を見ているかを名前で示すために分けておく)。
pub fn park_config() -> Config {
    let mut cfg = proxy_config();
    cfg.park_idle = true;
    cfg
}
