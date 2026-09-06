//! コマンドライン引数の解析 (外部クレートは使わない)。
//!
//! 引数は「対応する環境変数の上書き」として扱う。優先順位は
//! **引数 > `$HOME/.env` > 実際の環境変数**。

/// 解析の結果。
#[derive(Debug, PartialEq, Eq)]
pub enum Cli {
    /// このまま起動する (環境変数の上書き)
    Run(Vec<(String, String)>),
    /// メッセージを出して終了する (終了コード付き。0 は標準出力、それ以外は標準エラー)
    Print(String, i32),
}

const USAGE: &str = "\
rust-http-proxy — 認証不要の HTTP/HTTPS プロキシ (依存クレートゼロ)

usage: rust-http-proxy [OPTIONS]

  -p, --port <PORT>    listen port           (SERVER_PORT, default 8080)
      --bind <ADDRS>   listen addresses      (PROXY_BIND, comma separated)
      --no-cache       disable the cache     (PROXY_CACHE_ENABLED=off)
      --quiet          only warnings/errors  (PROXY_LOG_LEVEL=warn)
      --lite           fastest pass-through profile (PROXY_PROFILE=lite:
                       no cache, no statistics, no blocklist, warn level)
  -h, --help           show this help and exit
  -V, --version        show the version and exit

Every setting can also be given as an environment variable or in $HOME/.env
(command line > $HOME/.env > environment). Common ones:

  SERVER_PORT              listen port (8080)
  PROXY_BIND               listen addresses (dual stack by default)
  PROXY_MAX_CONNS          maximum concurrent connections (4096, 0 = unlimited)
  PROXY_TIMEOUT_SECS       origin timeout (30)
  PROXY_KEEPALIVE_SECS     client keep-alive idle time (15)
  PROXY_TUNNEL_IDLE_SECS   CONNECT tunnel idle timeout (300, 0 = never)
  PROXY_ALLOW_LOCAL        allow loopback/link-local origins (off)
  PROXY_CONNECT_PORTS      ports CONNECT may reach (unrestricted)
  PROXY_ALLOW_HOSTS        host allow list, wildcards allowed
  PROXY_DENY_HOSTS         host deny list, wildcards allowed
  PROXY_CACHE_ENABLED      on/off (on)
  PROXY_STATS_PERSIST      keep statistics across restarts (on)
  PROXY_LOG_LEVEL          error|warn|info|debug|trace (info)

See README.md for the full table.";

/// `args` は実行ファイル名を除いた引数。
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Cli {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut it = args.into_iter().peekable();
    while let Some(arg) = it.next() {
        // `--port=8080` 形式もそのまま受ける
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with('-') => (n.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |flag: &str| -> Result<String, Cli> {
            match inline.clone().or_else(|| it.next()) {
                Some(v) => Ok(v),
                None => Err(Cli::Print(
                    format!("rust-http-proxy: {} needs a value\n\n{}", flag, USAGE),
                    2,
                )),
            }
        };
        let set = |out: &mut Vec<(String, String)>, k: &str, v: String| {
            out.retain(|(key, _)| key != k);
            out.push((k.to_string(), v));
        };
        match name.as_str() {
            "-h" | "--help" => return Cli::Print(USAGE.to_string(), 0),
            "-V" | "--version" => {
                return Cli::Print(format!("rust-http-proxy {}", env!("CARGO_PKG_VERSION")), 0);
            }
            "-p" | "--port" => match value("--port") {
                Ok(v) => set(&mut out, "SERVER_PORT", v),
                Err(e) => return e,
            },
            "--bind" => match value("--bind") {
                Ok(v) => set(&mut out, "PROXY_BIND", v),
                Err(e) => return e,
            },
            "--no-cache" => set(&mut out, "PROXY_CACHE_ENABLED", "off".to_string()),
            "--quiet" => set(&mut out, "PROXY_LOG_LEVEL", "warn".to_string()),
            "--lite" => set(&mut out, "PROXY_PROFILE", "lite".to_string()),
            other => {
                return Cli::Print(
                    format!("rust-http-proxy: unknown option '{}'\n\n{}", other, USAGE),
                    2,
                );
            }
        }
    }
    Cli::Run(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Cli {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn maps_options_to_environment_variables() {
        let Cli::Run(vars) = parse_str(&["-p", "3128", "--lite"]) else {
            panic!("should run");
        };
        assert_eq!(
            vars,
            vec![
                ("SERVER_PORT".to_string(), "3128".to_string()),
                ("PROXY_PROFILE".to_string(), "lite".to_string()),
            ]
        );
        let Cli::Run(vars) = parse_str(&[
            "--port=8080",
            "--bind",
            "127.0.0.1",
            "--no-cache",
            "--quiet",
        ]) else {
            panic!("should run");
        };
        assert_eq!(vars.len(), 4);
        assert!(vars.contains(&("PROXY_CACHE_ENABLED".to_string(), "off".to_string())));
        assert!(vars.contains(&("PROXY_LOG_LEVEL".to_string(), "warn".to_string())));
        assert!(vars.contains(&("PROXY_BIND".to_string(), "127.0.0.1".to_string())));
        // 後から来た方が勝つ
        let Cli::Run(vars) = parse_str(&["-p", "1", "-p", "2"]) else {
            panic!("should run");
        };
        assert_eq!(vars, vec![("SERVER_PORT".to_string(), "2".to_string())]);
    }

    #[test]
    fn help_and_version_exit_zero() {
        match parse_str(&["--help"]) {
            Cli::Print(msg, 0) => assert!(msg.contains("usage:")),
            other => panic!("{:?}", other),
        }
        match parse_str(&["-V"]) {
            Cli::Print(msg, 0) => assert!(msg.starts_with("rust-http-proxy ")),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn unknown_options_and_missing_values_exit_two() {
        match parse_str(&["--nope"]) {
            Cli::Print(msg, 2) => assert!(msg.contains("unknown option '--nope'")),
            other => panic!("{:?}", other),
        }
        match parse_str(&["--port"]) {
            Cli::Print(msg, 2) => assert!(msg.contains("--port needs a value")),
            other => panic!("{:?}", other),
        }
        assert_eq!(parse_str(&[]), Cli::Run(Vec::new()));
    }
}
