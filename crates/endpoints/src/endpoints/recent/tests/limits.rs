//! 個票のリング (`/errors` `/connections` `/recent` `/bursts`) の応答が
//! [`MAX_BODY`] (256 KiB) に収まること (T.34.86 で tests.rs から分けた)。

use super::*;

/// 500 件の最悪 (宛先も接続元も上限いっぱい) でも 256 KiB に収まること。
#[test]
fn the_errors_response_stays_under_256_kib() {
    let m = Metrics::new();
    let long = "x".repeat(300);
    for i in 0..(MAX_ERRORS + 20) {
        // 宛先は 1 件ずつ変える (同じ失敗は 1 行にまとまるので、500 行にならない。T20.3)。
        // 頭の 3 桁が違うだけで、長さは今までどおり上限いっぱいに切られる
        m.record_error(
            true,
            &format!("{:03}{}", i, long),
            "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6",
            502,
            &Detail {
                cause: Some(ErrCause::Unreachable),
                dns_ms: u64::MAX,
                connect_ms: u64::MAX,
                ..Detail::default()
            },
        );
    }
    let ep_metrics = m;
    let (mut entries, total) = ep_metrics.errors.recent(MAX_ERRORS);
    // `repeats` も最悪の桁数にする (T20.3 で各行の末尾に足した)
    for e in &mut entries {
        e.repeats = u32::MAX;
    }
    assert_eq!(entries.len(), MAX_ERRORS);
    assert_eq!(total, MAX_ERRORS as u64 + 20);
    let mut body = String::from(SCHEMA_HEAD) + "\"errors\":";
    let (shown, cut) = array_within(&mut body, entries.iter().map(|e| e.to_json()));
    body.push('}');
    assert_eq!(shown, MAX_ERRORS, "500 件が全部入ること");
    assert!(!cut);
    assert!(body.len() <= MAX_BODY, "{} B", body.len());
    println!(
        "errors 500 件の応答: {} B (上限 {} B)",
        body.len(),
        MAX_BODY
    );
}

/// 240 本 (デプロイ先の上限) でも、1,000 本でも 256 KiB に収まること。
///
/// **余りは 1 行あたり 5 B しか無い** (欄を足す前の実測 1,000 本で 256,918 B、
/// [`array_within`] の予算は 261,632 B)。T15.0 (4) で足した 5 つの欄が
/// 「既定のままなら 1 バイトも出さない」形なのはこのためで、ここでは**最悪の値が
/// 全部入った行を 10 本混ぜて**、それでも 1,000 本入ることを見る。
///
/// **このテストが担保しているのは「証拠つきの行が少数である 1,000 本」だけ**である
/// (デプロイ先の上限は 240 本で、そのうち欄を持つのは `relaying` のトンネルだけ)。
/// **全部の行が証拠つきなら 1,000 本は入らない** — どこで切れるかは
/// [`the_connections_response_cuts_when_every_row_carries_evidence`] が実測で縛る。
#[test]
fn the_connections_response_stays_under_256_kib() {
    use crate::recent::{ORIGIN_SIDE, pack_revents};

    /// 最悪の値を入れる行数 (残りは既定のまま = 新しい欄が 1 つも出ない)。
    const WORST: usize = 10;

    let m = Metrics::new();
    let long_host = format!("{}.example.net:65535", "sub.".repeat(30));
    let now = Instant::now();
    for i in 0..1000u64 {
        let slot = m
            .conns
            .register(i, "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6", now)
            .expect("登録できる");
        slot.begin_tunnel(&long_host);
        slot.set_bytes(u64::MAX);
        slot.set_state(ConnState::Parked);
        if (i as usize) < WORST {
            // T15.0 (4) の 5 つが全部、起こりえない桁で入っている行。
            // `0x03d` = `IN|OUT|ERR|HUP|NVAL` (旗の名前が最長になる組み合わせ)
            slot.set_tid(u32::MAX);
            slot.set_relaying(u64::MAX, u64::MAX, pack_revents(0x03d, 0x03d));
            slot.set_half_closed(ORIGIN_SIDE);
            slot.sweep_rate(999_999_999_999);
            slot.sweep_rate(999_999_999_999);
        }
    }
    let all = m.conns.snapshot();
    assert_eq!(all.len(), 1000);
    assert_eq!(all[0].id, 0, "古い順に並ぶ");
    let worst = all[0].to_json(now);
    let plain = all[WORST].to_json(now);
    println!(
        "connections の 1 行: 最悪 {} B / 既定 {} B",
        worst.len(),
        plain.len()
    );
    println!("  最悪の 1 行: {}", worst);
    assert!(worst.len() > plain.len(), "最悪の行に欄が出ていない");
    for n in [240usize, 1000] {
        let mut body = String::from(SCHEMA_HEAD) + "\"connections\":";
        let (shown, cut) = array_within(&mut body, all.iter().take(n).map(|c| c.to_json(now)));
        body.push('}');
        assert_eq!(shown, n, "{} 本が全部入ること", n);
        assert!(!cut);
        assert!(body.len() <= MAX_BODY, "{} 本で {} B", n, body.len());
        println!(
            "connections {} 本の応答: {} B (上限 {} B)",
            n,
            body.len(),
            MAX_BODY
        );
    }
}

