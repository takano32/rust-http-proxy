//! `/healthz` — **本当の健康診断** (T14.12)。
//!
//! 以前は `/status` の写し (同じ 20 KB の JSON、いつでも 200) だったので、
//! Pterodactyl やモニタが「生きているか」を 200 / 503 で判断できなかった。
//!
//! 7 つ調べて、**`fatal` な検査**が 1 つでも偽なら **503** を返す。読めないもの
//! (Linux 以外、`/proc/net` の無いコンテナ、状態ファイル無し、まだ名前解決をしていない、
//! **起動から 5 分に満たない** CPU の絞り) は `null` で、**検査に入れない**
//! (分からないことを理由に落とさない)。
//!
//! **`fatal` でない検査** (T15.0 (6) の `cpu`) は本文の `ok` を偽にするが、状態は 200 の
//! まま。CPU の絞りは「遅いが動いている」状態で、パネルが 503 で再起動をかける作りだと
//! **再起動の輪に入る**ため (503 にするかは利用者が決める)。JSON では `"fatal":false` の
//! ときだけその鍵が出る (**無い = `fatal`**。既存の検査の形は 1 バイトも変わらない)。
//!
//! 応答は `/status` と別の**軽い JSON** (1 KiB 弱)。監視が 5 秒ごとに叩いても、
//! 読むのは `/proc/self/fd` を 1 回と、5 秒の標本が残したメモリ上の窓だけ。

use std::fmt::Write as _;
use std::sync::atomic::Ordering;

use super::Endpoint;
use crate::kernel;
use crate::metrics::SCHEMA_HEAD;

/// 記述子がこの割合を超えたら偽 (`fds` < `max_fds` の 90%)。
const FD_HEADROOM_NUM: u64 = 9;
const FD_HEADROOM_DEN: u64 = 10;

/// 名前解決の最後のミスがこれ以上かかっていたらリゾルバが死んでいるとみなす (ms)。
const RESOLVER_SLOW_MS: u64 = 2_000;

/// 1 つの検査。`None` = この環境では調べられない (`null` を出して `ok` に入れない)。
struct Check {
    name: &'static str,
    ok: Option<bool>,
    /// `"key":value` の並び (`ok` の隣に出す数字)
    detail: String,
    /// この検査が偽のとき **503 にするか** (T15.0 (6))。
    /// `false` の検査は本文の `ok` だけを偽にして、状態は 200 のまま
    fatal: bool,
}

impl Check {
    fn new(name: &'static str, ok: bool, detail: String) -> Check {
        Check {
            name,
            ok: Some(ok),
            detail,
            fatal: true,
        }
    }

    /// 偽でも 503 にしない検査 (T15.0 (6))。
    fn soft(name: &'static str, ok: bool, detail: String) -> Check {
        Check {
            fatal: false,
            ..Check::new(name, ok, detail)
        }
    }

    fn skipped(name: &'static str) -> Check {
        Check {
            name,
            ok: None,
            detail: String::new(),
            fatal: true,
        }
    }
}

/// 検査の一覧から `(本文の ok, 200 で出すか)` を決める (T15.0 (6))。
///
/// **本文の `ok` は全部込み、状態は `fatal` な検査だけ**。`None` (この環境では
/// 調べられない) はどちらにも入れない — 分からないことを理由に落とさない。
/// 2 行の取り違え (soft な検査で 503) は口の振舞いを変えてしまうので、
/// ここだけ切り出して単体テストで縛ってある。
fn verdict(checks: &[Check]) -> (bool, bool) {
    let ok = checks.iter().all(|c| c.ok != Some(false));
    let serving = checks.iter().all(|c| !c.fatal || c.ok != Some(false));
    (ok, serving)
}

/// `"checks"` の中身を書く (`{` と `}` は呼ぶ側)。
///
/// `"fatal":false` は **soft な検査のときだけ**出す (**無い = `fatal`**。
/// 既存の検査の形を 1 バイトも変えないため。T15.0 (6))。
fn write_checks(body: &mut String, checks: &[Check]) {
    for (i, c) in checks.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        let _ = write!(body, "\"{}\":", c.name);
        match c.ok {
            Some(v) => {
                let _ = write!(body, "{{\"ok\":{}", v);
                if !c.fatal {
                    body.push_str(",\"fatal\":false");
                }
                if !c.detail.is_empty() {
                    body.push(',');
                    body.push_str(&c.detail);
                }
                body.push('}');
            }
            // 調べられないものは `null` (Linux 以外、状態ファイル無し、ミスがまだ無い)
            None => body.push_str("null"),
        }
    }
}

