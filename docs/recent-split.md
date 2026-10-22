# `recent.rs` の割り方の見積もり (T24.8 段 1)

対象: `crates/metrics-recent/src/recent.rs` (3,585 行)。

測った日: 2026-10-22 (UTC)。worktree `~/.claude/worktrees/t248` (`wave24b/t248`、main = `081de1c`)。
機械は aarch64 / rustc 1.99.1。**この文書は数えただけで、コードは 1 行も動かしていない。**
行数は `wc -l`、項目は「インデント 0 の `pub`/`fn`/`impl` を数える使い捨ての Python (標準ライブラリのみ)」、
メモリは `scripts/build-memory.sh` の `report_rss()` と同じ採り方 (rustc の `/proc/<pid>/status` の
`RssAnon` を 20 ms ごとに見て、クレートごとの最大を取る) で測った。

## 1. 台帳の数字を測り直す

| ファイル | 台帳 (T22.6 の値) | 実測 (2026-10-22) | 差 |
|---|---|---|---|
| `crates/metrics-recent/src/recent.rs` | 3,585 | **3,585** | 0 |
| `crates/metrics-core/src/metrics.rs` | 2,834 | **2,876** | +42 |
| `crates/metrics-window/src/history.rs` | 2,375 | **2,375** | 0 |

`metrics.rs` だけ台帳より 42 行増えている (T22.6 の後に足したぶん)。目安の 2,000 行を超えているのは
この 3 本で、うち `recent.rs` が一番太い。前例の `anomaly.rs` は **1,014 行 + `anomaly_tests.rs` 1,092 行**
(`anomaly.rs` の末尾は `#[cfg(test)] #[path = "anomaly_tests.rs"] mod tests;` の 3 行だけ) で、
前回の割り方は「**試験を別ファイルへ出す**」だった。今回もその形が使える。

## 2. 3,585 行の中身 (実測)

`recent.rs` は 1 つの塊ではなく、**エラーのリング・接続の表・個票のリング・バーストのリング・
閉じた分布**の 5 つと、そのあとに 3 つの試験モジュールが続く形になっている。行の範囲は重ならない。

| 塊 | 行の範囲 | 行数 | 割合 | `pub` の型 | `pub` の定数 | `pub` の関数 | 私有の項目 | 関数 | `#[test]` |
|---|---|---|---|---|---|---|---|---|---|
| 前置き (`//!`・`use`) | 1–48 | 48 | 1.3% | 0 | 0 | 0 | 0 | 0 | 0 |
| `ERRORS` (エラーの個票とリング) | 49–471 | 423 | 11.8% | 4 | 4 | 1 | 4 | 24 | 0 |
| `CONNS` (`ConnState`・`ConnSlot`・`ConnTable`) | 472–1365 | 894 | 24.9% | 3 | 0 | 1 | 6 | 51 | 0 |
| `RECORDS` (段階の定数・`CloseReason`・`ConnTally`・`RecentEntry`・`RecentRing`) | 1366–1861 | 496 | 13.8% | 5 | 15 | 0 | 2 | 19 | 0 |
| `BURSTS` (`BurstShot`・`BurstRing`) | 1862–2245 | 384 | 10.7% | 2 | 5 | 0 | 4 | 23 | 0 |
| `CLOSED` (`ClosedCounts`) | 2246–2420 | 175 | 4.9% | 1 | 7 | 0 | 2 | 7 | 0 |
| `mod tests` (エラーの試験) | 2421–2792 | 372 | 10.4% | 0 | 0 | 0 | 0 | 15 | 13 |
| `mod conn_tests` | 2793–3297 | 505 | 14.1% | 0 | 0 | 0 | 0 | 14 | 14 |
| `mod burst_tests` (バーストと分布) | 3298–3585 | 288 | 8.0% | 0 | 0 | 0 | 0 | 9 | 8 |
| 合計 | | **3,585** | | **15** | **31** | **2** | **18** | **162** | **35** |

本体 (前置きを除く) は 2,372 行、試験は 1,165 行 (32.5%)。`pub` の項目は 48 個 (型 15・定数 31・関数 2) で、
`lib.rs` が `pub mod recent;` なので**全部が外から見える**。外から `recent::` を名指ししている
ファイルは 31 本ある (うち 3 本はクレートの内側。外は 28 本で、`crates/tunnel`・`crates/server`・
`crates/endpoints` の 3 本・`crates/metrics`・`tests/ring_prefault_test.rs` など)。

