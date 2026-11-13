//! `config` の単体テスト (T28.47 で `config.rs` が 2,000 行を越えたので別のファイルに移した。中身は同じ)。
//! 子のモジュールの中だけで閉じる試験 (上限の計算・出どころ・keepalive の読み方・一覧の切り方) は
//! それぞれの子に置いてある (T34.3)。ここは `Config` を組んで見るもの。

use super::*;

#[test]
fn test_valid_port() {
    let cfg = Config::new("9090", None, None, Duration::from_secs(10)).unwrap();
    assert_eq!(cfg.port, 9090);
    assert!(cfg.bind_addrs.is_empty());
    assert!(cfg.ipv6, "IPv6 on by default");
    assert_eq!(cfg.timeout, Duration::from_secs(10));
    assert_eq!(cfg.keepalive, Duration::from_secs(15));
    assert_eq!(cfg.pool_per_host, 64);
    assert_eq!(cfg.pool_total, 256);
    assert_eq!(cfg.malloc_arenas, 8);
    assert_eq!(cfg.malloc_mmap_threshold_kb, 256);
    assert_eq!(cfg.max_conns, default_max_conns());
    assert_eq!(cfg.tunnel_idle, Duration::from_secs(300));
    assert!(cfg.connect_ports.is_empty() && cfg.allow_local == AllowLocal::Off);
    // 接続元ごとの同時接続の上限は既定で無効 (T14.13)
    assert_eq!(cfg.max_conns_per_client, 0);
}

#[test]
fn test_cache_defaults() {
    let cfg = Config::new("8080", None, None, Duration::from_secs(30)).unwrap();
    assert!(cfg.cache.mem_limit.is_auto());
    assert!(cfg.cache.disk_limit.is_auto());
    assert_eq!(cfg.cache.mem_limit.target_percent(), Some(100));
    assert!(cfg.cache.enabled);
    assert_eq!(cfg.cache.reserve, proxy_cache::cache::Reserve::Staged);
}

/// `/config` に出る形は `.env` にそのまま書き戻せること (T14.52)。
#[test]
fn tcp_keepalive_spec_round_trips_through_parse() {
    let mut cfg = Config::new("9090", None, None, Duration::from_secs(5)).expect("port");
    assert_eq!(cfg.tcp_keepalive_spec(), "on:60:10:3");
    assert_eq!(
        TcpKeepalive::parse(&cfg.tcp_keepalive_spec()),
        Some(cfg.tcp_keepalive)
    );
    cfg.tcp_keepalive = None;
    assert_eq!(cfg.tcp_keepalive_spec(), "off");
    assert_eq!(TcpKeepalive::parse("off"), Some(None));
}

/// CONNECT の締め切りの規則を 1 本で縛る (T15.6、既定を 5 秒にした T23.3、3 秒にした T28.43)。
///
/// `(PROXY_TIMEOUT_SECS, PROXY_CONNECT_TIMEOUT_SECS) → 実効値` の 6 通り。
/// `Config::new` も同じ関数を通ることを、未設定の 4 通りで一緒に見る。
#[test]
fn connect_timeout_defaults_to_three_seconds() {
    let d = Duration::from_secs;
    // (timeout, 明示の値, 期待する実効値, 何を縛っているか)
    let cases: [(u64, Option<u64>, u64, &str); 6] = [
        (30, None, 3, "既定の 30 秒なら 3 秒に縮む (T28.43 の本体)"),
        (5, None, 3, "5 秒 (T23.3 の既定) の共通の締め切りでも 3 秒"),
        (2, None, 2, "3 秒より短い共通の締め切りには合わせる"),
        (
            0,
            None,
            3,
            "PROXY_TIMEOUT_SECS=0 (無期限) でも未設定なら 3 秒",
        ),
        (30, Some(0), 0, "0 と明示すれば無期限"),
        (
            30,
            Some(20),
            20,
            "書いてあればその値 (3 秒より長くてもよい)",
        ),
    ];
    for (timeout, explicit, want, why) in cases {
        assert_eq!(
            connect_timeout_for(d(timeout), explicit.map(d)),
            d(want),
            "PROXY_TIMEOUT_SECS={} / PROXY_CONNECT_TIMEOUT_SECS={:?}: {}",
            timeout,
            explicit,
            why
        );
        // 未設定の 3 通りは `Config::new` の側も同じ値になる
        // (`Config::new` を呼ぶ既存の所が全部この規則で動くことの土台)
        if explicit.is_none() {
            let cfg = Config::new("9090", None, None, d(timeout)).expect("port");
            assert_eq!(cfg.timeout, d(timeout), "共通の側は動かさない");
            assert_eq!(cfg.connect_timeout, d(want), "Config::new: {}", why);
        }
    }
    assert_eq!(DEFAULT_CONNECT_TIMEOUT, d(3));
}

