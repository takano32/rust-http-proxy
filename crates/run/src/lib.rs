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

// `--check` の印字 (T34.60 で `check_environment` をここから移した)
mod check;
mod startup;

/// 起動の全部 (ルートの `src/main.rs` から呼ばれる)。引数と設定を読み、残りは [`startup::run`] の段に渡す。
pub fn main() {
    // `-V` は版のあとにビルドの種類 (`release` / `dist`) を添える (T28.42)
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
            process::exit(check::check_environment());
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