/// `/healthz` の検査に入る測り (T.34.83)。読むのは [`read`] だけで、判定 ([`checks_for`])
/// はこの値だけから決まる。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Readings {
    /// 自分の待ち受けポート
    port: u16,
    /// 開いている接続の数 (**自分 = いまの `/healthz` の接続を除く**)
    others: u64,
    /// 接続数の上限 (`0` = 無制限)
    max_conns: u64,
    /// 開いている記述子と上限 (`/proc/self/fd` が読めなければ `(0, 0)`)
    fds: u64,
    max_fds: u64,
    /// カーネルと状態ファイルの 5 分の窓
    kernel: kernel::Health,
}

/// いまの値を読む (`/proc/self/fd` を 1 回と、5 秒の標本が残したメモリ上の窓)。
fn read(ep: &Endpoint<'_>) -> Readings {
    let conc = (ep.concurrency)();
    let active = ep.metrics.active_connections.load(Ordering::Relaxed);
    // `/proc/self/fd` はここで 1 回だけ数える (`/status` も同じことをしている)
    let (fds, max_fds) = crate::sysinfo::process_fds().unwrap_or((0, 0));
    Readings {
        port: ep.port,
        // **自分 (いまの `/healthz` の接続) を除く**。上限に当たっている最中でも、
        // この接続は T13.2 の「上限 + 4 本」の枠で届いているので、そのまま比べると
        // 上限に当たっていなくても偽になる
        others: active.saturating_sub(1) as u64,
        max_conns: conc.max_conns as u64,
        fds,
        max_fds,
        kernel: kernel::health(),
    }
}

/// 測りから 7 つの検査を決める (I/O の無い純粋な関数。閾はここだけにある。T.34.83)。
fn checks_for(r: &Readings) -> [Check; 7] {
    [
        // この応答が届いている = 待ち受けは生きていて accept できている
        Check::new("listening", true, format!("\"port\":{}", r.port)),
        match r.max_fds {
            0 => Check::skipped("fds"),
            max => Check::new(
                "fds",
                r.fds * FD_HEADROOM_DEN < max * FD_HEADROOM_NUM,
                format!("\"open\":{},\"max\":{}", r.fds, max),
            ),
        },
        match r.max_conns {
            // 0 = 無制限 (上限が無いので当たりようがない)
            0 => Check::new(
                "connections",
                true,
                format!("\"active\":{},\"max\":0", r.others),
            ),
            max => Check::new(
                "connections",
                r.others < max,
                format!("\"active\":{},\"max\":{}", r.others, max),
            ),
        },
        match r.kernel.state_file_errors_5m {
            None => Check::skipped("state_file"),
            Some(n) => Check::new("state_file", n == 0, format!("\"write_errors_5m\":{}", n)),
        },
        match r.kernel.listen_overflows_5m {
            None => Check::skipped("listen_overflows"),
            Some(n) => Check::new("listen_overflows", n == 0, format!("\"delta_5m\":{}", n)),
        },
        match r.kernel.dns_miss_ms {
            None => Check::skipped("resolver"),
            Some(ms) => Check::new(
                "resolver",
                ms < RESOLVER_SLOW_MS,
                format!("\"last_miss_ms\":{}", ms),
            ),
        },
        // CPU の絞り (T15.0 (6))。**`fatal` ではない**ので 503 にはしない。
        // 27 時間絞られ続けていたのに、この口にも異常の規則にも CPU が無かった
        match (r.kernel.cpu_throttled_ratio(), r.kernel.cpu_throttled_5m) {
            // **窓が 5 分ぶん溜まってから**だけ答える (異常の規則と同じ物差し。
            // `cpu_throttled_5m` は「いちばん新しい標本から 300 秒ぶん」を足すだけなので、
            // 起動 10 秒後 (標本 2 本) でもその 5 秒の割合で `ok:false` を出してしまう)
            (Some(ratio), Some((throttled, periods)))
                if r.kernel
                    .sampled_secs
                    .is_some_and(crate::anomaly::cpu_window_is_full) =>
            {
                Check::soft(
                    "cpu",
                    ratio < crate::anomaly::CPU_THROTTLED,
                    format!(
                        "\"throttled_5m\":{},\"periods_5m\":{},\"percent\":{:.0}",
                        throttled,
                        periods,
                        ratio * 100.0
                    ),
                )
            }
            // cgroup v1 / Linux 以外 / 履歴スレッドが動いていない / まだ 1 期間も過ぎていない /
            // 起動から 5 分に満たない
            _ => Check::skipped("cpu"),
        },
    ]
}

