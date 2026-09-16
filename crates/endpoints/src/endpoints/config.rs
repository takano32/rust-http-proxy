//! `/config` — **効いている設定とその出どころ**と、この環境で何が読めるか (T14.15)。
//!
//! `/status` の `settings` は再読込の回数と最後の結果しか持っていないので、
//! 「いま効いている値は何か」「それは既定なのか、`.env` に書いたのか、パネルが
//! 渡しているのか」が読めなかった。データを後から読む人が**そのときの設定**を
//! 1 枚で確かめられるようにするための口。
//!
//! - `settings`: 全 `PROXY_*` / `SERVER_*` の `{"value":…,"source":"default"|"env"|"env_file"|"cli"}`。
//!   `value` は**いま効いている値**で、`source` はその値が来た層。書いたのに読めない
//!   書き方だったキーは `default` のまま出る (効いていないことが分かる)。
//! - `capabilities`: この環境で何が読めるか ([`crate::sysinfo::capabilities`])。
//! - `reload`: `.env` の監視の状態 (`/status` の `settings` と同じもの)。
//!   **再起動が要る項目**を変えたときは `restart_required` に出るので、
//!   「`.env` に書いたのに `value` が変わらない」の答えがここで付く。
//!
//! **秘密の設定は無い** (認証を入れない方針なので、そもそも鍵を持っていない)。
//! `PROXY_TLS_CA_FILE` はパスだけで、証明書の中身は読まない。

use std::fmt::Write as _;
use std::time::Duration;

use super::Endpoint;
use crate::config::Config;
use crate::metrics::SCHEMA_HEAD;

/// 応答の上限 (64 KiB)。全 `PROXY_*` / `SERVER_*` (約 60 件) を並べても 5 KB 前後だが、
/// 長いパスや一覧が入っても越えないように、他の個票と同じくバイト数でも見張る。
pub const MAX_BODY: usize = 64 * 1024;

/// 末尾 (`},"capabilities":…}`) のために空けておくぶん。
const TRAILER: usize = 2048;

/// `/config` の本文を組み立てる。
pub fn render(ep: &Endpoint<'_>) -> (u16, &'static str, String) {
    (
        200,
        "application/json",
        body_for(live_config().as_ref(), ep.version),
    )
}

/// 効いている設定 (`Live` が無い単体テストでは環境から組み直したもの)。
fn live_config() -> Box<Config> {
    // 効いている設定は**接続ごとに参照しているもの** (再読込で差し替わる) をそのまま読む。
    // `Live` が無いのは単体テストのときだけなので、そのときは環境から組み直す
    match crate::reload::global().map(|l| l.config()) {
        Some(c) => Box::new((*c).clone()),
        None => Box::new(Config::from_env().unwrap_or_else(|_| {
            Config::new("8080", None, None, Duration::from_secs(30)).expect("8080 は必ず読める")
        })),
    }
}

