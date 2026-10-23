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

- [ ] **T24.16 ベンチの取り直し (T24.5 の `set_bytes` が CPU/要求 の表に出るか。段 1)**
  - 目的: T24.5 で keep-alive の HTTP 接続も `ConnSlot::set_bytes` を呼ぶようにした。debug の測定では forward の CPU/要求 が 134.07 → 136.06 us (+1.5%、組ごとの差 −1.5% / +1.5% / −0.2% でぶれの中) だったが、README の表 (41.4 / 22.0 / 58.1 us) はその前の測定のまま。デプロイ前に「表を書き換える必要があるか」を決める。
  - 変更箇所: なし (計測だけ。必要なら README の表 1 行)。
  - やること: release ビルドで `scripts/bench` を `--only forward --conc 8` と `--only forward --conc 8 --cacheable` で、**`main` の 1 つ前 (T24.5 の手前) と いまの `main`** を前後交互 3 組。`scripts/mx` のロックを取って 1 本ずつ。差が ±3% 以内なら「ぶれの中」として表は据え置き、超えたら表と「T24.5 の代償」を README に書く。
  - 受け入れ基準: 3 組の中央値と組ごとの差、`--check` の `[ok]`、測ったコミットの sha。表を書き換えたならその根拠。

- [ ] **T24.17 タグを打つ (`v0.23.3` と `v0.24.5`)**
  - 目的: 最新のタグが `v0.22.6` で、**デプロイ済みの `v0.23.3` にも、これから置く `v0.24.5` にもタグが無い** (`git tag --list 'v0.2*'` で確認)。README の運用 0 は「本体の版でタグを打つだけ」なので、版とタグがずれている。
  - 変更箇所: タグ (リポジトリの履歴。`release.yml` がタグで走る)。
  - やること: `v0.23.3` は `0.23.3` の版を持つ最後のコミット (`ae7c190`)、`v0.24.5` は `HEAD` に打って push する。**打つ前に takano32 の yes を取る** (タグは動かせない)。
  - 受け入れ基準: `git ls-remote --tags` に 2 つが出る、`release.yml` の成果物が付く、`crate-versions.py --check` がタグの上でも通る。

- [ ] **T24.18 揺れるテストの残りを洗う (段 1: どれが落ちるか数える)**
  - 目的: 波 24 で 4 本 (`stages_test`・`newclient_test`・`tunnel_test`・`overload_test`) を「値で待つ」型に直した。**まだ `tests/` に `thread::sleep` が 26 本**残っていて、そのうち cache 9・stall 3・rate 3・proxy 3・canary 3・stages 2・snapshots 2・keepalive 2・tunnel 系 3。40〜50 ms の窓に頼るものが残っている (`keepalive_test` の 40 ms、`stages_test` の `GAP`、各テストの `TICK = 50 ms`)。次の再デプロイ後に同じ種類の揺れで悩まないよう、**デプロイ前に**どれが危ないかを数えておく。
  - 変更箇所: なし (段 1 は計測だけ)。落ちたものがあれば段 2 で T24.6 の型に直す。
  - やること: CPU のループ 4 本 + 同じバイナリを別プロセスで並列に流す負荷の下で、`sleep` を持つテストのバイナリ (`cache` `stall` `rate` `proxy` `canary` `stages` `snapshots` `keepalive` `tunnel*`) を 20 回ずつ回し、落ちた本数と落ち方を記録する。落ちたものが「時間に頼る待ち」なら段 2 の候補として T25 に送る。
  - 受け入れ基準: バイナリごとの 20 回の結果 (通過/失敗の内訳)、落ちたテストの名前と `panicked at` の行、負荷の作り方。


