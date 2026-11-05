//! 再読込で当てた `PROXY_ENDPOINTS_READONLY`・`PROXY_ALLOW_CLIENTS`・`PROXY_MAX_CONNS_PER_CLIENT` の
//! `/config` の `source` が、`.env` に追随する (T.34.151)。
//!
//! 0.34.x までは、この 3 つは値を当てても出どころを写しておらず、`.env` に書いて再読込しても
//! `/config` の `source` が `default` のままだった (他の当てる項目は `env_file` になる)。

mod common;
use common::*;

const KEYS: [&str; 3] = [
    "PROXY_ENDPOINTS_READONLY",
    "PROXY_ALLOW_CLIENTS",
    "PROXY_MAX_CONNS_PER_CLIENT",
];

/// `/config` の `"KEY":{"value":…,"source":"…"}` の `source`。
fn source_of(json: &str, key: &str) -> String {
    let at = json
        .find(&format!("\"{}\":{{", key))
        .unwrap_or_else(|| panic!("{} が無い: {}", key, json));
    let row = &json[at..];
    let row = &row[..row.find('}').unwrap()];
    let s = row
        .split_once("\"source\":\"")
        .unwrap_or_else(|| panic!("{}", row))
        .1;
    s[..s.find('"').unwrap()].to_string()
}

#[test]
fn test_integration_reloaded_access_keys_take_their_source_from_the_env_file() {
    let dir = std::env::temp_dir().join(format!("rhp-t34151-src-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let base = "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_PROFILE=lite\nPROXY_LOG_LEVEL=info\n";
    std::fs::write(dir.join(".env"), base).unwrap();
    let proxy = ProxyProcess::start(&dir);
    let json = endpoint_json(proxy.port, "/config");
    for key in KEYS {
        assert_eq!(source_of(&json, key), "default", "{}", key);
    }

    std::fs::write(
        dir.join(".env"),
        format!(
            "{}PROXY_ENDPOINTS_READONLY=on\nPROXY_ALLOW_CLIENTS=127.0.0.1\n\
             PROXY_MAX_CONNS_PER_CLIENT=50\n",
            base
        ),
    )
    .unwrap();
    proxy.wait_for_log("PROXY_MAX_CONNS_PER_CLIENT");
    let json = endpoint_json(proxy.port, "/config");
    for key in KEYS {
        assert_eq!(source_of(&json, key), "env_file", "{}", key);
    }

    // `.env` から消すと既定に戻り、出どころも `default` に戻る
    std::fs::write(dir.join(".env"), base).unwrap();
    proxy.wait_for_log("PROXY_MAX_CONNS_PER_CLIENT");
    let json = endpoint_json(proxy.port, "/config");
    for key in KEYS {
        assert_eq!(source_of(&json, key), "default", "{}", key);
    }

    drop(proxy);
    let _ = std::fs::remove_dir_all(&dir);
}
