//! 要求ごとの記録 ([`Metrics::record`]。T.34.26 で `metrics.rs` から移した)。
//!
//! 行を動かしただけで中身は変えていない。

use super::host_table::{evict_oldest_host, web_port};
use super::*;

impl Metrics {
    /// ホスト別に 1 要求を数える (応答時間なし)。
    pub fn record_host(&self, host: &str, outcome: HostOutcome, bytes: u64) {
        self.record(host, outcome, bytes, None, &Detail::default());
    }

    /// ホスト別に 1 要求と応答時間を数える。
    pub fn record_host_timed(&self, host: &str, outcome: HostOutcome, bytes: u64, took: Duration) {
        self.record(host, outcome, bytes, Some(took), &Detail::default());
    }

    /// [`record_host_timed`](Self::record_host_timed) に内訳を添えた版 (T12.4 (2))。
    /// 内訳は**この関数が取る鍵の内側**でしか触らないので、原子操作は増えない。
    pub fn record_host_detail(
        &self,
        host: &str,
        outcome: HostOutcome,
        bytes: u64,
        took: Option<Duration>,
        detail: Detail,
    ) {
        self.record(host, outcome, bytes, took, &detail);
    }

    fn record(
        &self,
        host: &str,
        outcome: HostOutcome,
        bytes: u64,
        took: Option<Duration>,
        detail: &Detail,
    ) {
        // 起動時の自己ベンチ (T14.43) が自分で打った要求は `/hosts` にも窓にも入れない。
        // 入れると 20,000 要求ぶんの行・分位点・段階が実トラフィックの統計に混ざり、
        // デプロイ先の `/status` と `/history` が起動直後の 3 秒に支配される。
        // 費用は自己ベンチが回っていないときの原子の読み 1 回 (鍵も時計も触る前)
        if crate::selfbench::is_target(host) {
            return;
        }
        // 壁時計はここで 1 回だけ読む (ホスト別の `last_seen` と直近の標本 (T14.31) で
        // 使い回す。**読む回数は今までと同じ 1 回**)
        let now = crate::cache::now_epoch();
        // 取り合いを数える (T14.3 (3))。空いていれば `locked` と同じ費用
        let mut hosts = self
            .hosts
            .locked_counted(&crate::sync::LOCK_CONTENDED[crate::sync::LOCK_STATS]);
        let hosts = &mut *hosts;
        // 鍵の種類は 1 回だけ見る (CONNECT のホスト別統計の鍵は `connect://` で始まる。
        // `tunnel::report`。前綴りを見るだけで済むので、呼び出し側に旗を持たせない)
        let connect = host.starts_with("connect://");
        let counted = connect || !(host.starts_with("blocked://") || host.starts_with("loop://"));
        // 窓と時系列に入れる値 (ms)。**1 回だけ作る** (以前は Interval 2 つで 2 回作っていた。T14.22)
        let ms = took.map(|d| {
            detail
                .first_byte_ms
                .unwrap_or_else(|| d.as_millis().min(u64::MAX as u128) as u64)
        });
        // **エラーで終わった 1 件は、全体の確立 / 初バイトの窓・直近の標本・`wait`・段階の窓に
        // 入れない** (T20.4)。失敗した CONNECT が渡してくるのは「失敗するまでの時間」で、
        // 繋がらない宛先への再試行が続くと (2026-10-08 の実測: 接続の 17.2% が 1 本 約 1.3 秒)
        // `recent_quantiles.connect` の p90 も `/history` の確立の p95 も秒になって、
        // 速さの物差しにならなかった。これで `connects` / `forwards` は「エラーにならなかった
        // 本数」になり、`/slo` の `attempts = connects + forwards + errors` が 2 重に数えない。
        // 失敗は `errors` / `errors_by_cause` / `/errors` の個票 / **ホスト別の行**
        // (下の `stats.count`。「その相手に掛かった時間」なので失敗も入れたまま) に残る。
        // 比較はここで 1 回だけ (下の `errors` の分岐が元から見ていたものを使い回す)
        let failed = outcome == HostOutcome::Error;
        // 「確立 − カーネルの RTT」(T.28.45)。RTT が読めた CONNECT だけ (forward は 0 で来る)。
        // 確立は `dns + connect` (入口の ACL が先に引いた名前解決も入る = `/recent` の
        // `ms.dns + ms.connect`)。us で引いて ms に**切り上げ** (5.2 ms を 5 に丸めて閾を
        // 通さない)、負は 0。引き算 1 回で、時計もシステムコールも増えない
        // T.29.12: 名前解決を除いた `connect − RTT` も同じ引き算でもう 1 つ (`/slo` の確立の 2 つが読む。
        // 確立の外れの大半はリゾルバの遅さだった = 10-26 の 36 本で excess の 77%)
        let excess = (detail.origin_rtt_us > 0).then(|| {
            let rtt_us = detail.origin_rtt_us as u64;
            let minus_rtt = |ms: u64| {
                ms.saturating_mul(1_000)
                    .saturating_sub(rtt_us)
                    .div_ceil(1_000)
            };
            (
                minus_rtt(detail.dns_ms.saturating_add(detail.connect_ms)),
                minus_rtt(detail.connect_ms),
            )
        });
        let dns_failed = detail.cause == Some(ErrCause::Dns);
        // 443・80 以外の宛先への試み (T.30.3。`/slo` の `error_rate` の分子と分母から引く)。
        // 試み = 窓に入る成功か、原因 `dns` 以外のエラー (`/slo` の `attempts` と同じ数え方)。
        // ポートは鍵の末尾 4 バイトを比べるだけ ([`web_port`]。解かない)。成功の経路では
        // 比較 2 回で、確保もシステムコールも増えない
        let other_port = if failed {
            !dns_failed
        } else {
            ms.is_some() && counted
        } && !web_port(host);
        // 名前解決に成功した 443・80 宛てのエラー (T.30.4。いちばん多い宛先を `/slo` が外す)。
        // **エラーの経路だけ**: 成功は `failed` の比較 1 回で抜ける (`web_port` は T.30.3 の
        // `other_port` が成功のときだけ読んでいる)
        let web_error = failed && !dns_failed && web_port(host);
        if failed {
            hosts.dests.roll(&mut hosts.map, now);
        }
        // 全体の合計も同じ鍵の内側で足す (原子操作を増やさない)
        for iv in [&mut hosts.total, &mut hosts.interval] {
            iv.dns_misses += detail.dns_misses;
            iv.dns_ms_sum += detail.dns_ms;
            if dns_failed {
                iv.dns_fail_misses += detail.dns_misses;
                iv.dns_fail_ms_sum += detail.dns_ms;
            }
            if let Some(c) = detail.cause {
                iv.errors_by_cause[c as usize] += 1;
            }
            // 成功の経路の分岐は今までと同じ数 (`errors` を足すかどうかの 1 回を、
            // 窓に入れるかどうかと兼ねている)
            if other_port {
                iv.other_port_attempts += 1;
                iv.other_port_errors += failed as u64;
            }
            if failed {
                iv.errors += 1;
            } else if let Some(ms) = ms {
                if connect {
                    iv.connect.observe(ms);
                    if let Some((x, c)) = excess {
                        iv.excess.observe(x);
                        iv.conn_excess.observe(c);
                    }
                } else if counted {
                    iv.forward.observe(ms);
                }
            }
        }
        // 443・80 宛ての、名前解決に成功したミス (T.33.3。`dns_slow` が読む)。ふつうの要求は名前がキャッシュに
        // 当たってミスが 0 なので、比較 1 回で抜ける (上の繰り返しの中に置くと、成功の経路が +12 Ir だった)
        if detail.dns_misses != 0 && !dns_failed && web_port(host) {
            for iv in [&mut hosts.total, &mut hosts.interval] {
                iv.web_dns_misses += detail.dns_misses;
                iv.web_dns_ms_sum += detail.dns_ms;
            }
        }
        // 段階の窓 (T14.3 (1)) と、直近 1,024 本の標本そのもの (T14.31)。どちらも
        // `--lite` では時計を読んでいないので触らない。**旗も分岐も 1 つにまとめてある**
        // ので、`--lite` の経路には 1 命令も足していない。同じ鍵の内側なので、
        // 原子操作も鍵の取り直しも増えない。**`failed` は最後に見る** (`--lite` は
        // `profile::on()` で抜けるので、そこまでの命令は今までと同じ。T20.4)
        if let Some(d) = took
            && counted
            && crate::profile::on()
            && !failed
        {
            // 直近の標本は **us のまま**入れる (12 段の区間では 1 ms 単位で読めない)。
            // forward の初バイトは元が ms 刻みなので ×1,000 するだけ、CONNECT の確立は
            // `Duration` のまま来るので us が出る (ベンチの p50 と ±0.05 ms で比べる値)
            let us = match detail.first_byte_ms {
                Some(fb) => fb.saturating_mul(1_000).min(u32::MAX as u64) as u32,
                None => d.as_micros().min(u32::MAX as u128) as u32,
            };
            if connect {
                hosts.stages.observe_connect(detail);
                hosts.quantiles.connect.observe(us, now);
                // **利用者が待つ時間** = `queue + client_read + dns + connect` (T15.0 (2))。
                // 4 つがそろうのはここだけ。`--lite` はこの分岐に入らない
                let total_ms = ms.unwrap_or(0);
                // `connect_ms` は「全体 − 窓の内側の名前解決」なので、差が窓の内側のぶん
                let dns_inside = total_ms.saturating_sub(detail.connect_ms);
                let head_ms = (detail.stages.queue as u64 + detail.stages.client_read as u64)
                    .saturating_add(detail.dns_ms)
                    .saturating_sub(dns_inside);
                // 窓 (ms) は素直に 4 段の和
                let wait_ms = head_ms.saturating_add(total_ms);
                // 環は **us のまま**: 確立の段だけ `d` の us を使う (ms に丸めると
                // 1 ms 未満の確立が 0 に潰れて `connect` の環と比べられなくなる)。
                // これで標本ごとに必ず `wait ≥ connect` になる
                let wait_us = head_ms
                    .saturating_mul(1_000)
                    .saturating_add(us as u64)
                    .min(u32::MAX as u64) as u32;
                hosts.quantiles.wait.observe(wait_us, now);
                hosts.total.wait.observe(wait_ms);
                hosts.interval.wait.observe(wait_ms);
            } else {
                hosts.stages.observe_forward(detail);
                hosts.quantiles.forward.observe(us, now);
            }
        }
        // CONNECT のホストと SNI が食い違った本数の合計 (`/status`。T14.38)。
        // 旗が立つのはトンネルの終わりだけなので、ここは分岐 1 回 (原子は触らない)
        if detail.sni_mismatch {
            self.sni_mismatches.fetch_add(1, Ordering::Relaxed);
        }
        // 確立までの SYN の再送の合計 (`/status`。T14.46)。再送は稀なので、
        // 普通の接続はこの分岐 1 回で終わる (原子は触らない)
        if detail.syn_retrans != 0 {
            self.syn_retrans_total
                .fetch_add(detail.syn_retrans as u64, Ordering::Relaxed);
        }
        // 既にある行はキーを作り直さない (毎要求の String 確保をなくす)
        if let Some(stats) = hosts.map.get_mut(host) {
            stats.count(now, outcome, bytes, took, detail);
            if failed {
                hosts.dests.on_error(stats, host, took.is_some(), web_error);
            }
            // 上位 16 ホストなら時系列にも 1 標本ぶん (T14.22)。旗が無ければ分岐 1 回で終わり
            if let Some(slot) = stats.series_slot {
                hosts
                    .series
                    .add(slot, ms.unwrap_or(0), detail.dns_ms, failed);
            }
            return;
        }
        // 満杯なら `last_seen` がいちばん古い行を 1 つ捨てて (通算は `other` に足す) 新しい鍵を入れる
        // (T.32.10。利用者の決定 (a))。捨てられる行が無いときだけ今までどおり `other` に畳む
        let key = if hosts.map.len() < MAX_HOSTS || evict_oldest_host(hosts) {
            host.to_string()
        } else {
            "other".to_string()
        };
        let stats = hosts.map.entry(key).or_default();
        stats.count(now, outcome, bytes, took, detail);
        if failed {
            // `/slo` の宛先は畳まれても本当の鍵で数える (T.32.10 (c))
            hosts.dests.on_error(stats, host, took.is_some(), web_error);
        }
    }
}