## 3. T24.8 が挙げた 4 つの境目を測る

台帳の本文は「`ConnSlot` / `snapshot` / `persist` / `tests` の 4 つに分けたときの行数」と言っている。
実測すると、**この 4 つでは 3,585 行を分割できない**。4 つの行を単純に足すと 2,182 行だが、
`ConnSlot::to_json` (70 行) が `ConnSlot` の中にあるので二重に数えていて、重なりを除くと
**2,112 行 (58.9%)**。残り **1,473 行**は前置き 48 行と、`ErrorRing` のリング・`ConnTable` 285 行・
`RECORDS` 496 行・`BURSTS` 384 行・`ClosedCounts` の本体で、4 つのどれでもない。

| 境目 | 実測 | 中身 |
|---|---|---|
| `ConnSlot` (リングの 1 枠) | **609 行** (17.0%) | `ConnState` 41 + `ConnSlot` の定義 97 + 原子を詰めた定数 43 + `impl ConnSlot` 428。`ConnTable` (285 行) は別 |
| `snapshot` (JSON の組み立て) | **292 行** (8.1%) | 型ごとに散った 5 本の `to_json` (`ErrorEntry` 18・`ConnSlot` 70・`RecentEntry` 80・`BurstShot` 45・`ClosedCounts::push_row` 19) と補助 5 本 (`rtt_ms_json` 7・`top_counts` 9・`push_counts` 17・`process_counts` 14・`push_u64_array` 13) |
| `persist` (状態ファイル) | **116 行** (3.2%) | `take_unwritten` / `prefault` / `restore` / `restored` の 4 本 × 3 リング。**ファイルの書式の本体はここではなく** `crates/metrics/src/persist_recent.rs` (1,135 行、試験 9 本) |
| `tests` | **1,165 行** (32.5%) | 3 モジュール・35 本。内訳は 13 / 14 / 8 |

分かったこと 3 つ:

1. **`snapshot` は塊ではない。** 5 つの型の `impl` の中に 18〜80 行ずつ散っていて、出すと
   5 つの `impl` を 2 か所に裂くことになる (私有の欄を読むので、`recent` の**子モジュール**に置く必要がある)。
   動かす行は 292 行だが、触る `impl` は 5 つ。
2. **`persist` も塊ではない。** `recent.rs` にあるのは状態ファイルとの口 (116 行) だけで、書式は
   別クレートの別ファイルにある。ここを割っても `recent.rs` は 3,469 行のまま。
3. **一番大きい塊は試験 1,165 行**で、次が `CONNS` 894 行。`ConnSlot` 単体 (609 行) より
   `ConnTable` を含めた 894 行の方が境目として自然 (表の鍵とリングの枠は同じ所で使う)。

## 4. 呼び出しの向き (実測。コメント行を除いたコード行だけ)

型の名前が他の塊に何回出るかを数えた。**矢印は「呼ぶ側 → 呼ばれる側」**。

| 塊 | 呼ぶ相手 (コード行) | コメントにしか出ない言及 |
|---|---|---|
| `ERRORS` | なし (葉) | なし |
| `CONNS` | `RECORDS` 30、`ERRORS` 10 (`clip` 6・`MAX_CLIENT`/`MAX_TARGET` 4) | `RECORDS` 2 |
| `RECORDS` | なし (葉) | `CONNS` 3、`CLOSED` 2 |
| `BURSTS` | `CONNS` 1 (`BurstShot::take(rows: &[Arc<ConnSlot>])`) | `CONNS` 2、`CLOSED` 1、`RECORDS` 1 |
| `CLOSED` | `RECORDS` 1 (`observe(&RecentEntry)`) | なし |

きれいな DAG で、**輪が無い**: `ERRORS`・`RECORDS` が葉、`CONNS` がその上、`BURSTS` と `CLOSED` が上。
`RECORDS` から `CONNS` への言及は 4 行すべて `///` の中なので、`ConnSlot::closed_entry` が
`RecentEntry` を作る向きだけが本物の依存 (つまり `CONNS` → `RECORDS`)。

試験の側は `RECORDS` 75・`ERRORS` 62・`CONNS` 28・`CLOSED` 19・`BURSTS` 17 回名指しするが、
`mod tests` はエラーだけ (60 対 1)、`mod conn_tests` は `ConnSlot`/`ConnTable` と、その先に作られる
`RecentEntry` (40 回)、`mod burst_tests` はバーストと分布 (13 対 14) を見ている。**試験は塊ごとに
きれいに分かれる**ので、塊と一緒に移せる。

