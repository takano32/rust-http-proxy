//! 起動の段 (T34.59 で `main` (479 行) を割った)。段の順と、起動ログの文面と順は割る前と同じ。
//!
//! [`run`] が上から順に呼ぶ: malloc の調整 → 実行時の調整値 → 待ち受け → `.env` の監視 →
//! 統計と記録 → TLS → オリジンの接続 → 停止シグナル → 起動ログ → accept。

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::process;
use std::sync::{Arc, OnceLock};
use std::thread;

use proxy_server::cache::{Cache, MIB};
use proxy_server::config::Config;
use proxy_server::log;
use proxy_server::metrics::Metrics;
use proxy_server::net;
use proxy_server::persist::Store;
use proxy_server::pool::Pool;
use proxy_server::reload;
use proxy_server::serve;
use proxy_server::signal;
use proxy_server::tls::TlsClient;
use proxy_server::{Upstream, log_warn};
use proxy_server::{log_debug, log_error, log_info};

/// オリジンへのアイドル接続を保持する時間 (長いほどプールのヒット率が上がる)。
const ORIGIN_IDLE: std::time::Duration = std::time::Duration::from_secs(60);

/// 接続プールの掃除の相手 (`.env` の監視スレッドの一巡ごとに引く)。作るのはオリジンの接続の段。
type Sweeper = Arc<OnceLock<Arc<Upstream>>>;

/// 設定を読んだあとの起動の全部 ([`crate::main`] から呼ばれる。戻らない)。
pub(crate) fn run(config: Config) {
    tune_malloc(&config);
    apply_runtime_knobs(&config);
    let (listeners, self_bench) = bind_listeners(&config);
    let (live, sweeper) = start_reload(config);
    let config = live.config();
    let (metrics, cache, store) = start_records(&config);
    let tls = build_tls(&config);
    let pool = build_upstream(&config, tls, &metrics, &sweeper);
    install_shutdown(&config, &cache, &metrics, &store);
    log_startup(&config, &listeners, &cache);
    serve_all(live, config, listeners, self_bench, metrics, cache, pool);
}

/// malloc のアリーナ数と mmap の閾 (**スレッドを作る前**に呼ぶ)。
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn tune_malloc(config: &Config) {
    // malloc のアリーナ数を絞る。**スレッドを作る前に呼ぶ必要がある**ので、
    // 設定を読んだ直後・待ち受けを立てる前に置く
    #[cfg(target_os = "linux")]
    if config.malloc_arenas > 0 {
        let max = config.malloc_arenas.min(i32::MAX as usize) as i32;
        if proxy_server::sys::limit_malloc_arenas(max) {
            // 掛かった上限を覚えさせる (`/status` の `memory.arenas`。T14.21)
            proxy_server::sysinfo::malloc::set_arena_max(max as usize);
        } else {
            log_debug!(None, "mallopt(M_ARENA_MAX, {}) was refused", max);
        }
    }
    // mmap の閾を固定する (動く閾を止める。T32.3)。これも**スレッドを作る前**
    #[cfg(target_os = "linux")]
    if config.malloc_mmap_threshold_kb > 0 {
        let bytes = config.malloc_mmap_threshold_kb.saturating_mul(1024);
        if !proxy_server::sys::fix_malloc_mmap_threshold(bytes) {
            log_warn!(
                None,
                "mallopt(M_MMAP_THRESHOLD, {} KiB) was refused; the threshold stays dynamic",
                config.malloc_mmap_threshold_kb
            );
        }
    }
}