/// `/healthz` の本体。**`fatal` な検査**が偽なら 503 (本文の `ok` は全部込み)。
pub(super) fn healthz(ep: &Endpoint<'_>) -> (u16, &'static str, String) {
    let checks = checks_for(&read(ep));
    let (ok, serving) = verdict(&checks);
    let mut body = String::with_capacity(512);
    // 応答の形の版は**いちばん先頭の鍵** (T14.49)。`ok` は今までどおりその次
    body.push_str(SCHEMA_HEAD);
    let _ = write!(body, "\"ok\":{},\"checks\":{{", ok);
    write_checks(&mut body, &checks);
    let _ = write!(
        body,
        "}},\"uptime_secs\":{},\"version\":\"{}\"}}",
        ep.metrics.start_time.elapsed().as_secs(),
        crate::json::escape(ep.version)
    );
    (if serving { 200 } else { 503 }, "application/json", body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail() -> String {
        "\"n\":1".to_string()
    }

    /// **`fatal` な検査**が偽のときだけ 503 (T15.0 (6))。
    ///
    /// この 2 行 (`ok` と `serving`) はこの単位でいちばん効く振舞いで、取り違えても
    /// 口を叩くテストでは (`cpu` が読める機械でしか偽にならないので) 気づけない。
    #[test]
    fn a_soft_check_turns_ok_false_but_keeps_serving() {
        let all_true = [
            Check::new("listening", true, detail()),
            Check::soft("cpu", true, detail()),
            Check::skipped("fds"),
        ];
        assert_eq!(verdict(&all_true), (true, true), "全部真");

        let soft_false = [
            Check::new("listening", true, detail()),
            // 絞られている = 遅いが動いている。**状態は 200 のまま**
            Check::soft("cpu", false, detail()),
        ];
        assert_eq!(verdict(&soft_false), (false, true), "soft な偽は 200");

        let fatal_false = [
            Check::new("listening", false, detail()),
            Check::soft("cpu", true, detail()),
        ];
        assert_eq!(verdict(&fatal_false), (false, false), "fatal な偽は 503");

        // 調べられないもの (`null`) はどちらにも入れない
        let skipped = [Check::skipped("cpu"), Check::skipped("fds")];
        assert_eq!(verdict(&skipped), (true, true), "`null` では落とさない");
    }

    /// `"fatal":false` は **soft な検査のときだけ**出る (無い = `fatal`)。
    #[test]
    fn only_a_soft_check_prints_the_fatal_key() {
        let mut out = String::new();
        write_checks(
            &mut out,
            &[
                Check::new("listening", true, "\"port\":8080".to_string()),
                Check::soft("cpu", false, "\"percent\":98".to_string()),
                Check::skipped("fds"),
            ],
        );
        assert_eq!(
            out,
            "\"listening\":{\"ok\":true,\"port\":8080},\
             \"cpu\":{\"ok\":false,\"fatal\":false,\"percent\":98},\
             \"fds\":null"
        );
    }

    /// 名前で検査を引く。
    fn check<'c>(checks: &'c [Check; 7], name: &str) -> &'c Check {
        checks.iter().find(|c| c.name == name).expect(name)
    }

    /// 全部が健康な測り (どれも境目より 1 つ手前)。
    fn healthy() -> Readings {
        Readings {
            port: 8080,
            others: 0,
            max_conns: 0,
            fds: 0,
            max_fds: 0,
            kernel: kernel::Health::default(),
        }
    }

    /// 7 つの名前と並びは JSON の鍵の順そのもの (T.34.83)。
    #[test]
    fn the_checks_keep_their_order() {
        let names: Vec<&str> = checks_for(&healthy()).iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            [
                "listening",
                "fds",
                "connections",
                "state_file",
                "listen_overflows",
                "resolver",
                "cpu"
            ]
        );
        // 何も読めなければ listening と connections (上限 0 = 無制限) だけが答える
        let c = checks_for(&healthy());
        assert_eq!(verdict(&c), (true, true));
        for name in ["fds", "state_file", "listen_overflows", "resolver", "cpu"] {
            assert_eq!(check(&c, name).ok, None, "{}", name);
        }
    }

    /// 記述子は上限の 90% に**届いたら**偽 (T.34.83)。
    #[test]
    fn descriptors_fail_at_ninety_percent() {
        let at = |fds| {
            let r = Readings {
                fds,
                max_fds: 100,
                ..healthy()
            };
            check(&checks_for(&r), "fds").ok
        };
        assert_eq!(at(89), Some(true));
        assert_eq!(at(90), Some(false));
        let c = checks_for(&Readings {
            fds: 89,
            max_fds: 100,
            ..healthy()
        });
        assert_eq!(check(&c, "fds").detail, "\"open\":89,\"max\":100");
    }

    /// 接続の上限 0 は無制限 (いつも真)、上限があれば `others < max` (T.34.83)。
    #[test]
    fn connections_fail_only_at_a_real_limit() {
        let at = |others, max_conns| {
            let r = Readings {
                others,
                max_conns,
                ..healthy()
            };
            let c = checks_for(&r);
            (
                check(&c, "connections").ok,
                check(&c, "connections").detail.clone(),
            )
        };
        assert_eq!(
            at(5_000, 0),
            (Some(true), "\"active\":5000,\"max\":0".to_string())
        );
        assert_eq!(at(9, 10).0, Some(true));
        assert_eq!(
            at(10, 10),
            (Some(false), "\"active\":10,\"max\":10".to_string())
        );
    }

    /// 接続の数は**いまの `/healthz` の 1 本を除いて**数える (T.34.83)。
    #[test]
    fn the_health_check_does_not_count_itself() {
        let m = crate::metrics::Metrics::new();
        m.active_connections.store(10, Ordering::Relaxed);
        let cache = crate::cache::Cache::new(crate::cache::CacheConfig::disabled());
        let concurrency = || crate::metrics::Concurrency {
            max_conns: 10,
            max_threads: 0,
            live_threads: 0,
            idle_threads: 0,
            queued_jobs: 0,
        };
        let ep = Endpoint {
            metrics: &m,
            cache: &cache,
            conn_id: 1,
            port: 8080,
            host: None,
            client: None,
            pac_direct: &[],
            lite: false,
            readonly: false,
            version: "test",
            build: "test",
            concurrency: &concurrency,
        };
        let r = read(&ep);
        assert_eq!((r.others, r.max_conns, r.port), (9, 10, 8080));
        // 上限ちょうど 10 本 (うち 1 本は自分) は、まだ上限に当たっていない
        assert_eq!(check(&checks_for(&r), "connections").ok, Some(true));
        // 自分のほかに 10 本あれば偽
        m.active_connections.store(11, Ordering::Relaxed);
        assert_eq!(
            check(&checks_for(&read(&ep)), "connections").ok,
            Some(false)
        );
    }

    /// 状態ファイルの書込エラーと待ち行列の溢れは、5 分に 1 回でも偽 (T.34.83)。
    #[test]
    fn one_state_file_error_or_listen_overflow_fails() {
        for (n, want) in [(0, true), (1, false)] {
            let r = Readings {
                kernel: kernel::Health {
                    state_file_errors_5m: Some(n),
                    listen_overflows_5m: Some(n),
                    ..kernel::Health::default()
                },
                ..healthy()
            };
            let c = checks_for(&r);
            assert_eq!(check(&c, "state_file").ok, Some(want), "{}", n);
            assert_eq!(check(&c, "listen_overflows").ok, Some(want), "{}", n);
            assert_eq!(verdict(&c), (want, want), "どちらも fatal");
        }
    }

    /// 名前解決のミスが 2,000 ms に**届いたら**リゾルバが死んでいるとみなす (T.34.83)。
    #[test]
    fn the_resolver_fails_at_two_seconds() {
        let at = |ms| {
            let r = Readings {
                kernel: kernel::Health {
                    dns_miss_ms: Some(ms),
                    ..kernel::Health::default()
                },
                ..healthy()
            };
            check(&checks_for(&r), "resolver").ok
        };
        assert_eq!(at(1_999), Some(true));
        assert_eq!(at(2_000), Some(false));
    }

    /// CPU の絞りは窓が 5 分ぶん満ちるまで `null`、満ちたら soft な検査 (T.34.83)。
    #[test]
    fn cpu_throttling_waits_for_a_full_window_and_stays_soft() {
        let at = |sampled_secs, throttled| {
            let r = Readings {
                kernel: kernel::Health {
                    cpu_throttled_5m: Some((throttled, 100)),
                    sampled_secs: Some(sampled_secs),
                    ..kernel::Health::default()
                },
                ..healthy()
            };
            let c = checks_for(&r);
            (check(&c, "cpu").ok, check(&c, "cpu").fatal, verdict(&c))
        };
        // 299 秒ではまだ答えない (どれだけ絞られていても)
        assert_eq!(at(299, 100).0, None);
        // 満ちたら割合で決め、偽でも 503 にはしない
        assert_eq!(at(300, 49), (Some(true), false, (true, true)));
        assert_eq!(at(300, 50), (Some(false), false, (false, true)));
        // 期間が 0 なら割合が出ないので `null`
        let r = Readings {
            kernel: kernel::Health {
                cpu_throttled_5m: Some((0, 0)),
                sampled_secs: Some(600),
                ..kernel::Health::default()
            },
            ..healthy()
        };
        assert_eq!(check(&checks_for(&r), "cpu").ok, None);
    }
}