## 5. ビルドのメモリとの関係 (実測)

`cargo build --release` (`jobs = 1`、クリーンビルド 2 分 46 秒) の間に採った `RssAnon` の上位:

| クレート | `RssAnon` | 行数 (参考) |
|---|---|---|
| `proxy-server` | **74.9 MB** | — |
| `proxy-metrics-recent` | **74.5 MB** (2 回目の単独ビルドで 75.0 MB) | `recent.rs` 3,585 + `events.rs` 614 + `trace.rs` 325 |
| `proxy-endpoints` | 74.2 MB | 台帳の 82.5 MB は 2026-09-26 の aarch64 / rustc 1.98.1 の値 |
| `proxy-http` | 70.1 MB | — |
| `proxy-metrics-watch` | 69.1 MB | — |
| `proxy-metrics-profile` | 63.6 MB | ほぼ空のクレート = **この層の床** |

`RssAnon` は同じ機械の中の順位を見るためだけの値で、関門 (120 MB) と直接は比べない
(`scripts/build-memory.sh` の注記)。読み取れること:

- `proxy-metrics-recent` は **2 番目**で、1 番目の `proxy-server` と 0.4 MB 差。採取のぶれ
  (同じクレートを 2 回測って 74.5 / 75.0) と同じ大きさなので、**実質は並んでいる**。
- 床 (63.6 MB) からの上乗せは **約 11 MB**。`recent.rs` の 3,585 行が寄与できるのはこの 11 MB の中だけ。
  割って下がる余地は**最大でも 11 MB**。
- 関門は 120 MB、手元で通る最小は 90 MB (台帳)。**余裕は 30 MB** あるので、11 MB を削るために
  割る理由は今のところ無い。
- リポジトリの規則 (TODO.md §0) は「**効くのはクレートを小さく割ること。ファイルを割っても下がらない**」。
  つまり **§6 の案 A・B (モジュール分割) はメモリを 1 MB も下げない**。下げる道は案 C だけ。

## 6. 割り方の候補

### 案 A — 層で 3 ファイルに割る (試験は塊と一緒に移す)

`recent.rs` を `recent/mod.rs` + `recent/errors.rs` + `recent/conns.rs` にする。`mod.rs` で
`pub use errors::*; pub use conns::*;` するので、**外の 28 ファイルの経路 (`proxy_metrics::recent::…`) は
1 か所も変わらない**。

| ファイル | 中身 | 行数 |
|---|---|---|
| `recent/errors.rs` | `ERRORS` 423 + `mod tests` 372 | **795** |
| `recent/conns.rs` | `CONNS` 894 + `mod conn_tests` 505 | **1,399** |
| `recent/mod.rs` | 前置き 48 + `RECORDS` 496 + `BURSTS` 384 + `CLOSED` 175 + `mod burst_tests` 288 | **1,391** |

- 変更行数: 移す 2,194 行 (61.2%) + 配線 (`mod` 宣言 3・`use super::{…}` の追加 13 前後・各ファイルの
  前置きコメント 10 前後) で **約 2,220 行 (推測。移す行は実測、配線は推測)**。
- リスク: **低**。行の位置が変わるだけで、依存の向き (§4) どおり `conns` が `errors` と `mod.rs` を
  呼ぶ形になる。`ConnSlot::closed_entry` が `RecentEntry` を作るので `conns` → `mod.rs` の向きが増えるが、
  `mod.rs` から `conns` への**コードの**依存は無いので輪にならない。難点は diff が大きいこと
  (2,194 行) で、「中身が同じ」ことを目で確かめにくい。
- 試験の動かし方:
  `cargo test -p proxy-metrics-recent` (51 本 = `recent` 35 + `events` 12 + `trace` 4)、
  `cargo test --test recent_test --test bursts_test --test ring_prefault_test --test trace_test --test tunnel_evidence_test`
  (19 + 5 + 1 + 4 + 2 = 31 本)、`cargo clippy --workspace --all-targets -- -D warnings`。
  移す前後で**本数が同じ**であること (51 と 31) を確かめる。

### 案 B — リングごとに 5 ファイルへ

`errors.rs` / `conns.rs` / `records.rs` / `bursts.rs` / `closed.rs` + `mod.rs`。
1 ファイル 1 責務になり、最大は `conns.rs` の 1,399 行。

