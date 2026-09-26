//! ブラウザで開くページの HTML (`/dashboard` `/inspect` `/probe.html`)。
//!
//! 中身は `src/*.html` を `include_str!` した const 3 つだけ。**ここに論理は置かない**
//! (T15.12 段 1: 163 KB の文字列定数を `proxy-endpoints` の rustc から外に出すため。
//! 逆依存はこのクレートを読む `proxy-endpoints` だけ)。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 120 MB)。**外部クレートは 1 つも使っていない。**

/// コントロールパネル (`/dashboard`)。
pub const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// 「調査」ページ (T14.8)。`/dashboard` が「いま」を見る画面なのに対して、
/// **起きたことを時間軸で読む**ための別のページ (個票を描く)。
/// `--lite` でも 200 で返す (記録が無ければページの中で「記録していません」と出る)。
pub const INSPECT_HTML: &str = include_str!("inspect.html");

/// 「端末から測る」ページ (T14.33)。プロキシ側の計測は「プロキシに届いてから」しか
/// 見えないので、**利用者のブラウザから** `/status` の往復と、プロキシ経由で小さな URL を
/// 取る時間を測り、`/clients` の自分の行 (T14.7) と `rtt_ms` (T14.5) に並べる。
/// 測った値はサーバーへ送らない (端末の中だけ)。`--lite` でも 200 で返す
/// (接続元を記録していないことはページの中で伝える)。
pub const PROBE_HTML: &str = include_str!("probe.html");