/// `PROXY_CONNECT_TIMEOUT_SECS` を書いたら、**その値がそのまま**効く (T23.3)。
///
/// 既定を 3 秒にしても、10 秒 (T23.3 の前の既定)・5 秒 (T28.43 の前の既定) や 1 秒を
/// 書いた人の値は書き換えない (戻すのも `.env` の 1 行でできる)。
#[test]
fn connect_timeout_uses_the_explicit_value_as_is() {
    let d = Duration::from_secs;
    // 共通の締め切りが既定 (30 秒) でも、書いてあれば `min(3, timeout)` にしない
    for explicit in [1, 3, 5, 10, 30, 0] {
        let want = d(explicit);
        assert_eq!(
            connect_timeout_for(d(30), Some(want)),
            want,
            "{} 秒",
            explicit
        );
        // 共通の締め切りが明示より短くても、明示の方が勝つ (T15.6 の規則のまま)
        assert_eq!(
            connect_timeout_for(d(2), Some(want)),
            want,
            "{} 秒",
            explicit
        );
    }
    assert_ne!(
        connect_timeout_for(d(30), Some(d(10))),
        DEFAULT_CONNECT_TIMEOUT
    );
}

/// `PROXY_TIMEOUT_SECS` との `min` の関係は既定を変えても動かない (T23.3)。
///
/// 未設定の connect は `min(3, PROXY_TIMEOUT_SECS)`。共通の側 (`cfg.timeout`) は
/// どの通りでも書いた値のままで、connect を短くしても連動しない (効くのは CONNECT だけ)。
#[test]
fn connect_timeout_is_still_the_min_of_the_common_timeout() {
    let d = Duration::from_secs;
    for (timeout, want) in [(30, 3), (6, 3), (5, 3), (4, 3), (3, 3), (2, 2), (1, 1)] {
        let cfg = Config::new("9090", None, None, d(timeout)).expect("port");
        assert_eq!(cfg.connect_timeout, d(want), "timeout={}", timeout);
        assert_eq!(cfg.timeout, d(timeout), "共通の側は書いた値のまま");
    }
    // `0` (無期限) だけは「短い」ではないので既定に落ちる
    let cfg = Config::new("9090", None, None, d(0)).expect("port");
    assert_eq!(
        (cfg.timeout, cfg.connect_timeout),
        (d(0), DEFAULT_CONNECT_TIMEOUT)
    );
}

/// `settings()` (= `/config` と `--check`) に実効値で出る。短くしても
/// `PROXY_TIMEOUT_SECS` の側は動かない (効くのは CONNECT のオリジン接続だけ。T15.6)。
#[test]
fn settings_show_the_connect_timeout() {
    let mut cfg = Config::new("9090", None, None, Duration::from_secs(30)).expect("port");
    let find = |cfg: &Config, key: &str| {
        cfg.settings()
            .iter()
            .find(|s| s.key == key)
            .unwrap_or_else(|| panic!("{} が無い", key))
            .value
            .clone()
    };
    // 既定は 3 秒 (T15.6、T23.3、T28.43)。共通の締め切りは 30 秒のまま
    assert_eq!(find(&cfg, "PROXY_CONNECT_TIMEOUT_SECS"), "3");
    assert_eq!(find(&cfg, "PROXY_TIMEOUT_SECS"), "30");
    // 戻し方 (`.env` に `PROXY_CONNECT_TIMEOUT_SECS=30`) も実効値で出る
    cfg.connect_timeout = connect_timeout_for(cfg.timeout, Some(Duration::from_secs(30)));
    assert_eq!(find(&cfg, "PROXY_CONNECT_TIMEOUT_SECS"), "30");
    assert_eq!(find(&cfg, "PROXY_TIMEOUT_SECS"), "30", "共通の側は動かない");
}

