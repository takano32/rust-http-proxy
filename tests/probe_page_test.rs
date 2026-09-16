//! 「端末から測る」ページ `/probe.html` の結合テスト (T14.33)。
//!
//! 測るのはブラウザなので、ここで見るのは配線だけ:
//! **`--lite` でも 200 で開けること** (接続元を記録していないときに何と出すか)、
//! `/` の案内に載ること、ページが 64 KiB 以下であること。
//! 関数 (`median` / `pick` / `render`) が実出力と合っているかは
//! `scripts/check-dashboard.js` の 11 番目の検査が見る (Node があるときだけ)。

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
fn test_integration_probe_page_is_served_even_in_lite_mode() {
    let port = start_test_proxy(proxy_config());
    let page = get(port, "/probe.html");
    assert!(
        page.starts_with("HTTP/1.1 200 OK") && page.contains("text/html"),
        "{}",
        &page[..page.len().min(200)]
    );
    // 64 KiB 以下 (応答ヘッダーごと数えても超えないこと)
    assert!(
        page.len() <= 64 * 1024,
        "probe.html が 64 KiB を超えた: {}",
        page.len()
    );
    // 3 つの測りもの (`/status` の往復・プロキシ経由の取得・`/clients` の自分の行) と、
    // node の整形テストが抜き出す関数
    for needle in [
        "function median(",
        "function pick(",
        "function render(",
        "cache:'no-store'",
        "mode:'no-cors'",
        "id=\"verdict\"",
        "/clients?sort=recent&limit=200",
    ] {
        assert!(page.contains(needle), "{} が無い", needle);
    }
    // **測った値はサーバーに送らない** (端末の中だけ。POST も送信先も持たない)
    assert!(!page.contains("method:'POST'") && !page.contains("method: 'POST'"));
    assert!(page.contains("サーバーへは送りません"), "断り書きが無い");
    // `/probe` でも同じページ (綴りを 1 つ間違えても開ける)
    let same = get(port, "/probe");
    assert!(same.starts_with("HTTP/1.1 200 OK"), "{}", &same[..80]);
    assert_eq!(same.len(), page.len(), "/probe が別のページ");

    // **`--lite` でも 200**。接続元を記録していないことはページの中で伝える
    let mut lite = proxy_config();
    lite.lite = true;
    let lite_port = start_test_proxy(lite);
    let page = get(lite_port, "/probe.html");
    assert!(
        page.starts_with("HTTP/1.1 200 OK") && page.contains("text/html"),
        "{}",
        &page[..page.len().min(200)]
    );
    assert!(page.contains("記録していません"), "lite の断り書きが無い");

    // `/` の案内には `--lite` でも `/probe.html` が載る (持っている口だから)
    let index = get(lite_port, "/");
    assert!(index.contains("/probe.html"), "案内に /probe.html が無い");
    let index = get(port, "/");
    assert!(index.contains("/probe.html"), "案内に /probe.html が無い");
}