/// **全部の行が証拠つきだと 1,000 本は入らない** (単位 7 の `offset=` が入るまでの
/// 既知の穴。T15.0 (4))。
///
/// 上の [`the_connections_response_stays_under_256_kib`] は「1,000 本のうち証拠つきは
/// 少数」を前提にしていて、そこだけ読むと「1,000 本はいつでも担保されている」と
/// 誤読できる。ここでは逆に**全部の行に証拠を入れて**、入る本数を実測で縛る。
///
/// 実測 (2026-09-18):
///
/// | 行の中身 | 1 行 | 入る本数 |
/// |---|---|---|
/// | 既定 (新しい欄が 1 つも出ない) | 255 B | 1,000 本 (上のテスト) |
/// | 現実的な `relaying` の行 (`tid` と `revents` だけ) | 301 B | **860 本** |
/// | 最悪 (5 つとも起こりえない桁) | 442 B | **588 本** |
///
/// 切れても黙って落ちることは無く、`shown` が入った本数・`truncated` が `true` に
/// なる (口の側は `count` も返すので、読む側は「全部は見えていない」と分かる)。
#[test]
fn the_connections_response_cuts_when_every_row_carries_evidence() {
    use crate::recent::{ORIGIN_SIDE, pack_revents};

    /// 「`tid` と旗だけ」の現実的な行がこれより少なければ知らせる (回帰の下限)。
    const REALISTIC_MIN: usize = 800;
    /// 「5 つとも最悪」の行がこれより少なければ知らせる (回帰の下限)。
    const WORST_MIN: usize = 550;

    // `worst` = 5 つとも最悪、`false` = `tid` と `revents` だけの現実的な行
    let fill = |worst: bool| -> Vec<String> {
        let m = Metrics::new();
        let long_host = format!("{}.example.net:65535", "sub.".repeat(30));
        let now = Instant::now();
        for i in 0..1000u64 {
            let slot = m
                .conns
                .register(i, "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6", now)
                .expect("登録できる");
            slot.begin_tunnel(&long_host);
            slot.set_state(ConnState::Relaying);
            slot.set_tid(u32::MAX);
            if worst {
                // `0x03d` = `IN|OUT|ERR|HUP|NVAL` (旗の名前が最長になる組み合わせ)
                slot.set_relaying(u64::MAX, u64::MAX, pack_revents(0x03d, 0x03d));
                slot.set_half_closed(ORIGIN_SIDE);
                slot.sweep_rate(999_999_999_999);
                slot.sweep_rate(999_999_999_999);
            } else {
                // 生きているトンネルのありふれた形: 起床の旗はあるが空回りしていない
                slot.set_relaying(1 << 30, 0, pack_revents(0x001, 0x000));
            }
        }
        m.conns.snapshot().iter().map(|c| c.to_json(now)).collect()
    };

    for (what, rows, least) in [
        ("tid と旗だけ", fill(false), REALISTIC_MIN),
        ("5 つとも最悪", fill(true), WORST_MIN),
    ] {
        let mut body = String::from(SCHEMA_HEAD) + "\"connections\":";
        let (shown, cut) = array_within(&mut body, rows.iter().cloned());
        body.push('}');
        println!(
            "connections 1,000 本が全部「{}」: 1 行 {} B → 入るのは {} 本 ({} B / 上限 {} B)",
            what,
            rows[0].len(),
            shown,
            body.len(),
            MAX_BODY
        );
        assert!(body.len() <= MAX_BODY, "{}: {} B", what, body.len());
        assert!(
            shown < 1000 && cut,
            "{}: 1,000 本入ってしまった (欄が縮んだなら上の表と README を直す)",
            what
        );
        assert!(
            shown >= least,
            "{}: 入るのが {} 本まで減った (期待は {} 本以上)",
            what,
            shown,
            least
        );
    }
}

