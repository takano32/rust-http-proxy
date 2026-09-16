//! 「調査」ページ `/inspect` の結合テスト (T14.8)。
//!
//! 絵はブラウザが無いと確かめられないので、ここで見るのは配線だけ:
//! **`--lite` でも 200 で開けること** (個票が無いときに何と出すか)、
//! `/dashboard/inspect` が同じページであること、`/` の案内に載ること。
//! 描画関数が実出力と合っているかは `scripts/check-dashboard.js` が
//! `/snapshot` の実出力で見る (Node があるときだけ)。

mod common;

use common::{proxy_config, raw_get, start_test_proxy};

/// 自分宛ての GET を 1 本投げて応答を丸ごと返す (オリジン形式。`Host` のポートが
/// 待ち受けと同じときだけ自分宛て。T12.3)。
fn get(port: u16, path: &str) -> String {
    raw_get(
        port,
        &format!(
            "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            path, port
        ),
    )
}

#[test]
fn test_integration_inspect_page_is_served_even_in_lite_mode() {
    let port = start_test_proxy(proxy_config());
    let page = get(port, "/inspect");
    assert!(
        page.starts_with("HTTP/1.1 200 OK") && page.contains("text/html"),
        "{}",
        &page[..page.len().min(200)]
    );
    // 図と、node の整形テストが抜き出す関数と、(g) の `/snapshot` へのリンク
    for needle in [
        "id=\"ch-timeline\"",
        "id=\"ch-rtt\"",
        "function timeline(",
        "function slowRows(",
        "function burstCards(",
        "function clientRows(",
        "function rttScatter(",
        "function sinceStart(",
        "function eventMarks(",
        "<a href=\"/snapshot\" target=\"_blank\">",
    ] {
        assert!(page.contains(needle), "{} が無い", needle);
    }
    // `/dashboard/inspect` も同じページ (どちらの綴りでも開ける)
    let same = get(port, "/dashboard/inspect");
    assert!(same.starts_with("HTTP/1.1 200 OK"), "{}", &same[..80]);
    assert_eq!(same.len(), page.len(), "/dashboard/inspect が別のページ");

    // **`--lite` でも 200**。個票を記録していないことはページの中で伝える
    let mut lite = proxy_config();
    lite.lite = true;
    let lite_port = start_test_proxy(lite);
    let page = get(lite_port, "/inspect");
    assert!(
        page.starts_with("HTTP/1.1 200 OK") && page.contains("text/html"),
        "{}",
        &page[..page.len().min(200)]
    );
    assert!(page.contains("記録していません"), "lite の断り書きが無い");
    // `/dashboard` は今までどおり 1 行のテキスト (増やしていない)
    let dash = get(lite_port, "/dashboard");
    assert!(
        dash.contains("lite mode"),
        "{}",
        &dash[..dash.len().min(200)]
    );
    // `/` の案内には `--lite` でも `/inspect` が載る (持っている口だから)
    let index = get(lite_port, "/");
    assert!(index.contains("/inspect"), "案内に /inspect が無い");
    assert!(
        !index.contains("/dashboard"),
        "lite なのに /dashboard が載った"
    );
}