/// 設定 1 つを `"KEY":{"value":…,"source":"…"}` に並べた本文 (`/config` の中身)。
fn body_for(cfg: &Config, version: &str) -> String {
    let mut body = String::with_capacity(8192);
    // 応答の形の版は**いちばん先頭の鍵** (T14.49)
    body.push_str(SCHEMA_HEAD);
    let _ = write!(
        body,
        "\"version\":\"{}\",\"env_file\":{},\"settings\":{{",
        crate::json::escape(version),
        crate::json::quote_opt(
            crate::envfile::loaded_path()
                .or_else(crate::envfile::env_path)
                .map(|p| p.display().to_string())
                .as_deref()
        ),
    );
    let budget = MAX_BODY - TRAILER;
    let (mut n, mut truncated) = (0usize, false);
    for s in cfg.settings() {
        let line = format!(
            "{}\"{}\":{{\"value\":{},\"source\":\"{}\"}}",
            if n > 0 { "," } else { "" },
            s.key,
            s.value,
            s.source.as_str()
        );
        if body.len() + line.len() > budget {
            truncated = true;
            break;
        }
        body.push_str(&line);
        n += 1;
    }
    let _ = write!(
        body,
        "}},\"count\":{},\"truncated\":{},\"capabilities\":{},\"reload\":{}}}",
        n,
        truncated,
        crate::sysinfo::capabilities::status_json(),
        crate::reload::status_json(),
    );
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{Cache, CacheConfig};
    use crate::metrics::{Concurrency, Metrics};

    fn endpoint_json() -> String {
        let metrics = Metrics::new();
        let cache = Cache::new(CacheConfig::disabled());
        let concurrency = || Concurrency::default();
        let ep = Endpoint {
            metrics: &metrics,
            cache: &cache,
            conn_id: 1,
            port: 8080,
            host: None,
            pac_direct: &[],
            lite: false,
            // T14.18 で足った欄 (T14.15 はその前に枝を切っていた)。`/config` は読む口なので
            // 読み取り専用かどうかでは変わらない
            readonly: false,
            version: "0.1.0+test",
            concurrency: &concurrency,
        };
        let (status, kind, body) = render(&ep);
        assert_eq!((status, kind), (200, "application/json"));
        body
    }

    /// 全 `PROXY_*` / `SERVER_*` が並び、応答は 64 KiB 以下 (受け入れ基準)。
    #[test]
    fn lists_every_setting_and_stays_small() {
        let body = endpoint_json();
        assert!(body.len() <= MAX_BODY, "{} B", body.len());
        assert!(body.contains("\"truncated\":false"), "{}", body);
        // README の表にあるキーは全部出す (抜き取りで確かめる)
        for key in [
            "SERVER_PORT",
            "SERVER_MEMORY",
            "SERVER_DISK",
            "PROXY_BIND",
            "PROXY_IPV6",
            "PROXY_TIMEOUT_SECS",
            "PROXY_KEEPALIVE_SECS",
            "PROXY_DNS_TTL_SECS",
            "PROXY_DNS_NEGATIVE_SECS",
            "PROXY_DNS_WARM_SECS",
            "PROXY_MAX_CONNS",
            "PROXY_MAX_CONNS_PER_CLIENT",
            "PROXY_MAX_THREADS",
            "PROXY_TLS_CA_FILE",
            "PROXY_LOG_LEVEL",
            "PROXY_CACHE_ENABLED",
            "PROXY_CACHE_RESERVE",
            "PROXY_NEGATIVE_TTL_SECS",
        ] {
            assert!(body.contains(&format!("\"{}\":{{", key)), "{} が無い", key);
        }
        // 数の欄は数として出る (`"value":"30"` ではない)
        assert!(
            body.contains("\"PROXY_DNS_TTL_SECS\":{\"value\":60,\"source\":\""),
            "{}",
            body
        );
        // 真偽は真偽、一覧は配列、無いパスは null
        assert!(body.contains("\"PROXY_IPV6\":{\"value\":true,"), "{}", body);
        assert!(body.contains("\"PROXY_BIND\":{\"value\":[],"), "{}", body);
        assert!(
            body.contains("\"PROXY_TLS_CA_FILE\":{\"value\":null,"),
            "{}",
            body
        );
        // この環境で何が読めるかも同じ 1 枚に出る
        assert!(body.contains("\"capabilities\":"), "{}", body);
        assert!(body.contains("\"reload\":"), "{}", body);
    }

    /// 桁を振り切った値・長い一覧でも本文が 64 KiB を越えないこと (受け入れ基準)。
    ///
    /// 一覧 (`PROXY_PAC_DIRECT` など) は 1 件 1 KiB で切るので、**どの設定も落ちない**
    /// (件数で打ち切る他の個票と違い、`/config` は全部のキーが並ぶことに意味がある)。
    #[test]
    fn the_response_stays_under_64_kib_with_long_values() {
        let mut cfg =
            Config::new("65535", None, None, Duration::from_secs(u32::MAX as u64)).expect("port");
        let long = "n".repeat(255);
        cfg.tls_ca_file = Some(std::path::PathBuf::from(format!("/{}/{}.pem", long, long)));
        cfg.blocklist_file = Some(std::path::PathBuf::from(format!("/{}/hosts", long)));
        cfg.blocklist_url = Some(format!("https://example.com/{}", long));
        cfg.blocklist_exempt = (0..1000)
            .map(|i| format!("{}-{}.example.net", long, i))
            .collect();
        cfg.pac_direct = cfg.blocklist_exempt.clone();
        cfg.acl = crate::acl::AclConfig::new(
            Some(&cfg.pac_direct.join(",")),
            Some(&cfg.pac_direct.join(",")),
        );
        cfg.bind_addrs = (0..16)
            .map(|i| std::net::IpAddr::from([0x2001, 0xdb8, 0, 0, 0, 0, 0, i]))
            .collect();
        cfg.connect_ports = crate::acl::PortSet::parse("1-2,3-4,5-6,7-8,9-10,11-12");
        let body = body_for(&cfg, "0.1.0+test");
        assert!(body.len() <= MAX_BODY, "最悪の値で {} B", body.len());
        assert!(body.contains("\"truncated\":false"), "設定が落ちている");
        // 長い一覧は途中で切って「あと何件か」を出す
        assert!(body.contains("more\"]"), "{}", &body[..600]);
        // 最後のキー (キャッシュの列) まで残っていること
        assert!(body.contains("\"SERVER_MEMORY\":{"), "末尾が落ちている");
        println!(
            "settings {} 件 (最悪の値): {} B (上限 {} B)",
            cfg.settings().len(),
            body.len(),
            MAX_BODY
        );
    }
}
