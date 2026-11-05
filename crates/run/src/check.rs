//! `--check`: 起動前に**この環境で何が読めるか**と**効く設定**を印字する (T14.15)。
//!
//! T.34.60 で `lib.rs` の `check_environment` (135 行) をここへ移し、行の組み立てを純粋な関数
//! (`capability_description`・`flag_line`・`setting_value_for_print`・`setting_line`・`tls_line`) に、
//! 印字を `print_capabilities`・`print_settings`・`print_problems` に割った。文面と順は前と同じ。

use std::path::PathBuf;

use proxy_server::config::{Config, EnvProblems};
use proxy_server::signal;
use proxy_server::sysinfo::capabilities::Capabilities;
use proxy_server::tls::TlsClient;

/// `--check` の本体。終了コードを返す。
///
/// Pterodactyl のような触れないコンテナで、`/status` を引く前に「統計の `null` は
/// この環境のせいか」「`.env` に書いた値は効くのか」を確かめるための口。
/// 終了コードは `capabilities` の 6 項目 (名前解決を除く) が全部読めれば 0、
/// 1 つでも読めなければ 1。**名前解決を外す**のは、リゾルバが遅い環境でも
/// プロキシとしては動く (そしてそれ自体が測りたい数字) ため。
pub(crate) fn check_environment() -> i32 {
    // `--check | head` のように読み手が先に閉じても、パニックせずに黙って終わる (T.28.48)
    signal::quiet_on_closed_pipe();
    print_header();
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            println!("\nconfiguration error: {}", e);
            return 1;
        }
    };

    // 測るのはここで 1 回だけ (名前解決に最大 2 秒)
    let caps = proxy_server::sysinfo::capabilities::probe();
    print_capabilities(&caps, &config);
    print_settings(&config);
    print_problems(&config.problems);

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

/// 版・ビルドの種類・設定ファイル。
fn print_header() {
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
}

/// 読める項目の一覧 (`capabilities`)、名前解決の速さ、`cpu.stat` の場所、TLS。
fn print_capabilities(caps: &Capabilities, config: &Config) {
    println!("\ncapabilities (what this environment lets the proxy read):");
    for (name, ok) in caps.flags() {
        println!(
            "{}",
            flag_line(
                if ok { "ok" } else { "NO" },
                name,
                capability_description(name)
            )
        );
    }
    println!(
        "{}",
        flag_line(
            if caps.resolver_ms.is_some() {
                "ok"
            } else {
                "--"
            },
            "resolver_ms",
            &match caps.resolver_ms {
                Some(ms) => format!("{} ms for one lookup (not part of the exit code)", ms),
                None => "no answer within 2s (not part of the exit code)".to_string(),
            }
        )
    );
    // どの階層の `cpu.stat` を読むことになるか (T15.0 (6))。自分の cgroup に cpu
    // コントローラが無いと親の値を読むので、`nr_throttled` が誰のものかはここで分かる。
    // **引くのはここと `/status` を組むときだけ** (5 秒の標本では引かない)
    let cpu_path: Option<PathBuf> = proxy_server::sysinfo::cgroup::cgroup_cpu_path();
    println!(
        "{}",
        flag_line(
            if cpu_path.is_some() { "ok" } else { "--" },
            "cgroup_cpu_path",
            &match &cpu_path {
                Some(p) => format!("{} (where nr_throttled is read)", p.display()),
                None => "no cgroup v2 cpu.stat (not part of the exit code)".to_string(),
            }
        )
    );

    // OpenSSL は起動では読まず最初の https:// オリジンで読むので (T.32.5)、読めるかはここで確かめる
    if config.tls_enabled {
        let loaded = TlsClient::load(config.tls_verify, config.tls_ca_file.as_deref())
            .map(|t| t.map(|t| t.version()));
        let (ok, text) = tls_line(&loaded);
        println!("{}", flag_line(ok, "tls", &text));
    }
}

/// 効く設定の一覧 (出どころ・名前・値)。
fn print_settings(config: &Config) {
    println!("\nsettings (source, name, effective value):");
    for s in config.settings() {
        println!(
            "{}",
            setting_line(s.source.as_str(), s.key, &setting_value_for_print(&s.value))
        );
    }
}

/// 書いたのに効いていない行 (T.28.6)。正しい設定では 1 行も出ない。
fn print_problems(problems: &EnvProblems) {
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
}

/// 読める項目の名前から、何を読んでいるかの説明。
fn capability_description(name: &str) -> &'static str {
    match name {
        "proc_syscall" => "/proc/self/task/<tid>/syscall (per-thread state)",
        "tcp_info" => "getsockopt(SOL_TCP, TCP_INFO) (kernel RTT and retransmits)",
        "cgroup_cpu" => "cgroup cpu.stat (CPU throttling)",
        "cgroup_pressure" => "cgroup cpu.pressure (PSI: waiting for the CPU)",
        "ipv6_route" => "a default route in /proc/net/ipv6_route",
        "proc_schedstat" => "/proc/self/task/<tid>/schedstat (time runnable but not running)",
        _ => "$HOME is writable (statistics file, blocklist)",
    }
}

/// `  [ok] name             説明` の 1 行。
fn flag_line(mark: &str, name: &str, text: &str) -> String {
    format!("  [{}] {:<16} {}", mark, name, text)
}

/// `Setting::value` (JSON の値) を印字の形にする: 文字列の引用符は、中身が空でなく `\` を
/// 含まないときだけ外す。
///
/// 中身は `config::printable_value` (`/events` の `reload` の前後の値と同じ外し方。T.34.153 で 1 つにした)。
fn setting_value_for_print(value: &str) -> String {
    // JSON の値をそのまま出すと文字列に引用符が付くので、単純なものは外す
    proxy_server::config::printable_value(value)
}

/// `  source    name                           value` の 1 行。
fn setting_line(source: &str, key: &str, value: &str) -> String {
    format!("  {:<9} {:<30} {}", source, key, value)
}

/// OpenSSL を読んだ結果 (版) を `tls` の行の印と説明にする。
fn tls_line(loaded: &Result<Option<String>, String>) -> (&'static str, String) {
    match loaded {
        Ok(Some(version)) => (
            "ok",
            format!(
                "{} (read on the first https:// origin, not at start)",
                version
            ),
        ),
        Ok(None) => (
            "--",
            "libssl not found; https:// origins are unavailable (not part of the exit code)"
                .to_string(),
        ),
        Err(e) => ("--", format!("{} (not part of the exit code)", e)),
    }
}

#[cfg(test)]
mod tests;
