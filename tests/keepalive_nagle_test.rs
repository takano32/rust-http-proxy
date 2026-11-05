//! keep-alive の 2 本目からの応答が Nagle で遅れないことの結合テスト (と、その判定の単体試験)。
//!
//! 初めは `tests/keepalive_test.rs` にあった (T34.119 で割った)。

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::*;

/// 受け入れ基準: 5 本の束はこれ未満で返る。
const LIMIT: Duration = Duration::from_millis(100);

/// Nagle + delayed ACK なら 1 要求あたり約 40 ms 止まるので、5 本の束はこれ以上になる。
const NAGLE: Duration = Duration::from_millis(200);

/// 束を測り直す回数。
const ATTEMPTS: usize = 5;

/// 5 本の束を 1 回測る (要求を書いて応答を読み切るまで)。
fn measure_five(stream: &mut TcpStream, host: &str, tag: &str) -> Duration {
    let started = std::time::Instant::now();
    for i in 0..5 {
        let req = format!(
            "GET http://{}/{}{} HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, tag, i, host
        );
        stream.write_all(req.as_bytes()).unwrap();
        let (head, _body) = read_response(stream);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    }
    started.elapsed()
}

/// 束の測定値を判定する: **1 回でも 100 ms 未満なら通す** (機械が混んだ回を拾わない)。
///
/// ただし Nagle + delayed ACK は毎回 1 要求あたり約 40 ms 止まるので、束は**毎回**
/// 200 ms 以上になる。だから 200 ms 以上の回が 2 つ以上あれば、速い回が混ざっていても
/// Nagle と見て落とす (「遅い機械」を捨てる仕掛けが「Nagle の見逃し」に化けないための縛り)。
fn judge(samples: &[Duration]) -> Result<Duration, String> {
    let slow = samples.iter().filter(|s| **s >= NAGLE).count();
    if slow >= 2 {
        return Err(format!(
            "Nagle delay: {} of {} bundles took {:?} or more: {:?}",
            slow,
            samples.len(),
            NAGLE,
            samples
        ));
    }
    match samples.iter().filter(|s| **s < LIMIT).min() {
        Some(best) => Ok(*best),
        None => Err(format!(
            "no bundle under {:?} in {} attempts: {:?}",
            LIMIT,
            samples.len(),
            samples
        )),
    }
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// 応答ヘッダーと本文のあいだに `delay` あけるオリジン (Nagle + delayed ACK の代役)。
///
/// **はじめの `delayed` 本だけ**あける (残りはすぐ返す)。1 要求あたり `delay` 止まるので、
/// 5 本の束は `5 * delay` になる。実際に遅れた束を測り方が拾えること (測っている物差し
/// そのもの) と、遅れた束を捨てて測り直せることを縛るために使う。
fn start_delayed_origin(delay: Duration, delayed: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(AtomicUsize::new(0));
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let served = Arc::clone(&served);
            thread::spawn(move || {
                // このオリジン自身の Nagle を切る。切らないと「ヘッダーと本文を別々に
                // 書いた」だけで 40 ms 止まり、`delayed` を過ぎても遅いままになる
                // (そちらは本番のプロキシ側で見ている現象)
                let _ = stream.set_nodelay(true);
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut seen_any = false;
                    loop {
                        let mut line = String::new();
                        if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line.trim().is_empty() {
                            break;
                        }
                        seen_any = true;
                    }
                    if !seen_any {
                        return;
                    }
                    let n = served.fetch_add(1, Ordering::SeqCst);
                    let body = "delayed";
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n",
                        body.len()
                    );
                    if stream.write_all(head.as_bytes()).is_err() {
                        return;
                    }
                    let _ = stream.flush();
                    if n < delayed {
                        thread::sleep(delay);
                    }
                    if stream.write_all(body.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    port
}

/// 5 本の束を、速い回が出るまで最大 `ATTEMPTS` 回**測り直す**。
///
/// 判定は `judge` (1 回でも 100 ms 未満なら通す)。Nagle なら毎回 200 ms 以上なので落ちる。
fn measure_with_retries(
    stream: &mut TcpStream,
    host: &str,
    tag: &str,
) -> (Result<Duration, String>, Vec<Duration>) {
    let mut samples: Vec<Duration> = Vec::new();
    for attempt in 0..ATTEMPTS {
        let took = measure_five(stream, host, &format!("{}{}_", tag, attempt));
        samples.push(took);
        if took < LIMIT {
            // 速い回が 1 つあれば足りる (残りは測らない)
            break;
        }
    }
    (judge(&samples), samples)
}

#[test]
fn test_integration_keepalive_requests_are_not_delayed_by_nagle() {
    // TCP_NODELAY が立っていないと、応答ヘッダーと本文を別々に write したときに
    // Nagle + delayed ACK で 1 要求あたり約 40 ms 止まる (5 要求で 200 ms 以上)。
    //
    // 負荷の下では束が 1 回だけ 100 ms を超えることがある (T24.18 の段 1 の実測:
    // 10 回に 1 回 158.7 ms。Nagle の 200 ms ではない = 機械が混んだだけの回)。
    // sleep を伸ばさず**測り直し**、1 回でも 100 ms 未満なら通す。Nagle なら毎回
    // 200 ms 以上になるので、`judge` が取り違えない (下の 2 本で縛ってある)。
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        start_keepalive_origin(Arc::clone(&connections), Arc::clone(&requests));
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    // 1 要求目は接続確立を含むので測定から外す
    let warmup = format!("GET http://{}/w HTTP/1.1\r\nHost: {}\r\n\r\n", host, host);
    stream.write_all(warmup.as_bytes()).unwrap();
    read_response(&mut stream);

    let (verdict, samples) = measure_with_retries(&mut stream, &host, "nagle");
    let best = verdict.unwrap_or_else(|why| panic!("{}", why));
    println!(
        "bundle of 5 keep-alive requests: {:?} (best {:?} in {} attempts)",
        samples,
        best,
        samples.len()
    );
    assert!(best < LIMIT, "best bundle should be under {:?}", LIMIT);
}

/// 「遅い機械」と「Nagle」を取り違えないこと (測り直しで Nagle を見逃さない縛り)。
#[test]
fn test_the_nagle_judgment_still_fails_a_delayed_bundle() {
    // 実際に落ちた回 (T24.18 の段 1、158.7 ms) は Nagle ではない。速い回が 1 つあれば通す
    let a_slow_machine = [ms(158), ms(96), ms(155), ms(151), ms(147)];
    assert!(
        judge(&a_slow_machine).is_ok(),
        "a slow machine should be retried, not failed: {:?}",
        judge(&a_slow_machine)
    );
    // 1 回だけの引っかかりでも、100 ms 以上なら Nagle ではないので通す
    assert!(judge(&[ms(18), ms(205), ms(21)]).is_ok());
    // 全部 100 ms 以上なら、測り直しで隠さずに落とす
    let why = judge(&[ms(158); 5]).expect_err("five slow bundles must fail");
    assert!(why.contains("no bundle under"), "{}", why);
    // Nagle + delayed ACK = 毎回 200 ms 以上。速い回が混ざっていても落とす
    let nagle = [ms(212), ms(205), ms(209), ms(214), ms(207)];
    let why = judge(&nagle).expect_err("a Nagle bundle must fail");
    assert!(why.contains("Nagle"), "{}", why);
    let why = judge(&[ms(18), ms(205), ms(21), ms(210)]).expect_err("two slow bundles");
    assert!(why.contains("Nagle"), "{}", why);
}

/// 実際に 200 ms 以上かかる束を、プロキシ越しの測定が拾って落とすこと (物差しの裏取り)。
#[test]
fn test_the_nagle_measurement_fails_a_bundle_that_is_always_delayed() {
    // 1 要求 40 ms x 5 本 = 200 ms の遅れを**毎回**入れる (Nagle + delayed ACK と同じ)。
    // 測り直しても遅いままなので、5 回とも測って落とす
    let origin_port = start_delayed_origin(ms(40), usize::MAX);
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let (verdict, samples) = measure_with_retries(&mut stream, &host, "delayed");
    println!("always delayed bundles: {:?}", samples);
    assert_eq!(
        samples.len(),
        ATTEMPTS,
        "速い回が無いので全部測るはず: {:?}",
        samples
    );
    assert!(
        samples.iter().all(|s| *s >= NAGLE),
        "each delayed bundle should reach {:?}: {:?}",
        NAGLE,
        samples
    );
    let why = verdict.expect_err("an always-delayed bundle must fail");
    assert!(why.contains("Nagle"), "{}", why);
}

/// 1 束だけ遅れた回は、**測り直して**通すこと (「遅い機械」を拾わない)。
///
/// はじめの 5 本 (= 1 束ぶん) だけ 40 ms あける。1 束目は 200 ms で落とす判定だが、
/// 2 束目は速いので通る。ここで「測り直す」が効いていないと 1 束目で落ちる。
#[test]
fn test_the_nagle_measurement_retries_a_single_delayed_bundle() {
    let origin_port = start_delayed_origin(ms(40), 5);
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let (verdict, samples) = measure_with_retries(&mut stream, &host, "retry");
    println!("bundles with only the first one delayed: {:?}", samples);
    assert_eq!(
        samples.len(),
        2,
        "1 束目が遅く 2 束目が速い並びになるはず: {:?}",
        samples
    );
    assert!(samples[0] >= LIMIT, "1 束目は遅れているはず: {:?}", samples);
    let best = verdict.unwrap_or_else(|why| panic!("{}", why));
    assert!(best < LIMIT, "2 束目は速いはず: {:?}", samples);
}
