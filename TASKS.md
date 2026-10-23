# TASKS.md — 未着手のタスク

`rust-http-proxy` の**まだ着手していないタスクだけ**をここに置く (2026-10-22、T24.13)。入り口は [`TODO.md`](TODO.md)。

## 決まり

- **済んだタスクはここに残さない。** Phase が終わったら、その Phase の見出しごと [`tasks/TNN.md`](tasks/README.md) へ本文を 1 文字も変えずに移し、`TODO.md` の §5 には見出しと索引の 1 行だけ残す。
- **番号は使い回さない。** `Tn.n` は付けた順で、並び替えても振り直さない (コミットメッセージから引けるようにする)。各 Phase の `Tn.99` は「デプロイ後の様子見と修正、締めの文書」に予約する。
- **1 タスク = 1 worktree = 1 コミット。** 手順の型は `TODO.md` の「Phase 15 の渡し方」と「実装を Opus に渡すときの決まり」。
- このファイルと `tasks/` の 1 対 1 は `./scripts/check-docs.sh` の (g) が見る (`scripts/check-ledger.py`)。**未着手のタスクに `- [x]` が付いていたら NG** (移し忘れ)。

## いま開いている Phase

### Phase 24 (2026-10-22 に決めた: 起動の切り分けと heap の基準、短い TTL の引き直し、物差しの手入れ、既知の小物)

済んだ 12 本 (T24.0 〜 T24.12) の本文は [`tasks/T24.md`](tasks/T24.md) にある。残りは締めの 1 本。

- [ ] **T24.99 締める (0.24.5 を 24 時間走らせたあとに見る・決める・書く)**
  - 前提 (§0 の 9 の形): デプロイ先は `tokyo.sorahost.net:60357`。**版は 0.24.5** (波 24a〜24c の 9 本 + T24.11・T24.12 の揺れ + T24.9・T24.10・T24.13・T24.14・T24.15 の台帳)。再デプロイの**直前**に「前」の雪像、**起動から 60 秒たってから** 0 時間の雪像を撮る (`memory.baseline` が 60 秒で 1 回だけ書かれるため。すぐ撮ると heap が「判定できず」になる)。版と起動時刻 (`/events` の `start`)、`/status` の `boot.id` をここに書く。
  - リリース: タグは **`v0.24.5`** (`v0.23.3` はデプロイ済みだがタグを打っていない。打つなら今のうち)。
  - 判定: `--criteria phase24`。見ること: こちらが原因の無言の再起動が 0 回 (外からの再起動は `boot_id` の変化で切り分けて数から外す)、heap の増え < 5 MB が**起動ごとの基準**で読めること、引き直しが前の 0.5 倍以下 (T24.1 の段 2 を入れた場合) かつ `stale_retries` < `stale_hits` の 1%、`timeout` が 1 件 5 秒、`dns_slow` が合計 1,000 ms 未満の窓で 0 件。
  - 次のターンの準備: Phase 25 の完了の定義の下書き (候補: T24.8 の割り方を実行するか、`/errors` の `dns` 1,074 件の内訳 (NXDOMAIN と一時的な失敗)、`cache` が 0 ヒットのままなことの確認)、計器の確認、`T25.0`。

### Phase 25 (2026-10-23 に起こした候補。デプロイを待たずに段 1 だけ進められるものもある)

番号は仮。**Phase 25 の見出しを決めるのは T24.99 の「次のターンの準備」**で、そこで採るかどうかを決める。

- [ ] **T25.2 `recent.rs` (3,585 行) を T24.8 の案 A で割る**
  - 目的: T24.8 の見積もりで、`crates/metrics-recent/src/recent.rs` を **3 ファイル (errors 795 / conns 1,399 / mod.rs 1,391)** に割るのがよいと分かった (移す 2,194 行、外の 28 ファイルの経路は `pub use` で不変、ビルドメモリの寄与は約 11 MB で関門 120 MB には余裕 30 MB)。
  - 変更箇所: `crates/metrics-recent/src/recent/` の 3 ファイル、`docs/recent-split.md` の結論どおり。
  - やること: コードを動かすだけで意味は変えない。1 ファイルずつ動かしては `cargo test -p proxy-metrics-recent` を回す。移す行は `git mv` 相当 (差分が「動いた」だけに見えるように)。
  - 受け入れ基準: 公開している 48 個の名前が `pub use` で同じに見える、`cargo test --workspace` 全通過、`build-memory.sh 120` 通過、clippy 0。

- [ ] **T25.3 Prometheus に `boot` と `memory.baseline` を出すか決める**
  - 目的: T24.0 で `/status` に `boot` (`boot_id` と `pid`)、T24.4 で `memory.baseline` を足したが、`/metrics` (Prometheus) には出していない (`crates/prom/src/prom.rs` に `boot`/`baseline` の文字が 1 つも無い)。監視側で「ホストの再起動」と「heap の増え」を見たいなら要る。
  - 変更箇所: `crates/prom/src/prom.rs`、README の `/metrics` の節。
  - やること: 出すなら `proxy_boot_id_info{id="…"} 1` と `proxy_heap_baseline_bytes`、`proxy_heap_baseline_at_seconds`。**cardinality が増えない形** (id は info 型で 1 本) にする。出さないなら README に「出さない理由」を書く。
  - 受け入れ基準: 出す場合は `/metrics` の行と README の表、`test_prom` の 1 本。出さない場合は README の 1 段落。
