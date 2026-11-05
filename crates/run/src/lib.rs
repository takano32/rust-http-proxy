//! 実行ファイルの中身 (`rust-http-proxy` の `main`)。
//!
//! ルートの `src/main.rs` はこの [`main`] を呼ぶだけの 3 行で、本体は
//! [`proxy_server`] にある (T15.12 段 6')。ここを別クレートにしてあるのは、
//! `proxy_server::serve` の総称の引数 (`config_of: impl Fn()`) の単相化を
//! **bin の最後の段ではなくこのクレートの `rustc`** で行わせるため
//! (最後の段はリンクのとき全部を一度に抱えるので、そこが床になっていた)。

use std::process;

use proxy_server::config::Config;
use proxy_server::log;
use proxy_server::log_error;
use proxy_server::signal;
use proxy_server::tls::TlsClient;

/// `--check`: 起動前に**この環境で何が読めるか**と**効く設定**を印字する (T14.15)。
///
/// Pterodactyl のような触れないコンテナで、`/status` を引く前に「統計の `null` は
/// この環境のせいか」「`.env` に書いた値は効くのか」を確かめるための口。
/// 終了コードは `capabilities` の 6 項目 (名前解決を除く) が全部読めれば 0、
/// 1 つでも読めなければ 1。**名前解決を外す**のは、リゾルバが遅い環境でも
/// プロキシとしては動く (そしてそれ自体が測りたい数字) ため。
fn check_environment() -> i32 {
    // `--check | head` のように読み手が先に閉じても、パニックせずに黙って終わる (T.28.48)
    signal::quiet_on_closed_pipe();
    println!("rust-http-proxy {} --check", proxy_server::VERSION);
    // どの最適化で作ったバイナリか (cargo のプロファイル名。T.28.42)
    println!("build profile: {}", proxy_server::BUILD);
    match proxy_server::envfile::loaded_path() {
        Some(p) => println!(
            "settings file: {} ({} variables)",
            p.display(),
            proxy_server::envfile::loaded_count()
        ),
        None => println!(
            "settings file: none ({})",
            proxy_server::envfile::env_path()
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
    let caps = proxy_server::sysinfo::capabilities::probe();
    let what = |name: &str| match name {
        "proc_syscall" => "/proc/self/task/<tid>/syscall (per-thread state)",
        "tcp_info" => "getsockopt(SOL_TCP, TCP_INFO) (kernel RTT and retransmits)",
        "cgroup_cpu" => "cgroup cpu.stat (CPU throttling)",
        "cgroup_pressure" => "cgroup cpu.pressure (PSI: waiting for the CPU)",
        "ipv6_route" => "a default route in /proc/net/ipv6_route",
        "proc_schedstat" => "/proc/self/task/<tid>/schedstat (time runnable but not running)",
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
    // どの階層の `cpu.stat` を読むことになるか (T15.0 (6))。自分の cgroup に cpu
    // コントローラが無いと親の値を読むので、`nr_throttled` が誰のものかはここで分かる。
    // **引くのはここと `/status` を組むときだけ** (5 秒の標本では引かない)
    let cpu_path = proxy_server::sysinfo::cgroup::cgroup_cpu_path();
    println!(
        "  [{}] {:<16} {}",
        if cpu_path.is_some() { "ok" } else { "--" },
        "cgroup_cpu_path",
        match &cpu_path {
            Some(p) => format!("{} (where nr_throttled is read)", p.display()),
            None => "no cgroup v2 cpu.stat (not part of the exit code)".to_string(),
        }
    );

    // OpenSSL は起動では読まず最初の https:// オリジンで読むので (T.32.5)、読めるかはここで確かめる
    if config.tls_enabled {
        let (ok, text) = match TlsClient::load(config.tls_verify, config.tls_ca_file.as_deref()) {
            Ok(Some(t)) => (
                "ok",
                format!(
                    "{} (read on the first https:// origin, not at start)",
                    t.version()
                ),
            ),
            Ok(None) => (
                "--",
                "libssl not found; https:// origins are unavailable (not part of the exit code)"
                    .to_string(),
            ),
            Err(e) => ("--", format!("{} (not part of the exit code)", e)),
        };
        println!("  [{}] {:<16} {}", ok, "tls", text);
    }

    println!("\nsettings (source, name, effective value):");
    for s in config.settings() {
        // JSON の値をそのまま出すと文字列に引用符が付くので、単純なものは外す
        let value = match s.value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            Some(inner) if !inner.is_empty() && !inner.contains('\\') => inner.to_string(),
            _ => s.value.clone(),
        };
        println!("  {:<9} {:<30} {}", s.source.as_str(), s.key, value);
    }
    // 書いたのに効いていない行 (T.28.6)。正しい設定では 1 行も出ない
    let problems = &config.problems;
    if !problems.invalid.is_empty() {
        println!("\nnot readable (ignored; the default is in effect):");
        for v in &problems.invalid {
            println!("  {:<30} {:?} (expected {})", v.key, v.value, v.expected);
        }
    }
    if !problems.unknown.is_empty() {
        println!("\nunknown names in the settings file (not read by this proxy):");
        for u in &problems.unknown {
            match u.suggest {
                Some(s) => println!("  {:<30} did you mean {}?", u.key, s),
                None => println!("  {}", u.key),
            }
        }
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

mod startup;

/// 起動の全部 (ルートの `src/main.rs` から呼ばれる)。引数と設定を読み、残りは [`startup::run`] の段に渡す。
pub fn main() {
    // `-V` は版のあとにビルドの種類 (`release` / `dist`) を添える (T.28.42)
    let version = format!("{} ({})", proxy_server::VERSION, proxy_server::BUILD);
    match proxy_server::cli::parse(std::env::args().skip(1), &version) {
        proxy_server::cli::Cli::Print(msg, 0) => {
            println!("{}", msg);
            process::exit(0);
        }
        proxy_server::cli::Cli::Print(msg, code) => {
            eprintln!("{}", msg);
            process::exit(code);
        }
        // `--check` は起動しない: この環境で何が読めるかと、効く設定を印字して終わる
        proxy_server::cli::Cli::Check(vars) => {
            proxy_server::envfile::set_overrides(vars);
            process::exit(check_environment());
        }
        proxy_server::cli::Cli::Run(vars) => proxy_server::envfile::set_overrides(vars),
    }
    log::init_from_env();

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            log_error!(None, "configuration error: {}", e);
            process::exit(1);
        }
    };

    startup::run(config);
}
