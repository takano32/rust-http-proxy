//! `/healthz` — **本当の健康診断** (T14.12)。
//!
//! 以前は `/status` の写し (同じ 20 KB の JSON、いつでも 200) だったので、
//! Pterodactyl やモニタが「生きているか」を 200 / 503 で判断できなかった。
//!
//! 7 つ調べて、**`fatal` な検査**が 1 つでも偽なら **503** を返す。読めないもの
//! (Linux 以外、`/proc/net` の無いコンテナ、状態ファイル無し、まだ名前解決をしていない) は
//! `null` で、**検査に入れない** (分からないことを理由に落とさない)。
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

/// `/healthz` の本体。**`fatal` な検査**が偽なら 503 (本文の `ok` は全部込み)。
pub(super) fn healthz(ep: &Endpoint<'_>) -> (u16, &'static str, String) {
    let conc = (ep.concurrency)();
    let active = ep.metrics.active_connections.load(Ordering::Relaxed);
    // **自分 (いまの `/healthz` の接続) を除く**。上限に当たっている最中でも、
    // この接続は T13.2 の「上限 + 4 本」の枠で届いているので、そのまま比べると
    // 上限に当たっていなくても偽になる
    let others = active.saturating_sub(1) as u64;
    let max_conns = conc.max_conns as u64;
    // `/proc/self/fd` はここで 1 回だけ数える (`/status` も同じことをしている)
    let (fds, max_fds) = crate::sysinfo::process_fds().unwrap_or((0, 0));
    let h = kernel::health();

    let checks = [
        // この応答が届いている = 待ち受けは生きていて accept できている
        Check::new("listening", true, format!("\"port\":{}", ep.port)),
        match max_fds {
            0 => Check::skipped("fds"),
            max => Check::new(
                "fds",
                fds * FD_HEADROOM_DEN < max * FD_HEADROOM_NUM,
                format!("\"open\":{},\"max\":{}", fds, max),
            ),
        },
        match max_conns {
            // 0 = 無制限 (上限が無いので当たりようがない)
            0 => Check::new(
                "connections",
                true,
                format!("\"active\":{},\"max\":0", others),
            ),
            max => Check::new(
                "connections",
                others < max,
                format!("\"active\":{},\"max\":{}", others, max),
            ),
        },
        match h.state_file_errors_5m {
            None => Check::skipped("state_file"),
            Some(n) => Check::new("state_file", n == 0, format!("\"write_errors_5m\":{}", n)),
        },
        match h.listen_overflows_5m {
            None => Check::skipped("listen_overflows"),
            Some(n) => Check::new("listen_overflows", n == 0, format!("\"delta_5m\":{}", n)),
        },
        match h.dns_miss_ms {
            None => Check::skipped("resolver"),
            Some(ms) => Check::new(
                "resolver",
                ms < RESOLVER_SLOW_MS,
                format!("\"last_miss_ms\":{}", ms),
            ),
        },
        // CPU の絞り (T15.0 (6))。**`fatal` ではない**ので 503 にはしない。
        // 27 時間絞られ続けていたのに、この口にも異常の規則にも CPU が無かった
        match (h.cpu_throttled_ratio(), h.cpu_throttled_5m) {
            (Some(ratio), Some((throttled, periods))) => Check::soft(
                "cpu",
                ratio < crate::anomaly::CPU_THROTTLED,
                format!(
                    "\"throttled_5m\":{},\"periods_5m\":{},\"percent\":{:.0}",
                    throttled,
                    periods,
                    ratio * 100.0
                ),
            ),
            // cgroup v1 / Linux 以外 / 履歴スレッドが動いていない / まだ 1 期間も過ぎていない
            _ => Check::skipped("cpu"),
        },
    ];

    // 本文の `ok` は全部込み、**状態は `fatal` な検査だけ**で決める (T15.0 (6))
    let ok = checks.iter().all(|c| c.ok != Some(false));
    let serving = checks.iter().all(|c| !c.fatal || c.ok != Some(false));
    let mut body = String::with_capacity(512);
    // 応答の形の版は**いちばん先頭の鍵** (T14.49)。`ok` は今までどおりその次
    body.push_str(SCHEMA_HEAD);
    let _ = write!(body, "\"ok\":{},\"checks\":{{", ok);
    for (i, c) in checks.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        let _ = write!(body, "\"{}\":", c.name);
        match c.ok {
            Some(v) => {
                let _ = write!(body, "{{\"ok\":{}", v);
                // **無い = `fatal`** (既存の検査の形を 1 バイトも変えないため。T15.0 (6))
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
    let _ = write!(
        body,
        "}},\"uptime_secs\":{},\"version\":\"{}\"}}",
        ep.metrics.start_time.elapsed().as_secs(),
        crate::json::escape(ep.version)
    );
    (if serving { 200 } else { 503 }, "application/json", body)
}