/// `/recent` は 2,000 件のリングを丸ごと読んでも 256 KiB に収まること (T14.4)。
///
/// 1 件はありふれた値で 225 B なので **2,000 件 (440 KiB) は入りきらない**のが
/// 設計どおり。入らない分はバイト数で打ち切って `"truncated":true` を出す
/// (既定の 200 件は最悪の値でも入る)。ここは `/recent` そのものを通して見る。
#[test]
fn the_recent_response_stays_under_256_kib() {
    use crate::recent::{CloseReason, ConnTally, MAX_RECENT, SIDES, STAGES};

    let m = Metrics::new();
    let now = Instant::now();
    let long_host = format!("{}.example.net:65535", "sub.".repeat(30));
    for i in 0..(MAX_RECENT as u64) {
        let slot = m
            .conns
            .register(i, "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6", now)
            .expect("登録できる");
        slot.begin_tunnel(&long_host);
        slot.finish(
            CloseReason::Error(crate::metrics::ErrCause::Unreachable),
            ConnTally {
                up: u64::MAX,
                down: u64::MAX,
                status: 599,
                stage_ms: [u64::MAX; STAGES],
                rtt_us: [u32::MAX; SIDES],
                retrans: [u32::MAX; SIDES],
                syn_retrans: u8::MAX,
                stall_ms: [u32::MAX; SIDES],
            },
            u32::MAX,
        );
        m.record_closed(i);
    }
    assert_eq!(m.closed.len(), MAX_RECENT);
    assert!(m.conns.is_empty(), "抹消は接続の終了で 1 回ずつ");

    with_ep(&m, |ep| {
        // **エンドポイントそのものを通す** (既定の 200 件は最悪の値でも入り、2,000 件は切れる)
        for (q, want_cut) in [("n=200", false), ("n=2000", true)] {
            let body = recent(ep, Some(q)).2;
            assert!(body.len() <= MAX_BODY, "{} で {} B", q, body.len());
            assert!(
                body.contains(&format!("\"truncated\":{}", want_cut)),
                "{} の truncated が {} でない: …{}",
                q,
                want_cut,
                &body[body.len() - 220..]
            );
            assert!(body.contains("\"matched\":2000"), "{}", q);
            let shown = body.matches("\"id\":").count();
            assert_eq!(shown == 200, !want_cut, "{} で {} 件", q, shown);
            println!(
                "recent {} (最悪の値) の応答: {} B / 出せたのは {} 件 (上限 {} B)",
                q,
                body.len(),
                shown,
                MAX_BODY
            );
        }
    });
}

/// `/bursts` は 50 枚の最悪 (接続元も宛先も上限いっぱい) でも 256 KiB に収まること (T14.6)。
#[test]
fn the_bursts_response_stays_under_256_kib() {
    use crate::recent::{BurstShot, MAX_BURSTS, MAX_SHOT_CLIENTS, MAX_SHOT_TARGETS};

    let m = Metrics::new();
    let now = Instant::now();
    let long_host = format!("{}.example.net:65535", "sub.".repeat(30));
    for i in 0..(MAX_SHOT_CLIENTS + MAX_SHOT_TARGETS + 8) as u64 {
        let slot = m
            .conns
            .register(
                i,
                &format!("2001:0db8:0000:0000:0000:ff00:0042:{:04x}%enp0s31f6", i),
                now,
            )
            .expect("登録できる");
        slot.begin_tunnel(&format!("{}{}", i, long_host));
    }
    let rows = m.conns.snapshot();
    for i in 0..(MAX_BURSTS as u64 + 5) {
        m.bursts.push(BurstShot::take(
            &rows,
            i + 1,
            usize::MAX,
            usize::MAX,
            4096,
            2048,
            u64::MAX,
            u64::MAX,
        ));
    }
    assert_eq!(m.bursts.len(), MAX_BURSTS);

    with_ep(&m, |ep| {
        let body = bursts(ep, None).2;
        assert!(body.len() <= MAX_BODY, "{} B", body.len());
        assert!(
            body.contains("\"count\":50"),
            "50 枚が全部入ること: {}",
            &body[body.len() - 300..]
        );
        assert!(
            body.contains("\"truncated\":false"),
            "{}",
            &body[body.len() - 300..]
        );
        assert!(
            body.contains("\"recorded\":55"),
            "{}",
            &body[body.len() - 300..]
        );
        println!(
            "bursts 50 枚 (最悪の 1 枚): {} B / 1 枚 {} B (上限 {} B)",
            body.len(),
            body.len() / MAX_BURSTS,
            MAX_BODY
        );
        // `?n=` で絞れ、1 枚も無ければ空の一覧
        assert_eq!(bursts(ep, Some("n=3")).2.matches("\"seq\":").count(), 3);
        let empty = Metrics::new();
        let ep2 = Endpoint {
            metrics: &empty,
            ..*ep
        };
        let none = bursts(&ep2, None).2;
        assert!(none.contains("\"bursts\":[]"), "{}", none);
        assert!(none.contains("\"recorded\":0"), "{}", none);
        assert!(none.contains("\"armed\":true"), "{}", none);
        assert!(none.contains("\"pending\":false"), "{}", none);
    });
}