/// `--lite` の起動バナーと、要求の経路が読む調整値 (IPv6・Happy Eyeballs・名前解決・canary)。
fn apply_runtime_knobs(config: &Config) {
    if config.lite {
        // ログレベルに依らず必ず出す起動バナー
        // 「統計を取らない」ではない (`/hosts` などはメモリに積む)。止めるものだけを並べる (T28.44)
        println!(
            "profile: lite (no cache, no blocklist, no saved statistics, no per-connection records, warn log level)"
        );
    }
    net::set_ipv6_enabled(config.ipv6);
    // Happy Eyeballs の間隔 (T17.6)。起動時に 1 回だけ (要求の経路は atomic を 1 回読むだけ)
    net::set_stagger(config.he_stagger);
    // 続けて時間切れになった宛先を待たずに断る長さ (T36.8。再読込でも当て直す)
    net::set_connect_negative(config.connect_negative);
    proxy_server::dns::set_ttl(config.dns_ttl);
    proxy_server::dns::set_negative_ttl(config.dns_negative);
    proxy_server::dns::set_warm_window(config.dns_warm);
    proxy_server::dns::set_serve_stale(config.dns_stale);
    // 名前解決の締め切りと、引かせるスレッド群の上限 (ワーカーの 1/4。T32.11)
    proxy_server::dns::set_timeout(config.dns_timeout);
    proxy_server::dns::set_resolver_limit(proxy_server::dns::resolver_limit_for_threads(
        config.max_threads,
    ));
    // canary の宛先と周期 (T14.10)。実際に回すのは履歴スレッドの周期から (`--lite` と
    // `PROXY_STATS_PERSIST=off` では履歴スレッドが無いので canary も回らない)
    proxy_server::canary::configure(&config.canary, config.canary_secs, config.canary_ipv6);
}

/// 待ち受けを開く (開けなければ終わる)。2 つ目は自己ベンチの宛先 (`PROXY_SELF_BENCH=on` のときだけ)。
fn bind_listeners(config: &Config) -> (Vec<TcpListener>, Option<SocketAddr>) {
    let listeners = match net::bind_all(&config.bind_addrs, config.port, config.listen_backlog) {
        Ok(l) => l,
        Err(e) => {
            log_error!(None, "failed to bind port {}: {}", config.port, e);
            process::exit(1);
        }
    };

    // `TCP_INFO` が読めるかを試す相手として、待ち受けソケットを 1 本覚えておく (T14.15)。
    // 判定そのものは `.env` の監視スレッドが起動時と 1 時間ごとに行う (要求の経路では触らない)
    if let Some(l) = listeners.first() {
        proxy_server::sysinfo::capabilities::set_listener(l);
    }
    // 起動時の自己ベンチ (`PROXY_SELF_BENCH=on`、既定 off。T14.43) の宛先 = **自分の待ち受け**。
    // `SERVER_PORT=0` でも実際のポートが要るので、待ち受けを開いたここで読む。
    // **既定ではこの `then` の判定 1 回だけ**で、自己ベンチのコードは 1 命令も走らない
    let self_bench = config
        .self_bench
        .then(|| listeners.first().and_then(|l| l.local_addr().ok()))
        .flatten()
        .map(loopback_target);
    (listeners, self_bench)
}

/// 設定をプロセス全体に置き、`.env` の監視スレッドを起こす。
fn start_reload(config: Config) -> (Arc<reload::Live>, Sweeper) {
    let live = reload::Live::new(config);
    // 接続プールの掃除は専用スレッドを立てず、.env の監視スレッドの一巡ごとに行う
    let sweeper: Arc<std::sync::OnceLock<Arc<Upstream>>> = Arc::new(std::sync::OnceLock::new());
    let _reload = {
        let sweeper = Arc::clone(&sweeper);
        reload::spawn(Arc::clone(&live), move || {
            if let Some(up) = sweeper.get() {
                let n = up.pool.sweep();
                if n > 0 {
                    log_debug!(None, "origin pool: dropped {} idle connections", n);
                }
            }
        })
    };
    if _reload.is_none() {
        // `HOME` が無くて監視スレッドが立たない環境でも、何が読めるかは 1 回測る (T14.15)
        let _ = thread::Builder::new()
            .name("capabilities".into())
            .spawn(proxy_server::sysinfo::capabilities::refresh);
    }
    (live, sweeper)
}

