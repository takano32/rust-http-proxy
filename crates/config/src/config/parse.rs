//! 環境変数の値の**純粋な読み方** (文字列を受けて判断だけを返す)。`Config::from_env` は
//! ここで読んだ結果を欄に写し、読めなかったときの WARN と印 (`EnvProblems`) を付ける。

use std::net::IpAddr;
use std::str::FromStr;

/// `PROXY_BIND` のカンマ区切りアドレス (`::`, `0.0.0.0`, `127.0.0.1`, `[::1]`)。空なら自動。
pub fn parse_bind_list(s: &str) -> Result<Vec<IpAddr>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            item.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .map_err(|e| format!("Invalid PROXY_BIND entry '{}': {}", item, e))
        })
        .collect()
}

/// 「数か `auto`」を受ける鍵 (`PROXY_MAX_CONNS` / `PROXY_MAX_THREADS` / `PROXY_LISTEN_BACKLOG`) の読み方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CountSpec<T> {
    /// `auto` (大小は問わない)
    Auto,
    /// 数として読めた
    Number(T),
    /// 空 (前後の空白だけ)。「書いていない」と同じで、WARN も出さない
    Empty,
    /// 数でも `auto` でもない (WARN を出して既定のまま)
    Invalid,
}

/// 前後の空白を落として、`auto` か数か空かを見分ける。
pub(super) fn parse_count_or_auto<T: FromStr>(v: &str) -> CountSpec<T> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("auto") {
        CountSpec::Auto
    } else if let Ok(n) = v.parse::<T>() {
        CountSpec::Number(n)
    } else if v.is_empty() {
        CountSpec::Empty
    } else {
        CountSpec::Invalid
    }
}

/// `PROXY_PEEK_SNI` の読み方 (T14.38): `off` / `on` / `on:<port>`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PeekSniSpec {
    /// `0` / `false` / `off` / `no` (`off:<port>` と書いても `off`): 1 度も覗かない
    Off,
    /// `on:<port>` (1 以上): 443 に加えてこのポートでも覗く
    Port(u16),
    /// `on` と書いたか空 (既定と同じなので欄は動かさない)
    Default,
    /// 読めない綴りと `on:<0 か数でないもの>` (WARN を出して既定のまま)
    Invalid,
}

pub(super) fn parse_peek_sni(v: &str) -> PeekSniSpec {
    let v = v.trim().to_ascii_lowercase();
    let (head, port) = match v.split_once(':') {
        Some((head, port)) => (head, port.trim().parse::<u16>().ok()),
        None => (v.as_str(), None),
    };
    let has_port = v.contains(':');
    if matches!(head.trim(), "0" | "false" | "off" | "no") {
        PeekSniSpec::Off
    } else if let Some(p) = port.filter(|p| *p > 0) {
        PeekSniSpec::Port(p)
    } else if !v.is_empty() && (has_port || !matches!(head.trim(), "1" | "true" | "on" | "yes")) {
        // `on` (既定と同じ) はここに来ない。読めないのは綴りと `on:<0 か数でないもの>`
        PeekSniSpec::Invalid
    } else {
        PeekSniSpec::Default
    }
}

/// カンマ区切りの一覧 (`PROXY_PAC_DIRECT` / `PROXY_BLOCKLIST_EXEMPT`)。前後の空白を落として小文字に、空は捨てる。
pub(super) fn lowercase_list(v: &str) -> Vec<String> {
    v.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// `PROXY_BLOCKLIST_URL` として受ける形 (`http://` か `https://` で始まる)。
pub(super) fn is_http_url(u: &str) -> bool {
    u.starts_with("http://") || u.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bind_list() {
        let list = parse_bind_list(" ::, 0.0.0.0 ,[::1]").unwrap();
        assert_eq!(list.len(), 3);
        assert!(list[0].is_ipv6() && list[1].is_ipv4() && list[2].is_loopback());
        assert!(parse_bind_list("nope").is_err());
        assert!(parse_bind_list("").unwrap().is_empty());
    }

    /// `auto` は大小を問わず、空は「書いていない」、それ以外の読めない値だけが WARN の対象。
    #[test]
    fn count_or_auto_tells_auto_numbers_empty_and_junk_apart() {
        assert_eq!(parse_count_or_auto::<usize>("auto"), CountSpec::Auto);
        assert_eq!(parse_count_or_auto::<usize>(" AUTO "), CountSpec::Auto);
        assert_eq!(
            parse_count_or_auto::<usize>(" 240 "),
            CountSpec::Number(240)
        );
        assert_eq!(parse_count_or_auto::<u32>("0"), CountSpec::Number(0));
        assert_eq!(parse_count_or_auto::<u32>("   "), CountSpec::Empty);
        assert_eq!(parse_count_or_auto::<u32>("many"), CountSpec::Invalid);
        assert_eq!(parse_count_or_auto::<u32>("-1"), CountSpec::Invalid);
        assert_eq!(parse_count_or_auto::<u32>("4294967296"), CountSpec::Invalid);
    }

    /// `PROXY_PEEK_SNI` (T14.38): `off` / `on` / `on:<port>` と、読めない書き方。
    #[test]
    fn peek_sni_reads_off_on_and_a_port() {
        for off in ["off", "OFF", "0", "false", "no", " off ", "off:8443"] {
            assert_eq!(parse_peek_sni(off), PeekSniSpec::Off, "{:?}", off);
        }
        for on in ["on", "ON", "1", "true", "yes", "", "  "] {
            assert_eq!(parse_peek_sni(on), PeekSniSpec::Default, "{:?}", on);
        }
        assert_eq!(parse_peek_sni("on:8443"), PeekSniSpec::Port(8443));
        assert_eq!(parse_peek_sni(" on : 443 "), PeekSniSpec::Port(443));
        // 頭の綴りは見ず、ポートが読めればそのポート (今までどおり)
        assert_eq!(parse_peek_sni("junk:8443"), PeekSniSpec::Port(8443));
        for bad in [
            "on:0",
            "on:abc",
            "on:70000",
            "yes:",
            "maybe",
            "sometimes:443x",
        ] {
            assert_eq!(parse_peek_sni(bad), PeekSniSpec::Invalid, "{:?}", bad);
        }
    }

    #[test]
    fn lists_are_trimmed_lowercased_and_pruned() {
        assert_eq!(
            lowercase_list(" A.Example.com, ,b.example.COM,"),
            vec!["a.example.com", "b.example.com"]
        );
        assert!(lowercase_list("").is_empty());
        assert!(is_http_url("http://x/hosts") && is_http_url("https://x/hosts"));
        assert!(!is_http_url("ftp://x") && !is_http_url("HTTP://x") && !is_http_url(""));
    }
}