/// 要求行とヘッダーの締め切りの規則 (T28.0。`connect_timeout_for` と同じ形)。
#[test]
fn header_timeout_defaults_to_ten_seconds() {
    let d = Duration::from_secs;
    // (timeout, 明示の値, 期待する実効値)
    for (timeout, explicit, want) in [
        (30, None, 10),
        (3, None, 3),
        (0, None, 10),
        (30, Some(0), 0),
        (30, Some(20), 20),
        (3, Some(20), 20),
    ] {
        assert_eq!(
            header_timeout_for(d(timeout), explicit.map(d)),
            d(want),
            "PROXY_TIMEOUT_SECS={} / PROXY_HEADER_TIMEOUT_SECS={:?}",
            timeout,
            explicit
        );
        if explicit.is_none() {
            let cfg = Config::new("9090", None, None, d(timeout)).expect("port");
            assert_eq!(cfg.header_timeout, d(want));
        }
    }
    assert_eq!(DEFAULT_HEADER_TIMEOUT, d(10));
    let cfg = Config::new("9090", None, None, d(30)).expect("port");
    let shown = cfg
        .settings()
        .iter()
        .find(|s| s.key == "PROXY_HEADER_TIMEOUT_SECS")
        .map(|s| s.value.clone());
    assert_eq!(shown.as_deref(), Some("10"));
}

/// 1 要求目の 1 バイト目を待つ長さは小さい方。`0` は「無し」なのでもう片方 (T28.0)。
#[test]
fn first_read_timeout_is_the_shorter_one() {
    let d = Duration::from_secs;
    for (timeout, header, want) in [
        (30, 10, 10),
        (3, 10, 3),
        (0, 10, 10),
        (30, 0, 30),
        (0, 0, 0),
    ] {
        assert_eq!(
            first_read_timeout(d(timeout), d(header)),
            d(want),
            "timeout={} header={}",
            timeout,
            header
        );
    }
}

/// `PROXY_HE_STAGGER_MS` (T17.6): 範囲 10〜2,000 ms の中だけ受け、外れた値と読めない値は
/// `None` (= 既定の 250 ms のまま)。`settings()` には既定が ms で出る。
#[test]
fn he_stagger_accepts_only_the_rfc_range() {
    let ms = Duration::from_millis;
    for (v, want) in [
        ("250", Some(ms(250))),
        (" 100 ", Some(ms(100))),
        ("10", Some(ms(10))),
        ("2000", Some(ms(2000))),
        ("9", None),
        ("0", None),
        ("2001", None),
        ("-5", None),
        ("fast", None),
        ("", None),
    ] {
        assert_eq!(parse_stagger_ms(v), want, "PROXY_HE_STAGGER_MS={:?}", v);
    }
    let cfg = Config::new("9090", None, None, Duration::from_secs(30)).expect("port");
    assert_eq!(cfg.he_stagger, crate::net::DEFAULT_STAGGER);
    let s = cfg
        .settings()
        .into_iter()
        .find(|s| s.key == "PROXY_HE_STAGGER_MS")
        .expect("PROXY_HE_STAGGER_MS が無い");
    assert_eq!(s.value, "250");
}