/// 統計・キャッシュ・状態ファイル・記録のリング・履歴スレッド。
fn start_records(config: &Config) -> (Arc<Metrics>, Arc<Cache>, Option<Arc<Store>>) {
    let metrics = Arc::new(Metrics::new());
    // `--lite` では `/connections` に登録しない (空の一覧を返す。T1.4 の方針。T13.4)
    metrics.conns.set_enabled(!config.lite);
    // `--lite` では段階の時計も読まない (`/profile` は off。T14.3)
    proxy_server::profile::set_enabled(!config.lite);
    // CONNECT の最初のバイトから SNI を覗くか (`PROXY_PEEK_SNI`、既定 `on` = 443 だけ。T14.38)
    proxy_server::sni::set_peek(config.peek_sni);
    let _profile = (!config.lite)
        .then(|| proxy_server::profile::spawn(Arc::clone(&metrics), config.profile_sample_ms));
    let cache = Arc::new(Cache::new(config.cache.clone()));
    let _probe = Cache::spawn_probe(&cache);
    let store = if config.stats_persist {
        proxy_server::persist::Store::default_path()
            .and_then(|p| proxy_server::persist::start(p, &metrics))
    } else {
        None
    };
    let store = store.map(|(s, _handle)| s);
    // 記録のリングの置き場を満杯のぶん確保して触る (T17.8)。RSS を起動直後から天井の
    // 近くに置き、そこから増えたらそのまま「増えた」と読めるようにする。状態ファイルを
    // 読み戻したあとに置いたが、読み戻した件はそのまま残る (前でも後でも同じ結果)。
    // `--lite` は「使わないときに一切コストがかからない」の対象なので触らない。
    // Linux 以外は今までどおり (満ちるまで伸びる)
    #[cfg(target_os = "linux")]
    if !config.lite {
        let t = std::time::Instant::now();
        let bytes = metrics.prefault_rings(config.stats_persist);
        log_info!(
            None,
            "record rings prefaulted: {} KiB in {:.1} ms",
            bytes / 1024,
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
    if let Some(s) = &store {
        proxy_server::blocklist::set_store(Arc::clone(s));
    }
    // 日次の要約 (`$HOME/.rust-http-proxy.daily.jsonl`。T14.20)。書くのは履歴スレッドで、
    // `PROXY_STATS_PERSIST=off` ではここを呼ばないので 1 行も書かない
    if config.stats_persist {
        proxy_server::daily::configure(proxy_server::daily::default_path(), proxy_server::VERSION);
        // 日次の `/snapshot` (`$HOME/.rust-http-proxy/snapshots/`。T14.34)。書くのも
        // 履歴スレッドで、組み方は `serve` が預ける。`PROXY_SNAPSHOT_DAYS=0` で止まる
        proxy_server::snapshots::configure(
            proxy_server::snapshots::default_dir(),
            config.snapshot_days,
        );
    }
    // 異常の自動検知の閾 (T14.23)。判定するのは履歴スレッドなので、渡すのは
    // 同時接続の上限と、山と見なす本数 (T14.6 の写真と同じ閾) の 2 つだけ
    proxy_server::anomaly::configure(config.max_conns, config.burst_at);
    // SLO の 4 つの閾 (`PROXY_SLO`。T14.50)。判定するのも履歴スレッドで、
    // 5 秒の標本 1 本ごとに 4 つを当てて時間ごとの達成率に足す (`/slo`)
    proxy_server::slo::configure(proxy_server::slo::Thresholds {
        connect_p50_ms: config.slo.connect_p50_ms,
        connect_p95_ms: config.slo.connect_p95_ms,
        error_rate: config.slo.error_rate,
        dns_miss_per_connect: config.slo.dns_miss_per_connect,
    });
    // 永続化しないなら履歴スレッドも起動しない (/history とダッシュボードのグラフは空になる)
    let _history = config.stats_persist.then(|| {
        proxy_server::history::spawn(Arc::clone(&metrics), Arc::clone(&cache), store.clone())
    });
    (metrics, cache, store)
}

/// https:// オリジンのための TLS (OpenSSL は最初の https:// オリジンで読む)。
fn build_tls(config: &Config) -> Option<TlsClient> {
    if !config.tls_enabled {
        log_info!(
            None,
            "TLS: disabled (PROXY_TLS=off); https:// origins are unavailable"
        );
        None
    } else {
        // **OpenSSL は最初の https:// オリジンへの取得のときに読む** (T32.5)。CA の束の解析が起動の
        // 主スレッドの 70% (callgrind で 1.77 億 Ir)、dlopen と合わせて RSS 約 5.5 MB で、CONNECT だけの
        // 置き場では使われていなかった。読めるかどうかは `--check` が今ここで読んで確かめる。
        // 指定された CA ファイルが開けないことだけは、今までどおり起動で止める
        if let Some(path) = config.tls_ca_file.as_ref()
            && let Err(e) = std::fs::File::open(path)
        {
            log_error!(None, "TLS setup failed: CA file {}: {}", path.display(), e);
            process::exit(1);
        }
        log_info!(
            None,
            "TLS: OpenSSL will be loaded on the first https:// origin (certificate verification {}{})",
            if config.tls_verify { "on" } else { "OFF" },
            config
                .tls_ca_file
                .as_ref()
                .map(|p| format!(", CA file {}", p.display()))
                .unwrap_or_default()
        );
        Some(TlsClient::deferred(
            config.tls_verify,
            config.tls_ca_file.as_deref(),
            Some(log_tls_loaded),
        ))
    }
}

/// オリジンの接続プールとブロックリスト。
fn build_upstream(
    config: &Config,
    tls: Option<TlsClient>,
    metrics: &Arc<Metrics>,
    sweeper: &Sweeper,
) -> Arc<Upstream> {
    let mut origin_pool = Pool::with_total(config.pool_per_host, config.pool_total, ORIGIN_IDLE);
    // 捨てるオリジン接続のカーネルの RTT をホスト別統計に足す (T14.5)
    proxy_server::attach_origin_rtt(&mut origin_pool, Arc::clone(metrics));
    let pool = Arc::new(Upstream {
        pool: origin_pool,
        tls,
    });
    proxy_server::blocklist::configure(proxy_server::blocklist::Sources::from_config(config));
    let _ = sweeper.set(Arc::clone(&pool));
    let _blocklist = proxy_server::blocklist::spawn(Arc::clone(&pool), config.timeout);
    pool
}

/// 停止シグナルの後始末 (統計と個票を書き、`/snapshot` を 1 枚撮り、ballast を空にする)。
fn install_shutdown(
    config: &Config,
    cache: &Arc<Cache>,
    metrics: &Arc<Metrics>,
    store: &Option<Arc<Store>>,
) {
    // 停止シグナルで統計を状態ファイルに書き、まだ書いていない個票も落とし、
    // ballast.reserve を空にしてから終わる (Wings のディスク計測に残さない)
    let ballast =
        (config.cache.enabled && config.cache.reserve.is_on()).then(|| cache.ballast_path());
    let store = store.clone();
    let m = Arc::clone(metrics);
    let c = Arc::clone(cache);
    signal::install(
        ballast.as_deref(),
        Box::new(move || {
            // 出来事の時系列に 1 件 (シグナルハンドラではなく後始末のスレッドで走る。T14.11)
            proxy_server::events::push(
                proxy_server::events::EventKind::Shutdown,
                "stop signal received; saving statistics",
            );
            // 状態ファイルを書く前に `/snapshot` を 1 枚 (T28.20。締めの「前」)。
            // `recent` は切らずに全部入れる。後始末の上限 (`signal::GRACE` = 3 秒) の中に
            // 収まるよう、組んで書くまでにかかった時間をログに出す
            let t = std::time::Instant::now();
            if let Some(path) = proxy_server::snapshots::take_shutdown(&c) {
                log_info!(
                    None,
                    "shutdown snapshot {} built and written in {:.1} ms",
                    path.display(),
                    t.elapsed().as_secs_f64() * 1000.0
                );
            }
            if let Some(st) = store {
                st.flush_stats(&m);
                // 最後の 5 秒ぶんの個票も書いてから終わる (T14.9)
                let n = st.write_recent(&m);
                log_info!(
                    None,
                    "statistics saved to {} ({} individual records appended)",
                    st.path.display(),
                    n
                );
            }
        }),
    );
}

/// 起動ログ (待ち受け・出来事の `start`・設定ファイル・キャッシュ・締め切りと上限・許可の一覧)。
fn log_startup(config: &Config, listeners: &[TcpListener], cache: &Cache) {
    log_listening(config, listeners);
    log_cache(cache);
    log_limits(config);
}

/// 待ち受けの 1 行、出来事の時系列の 1 件目、設定ファイル。
fn log_listening(config: &Config, listeners: &[TcpListener]) {
    log_info!(
        None,
        // 版を先頭に出す: デプロイ先でどのコミットが動いているかをログだけでも
        // 追えるようにするため (同じ文字列が `-V` と `/status` の `version` に出る。T12.6)。
        // 待ち受けのあとの backlog は T14.47 (128 では足りないことがあるので、効いている値が
        // ログだけで分かるようにする。カーネルは somaxconn で頭打ちにする)
        "rust-http-proxy {} listening on {} (backlog {}, log level: {})",
        proxy_server::VERSION,
        listeners
            .iter()
            .map(net::describe_listener)
            .collect::<Vec<_>>()
            .join(", "),
        config.listen_backlog,
        log::current_level().as_str().trim()
    );
    // 出来事の時系列の 1 件目 (`/events`。T14.11)。再デプロイの時刻を
    // `since_start_secs` から逆算しなくて済むように、版と設定の要約をここで残す。
    // **ホストの起動ごとに変わる `boot_id` の先頭 12 文字も載せる** (T24.0): 1 件の枠は 128 B で
    // 全体 (36 文字) は入らず末尾の設定が切れてしまうので、同じ起動かどうかが分かる先頭だけにする
    // (全体と、ホストの再起動の切り分けは `/status` の `boot.id`)。
    let boot = match server_boot_id() {
        Some(id) => format!(" boot {}", id.chars().take(12).collect::<String>()),
        None => String::new(),
    };
    proxy_server::events::push(
        proxy_server::events::EventKind::Start,
        &format!(
            "version {} on port {}{} (profile {}, cache {}, timeout {}s, max conns {})",
            proxy_server::VERSION,
            config.port,
            boot,
            if config.lite { "lite" } else { "default" },
            if config.cache.enabled { "on" } else { "off" },
            config.timeout.as_secs(),
            config.max_conns,
        ),
    );
    if let Some(path) = proxy_server::envfile::loaded_path() {
        log_info!(
            None,
            "settings file {} loaded ({} variables; file values override the real environment)",
            path.display(),
            proxy_server::envfile::loaded_count()
        );
    }
}

/// キャッシュの予算 (と Pterodactyl の割り当て)。
fn log_cache(cache: &Cache) {
    // 設定ではなくキャッシュの側から読む (既定の候補を開けずに次の候補へ回ったとき、
    // `dir` は開けた方になっている。T36.10)
    let c = cache.config();
    log_info!(
        None,
        "cache: {} (memory {}, disk {}, reserve {}, probe every {}s, dir {}, default TTL {}s, max object {} MiB)",
        if c.enabled { "enabled" } else { "disabled" },
        c.mem_limit,
        c.disk_limit,
        c.reserve,
        c.probe_interval.as_secs(),
        c.dir.display(),
        c.default_ttl.as_secs(),
        c.max_object_size / MIB
    );
    if c.pterodactyl {
        log_info!(
            None,
            "Pterodactyl detected (memory allocation {}, disk quota {})",
            c.mem_alloc
                .map(|m| format!("{} MiB", m / MIB))
                .unwrap_or_else(|| "unknown".to_string()),
            format!(
                "{} under {}",
                cache.disk_quota(),
                c.quota_root
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            )
        );
    }
}

/// 締め切り・上限・許可の一覧。
fn log_limits(config: &Config) {
    log_info!(
        None,
        "timeout: {}s, keep-alive: {}s, origin pool: {} per host, DNS cache: {}s (keep-warm {}), canary: {} (every {}s), IPv6: {}",
        config.timeout.as_secs(),
        config.keepalive.as_secs(),
        config.pool_per_host,
        config.dns_ttl.as_secs(),
        if config.dns_warm.is_zero() {
            "off".to_string()
        } else {
            format!("{}s", config.dns_warm.as_secs())
        },
        if config.stats_persist {
            config.canary.clone()
        } else {
            format!("{} (no history thread)", config.canary)
        },
        config.canary_secs.as_secs(),
        if config.ipv6 {
            "on"
        } else {
            "off (PROXY_IPV6=on to enable)"
        }
    );
    // 上限の根拠を追えるように、決まった値と元になった記述子の上限を出す (T8.5)
    log_info!(
        None,
        "max connections: {} (open file limit {}, {} descriptors per connection)",
        if config.max_conns == 0 {
            "unlimited".to_string()
        } else {
            config.max_conns.to_string()
        },
        nofile_limit()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        proxy_server::config::FDS_PER_CONN
    );
    // 生きている接続スレッドの上限 (T10.5)。上限に達したら仕事は待たせる (捨てない)。
    // `.env` の再読込で変えられる (T11.6) ので、これは起動時の値
    log_info!(
        None,
        "max connection threads: {}",
        if config.max_threads == 0 {
            "unlimited".to_string()
        } else {
            config.max_threads.to_string()
        }
    );
    if !config.acl.allow_hosts.is_empty() {
        log_info!(None, "allowed hosts: {:?}", config.acl.allow_hosts);
    }
    if !config.acl.deny_hosts.is_empty() {
        log_info!(None, "denied hosts: {:?}", config.acl.deny_hosts);
    }
    // 読めた項目をそのまま出す (書き損じた項目は消えているので、ここで気づける。T14.18)
    if !config.allow_clients.is_empty() {
        log_info!(None, "allowed clients: {}", config.allow_clients);
    }
}

/// 接続の受け口を立てて回す (最後の 1 つの待ち受けはこのスレッドで回すので、戻らない)。
fn serve_all(
    live: Arc<reload::Live>,
    config: Arc<Config>,
    listeners: Vec<TcpListener>,
    self_bench: Option<SocketAddr>,
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
    pool: Arc<Upstream>,
) {
    let limiter = proxy_server::Limiter::new();
    // 接続スレッドを使い回す (生成・破棄の約 16 システムコールを接続ごとに払わない)。
    // 上限は起動時の値から始め、`.env` の再読込で変わったら `serve` が当て直す (T11.6)
    let workers = Arc::new(proxy_server::workers::Workers::new(config.max_threads));
    let park = start_park(&config, &workers, &metrics);
    // 起動時の自己ベンチ (T14.43)。**待ち受けが accept を始める直前**に起こし、
    // 中で少し待ってから loopback だけで 3 秒回す (`off` ならここは `None`)
    if let Some(addr) = self_bench {
        let _ = thread::Builder::new()
            .name("selfbench".into())
            .spawn(move || run_self_bench(addr));
    }
    // 待ち受けソケットごとに accept スレッドを持つ (最後の 1 つはこのスレッドで回す)
    let mut listeners = listeners.into_iter();
    let last = listeners.next_back().expect("at least one listener");
    for listener in listeners {
        let shared = (
            Arc::clone(&live),
            Arc::clone(&limiter),
            Arc::clone(&workers),
            Arc::clone(&metrics),
            Arc::clone(&cache),
            Arc::clone(&pool),
            park.clone(),
        );
        thread::spawn(move || {
            let live = shared.0;
            serve(
                listener,
                || live.config(),
                shared.1,
                shared.2,
                shared.3,
                shared.4,
                shared.5,
                shared.6,
            )
        });
    }
    drop(config);
    let l = Arc::clone(&live);
    serve(
        last,
        || l.config(),
        limiter,
        workers,
        metrics,
        cache,
        pool,
        park,
    );
}

/// アイドルな keep-alive 接続を epoll に預ける監視スレッド (作れなければ `None`)。
fn start_park(
    config: &Config,
    workers: &Arc<proxy_server::workers::Workers>,
    metrics: &Arc<Metrics>,
) -> Option<Arc<proxy_server::idle::IdleWatch>> {
    // アイドルな keep-alive 接続をスレッドから外して epoll に預ける監視スレッド。
    // 作れなければ何もせず、接続ごとにスレッドが待つ元の動きのままになる
    if config.park_idle {
        match proxy_server::idle::IdleWatch::start(Arc::clone(workers), Arc::clone(metrics)) {
            Ok(w) => {
                log_info!(
                    None,
                    "parking idle keep-alive connections (grace {}ms)",
                    config.park_grace.as_millis()
                );
                Some(w)
            }
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                // Linux 以外。接続ごとにスレッドが待つ元の動きになるだけ
                log_debug!(None, "not parking idle connections: {}", e);
                None
            }
            Err(e) => {
                log_warn!(None, "cannot park idle connections: {}", e);
                None
            }
        }
    } else {
        None
    }
}

/// 起動ログ用に `RLIMIT_NOFILE` の soft limit を引く (読めない環境では `None`)。
fn nofile_limit() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        proxy_server::sys::max_open_files()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// ホスト (カーネル) の起動ごとに変わる識別子 (`/proc/sys/kernel/random/boot_id`。T24.0)。
///
/// **ホストの再起動とプロセスだけの再起動を切り分ける**ために、起動の出来事の文面と
/// `/status` の `boot.id` に載せる。読めない環境は `None` (どちらも `null` / 文面から省く)。
fn server_boot_id() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        proxy_server::sys::boot_id()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// 最初の https:// オリジンで OpenSSL を読んだ結果を 1 行 (T32.5)。起動で読んでいたときと同じ文面。
fn log_tls_loaded(r: &Result<Option<String>, String>) {
    match r {
        Ok(Some(version)) => log_info!(None, "TLS: {} loaded", version),
        Ok(None) => log_warn!(
            None,
            "TLS: libssl not found; https:// origins are unavailable"
        ),
        Err(e) => log_error!(
            None,
            "TLS setup failed: {}; https:// origins are unavailable",
            e
        ),
    }
}

/// 待ち受けアドレスを「自分に繋ぎに行けるアドレス」に直す (T14.43)。
///
/// `0.0.0.0` / `[::]` で待っているときはループバックに読み替える。特定のアドレスで
/// 待っているならそのアドレスへ繋ぐ (そこが唯一の入口なので、loopback では届かない)。
fn loopback_target(addr: SocketAddr) -> SocketAddr {
    if !addr.ip().is_unspecified() {
        return addr;
    }
    match addr {
        SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::LOCALHOST, addr.port())),
        SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::LOCALHOST, addr.port())),
    }
}

