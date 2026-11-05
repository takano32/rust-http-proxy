//! 試験用プロキシをこのプロセスの中で起こす口と、試験用の設定。

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rust_http_proxy::Upstream;
use rust_http_proxy::cache::{Cache, CacheConfig, MIB};
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

/// `serve` を呼ぶ前に `early` 本の接続の握手を済ませておく版 (起動直後の接続のテスト用。T.28.30)。
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
    let port = listener.local_addr().unwrap().port();
    let cfg = Arc::new(config);
    let metrics = Arc::new(Metrics::new());
    let cache = Arc::new(Cache::new(cache_cfg));
    // 本番 (`crates/run`) と同じく、OpenSSL は最初の https:// オリジンで読む (T.32.5)。
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
            || Arc::clone(&cfg),
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
/// (本番の `reload::Live::config()` と同じ形)。
pub fn start_test_proxy_with_live_config(
    config: Config,
) -> (u16, Arc<std::sync::RwLock<Arc<Config>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let live = Arc::new(std::sync::RwLock::new(Arc::new(config)));
    let cfg = live.read().unwrap().clone();
    let metrics = Arc::new(Metrics::new());
    let cache = Arc::new(Cache::new(CacheConfig::disabled()));
    let pool = Arc::new(Upstream {
        pool: Pool::new(cfg.pool_per_host, Duration::from_secs(30)),
        tls: None,
    });
    let workers = Arc::new(rust_http_proxy::workers::Workers::new(cfg.max_threads));
    let park = cfg.park_idle.then(|| {
        rust_http_proxy::idle::IdleWatch::start(Arc::clone(&workers), Arc::clone(&metrics))
            .expect("idle watcher")
    });
    let shared = Arc::clone(&live);
    thread::spawn(move || {
        rust_http_proxy::serve(
            listener,
            || Arc::clone(&shared.read().unwrap()),
            rust_http_proxy::Limiter::new(),
            workers,
            metrics,
            cache,
            pool,
            park,
        )
    });
    (port, live)
}

pub fn proxy_config() -> Config {
    let mut cfg = Config::new("0", None, None, Duration::from_secs(5)).unwrap();
    cfg.keepalive = Duration::from_secs(2);
    // テストのオリジンは 127.0.0.1 なので、ローカル宛て (と私設アドレス宛て。T.28.41) の既定の拒否を外す
    cfg.allow_local = rust_http_proxy::acl::AllowLocal::On;
    cfg
}

/// 固定上限 (メモリ 200 MiB / ディスク 2048 MiB) のテスト用キャッシュ設定。
pub fn cache_cfg(dir: &str) -> CacheConfig {
    let dir = std::env::temp_dir().join(dir);
    let _ = std::fs::remove_dir_all(&dir);
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
