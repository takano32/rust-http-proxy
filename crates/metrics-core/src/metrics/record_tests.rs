//! 1 件の分類 (`Shape::of`) と区間への加算 (`Interval::observe`) の単体試験 (T.34.30)。

use super::record::Shape;
use super::*;

fn detail() -> Detail {
    Detail::default()
}

/// 10 形の分類 (T.34.30)。`counted`・`ms`・`other_port`・`web_error` の組み合わせを表で縛る。
#[test]
fn the_shape_of_one_request_is_a_pure_table() {
    let ms = |v| Some(Duration::from_millis(v));
    let rtt = Detail {
        origin_rtt_us: 4_500,
        dns_ms: 3,
        connect_ms: 10,
        ..detail()
    };
    let fb = Detail {
        first_byte_ms: Some(7),
        ..detail()
    };
    let dns = Detail {
        cause: Some(ErrCause::Dns),
        ..detail()
    };
    let timeout = Detail {
        cause: Some(ErrCause::Timeout),
        ..detail()
    };
    // (鍵, 結果, 時間, 内訳) → (connect, counted, ms, failed, excess, dns_failed, other_port, web_error)
    #[allow(clippy::type_complexity)]
    let table: [(&str, HostOutcome, Option<Duration>, &Detail, Shape); 10] = [
        // 1. CONNECT 443 の成功、RTT あり: excess は (13 − 4.5 → 9、10 − 4.5 → 6)
        (
            "connect://a:443",
            HostOutcome::Bypass,
            ms(12),
            &rtt,
            Shape {
                connect: true,
                counted: true,
                ms: Some(12),
                failed: false,
                excess: Some((9, 6)),
                dns_failed: false,
                other_port: false,
                web_error: false,
            },
        ),
        // 2. CONNECT 993 の成功: 443・80 以外の試み
        (
            "connect://m:993",
            HostOutcome::Bypass,
            ms(5),
            &detail(),
            Shape {
                connect: true,
                counted: true,
                ms: Some(5),
                failed: false,
                excess: None,
                dns_failed: false,
                other_port: true,
                web_error: false,
            },
        ),
        // 3. forward 80 の成功: 初バイトが窓の値になる
        (
            "http://a:80",
            HostOutcome::Miss,
            ms(40),
            &fb,
            Shape {
                connect: false,
                counted: true,
                ms: Some(7),
                failed: false,
                excess: None,
                dns_failed: false,
                other_port: false,
                web_error: false,
            },
        ),
        // 4. forward 8080 の成功 (`:8080` は `:80` で終わらない)
        (
            "http://a:8080",
            HostOutcome::Miss,
            ms(3),
            &detail(),
            Shape {
                connect: false,
                counted: true,
                ms: Some(3),
                failed: false,
                excess: None,
                dns_failed: false,
                other_port: true,
                web_error: false,
            },
        ),
        // 5. 時間の無い forward: 窓に入らないので試みでもない
        (
            "http://a:8080",
            HostOutcome::Hit,
            None,
            &detail(),
            Shape {
                connect: false,
                counted: true,
                ms: None,
                failed: false,
                excess: None,
                dns_failed: false,
                other_port: false,
                web_error: false,
            },
        ),
        // 6. `blocked://` は窓に入れない
        (
            "blocked://a:8443",
            HostOutcome::Blocked,
            ms(1),
            &detail(),
            Shape {
                connect: false,
                counted: false,
                ms: Some(1),
                failed: false,
                excess: None,
                dns_failed: false,
                other_port: false,
                web_error: false,
            },
        ),
        // 7. `loop://` も窓に入れない
        (
            "loop://self:8080",
            HostOutcome::Miss,
            ms(2),
            &detail(),
            Shape {
                connect: false,
                counted: false,
                ms: Some(2),
                failed: false,
                excess: None,
                dns_failed: false,
                other_port: false,
                web_error: false,
            },
        ),
        // 8. 443 の時間切れ: 名前解決に成功した 443・80 宛てのエラー
        (
            "connect://a:443",
            HostOutcome::Error,
            ms(3_000),
            &timeout,
            Shape {
                connect: true,
                counted: true,
                ms: Some(3_000),
                failed: true,
                excess: None,
                dns_failed: false,
                other_port: false,
                web_error: true,
            },
        ),
        // 9. 993 の時間切れ: 443・80 以外の試みのエラー
        (
            "connect://m:993",
            HostOutcome::Error,
            None,
            &timeout,
            Shape {
                connect: true,
                counted: true,
                ms: None,
                failed: true,
                excess: None,
                dns_failed: false,
                other_port: true,
                web_error: false,
            },
        ),
        // 10. 名前解決の失敗: 試みにも 443・80 のエラーにも数えない
        (
            "connect://nx:443",
            HostOutcome::Error,
            ms(20),
            &dns,
            Shape {
                connect: true,
                counted: true,
                ms: Some(20),
                failed: true,
                excess: None,
                dns_failed: true,
                other_port: false,
                web_error: false,
            },
        ),
    ];
    for (i, (host, outcome, took, d, want)) in table.iter().enumerate() {
        assert_eq!(
            Shape::of(host, *outcome, *took, d),
            *want,
            "{}: {}",
            i + 1,
            host
        );
    }
}

/// `Interval::observe` を `Metrics` なしで回す (T.34.30)。成功は窓へ、失敗は件数と原因へ。
#[test]
fn the_interval_takes_one_shape_without_metrics() {
    let mut iv = Interval::default();
    let ok = Detail {
        origin_rtt_us: 2_000,
        connect_ms: 8,
        dns_ms: 2,
        dns_misses: 1,
        ..Detail::default()
    };
    let took = Some(Duration::from_millis(10));
    iv.observe(
        &Shape::of("connect://a:443", HostOutcome::Bypass, took, &ok),
        &ok,
    );
    let fail = Detail {
        cause: Some(ErrCause::Dns),
        dns_ms: 30,
        dns_misses: 2,
        ..Detail::default()
    };
    iv.observe(
        &Shape::of("connect://nx:993", HostOutcome::Error, took, &fail),
        &fail,
    );
    let fwd = Detail::default();
    iv.observe(
        &Shape::of("http://a:8080", HostOutcome::Miss, took, &fwd),
        &fwd,
    );
    assert_eq!((iv.connect.count, iv.connect.ms_sum), (1, 10));
    assert_eq!((iv.excess.count, iv.excess.ms_sum), (1, 8));
    assert_eq!((iv.conn_excess.count, iv.conn_excess.ms_sum), (1, 6));
    assert_eq!((iv.forward.count, iv.forward.ms_sum), (1, 10));
    assert_eq!(iv.errors, 1);
    assert_eq!(iv.errors_by_cause[ErrCause::Dns as usize], 1);
    assert_eq!((iv.dns_misses, iv.dns_ms_sum), (3, 32));
    assert_eq!((iv.dns_fail_misses, iv.dns_fail_ms_sum), (2, 30));
    // 8080 の成功だけが 443・80 以外の試み (名前解決の失敗は試みに入れない)
    assert_eq!((iv.other_port_attempts, iv.other_port_errors), (1, 0));
    // 443・80 宛ての名前解決のミスは呼ぶ側が別に足す
    assert_eq!((iv.web_dns_misses, iv.web_dns_ms_sum), (0, 0));
    assert_eq!(iv.wait.count, 0);
}
