#!/usr/bin/env node
// ダッシュボード (`crates/web/src/dashboard.html`) の JS を、ブラウザ無しで確かめる。
//
// ブラウザが無い環境でも壊れに気づけるように、見るのは次の 19 項目 (番号は下の本文の節と同じ。
// 本文には番号の重なる節が 2 つある: 2 つめの「5.」= canary の配列は 2 の続き、
// 2 つめの「11.」= 匿名化した実データ (T14.35) で 2〜5 と 10 をもう一度回す節):
//   1. <script> の中身が構文として通ること (`new Function` = `node --check` と同じ判定)
//   2. **`/history` の配列の配列を読む関数の読み方が、実際の出力と合っていること**
//      (キーの並び・入れ子の配列・区間の分位点。T12.4)
//   3. **`/status` を読む関数が実出力で例外なく描けること** (名前解決の KPI、
//      「悪いホスト」の表、直近の山。T13.3)
//   4. **個票 (`/errors` `/connections`) を読む関数**が、形の違う入力でも落ちないこと
//      (作り置きは架空のホスト名。T13.4)。`/errors` のまとめた行 (`repeats` > 1。T20.3) は
//      「最近のエラー」の行に `×N` と最後の時刻が出ること (T21.5)
//   5. **`/history` の `closed` (閉じた接続の分布) と `/bursts` の写真**が読めること
//      (区間の数・合計と件数の一致・並び。T14.6)
//   6. **カーネルと cgroup の窓** (`/history` の `kernel`、`/status` の `kernel`) が読めること (T14.12)
//   7. **`/events` の時系列**が読めること (12 種の綴り・新しい順・`?since=` の絞り。T14.11 / T14.23 / T14.54)
//   8. **`/profile` を読む関数** (段階・スレッド・ロック) が実出力と合っていること (T14.3)
//   9. **`/history` の `transfer`** (転送速度と半閉じの分布) が読めること (区間の数・合計と件数の一致。T14.25。
//      末尾の `stall_client_ms_sum` / `stall_origin_ms_sum` = 中継の詰まりの向きの合計は T14.42)
//  10. **「調査」ページ (`inspect.html`) の描画関数**が `/snapshot` の実出力で例外なく通ること
//      (タイムライン・遅い接続・山・接続元・RTT の散布・起動からの窓・出来事の印。T14.8)
//  11. **「端末から測る」ページ (`probe.html`) の関数** (`median` / `pick` / `render`) が
//      作り物の数列と `/status` の実出力で通り、「経由している / していない」を判定できること (T14.33)
//  12. **KPI「CONNECT 確立 p50」が `/status` の `recent_quantiles` (直近 1,024 本の実測) を
//      使い、無い版の出力では区間の補間に落ちること** (T14.31)
//  13. **KPI「今日の SLO」が `/slo` の応答を読めること** (今日の達成率・外れの最多・
//      直近の外れた時間帯。`/slo` を持たない版では `null` を返してカードごと出さない。T14.50)
//  14. **応答の形の版 (`schema`)** で分岐しても読み方が変わらないこと (T14.49)。
//      **版の無い古い出力 (版 0) も版 1 の出力も同じ関数で同じ結果**になること
//  15. **「調査」ページの「今日」「今週」「出来事と異常」** (`dailyRows` / `weeklyRows` /
//      `eventRows`) が `/daily` (T14.20)・`/snapshots` (T14.34)・`/events` (T14.11 / T14.23) の
//      出力と、**匿名化した実データ**の「無い版」の分岐で通ること (T14.44)
//  16. **T15.0 の 5 枚のカード** (CPU の絞り・動かないトンネル・名前解決の内訳・
//      受付待ち・利用者が待つ時間) が、**欄がある版**と**欄が無い古い版**の両方で通ること。
//      古い版では例外を出さず「無い」と分かる形 (null / 0 件) に落ちること
//  17. **`dashboard.html` の大きさ**が 96 KiB 以下であること (inspect と同じ作法。T15.0 で 80 KiB、
//      T28.25 で 96 KiB に上げた。前はここだけ 80 KiB のままで、実装の `DASH_MAX` と食い違っていた。T29.27)
//  18. **起動のカード**の `bootKind` (普通の再起動・無言の再起動・窓の先頭・記録なし。T28.25)
//  19. **設定の帯・版の隣のビルド・`/slo` の読み方** (T29.27): `.env` の読めない値と知らない鍵
//      (`settings.invalid` / `unknown`) が 1 つ 1 行で出ること (知らない鍵は候補つき)、`/status.build` が
//      版の隣に `(release)` で出ること、`/slo` の `basis` があれば tooltip が閾ごとの読み方になること。
//      どれも**欄がある版と無い古い版**の両方で通ること
//
// 使い方: node scripts/check-dashboard.js [/history の実出力.json] [/status の実出力.json]
//                                         [/profile の実出力.json] [/snapshot の実出力.json]
//   引数を省くと下の作り置き (手元のプロキシから取った実出力と、架空のホスト名の見本) を使う。
// 依存なし。Node があるときだけ回す補助的な確認で、`cargo test` の代わりではない。

const fs = require('fs');
const path = require('path');

// 本文は節ごとに `scripts/check-dashboard/NN-*.js` に割ってある (T34.140。上の番号の節が 1 つか 2 つずつ)。
// 節どうしは前の節の `const` (切り出した `api` `ins`・読んだ `hist` `st` `snap` …) をそのまま使うので、
// **名前の順につないで 1 つの関数の本文として回す** (割る前の 1 ファイルと同じ 1 つのスコープ)。
// `scripts/check-docs.sh` の画面の関数の検査も同じ順につないで読む。
const partsDir = path.join(__dirname, 'check-dashboard');
const body = fs
  .readdirSync(partsDir)
  .filter((f) => /^\d\d-.*\.js$/.test(f))
  .sort()
  .map((f) => fs.readFileSync(path.join(partsDir, f), 'utf8'))
  .join('');
new Function('require', '__dirname', 'fs', 'path', body)(require, __dirname, fs, path);