| ファイル | 行数 |
|---|---|
| `recent/errors.rs` | 795 (423 + 372) |
| `recent/conns.rs` | 1,399 (894 + 505) |
| `recent/records.rs` | 496 |
| `recent/bursts.rs` | 558 (384 + `mod burst_tests` の前半 174) |
| `recent/closed.rs` | 289 (175 + 後半 114) |
| `recent/mod.rs` | 48 + 再輸出 |

- 変更行数: 移す 3,537 行 (98.7%) + 配線 35 前後で **約 3,570 行 (推測)**。
- リスク: **中**。`mod burst_tests` (288 行) をバーストと分布に**裂く**必要があり、試験の中身に
  手が入る (行の移動だけでは済まない)。型 5 つと定数 15 個だけの `records.rs` は 496 行で、割る利益は小さい。
- 試験の動かし方: 案 A と同じ 2 コマンド。加えて `mod burst_tests` を裂いたぶんは
  `cargo test -p proxy-metrics-recent` の本数 (35) が変わらないことで見る。

### 案 C — クレートを割る (メモリを下げたいときだけ)

§4 の DAG に沿って 3 つに割る。`ERRORS` の `clip` は `CONNS` から 6 回、`RECORDS` の `RecentEntry` は
30 回呼ばれるので、この 2 つは `CONNS` より**下**の層に置く (逆向きには置けない)。

| 新しいクレート | 中身 | 行数 |
|---|---|---|
| `proxy-metrics-record` | `ERRORS` 423 + `RECORDS` 496 + `mod tests` 372 | **1,291** |
| `proxy-metrics-conn` | `CONNS` 894 + `mod conn_tests` 505 | **1,399** |
| `proxy-metrics-recent` (残る) | 前置き 48 + `BURSTS` 384 + `CLOSED` 175 + `mod burst_tests` 288 | **895** |

- 変更行数: 移す 2,690 行 + `Cargo.toml` 2 枚 (約 40 行) + 再輸出の配線 (`crates/metrics-profile/src/lib.rs`
  と `crates/metrics/src/lib.rs` の 2 か所) + README のクレート表 1 行と「(N + 本体)」の数 +
  版 (T21.6) で **約 2,750 行 (推測)**。
- メモリ: 割れて下がる余地は**最大 11 MB** (床 63.6 MB からの上乗せ)。ただし関門 120 MB に対して
  余裕 30 MB なので、**関門は動かない**。下げたい理由が別に要る。
- リスク: **中〜高**。`scripts/check-docs.sh` の (e) が README のクレート表と `crates/*/Cargo.toml` を
  突き合わせるので、**README を同じコミットで直さないと CI が落ちる**。`crate-versions.py --check` も通す必要がある。
- 試験の動かし方: 案 A の 2 コマンド + `cargo test -p proxy-metrics` (状態ファイルの試験 9 本) +
  `python3 scripts/crate-versions.py --check` + `scripts/check-docs.sh`。

## 7. 結論

**次にやるなら案 A (3 ファイル、`errors` / `conns` / 残り)。クレートは割らない。**

理由: (1) 2,000 行の目安を満たすのに要るのは 3 ファイルで足りる (最大 1,399 行)。(2) 移す行が
2,194 行で済み、案 B の 3,537 行より diff が 4 割小さい。(3) 境目が §4 の実測の依存の向きと一致する
(`errors` は葉、`conns` は `errors` と `records` を呼ぶだけ) ので、輪を作らずに済む。(4) 外の 28 ファイルの
経路は `pub use` で保たれるので、呼ぶ側は 1 行も変わらない。(5) クレートを割っても関門は動かず
(余裕 30 MB、下がる余地は最大 11 MB)、README と版の配線が増えるだけ。メモリを下げたくなった時に
案 C を別のタスクとして起こせばよい。

## 8. 数え方 (再現する手順)

```bash
wc -l crates/metrics-recent/src/recent.rs crates/metrics-core/src/metrics.rs \
      crates/metrics-window/src/history.rs crates/metrics/src/persist_recent.rs
# 塊の行の範囲と pub の数: インデント 0 の pub/fn/impl/mod を拾う (この文書のために使い捨てた)
grep -c '^pub ' crates/metrics-recent/src/recent.rs            # 48
grep -c '#\[test\]' crates/metrics-recent/src/recent.rs        # 35
# 依存の向き: 塊ごとに、他の塊の公開名の出現を数える (コメント行を除く)
# メモリ: scripts/build-memory.sh の report_rss() と同じ採り方で cargo build --release の間だけ採る
```
