//! 規則 6 (`new_client`。初めて見た接続元。T14.54) の試験。

use super::*;

// ------------------------------------------------------------ 規則 6 (T14.54)

/// 接続元 1 件を 1 要求ぶん記録する (本番と同じ口だけを使う)。
fn visit(m: &Metrics, client: &str, agent: Option<&str>, target: &str) {
    if let Some(a) = agent {
        // `User-Agent` を読むのは接続の最初の要求だけ (T14.7)
        m.record_client_agent(client, a);
    }
    m.record_client(
        client,
        crate::metrics::HostOutcome::Bypass,
        0,
        (0, 0),
        None,
        Some(target),
    );
}

/// いま入っている接続元の `first_seen` (**本物の壁時計**)。
///
/// 窓はこの値に合わせて置く: 標本の時刻は注入できても `first_seen` は
/// [`crate::cache::now_epoch`] なので、勝手な時刻で窓を切ると秒の境目で落ちる。
fn first_seen_of(m: &Metrics, client: &str) -> u64 {
    m.clients_first_seen_in(0, u64::MAX, 100)
        .0
        .iter()
        .find(|c| c.client == client)
        .unwrap_or_else(|| panic!("{} が居ない", client))
        .first_seen
}

/// (6) 初めて見た接続元が 1 回だけ出る (2 回目の要求では増えない)。
#[test]
fn f_a_new_client_is_written_once_per_client() {
    let m = Metrics::new();
    let mut d = Detector::new();
    visit(
        &m,
        "198.51.100.7",
        Some("t1454/1.0"),
        "connect://a.example:443",
    );
    let ta = first_seen_of(&m, "198.51.100.7");
    assert!(
        d.observe_clients(&m, ta - 1).is_empty(),
        "1 本目は基準にするだけ"
    );
    assert_eq!(
        d.observe_clients(&m, ta),
        vec![
            "new_client: 198.51.100.7 first seen \
             (1 req, first target port 443 (name), agent \"t1454/1.0\")"
                .to_string()
        ]
    );
    // 2 回目の要求では増えない。窓 (`[ta, tb]`) はこの接続元の `first_seen` を
    // また含むので、「もう書いた」を覚えていないと 2 件目が出る
    visit(
        &m,
        "198.51.100.7",
        Some("t1454/1.0"),
        "connect://b.example:443",
    );
    // 別の接続元は出る (IP リテラル宛て・`User-Agent` 無し)
    visit(&m, "198.51.100.8", None, "connect://203.0.113.9:8443");
    let tb = first_seen_of(&m, "198.51.100.8");
    assert_eq!(
        d.observe_clients(&m, tb),
        vec![
            "new_client: 198.51.100.8 first seen \
             (1 req, first target port 8443 (literal), no agent)"
                .to_string()
        ]
    );
}

/// (6) 読み戻した接続元 (`first_seen == 0`) は「新しい」ではない。
#[test]
fn f_restored_clients_are_not_new() {
    let m = Metrics::new();
    let mut d = Detector::new();
    let t0 = crate::cache::now_epoch();
    m.restore(
        Vec::new(),
        vec![(
            "198.51.100.1".to_string(),
            crate::metrics::HostStats {
                requests: 42,
                ..Default::default()
            },
        )],
    );
    assert!(d.observe_clients(&m, t0 - 1).is_empty());
    assert!(
        d.observe_clients(&m, t0 + 300).is_empty(),
        "前から居た接続元は新しくない"
    );
}

/// (6) 1 周期に何百も現れたら (走査) 上限で切って、残りは 1 行にまとめる。
#[test]
fn f_a_scan_is_folded_into_one_line() {
    let m = Metrics::new();
    let mut d = Detector::new();
    for i in 1..=(NEW_CLIENT_MAX + 3) {
        visit(
            &m,
            &format!("198.51.100.{}", i),
            None,
            "connect://a.example:443",
        );
    }
    let seen: Vec<u64> = m
        .clients_first_seen_in(0, u64::MAX, 100)
        .0
        .iter()
        .map(|c| c.first_seen)
        .collect();
    let (lo, hi) = (*seen.iter().min().unwrap(), *seen.iter().max().unwrap());
    assert!(d.observe_clients(&m, lo - 1).is_empty());
    let out = d.observe_clients(&m, hi);
    assert_eq!(out.len(), NEW_CLIENT_MAX + 1, "{:?}", out);
    assert!(
        out[NEW_CLIENT_MAX].starts_with("new_client: 3 more clients first seen in the same "),
        "{}",
        out[NEW_CLIENT_MAX]
    );
    assert!(
        out[NEW_CLIENT_MAX].ends_with(&format!("window (wrote {})", NEW_CLIENT_MAX)),
        "{}",
        out[NEW_CLIENT_MAX]
    );
    // まとめた分も「もう書いた」に入るので、次の周期では出てこない
    assert!(d.observe_clients(&m, hi).is_empty());
}

/// 履歴スレッドの口 ([`check`]) が `/events` に `new_client` で 1 件書き、
/// 長い `User-Agent` でも 1 件 128 B に収まる (接続元と数字は残る)。
#[test]
fn check_writes_one_new_client_event() {
    let _g = crate::events::TEST_LOCK.locked();
    crate::events::clear();
    reset();
    let metrics = Metrics::new();
    let mut sample = Sample {
        t: crate::cache::now_epoch(),
        ..Sample::default()
    };
    check(&metrics, &sample);
    assert!(crate::events::is_empty(), "1 本目では書かない");
    visit(
        &metrics,
        "198.51.100.9",
        Some(&"u".repeat(200)),
        "connect://a.example:443",
    );
    sample.t = first_seen_of(&metrics, "198.51.100.9");
    check(&metrics, &sample);
    let (events, _) = crate::events::select(0, 10);
    assert_eq!(events.len(), 1, "{:?}", events);
    assert_eq!(events[0].kind, crate::events::EventKind::NewClient);
    assert!(
        events[0]
            .text
            .starts_with("new_client: 198.51.100.9 first seen (1 req, first target port 443"),
        "{}",
        events[0].text
    );
    assert!(
        events[0].text.len() <= crate::events::MAX_TEXT,
        "{} B",
        events[0].text.len()
    );
    // 同じ接続元は 1 回だけ (同じ秒の窓をもう一度見ても増えない)
    check(&metrics, &sample);
    assert_eq!(crate::events::len(), 1);
    crate::events::clear();
    reset();
}