/// keep-warm の既定は止める (T22.4)。`/config` と `--check` にも 0 と出る。
#[test]
fn dns_warm_defaults_to_off() {
    let cfg = Config::new("9090", None, None, Duration::from_secs(30)).expect("port");
    assert_eq!(cfg.dns_warm, Duration::ZERO, "既定は keep-warm を止める");
    assert_eq!(cfg.dns_warm, crate::dns::WARM_DEFAULT);
    let s = cfg
        .settings()
        .into_iter()
        .find(|s| s.key == "PROXY_DNS_WARM_SECS")
        .expect("PROXY_DNS_WARM_SECS が無い");
    assert_eq!(s.value, "0");
}

#[test]
fn settings_show_the_effective_value_of_every_key() {
    let mut cfg =
        Config::new("9090", Some("*.example.com"), None, Duration::from_secs(5)).expect("port");
    cfg.lite = true;
    cfg.connect_ports = PortSet::parse("443,8080-8099");
    cfg.dns_warm = Duration::from_secs(0);
    let settings = cfg.settings();
    let find = |key: &str| {
        settings
            .iter()
            .find(|s| s.key == key)
            .unwrap_or_else(|| panic!("{} が無い", key))
            .value
            .clone()
    };
    assert_eq!(find("SERVER_PORT"), "9090");
    assert_eq!(find("PROXY_TIMEOUT_SECS"), "5");
    assert_eq!(find("PROXY_DNS_WARM_SECS"), "0");
    // serve-stale の窓の既定 (T21.3)
    assert_eq!(find("PROXY_DNS_STALE_SECS"), "21600");
    assert_eq!(find("PROXY_ALLOW_HOSTS"), "[\"*.example.com\"]");
    assert_eq!(find("PROXY_CONNECT_PORTS"), "\"443,8080-8099\"");
    assert_eq!(find("PROXY_PROFILE"), "\"lite\"");
    assert_eq!(find("PROXY_TLS_CA_FILE"), "null");
    assert_eq!(find("PROXY_TCP_KEEPALIVE"), "\"on:60:10:3\"");
    assert_eq!(find("PROXY_MEM_CACHE_MB"), "\"auto\"");
    assert_eq!(find("PROXY_CANARY"), "\"auto\"");
    assert_eq!(find("PROXY_CANARY_SECS"), "60");
    assert_eq!(find("PROXY_PROFILE_SAMPLE_MS"), "1000");
    assert_eq!(find("SERVER_DISK"), find("PROXY_DISK_QUOTA_MB"), "別名");
    // 出どころは既定 (このテストは環境変数を触っていない)
    assert!(
        settings
            .iter()
            .all(|s| s.source == Source::Default || Source::of(s.key) != Source::Default),
        "書かれていないキーに出どころが付いている"
    );
    // 全部のキーが `PROXY_` か `SERVER_` で、重複しない (別名の 2 行を除く)
    assert!(
        settings
            .iter()
            .all(|s| s.key.starts_with("PROXY_") || s.key.starts_with("SERVER_"))
    );
    let mut keys: Vec<&str> = settings.iter().map(|s| s.key).collect();
    let total = keys.len();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), total, "同じキーが 2 度出ている");
    assert!(total >= 50, "{} 件しか出ていない", total);
}

/// `PROXY_PEEK_SNI` の書き戻し (T14.38): `off` / `on` (= 443 だけ) / `on:<port>`。
#[test]
fn peek_sni_spec_writes_off_on_and_the_extra_port() {
    let mut cfg = Config::new("9090", None, None, Duration::from_secs(5)).expect("port");
    assert_eq!(cfg.peek_sni_spec(), "on", "既定は 443 だけ");
    cfg.peek_sni = None;
    assert_eq!(cfg.peek_sni_spec(), "off");
    cfg.peek_sni = Some(8443);
    assert_eq!(cfg.peek_sni_spec(), "on:8443");
}

#[test]
fn test_invalid_port() {
    assert!(Config::new("invalid", None, None, Duration::from_secs(30)).is_err());
    assert!(Config::new("99999", None, None, Duration::from_secs(30)).is_err());
}
