use std::process;
use std::sync::Arc;
use std::thread;

use rust_http_proxy::cache::{Cache, MIB};
use rust_http_proxy::config::Config;
use rust_http_proxy::log;
use rust_http_proxy::metrics::Metrics;
use rust_http_proxy::net;
use rust_http_proxy::pool::Pool;
use rust_http_proxy::reload;
use rust_http_proxy::serve;
use rust_http_proxy::signal;
use rust_http_proxy::tls::TlsClient;
use rust_http_proxy::{Upstream, log_warn};
use rust_http_proxy::{log_debug, log_error, log_info};

/// オリジンへのアイドル接続を保持する時間 (長いほどプールのヒット率が上がる)。
const ORIGIN_IDLE: std::time::Duration = std::time::Duration::from_secs(60);

/// 起動ログ用に `RLIMIT_NOFILE` の soft limit を引く (読めない環境では `None`)。
fn nofile_limit() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        rust_http_proxy::sys::max_open_files()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// `--check`: 起動前に**この環境で何が読めるか**と**効く設定**を印字する (T14.15)。
///
/// Pterodactyl のような触れないコンテナで、`/status` を引く前に「統計の `null` は
/// この環境のせいか」「`.env` に書いた値は効くのか」を確かめるための口。
/// 終了コードは `capabilities` の 6 項目 (名前解決を除く) が全部読めれば 0、
/// 1 つでも読めなければ 1。**名前解決を外す**のは、リゾルバが遅い環境でも
/// プロキシとしては動く (そしてそれ自体が測りたい数字) ため。
fn check_environment() -> i32 {
    println!("rust-http-proxy {} --check", rust_http_proxy::VERSION);
    match rust_http_proxy::envfile::loaded_path() {
        Some(p) => println!(
            "settings file: {} ({} variables)",
            p.display(),
            rust_http_proxy::envfile::loaded_count()
        ),
        None => println!(
            "settings file: none ({})",
            rust_http_proxy::envfile::env_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "$HOME is not set".to_string())
        ),
    }
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            println!("\nconfiguration error: {}", e);
            return 1;
        }
    };

    // 測るのはここで 1 回だけ (名前解決に最大 2 秒)
    let caps = rust_http_proxy::sysinfo::capabilities::probe();
    let what = |name: &str| match name {
        "proc_syscall" => "/proc/self/task/<tid>/syscall (per-thread state)",
        "tcp_info" => "getsockopt(SOL_TCP, TCP_INFO) (kernel RTT and retransmits)",
        "cgroup_cpu" => "cgroup cpu.stat (CPU throttling)",
        "cgroup_pressure" => "cgroup cpu.pressure (PSI: waiting for the CPU)",
        "ipv6_route" => "a default route in /proc/net/ipv6_route",
        _ => "$HOME is writable (statistics file, blocklist)",
    };
    println!("\ncapabilities (what this environment lets the proxy read):");
    for (name, ok) in caps.flags() {
        println!(
            "  [{}] {:<16} {}",
            if ok { "ok" } else { "NO" },
            name,
            what(name)
        );
    }
    println!(
        "  [{}] {:<16} {}",
        if caps.resolver_ms.is_some() {
            "ok"
        } else {
            "--"
        },
        "resolver_ms",
        match caps.resolver_ms {
            Some(ms) => format!("{} ms for one lookup (not part of the exit code)", ms),
            None => "no answer within 2s (not part of the exit code)".to_string(),
        }
    );

    println!("\nsettings (source, name, effective value):");
    for s in config.settings() {
        // JSON の値をそのまま出すと文字列に引用符が付くので、単純なものは外す
        let value = match s.value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            Some(inner) if !inner.is_empty() && !inner.contains('\\') => inner.to_string(),
            _ => s.value.clone(),
        };
        println!("  {:<9} {:<30} {}", s.source.as_str(), s.key, value);
    }

    let missing = caps.missing();
    if missing.is_empty() {
        println!("\ncheck: ok (everything this proxy reads is readable)");
        0
    } else {
        println!(
            "\ncheck: {} of {} not readable ({}) — those show up as null in /status and /history",
            missing.len(),
            caps.flags().len(),
            missing.join(", ")
        );
        1
    }
}