- [ ] **T24.99 締める (0.24.5 を 24 時間走らせたあとに見る・決める・書く)**
  - 前提 (§0 の 9 の形): デプロイ先は `tokyo.sorahost.net:60357`。**版は 0.24.5** (波 24a〜24c の 9 本 + T24.11・T24.12 の揺れ + T24.9・T24.10・T24.13・T24.14・T24.15 の台帳)。再デプロイの**直前**に「前」の雪像、**起動から 60 秒たってから** 0 時間の雪像を撮る (`memory.baseline` が 60 秒で 1 回だけ書かれるため。すぐ撮ると heap が「判定できず」になる)。版と起動時刻 (`/events` の `start`)、`/status` の `boot.id` をここに書く。
  - リリース: タグは **`v0.24.5`** (`v0.23.3` はデプロイ済みだがタグを打っていない。打つなら今のうち)。
  - 判定: `--criteria phase24`。見ること: こちらが原因の無言の再起動が 0 回 (外からの再起動は `boot_id` の変化で切り分けて数から外す)、heap の増え < 5 MB が**起動ごとの基準**で読めること、引き直しが前の 0.5 倍以下 (T24.1 の段 2 を入れた場合) かつ `stale_retries` < `stale_hits` の 1%、`timeout` が 1 件 5 秒、`dns_slow` が合計 1,000 ms 未満の窓で 0 件。
  - 次のターンの準備: Phase 25 の完了の定義の下書き (候補: T24.8 の割り方を実行するか、`/errors` の `dns` 1,074 件の内訳 (NXDOMAIN と一時的な失敗)、`cache` が 0 ヒットのままなことの確認)、計器の確認、`T25.0`。

### Phase 25 (2026-10-23 に起こした候補。デプロイを待たずに段 1 だけ進められるものもある)

番号は仮。**Phase 25 の見出しを決めるのは T24.99 の「次のターンの準備」**で、そこで採るかどうかを決める。

- [ ] **T25.0 `/errors` の `dns` 447 件の内訳 (NXDOMAIN と一時的な失敗)**
  - 目的: 2026-10-23 00:57Z の雪像の `/errors` 500 件のうち **`dns` が 447 件 (89%)**、`timeout` 40・`refused` 8・`unreachable` 5。名前が引けないのか、引けても遅いのか、リゾルバが落ちているのかで意味が違うのに、いまは 1 つの `dns` にまとまっている。
  - 変更箇所: `crates/net-*` / `crates/metrics-*` の原因の分け方 (`ErrCause::Dns` の内訳) と `/status` の `errors_by_cause` (足すだけ)、`scripts/` の集計。
  - やること: 段 1 で `/errors` の `dns` の行の `target` と `client`、`/recent` の `reason` から、宛先ごと・接続元ごとの偏りを数える (一時的な失敗の典型は同じ宛先の繰り返し)。分け方が決まったら段 2 で原因を 2〜3 に割る (`nxdomain` / `servfail` / `resolver_timeout`)。
  - 受け入れ基準: 段 1 は「`dns` 447 件のうち NXDOMAIN 相当が N 件、同じ宛先の繰り返しが M 件」の表。段 2 は原因別の欄を足して schema 1 のまま、過去の phase の出力は不変。

- [ ] **T25.1 `/status` の大きさ (33 KB) と、ポーリングの費用**
  - 目的: 2026-10-23 の雪像で `/status` の JSON が **33,280 B (49 の鍵)**。`hosts` (最大 800) と `clients` を丸ごと含むため、宛先が増えるほど育つ。監視が 30 秒ごとに引く使い方をすると、その帯域と CPU が積み上がる。
  - 変更箇所: なし (段 1 は計測)。段 2 なら `/status` の軽い口 (例: `?hosts=0` か `/summary`) と README。
  - やること: 段 1 で 800 宛先まで育ったときの `/status` の大きさを模型で見積もり (`/hosts` の 1 行あたりのバイト数 × 800 + 固定部)、`/snapshot` と役割が重なっていないかを確かめる。段 2 は「監視は `/status?hosts=0` を使う」を README に書くだけでも足りるかもしれない。
  - 受け入れ基準: 段 1 は「いま何 B、800 宛先で何 B」の実測/見積もりと、`/status` を引いているのは誰か (`/readers`)。

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