/// 起動時の自己ベンチを 1 回だけ回す (`PROXY_SELF_BENCH=on`。T14.43)。
///
/// 待ち受けが accept を始めるのを少しだけ待ってから、**loopback だけで 3 秒**
/// (内蔵のオリジンへ forward 8 並列 1.5 秒 + CONNECT 8 並列 1.5 秒)。外へは 1 バイトも出さない。
/// 結果は `/status` の `self_bench` と `/events` の `start` に残る。
///
/// **測っている間だけログ水準を warn に下げる**: 既定プロファイル (info) のままだと
/// アクセスログが数万行出て、コンソールにも CPU/要求 にも乗ってしまう (1 行 7.2 us。T10.10)。
/// §2 の表も warn で測ってあるので、下げた方が**同じ物差し**になる。
fn run_self_bench(addr: SocketAddr) {
    // accept ループが回り出すのを待つ (待ち受けは既に開いているので、繋ぎに行っても
    // 取りこぼしはしない。測る前の 100 ms は起動直後のばたつきを避けるためのもの)
    thread::sleep(std::time::Duration::from_millis(100));
    let level = log::current_level();
    if level > log::Level::Warn {
        log::set_level(log::Level::Warn);
    }
    let report = proxy_server::selfbench::run(addr, &|ports| {
        proxy_server::acl::set_self_bench_ports(ports)
    });
    log::set_level(level);
    log_info!(None, "{}", report.summary());
    // 出来事の時系列にも 1 件 (種類は増やさず、起動の `start` に添える。T14.11 / T14.43)
    proxy_server::events::push(proxy_server::events::EventKind::Start, &report.summary());
}

#[cfg(test)]
mod tests {
    use super::loopback_target;
    use std::net::SocketAddr;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// T34.59: 全部のアドレスで待っているときだけループバックに読み替え、ポートは保つ。
    #[test]
    fn an_unspecified_listener_is_reached_over_loopback() {
        assert_eq!(
            loopback_target(addr("0.0.0.0:8080")),
            addr("127.0.0.1:8080")
        );
        assert_eq!(loopback_target(addr("[::]:8443")), addr("[::1]:8443"));
    }

    #[test]
    fn a_specific_listener_is_reached_at_its_own_address() {
        for a in [
            "192.0.2.7:3128",
            "127.0.0.2:80",
            "[2001:db8::1]:8080",
            "[::1]:1",
        ] {
            assert_eq!(loopback_target(addr(a)), addr(a), "{a}");
        }
    }
}