fn main() {
    match rust_http_proxy::cli::parse(std::env::args().skip(1), rust_http_proxy::VERSION) {
        rust_http_proxy::cli::Cli::Print(msg, 0) => {
            println!("{}", msg);
            process::exit(0);
        }
        rust_http_proxy::cli::Cli::Print(msg, code) => {
            eprintln!("{}", msg);
            process::exit(code);
        }
        // `--check` は起動しない: この環境で何が読めるかと、効く設定を印字して終わる
        rust_http_proxy::cli::Cli::Check(vars) => {
            rust_http_proxy::envfile::set_overrides(vars);
            process::exit(check_environment());
        }
        rust_http_proxy::cli::Cli::Run(vars) => rust_http_proxy::envfile::set_overrides(vars),
    }
    log::init_from_env();

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            log_error!(None, "configuration error: {}", e);
            process::exit(1);
        }
    };

    // malloc のアリーナ数を絞る。**スレッドを作る前に呼ぶ必要がある**ので、
    // 設定を読んだ直後・待ち受けを立てる前に置く
    #[cfg(target_os = "linux")]
    if config.malloc_arenas > 0 {
        let max = config.malloc_arenas.min(i32::MAX as usize) as i32;
        if !rust_http_proxy::sys::limit_malloc_arenas(max) {
            log_debug!(None, "mallopt(M_ARENA_MAX, {}) was refused", max);
        }
    }
    if config.lite {
        // ログレベルに依らず必ず出す起動バナー
        println!("profile: lite (no cache, no statistics, no blocklist, warn log level)");
    }
    net::set_ipv6_enabled(config.ipv6);
    rust_http_proxy::dns::set_ttl(config.dns_ttl);
    rust_http_proxy::dns::set_negative_ttl(config.dns_negative);
    rust_http_proxy::dns::set_warm_window(config.dns_warm);
    // canary の宛先と周期 (T14.10)。実際に回すのは履歴スレッドの周期から (`--lite` と
    // `PROXY_STATS_PERSIST=off` では履歴スレッドが無いので canary も回らない)
    rust_http_proxy::canary::configure(&config.canary, config.canary_secs);
    let listeners = match net::bind_all(&config.bind_addrs, config.port) {
        Ok(l) => l,
        Err(e) => {
            log_error!(None, "failed to bind port {}: {}", config.port, e);
            process::exit(1);
        }
    };

    // `TCP_INFO` が読めるかを試す相手として、待ち受けソケットを 1 本覚えておく (T14.15)。
    // 判定そのものは `.env` の監視スレッドが起動時と 1 時間ごとに行う (要求の経路では触らない)
    if let Some(l) = listeners.first() {
        rust_http_proxy::sysinfo::capabilities::set_listener(l);
    }

    let live = reload::Live::new(config);
    let config = live.config();
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
            .spawn(rust_http_proxy::sysinfo::capabilities::refresh);
    }
    let metrics = Arc::new(Metrics::new());
    // `--lite` では `/connections` に登録しない (空の一覧を返す。T1.4 の方針。T13.4)
    metrics.conns.set_enabled(!config.lite);
    let cache = Arc::new(Cache::new(config.cache.clone()));
    let _probe = Cache::spawn_probe(&cache);
    let store = if config.stats_persist {
        rust_http_proxy::persist::Store::default_path()
            .and_then(|p| rust_http_proxy::persist::start(p, &metrics))
    } else {
        None
    };
    let store = store.map(|(s, _handle)| s);
    if let Some(s) = &store {
        rust_http_proxy::blocklist::set_store(Arc::clone(s));
    }
    // 日次の要約 (`$HOME/.rust-http-proxy.daily.jsonl`。T14.20)。書くのは履歴スレッドで、
    // `PROXY_STATS_PERSIST=off` ではここを呼ばないので 1 行も書かない
    if config.stats_persist {
        rust_http_proxy::daily::configure(
            rust_http_proxy::daily::default_path(),
            rust_http_proxy::VERSION,
        );
    }
    // 永続化しないなら履歴スレッドも起動しない (/history とダッシュボードのグラフは空になる)
    let _history = config.stats_persist.then(|| {
        rust_http_proxy::history::spawn(Arc::clone(&metrics), Arc::clone(&cache), store.clone())
    });
    let tls = if !config.tls_enabled {
        log_info!(
            None,
            "TLS: disabled (PROXY_TLS=off); https:// origins are unavailable"
        );
        None
    } else {
        match TlsClient::load(config.tls_verify, config.tls_ca_file.as_deref()) {
            Ok(Some(t)) => {
                log_info!(
                    None,
                    "TLS: {} (certificate verification {}{})",
                    t.version(),
                    if t.verifies() { "on" } else { "OFF" },
                    config
                        .tls_ca_file
                        .as_ref()
                        .map(|p| format!(", CA file {}", p.display()))
                        .unwrap_or_default()
                );
                Some(t)
            }
            Ok(None) => {
                log_warn!(
                    None,
                    "TLS: libssl not found; https:// origins are unavailable"
                );
                None
            }
            Err(e) => {
                log_error!(None, "TLS setup failed: {}", e);
                process::exit(1);
            }
        }
    };
    let mut origin_pool = Pool::with_total(config.pool_per_host, config.pool_total, ORIGIN_IDLE);
    // 捨てるオリジン接続のカーネルの RTT をホスト別統計に足す (T14.5)
    rust_http_proxy::attach_origin_rtt(&mut origin_pool, Arc::clone(&metrics));
    let pool = Arc::new(Upstream {
        pool: origin_pool,
        tls,
    });
    rust_http_proxy::blocklist::configure(rust_http_proxy::blocklist::Sources::from_config(
        &config,
    ));
    let _ = sweeper.set(Arc::clone(&pool));
    let _blocklist = rust_http_proxy::blocklist::spawn(Arc::clone(&pool), config.timeout);
    // 停止シグナルで統計を状態ファイルに書き、まだ書いていない個票も落とし、
    // ballast.reserve を空にしてから終わる (Wings のディスク計測に残さない)
    {
        let ballast =
            (config.cache.enabled && config.cache.reserve.is_on()).then(|| cache.ballast_path());
        let store = store.clone();
        let m = Arc::clone(&metrics);
        signal::install(
            ballast.as_deref(),
            Box::new(move || {
                // 出来事の時系列に 1 件 (シグナルハンドラではなく後始末のスレッドで走る。T14.11)
                rust_http_proxy::events::push(
                    rust_http_proxy::events::EventKind::Shutdown,
                    "stop signal received; saving statistics",
                );
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

    log_info!(
        None,
        // 版を先頭に出す: デプロイ先でどのコミットが動いているかをログだけでも
        // 追えるようにするため (同じ文字列が `-V` と `/status` の `version` に出る。T12.6)
        "rust-http-proxy {} listening on {} (log level: {})",
        rust_http_proxy::VERSION,
        listeners
            .iter()
            .map(net::describe_listener)
            .collect::<Vec<_>>()
            .join(", "),
        log::current_level().as_str().trim()
    );
    // 出来事の時系列の 1 件目 (`/events`。T14.11)。再デプロイの時刻を
    // `since_start_secs` から逆算しなくて済むように、版と設定の要約をここで残す
    rust_http_proxy::events::push(
        rust_http_proxy::events::EventKind::Start,
        &format!(
            "version {} on port {} (profile {}, cache {}, timeout {}s, max conns {})",
            rust_http_proxy::VERSION,
            config.port,
            if config.lite { "lite" } else { "default" },
            if config.cache.enabled { "on" } else { "off" },
            config.timeout.as_secs(),
            config.max_conns,
        ),
    );
    if let Some(path) = rust_http_proxy::envfile::loaded_path() {
        log_info!(
            None,
            "settings file {} loaded ({} variables; file values override the real environment)",
            path.display(),
            rust_http_proxy::envfile::loaded_count()
        );
    }
    let c = &config.cache;
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
        rust_http_proxy::config::FDS_PER_CONN
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

    let limiter = rust_http_proxy::Limiter::new();
    // 接続スレッドを使い回す (生成・破棄の約 16 システムコールを接続ごとに払わない)。
    // 上限は起動時の値から始め、`.env` の再読込で変わったら `serve` が当て直す (T11.6)
    let workers = Arc::new(rust_http_proxy::workers::Workers::new(config.max_threads));
    // アイドルな keep-alive 接続をスレッドから外して epoll に預ける監視スレッド。
    // 作れなければ何もせず、接続ごとにスレッドが待つ元の動きのままになる
    let park = if config.park_idle {
        match rust_http_proxy::idle::IdleWatch::start(Arc::clone(&workers), Arc::clone(&metrics)) {
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
    };
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
