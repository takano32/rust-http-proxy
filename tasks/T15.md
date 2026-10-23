# Phase 15 — (2026-09-18 に組み替えた: まず載せる、それから決める)

> `TODO.md` の §5 から 2026-10-22 に移した (T24.9)。**本文は 1 文字も変えていない**。
> 番号 (`T15.12` 〜 `T15.12`) はそのまま。コードのコメントの `TODO.md Tn` は、T15 以降はこのディレクトリ、T15 より前は `docs/HISTORY.md` を指す。

- [x] **T15.12 ビルドメモリの関門を 120 MB にする (理想値。旧 T15.0 — 2026-09-18 に番号を移した。デプロイのあと、24 時間の待ちに充てる)**
  - 目的: 関門は `jobs = 1` なので効くのは**いちばん重い 1 クレート**。T10.9 の 26 クレート構成では 100 MB で通っていた (最大 85.5 MB、最小 36 MB)。
    120 MB を切るには**全クレートを 100 MB 未満**に保つ必要がある (T14.55 の実測: 全部 100 MB 未満なら通る最小は約 120)。
  - **目標の決め直し (2026-09-19、利用者の決定)**: 関門の目標は 120 MB のまま、**目安は「通る最小が 100 MB 未満」** (関門に対して余裕を持たせる。120 ぎりぎりで通しても CI が揺れるだけ)。
    経験則は「通る最小 ≒ いちばん重い 1 クレートの RssAnon + 18〜20 MB」(T14.55 と 2026-09-19 の実測: 121.7 → 140) なので、**全クレートを約 80 MB 未満**にする必要がある
    (T10.9 の 26 クレート構成は最大 85.5 MB で通る最小 100 MB だった)。2026-09-19 の段 1〜3 のあとの実測で 80 MB を超えているのは 11 クレート:
    `proxy-metrics-watch` 121.7 / `proxy-metrics-window` 106.8 / `proxy-endpoints` 97.9 / `proxy-metrics-recent` 95.5 / 本体 94.6 / `proxy-net-dns` 89.7 / `proxy-metrics-core` 88.6 /
    `proxy-http` 86.3 / `proxy-metrics-types` 85.1 / `proxy-sysinfo` 82.8 / `proxy-blocklist` 81.9。**段 1〜3 だけでは足りず、段 5 以降を足す** (上から順に割り、割るたびに測る。
    RssAnon は行数に比例しないので、何が重いのか (単相化、`format!`、大きな `match`、テーブル) を先に調べてから線を決める)。
  - **T15.0 / T15.4 / T15.6 と並行しない**: このタスクが割るのは `proxy-endpoints` / `proxy-metrics-core` / `proxy-net` の 3 つで、しかも**ファイルを割る**ので、同じクレートに
    行を足すタスクと並行すると delete / modify の衝突になる。T15.4 (`crates/net/src/dns.rs`) が main に入ってから始める。**着手の前に、まず `scripts/build-memory.sh --find` と
    クレートごとの RssAnon の表を測り直す** (T14.55 の表は T15.0 の波の前のもの。T15.0 は 3 クレートとも太らせた)。
  - **いまの大きさ (2026-09-19 の下読み。main `5f6523b`、総行数 / 非テスト行)**: `proxy-endpoints` **4,808 / 3,001** (`endpoints/mod.rs` 1,158 / 813、`recent.rs` 1,932 / 838、
    `explain.rs` 817 / 694、`health.rs` 265 / 206、`profile.rs` 201 / 201、`config.rs` 214 / 97、`pac.rs` 130 / 61、`blocklist.rs` 67、`lib.rs` 24。ほかに `src/web/` の HTML 3 本 =
    81,673 + 58,188 + 23,754 = **163,615 B** を `include_str!`)。`proxy-metrics-core` **3,310 / 1,956** (`metrics.rs` 2,554 / 1,498、`canary.rs` 737 / 439、`lib.rs` 19)。
    `proxy-net` **3,725 / 2,088** (`dns.rs` 2,028 / 1,176、`net.rs` 1,235 / 589、`acl.rs` 448 / 309、`lib.rs` 14)。T14.55 の表で 100 MB を超えていたのはこの 3 つだけ
    (115.6 / 110.2 / 108.5 MB)。**RssAnon は行数に比例しない** (net は endpoints の 2/3 の行数でほぼ同じ)。
  - **割るときに当たる所 (下読みが見つけた。知らずに始めると途中で設計をやり直すことになる)**:
    - `recent.rs` は **`mod.rs` を上向きに呼ぶ**: `recent::snapshot` の中の `recent.rs:724-726` → `super::status_body`、`730,734,738` → `super::history_body`、
      **`751` → `super::profile::profile`**。`profile.rs:64` も `super::recent::MAX_BODY`。submodule が使う私有ヘルパは `pub(super)` の `has_flag` (`mod.rs:272`) / `offset_param` (285) /
      `next_offset` (295) / `status_body` (601) / `history_body` (623) / `history_summary` (636) と `pub` の `parse_query` (771) / `percent_decode` (783)。
    - `health.rs:173` は `crate::anomaly::cpu_window_is_full` (= `proxy-metrics-watch`) を見る (T15.0 (6))。facade 越しなのでクレートの層は壊さない。
    - **`pub(crate) static RESOLVE_TEST_LOCK`** (`crates/net/src/dns.rs:256`) を `net.rs:1185` と `acl.rs:436` のテストが取る → 割るとコンパイルが通らない (`pub` にする。
      T14.55 の `events::TEST_LOCK` と同じ)。割ったあとは 3 クレートのテストが**別プロセスになり、この鍵では直列化が効かなくなる** (`dns.rs:253-256` のコメントが前提を書いている)
      ので、`mx cargo test -p proxy-net-dns` / `-p proxy-net-conn` / `-p proxy-net` を別々に回す。
    - **`net.rs:1203` のテストが `crate::acl::resolve_target` を呼ぶ** → そのまま割ると `proxy-net-conn` が dev-dependency で acl 側を要求し、**T14.55 が踏んだ
      「dev-dependency の輪でクレートが 2 回コンパイルされてトレイトが別物になる」そのもの**になる。このテスト (`net.rs:1185-1225` 付近) は acl 側へ移す。
    - 逆向きの依存は本文どおり 1 か所: `crates/net/src/dns.rs:644` の `crate::net::split_host_port_ref`。`proxy-sys` を使うのは `net.rs:158,299` だけなので、
      `proxy-net-dns` の依存は `proxy-base` だけで済む。
  - やること (**4 つに分けて、1 つ = 1 エージェント = 1 worktree。1〜3 は単独でマージできて価値がある。順番は 段 2 と段 3 を同時に着手 → 段 2 を先に取り込み → 段 3 →
    段 1 → 段 4**。段 2 は新しいクレートを作らないので `Cargo.toml` の `members`・`Cargo.lock`・README のクレートの表・`check-docs.sh` の (e) に一切触らず、ほかの段と完全に並列でよい。
    段 1 と段 3 はその 4 つを共有する (行の追加なので衝突は自明に解ける)。段 1 は `explain.rs:92,176` / `pac.rs:17` / `blocklist.rs:18` で `split_host_port_ref` を呼ぶので、
    段 3 を先に取り込めば段 1 のリベースは 1 回で済む):
    1. **`proxy-endpoints` を割る** (逆依存は本体だけ)。**段階的に、測りながら**: (i) まず HTML 3 本だけを葉クレート `proxy-web` (中身は `include_str!` の 3 つの const だけ) に出して
       `--find` で測る (160 KB の文字列定数が rustc に効くかは未測定。逆依存ゼロ・20 行で、効けばここで終わる)。(ii) 足りなければ `explain.rs` (694 行。`super::{Endpoint, parse_query}` しか
       使わず、呼ぶのは `mod.rs:484` の 1 か所) を下ろす。(iii) それでも足りなければ `Endpoint` (`mod.rs:32-59`) + 私有ヘルパ + `status_body` / `history_body` / `history_summary` +
       recent / profile / health / config / pac / blocklist をまとめて下の 1 クレート (`proxy-endpoints-detail`) へ移し、`mod.rs` には `handle` (313 行〜)・`local_path`・`endpoint_list`・
       `is_heavy` / `begin_heavy`・`register_snapshot`・`purge_url`・`lookup` を残す (`path == "/…"` の 33 本が `mod.rs` に残るので `check-docs.sh` の (a) が生きる。recent → mod の
       上向き 3 本もこれで一方向になる)。
    2. **`canary` を `proxy-metrics-core` から `proxy-metrics-watch` へ上げる** (逆依存は `metrics-watch` だけ)。`crates/metrics-core/src/metrics.rs:1274` (`push_env` の中) の
       `crate::canary::status_json()` は、**`crates/metrics-types/src/kernel.rs:312-318` の `set_state_file_errors(f: fn() -> Option<u64>)` を写した口**
       (`metrics::set_canary_status(fn() -> String)`) でほどく。**既定 (預けていないとき) は canary が off のときと 1 バイト同じ文字列を返すこと** (預け忘れると `/status` の `canary` が
       静かに変わる)。`crates/metrics-watch/src/lib.rs:16` の再輸出から `canary` を外して `pub mod canary;` にし、`crates/metrics/src/lib.rs:24-27` の再輸出はそのまま →
       `prom.rs:589` / `reload.rs:228` / `tick.rs:85` / `tests/canary_test.rs` は 1 行も変わらない。`canary.rs` のテスト (440〜737 行) は `Metrics` を使うが、上へ上げれば向きが揃う。
       ~~`metrics-watch` は T14.55 で 90.5 MB だったので、canary を足して 100 を超えたら `push_head` / `push_tables` / `push_env` / `memory_json` を新しい葉クレートへ出す。~~
       **(2026-09-19 訂正: この保険は宛先違い。4 つとも `crates/metrics-core/src/metrics.rs` に居るので、出して減るのは `metrics-core` の方で `metrics-watch` には効かない。
       親の書き間違い。`metrics-watch` を軽くするのは段 5。)**
    3. **`proxy-net` を `proxy-net-dns` → `proxy-net-conn` → `proxy-net` (acl + facade) に割る** (逆依存 11 クレート)。`split_host_port` と `split_host_port_ref` (とそのテスト) を
       **`proxy-base` の新しい `hostport` モジュール**へ移し、`proxy-net-conn` の `net` モジュールで `pub use` して**綴りを維持**する (使っているのは 12 か所)。
       **facade の形 (`crates/net/src/lib.rs:6-14`) を崩さなければ 11 クレートは 1 行も変わらない**。
    4. **3 つ入って `--find` が 120 で通ったら**関門を 120 に下げる。書き換え先: `scripts/build-memory.sh:26,31,32`、`.github/workflows/ci.yml:54,61,163` (+ ladder の既定の 14 行と 217 行)、
       `src/lib.rs:6`、**`crates/*/src/lib.rs` の冒頭コメント ちょうど 30 本** (`grep -l "180 MB" crates/*/src/lib.rs`)、`README.md:1717,1718,1724,1728,1729,1767,1768,1910,1925,1937` と
       1674 行の「(32 + 本体)」・1720〜1721 行の「いちばん大きいクレート」・1886 / 1926〜1927 行の「26 クレート」、この TODO.md の §0 (57 行) / §1 (267・280 行) / §2 (368 行。親が書く)。
       手本は `2ac988f` (関門の数字を 29 ファイル一括で書き換えた)。通らなければ 4 はやらず、測った表だけ残す。
    - どの段も**中身は 1 バイトも変えない**。手本は `6967236` (`proxy-metrics` を 6 つに割った。`git mv` で rename の検出を効かせ、各 `lib.rs` で下の層を今までの名前で再輸出) と
      `13fa546` (層の制約を分けてほどいた形)。割るたびに `mx bash -c 'cargo clean --release && scripts/build-memory.sh --find 110 115 120 125 130 135'` (1 周 約 1 分 24 秒。いまの最小は 135 MB) と
      クレートごとの RssAnon の表を `結果:` に貼る。配布用 `dist` (LTO) は関門の外のまま。
  - **直値で見張っているもの** (動かすと落ちる): `scripts/check-docs.sh` の (e) (README のクレートの表と「(32 + 本体)」の数 → 段 1 と 3 で必ず落ちるので表を直す)、同 26 行の
    `MOD=crates/endpoints/src/endpoints/mod.rs`・27 行の `REC=…/recent.rs`・170 行の `open('crates/endpoints/src/web/' + html)` (段 1 でファイルを動かしたら 3 か所とも書き換える)、
    `scripts/check-dashboard.js:46 / 757 / 1134` (HTML 3 本を直に開く。**`dashboard.html` は上限まで余り 247 B なので、移すときに 1 バイトも増やさない**)、
    `crates/endpoints/src/endpoints/mod.rs:880` と `:1043` のテスト (私有の `super::DASHBOARD_HTML` / `super::INSPECT_HTML` を読み、`inspect.html <= 64 KiB` を直値で見る → const と一緒に動かす)。
  - テスト: 新しいテストは要らない (移したテストが同じ名前で残ることを確かめる)。段ごとに `mx cargo test --test schema_test` (`/` の案内から全部の口を機械的に取り出して触る、いちばん安い全口の確かめ)、
    `node scripts/check-dashboard.js`、`./scripts/check-docs.sh`。**このタスクは段ごとに全体テストを回す** (`cargo test --workspace --no-fail-fast`。中身を変えないことの証拠がそれしか無い)。
    段 1 は、24 本のエンドポイントを分割の前後で取り、入れ子まで鍵の並びを突き合わせる (`/status` の出力の順序)。最後に `cargo build --profile dist`。
  - 受け入れ基準: 1〜3 はそれぞれ、割ったクレートの RssAnon が 100 MB 未満になったことを表で示すこと。4 は `scripts/build-memory.sh 120` が通ること。
    ビルド時間の増分を報告 (クレートが増えるぶん)。全体テスト全通過、24 本の JSON の鍵の並びが前後で同じ。
  - 結果 (段 1〜4、2026-09-19、波 `wave10`。取り込み `085cee0` 段 3 → `cf908a1` 段 2 → `9626c8b` 段 1、段 4 は変更なし): **段 1〜3 はそれぞれ狙ったクレートを 100 MB 未満にしたが、
    関門 120 MB は通らない。通る最小は 135 → 140 MB に上がった** (135 は 2 回とも本物の cgroup OOM-kill)。クレートは 32 → **37 + 本体**、release ビルド 89 → 95 秒、テスト 790 → 792 本。
    中身は 1 バイトも変えていない (全体テスト全通過、`--lite` の費用は システムコール 5.028 回/要求・確保 10.006 回/要求で不変、`/` の案内の 32 経路すべて応答)。
    - 測り方はどれも同じ: 手元 aarch64、`cargo clean --release` → `cargo build --release`、`jobs = 1`、20 ms ごとに `/proc/<pid>/status` の RssAnon の最大。
  - 結果 (2026-09-26、CI run 36249911685、`build-memory-find`、ubuntu-24.04 / x86_64、main `e8aef0a` = T17.11 の後、`-f ladder="80 85 90 95 100 110 120"`): **80・85・90・95 MB は落ち (exit 143)、100 MB で通った** (通る最小の上限 100 MB)。手元 (aarch64、2 コア) は毎回落ちる最大 85 / 毎回通る最小 90 MB なので、**CI は手元より 10 MB 重い**。関門 120 MB に対し CI で 20 MB、手元で 30 MB の余裕。push の `build-memory` ジョブ (120 MB) も通過 (run 36249391531)。

      | クレート | 着手前 (`930b90f`) | 段 1〜3 の後 (`9626c8b`) | 何が起きたか |
      |---|---|---|---|
      | `proxy-metrics-watch` | 94.9 MB | **121.7** | 段 2 で canary を受け取って +26.8 (いまの最大) |
      | `proxy-metrics-window` | 107.5 | **106.8** | 手を付けていない (T15.0 の波で 97.1 → 107.5 と 100 を超えていた。計画に段が無かった) |
      | `proxy-endpoints` | 119.0 | 97.9 | 段 1 (ii) で -21.1 |
      | `proxy-metrics-recent` | 94.6 | 95.5 | — |
      | 本体 | 93.8 | 94.6 | — |
      | `proxy-net-dns` | (旧 `proxy-net` 111.2) | 89.7 | 段 3 で新設。`proxy-net-conn` と `proxy-net` は 80 未満 |
      | `proxy-metrics-core` | 111.7 | 88.6 | 段 2 で -23.1 |
      | `proxy-http` / `metrics-types` / `sysinfo` / `blocklist` | 86.4 / 84.5 / 82.6 / 83.6 | 86.3 / 85.1 / 82.8 / 81.9 | — |
      | 新しいクレート | — | `endpoints-explain` 61.3 / `endpoints-core` 30.3 / `web` 7.2 | 段 1 |

    - **段 1** (`31dd143` / `e1168cb`): (i) HTML 3 本 (163,615 B) を葉クレート `proxy-web` へ → **-0.9 MB しか効かない。160 KB の文字列定数は rustc のメモリにほぼ関係ない** (本文の「未測定」への答え)。
      (ii) `explain.rs` と `Endpoint` + 問い合わせ文字列のヘルパを `proxy-endpoints-explain` / `proxy-endpoints-core` に下ろして 119.0 → 98.4 MB。100 を切ったので (iii) はやっていない。
    - **段 2** (`37ffb70`): `canary.rs` 737 行を `git mv` で `metrics-watch` へ。`push_env` からの呼び出しは `metrics::set_canary_status(fn() -> String)` の預け口でほどき、預かっていないときの既定
      `CANARY_STATUS_UNSET` が canary off の `status_json()` と 1 バイト同じことを新しいテスト 2 本で縛った。`metrics-core` は 100 未満になったが、**同じ重さが `metrics-watch` に移っただけ**
      (-23.1 と +26.8。canary はどこに置いても約 25 MB)。レビューの must_fix 2 件: README のクレートの表の説明が古いまま (直した)、本文の保険が宛先違い (上の訂正)。
    - **段 3** (`435bcee` / `938072c`): `proxy-net` を `proxy-net-dns` → `proxy-net-conn` → `proxy-net` (acl + facade) に。逆依存 11 クレートと本体は 1 行も変えていない。本文に無かった判断:
      `IPV6_TEST_LOCK` も `pub` にした (`crates/net-conn/src/net.rs:27`)、`proxy-net-conn` は `proxy-base` にも直接依存する (`proxy_base::timeout` を名指しで呼ぶ)。
      古いパス (`crates/net/src/net.rs` / `dns.rs`) を指すコメント 8 か所を直した。
    - **段 4**: `--find 105…130` は全部落ち、`135` も落ち、**通る最小は 140 MB** (= 121.7 + 18)。関門は 180 のまま、書き換えはしていない。
    - 気づき: (1) 重さは「移す」だけでは減らない。canary の約 25 MB のように、**どのモジュールが何 MB 持っているかを先に測ってから線を引く** (段 5 以降の前提)。
      (2) 計測スクリプトは機械上の全 rustc を拾うので、`mx` が 2 口のときは 1 回の値を信じない (2 回取る)。(3) 揺れるテストが 1 本見つかった (`clientacl_test`。Phase 14 の「既知の小物」に足した)。
  - **何が重いのかの調査 (2026-09-19、親が測った。段 5 以降の前提)**: `RUSTC_BOOTSTRAP=1 mx cargo rustc --release -p proxy-metrics-watch -- -Ztime-passes` (安定版の rustc 1.96.0 でそのまま動く。
    `rss:` は VmRSS でファイル由来のページを含むので、絶対値ではなく段の間の差を見る) で、**山はコード生成 (LLVM) の段**だった: 型検査の終わり 151 MB → メタデータ 187 MB →
    `codegen_to_LLVM_IR` + `LLVM_passes` で **259 MB (+72 MB)**。`[profile.release]` の `codegen-units = 1` (最初のコミットからあり、**一度も測って決めていない**。`git log -S` で確認) が
    クレート全体の LLVM IR を 1 つのモジュールとして一度に抱えさせている。
    - **`codegen-units` を増やすと山が削れる** (`jobs = 1` なので rustc の中の LLVM の並列もトークン 1 つで、モジュールは順に最適化して捨てられる)。全クレートに当てた実測
      (`CARGO_PROFILE_RELEASE_CODEGEN_UNITS=<n>`、測り方は上の表と同じ):

      | | `codegen-units = 1` (いま) | `= 4` | `= 16` |
      |---|---|---|---|
      | `proxy-metrics-watch` | 121.7 MB | **92.5** | 84.3 |
      | `proxy-metrics-window` | 106.8 | 83.2 | 81.4 |
      | `proxy-endpoints` | 97.9 | 80.2 | 80.2 |
      | 本体 (`rust_http_proxy`。lib と bin が同じ名前で混ざる) | 94.6 | 79.2 | 79.3 |
      | `proxy-metrics-recent` | 95.5 | 76.9 | 75.4 |
      | `proxy-config` / `proxy-http` / `metrics-core` / `metrics-types` / `net-dns` | 76.0 / 86.3 / 88.6 / 85.1 / 89.7 | 74.6 / 72.4 / 70.5 / 68.9 / 68.1 | 74.6 / 71.8 / 69.7 / 66.6 / 66.6 |
      | **通る最小の上限** (`--find`) | **140 MB** | **120 MB** (115 は落ちる) | 115 でも通らない (それ以上は未測定) |
      | release ビルドの時間 / バイナリ | 95 秒 / 2,498,360 B | 106 秒 / 2,694,968 B | 133 秒 / 2,760,504 B |

      4 → 16 はほとんど増えない (コード生成の前の段 = 型検査とメタデータの床が約 80 MB)。**4 で十分**。
    - **`= 4` にすると床が「最後の 1 段」に移る**: 115 MB で落ちるのは `proxy-metrics-watch` ではなく、**bin の `rust-http-proxy` (`src/main.rs` 599 行) をコンパイルしてリンクする最後の段**。
      `touch src/main.rs` してこの段だけを cgroup に入れると 110 MB で落ち 118 MB で通る (110〜118 の間は回ごとに揺れる)。`dmesg` の OOM の記録は毎回
      `Killed process (rustc) anon-rss:55 MB, file-rss:79 MB`。`-Ztime-passes` では bin なのに `codegen_crate` が **+62 MB** (130 → 192 MB、LLVM 1.5 秒) ある = lib から bin へ単相化・インライン化されて
      来るものが多い。~~リンカを変えても効かない~~ (`-fuse-ld=gold` は 95 MB で落ちる。`-Wl,--no-keep-memory` は 110 MB が通るようになる)。
      **(2026-09-19 訂正、段 6 の実測: 最後の段の山はコード生成ではなく「リンク中」= rustc が 55〜59 MB を握ったまま `ld` が 38〜49 MB を積む。`ld` は子プロセスなので `-Ztime-passes` の `rss:` に入らず、親が見落とした。下の段 6 の結果)****ここを削らないと、クレートをどれだけ割っても通る最小は約 115 MB から下がらない。**
    - 代償は実行時の速さの可能性 (クレートの中でのインライン化の範囲が狭まる)。CPU/要求 の 7 割はカーネル側 (T9.1) なので大きくは動かないはずだが、**測って決める**。
      いまこの機械は利用者のほかの作業で CPU が汚れていて A/B が取れない (2026-09-19) → **要求の経路に乗らないクレートだけ先に 4 にする** (段 5)。要求の経路のクレートは段 7 で (手元の A/B ではなくデプロイ先で見る。段 7 の本文)。
  - **続きの段 (2026-09-19 に足した。段 5 と段 6 は触るファイルが別なので並列でよい。枝は `wave11/<id>`)**:
    5. **要求の経路に乗らないクレートの `codegen-units` を 4 にする** (`t1512s5`)。ルートの `Cargo.toml` に `[profile.release.package.<名前>]` `codegen-units = 4` を並べる (`dist` は `inherits = "release"` なので
       同じ上書きが効く。`dist` は関門の外で fat LTO なので、`[profile.dist.package.<名前>]` で 1 に戻す必要があるかを `cargo build --profile dist` のバイナリの大きさで確かめ、変わるなら戻す)。
       **(2026-09-19 訂正: 下の 14 個のうち `proxy-endpoints` / `proxy-metrics-window` / `proxy-selfbench` / `proxy-reload` / `proxy-sysinfo` の 5 つは要求の経路から呼ばれていた。親の思い込み。`src/lib.rs` に `handle_client` という関数も無い。段 5 の結果)**
       **対象 (親が決めた。1 要求・1 接続ごとには呼ばれない = tick スレッド・管理用の口・起動と再読込・道具)**: `proxy-metrics-watch`、`proxy-metrics-window`、`proxy-endpoints`、`proxy-endpoints-explain`、
       `proxy-endpoints-core`、`proxy-config`、`proxy-sysinfo`、`proxy-prom`、`proxy-reload`、`proxy-selfbench`、`proxy-rrd`、`proxy-diskprobe`、`proxy-capacity`、`proxy-bench`。
       **対象にしない (要求か接続ごとに通る。段 7 まで 1 のまま)**: 本体、`proxy-http`、`proxy-tunnel`、`proxy-net` / `-conn` / `-dns`、`proxy-base`、`proxy-sys`、`proxy-metrics` / `-types` / `-core` / `-recent`、
       `proxy-blocklist`、`proxy-cache*`、`proxy-origin`、`proxy-msg`、`proxy-tls`、`proxy-workers`、`proxy-freshness`、`proxy-web`。着手したら、対象のクレートの関数が要求の経路から呼ばれていないことを
       `src/lib.rs` の `handle_client` からたどって確かめ、呼ばれているものがあれば対象から外して報告する (`proxy-config` の `Config` の読み出しはフィールドの参照なので対象のままでよい)。
       受け入れ基準: 対象のクレートが全部 95 MB 未満 (表)、`--find` の通る最小が 140 MB から下がること (見込みは 115〜120。`metrics-recent` 95.5 + 18 と最後の段のどちらかで決まる)、
       `--lite` の システムコール/要求 と 確保/要求 が不変 (`scripts/cpu-per-request.sh`。CPU/要求 は機械が汚れているので参考値)、全体テスト全通過、`cargo build --profile dist` が通ること。
       `Cargo.toml` の `[profile.release]` のコメントに、この実測の表 (1 / 4 / 16) と「要求の経路のクレートは 1 のまま」の理由を書く。
    6. **最後の段 (bin のコンパイルとリンク) を 95 MB で通るようにする** (`t1512s6`)。まず再現と切り分け: `mx cargo build --release` のあと `touch src/main.rs` →
       `mx systemd-run --user --scope --quiet -p MemoryMax=<N>M -p MemorySwapMax=0 "$(command -v cargo)" rustc --release --bin rust-http-proxy` を N = 90 / 100 / 110 / 120 で (各 3 回。110〜118 は揺れる)。
       `-Ztime-passes` と、cgroup の `memory.peak` / `memory.stat` (anon / file) を 20 ms ごとに読んで、**山が rustc のコード生成なのかリンカなのか**を先に決める。コード生成なら、bin に単相化されて来るものを
       `RUSTC_BOOTSTRAP=1 cargo rustc --release --bin rust-http-proxy -- -Zprint-mono-items=lazy` (または `--emit=llvm-ir` の大きさ) で数え、`src/main.rs` の `main` の中身 (`check_environment` / `run_self_bench` /
       起動の組み立て) を **lib の側の、総称でも `#[inline]` でもない `pub fn`** へ移して、bin は `fn main() { std::process::exit(rust_http_proxy::run_main()) }` に近い形にする
       (lib の側が太るので、lib の RssAnon が 95 MB を超えないことを表で確かめる。超えるなら移す先を新しいクレートにする)。リンカが山なら `.cargo/config.toml` の `[target.…] rustflags` ではなく
       (全クレートが作り直しになり、利用者の環境のリンカに依存する) まず rlib の大きさ (`target/release/deps/*.rlib` 合計 26 MB) と `strip` / `debuginfo` の設定を見る。
       **挙動は 1 バイトも変えない** (`--check` の 9 行、`--help`、終了コード、起動ログの順序。`tests/` の起動系のテストと `scripts/check-docs.sh` が見張っている)。
       受け入れ基準: 最後の段だけを入れた cgroup が **95 MB で 3 回中 3 回通る**こと、前後の `-Ztime-passes` の比較、全体テスト全通過、`--lite` の費用が不変。届かなければ、測った内訳と「次に何を動かせば何 MB 減るか」を報告する (変更なしでもよい)。
       - 結果 (2026-09-19、波 `wave12`。`af74634` (server) → `434f7e1` (run) → `482c651` (レビューの直し)、取り込み `309fe81`): **最後の段の床を 106 → 65〜70 MB に下げた。通る最小は動かない (見込みどおり) —
         ただし「120」は通ったり落ちたりする境目だと分かった**。`src/lib.rs` / `src/idle.rs` / `build.rs` を `crates/server/` へ、`src/main.rs` を `crates/run/src/lib.rs` へ `git mv`。ルートは
         `pub use proxy_server::*;` の facade と `fn main() { proxy_run::main() }` だけ。`pub` を足した項目は 0 (`fn main` → `pub fn main` だけ)。`.rs` の差分は doc リンク `[proxy_stats]` → `[proxy_metrics]`、
         `pub fn main`、rustfmt が 2 か所を 1 行に詰め直した (綴りが 4 文字短くなったため) の 4 つ。
         **最後の段だけの cgroup: 70 MB 3/3 (別の組で 2/3)・75 MB 3/3・65 MB 0/3** (前は 110 MB 2/3・120 MB 3/3)。RssAnon: `proxy-server` 94.4 / `proxy-run` 94.2 / ルート (facade + bin) 93.5 → **18.6**。
         上位は `metrics-window` 107.8 > `endpoints` 98.4 > `metrics-recent` 95.7 で変わらず。**`--find`: 同じ日に main (`e797376`) と交互に 6 回ずつで、120 MB は main 2/6・枝 0/6、直しの担当の測り直しで枝 1/2、
         125 MB は毎回通る、115 以下は毎回落ちる、落ちるのは毎回 `proxy-metrics-window`** → README / `Cargo.toml` / `build-memory.sh` の「120 MB で通る」を「120〜125 MB (120 は境目)」に直した。
         挙動は同一: `--help` / `-h` / `-V` / `--version` / `--check` / 未知のフラグの出力と終了コードは一時パス・版の hash・`resolver_ms` 以外 `diff` なし、`--lite` は 5.021 → 5.020 回/要求・確保 10.006 → 10.006、
         release 2,563,896 B で不変 (dist −8 B)。版の文字列は `build.rs` ごと移して `--version` が新しい HEAD になることを確認。`Dockerfile` の `COPY` と README の Docker の節から `build.rs` を外し、
         README のクレートの表に 2 行 (39 + 本体)、パスのコメント 19 か所。全体チェック (絞った版、約 25 分): fmt / clippy 警告 0 / **792 本通過** / 関門 180 / `dist` / scripts 213 本 / dashboard / check-docs 差分 0 /
         起動して `--version` と `/status` の版が `0.1.0+309fe81`。実装中に `proxy-sys` の fd を数えるテストが 1 回揺れた (「既知の小物」に足した)。残り: `crates/web/src/probe.html:137` の `src/lib.rs` という文言は
         配る HTML の中身なので直していない (次に probe.html を触るときに)。
       - 気づき: **cgroup の合否も境目では揺れる** (120 MB が 2/6)。通る最小は 1 回の `--find` の値ではなく「毎回通る最小 / 毎回落ちる最大」の幅で書く (いまは 125 / 115)。関門を下げるときは「毎回通る最小 + 余裕」で決める。
    7. **全クレートを `codegen-units = 4` にする (T15.99 のあと、段 6' が入ってから。手元の CPU の A/B はやらない)**。
       **2026-09-24、利用者の決定: 4 で次の再デプロイに入れて試す。問題があれば利用者が戻すよう指示する** (戻し方は `[profile.release]` の `codegen-units` を 1 にする 1 行。段 5 の上書きは消したままでよい)。
       - 結果 (2026-09-24、`3acd5b5`、取り込みは段 7 のマージ): `[profile.release]` を `codegen-units = 4` にし段 5 の上書き 9 個を消した (戻すときはこの 1 行)。`dist` は release の 4 を継ぐとファイルは 1,842,976 B のまま
         (ページの詰め物で隠れる) だが `.text` が 1,769,784 → 1,786,700 B に太ったので `[profile.dist]` に `codegen-units = 1` を足した (`.text` 1,766,292 B)。**段 5 の「dist は前後で不変」もファイルの大きさだけの判定だった**。
         RssAnon の最大は `metrics-window` 108.0 → **`metrics-watch` 92.3 MB** (以下 window 82.9 / endpoints 79.9 / server 79.6 / recent 75.4 / config 73.9 / http 72.2 / core 70.8 / net-dns 68.0 / types 66.9 / run 64.5)。
         release 2,563,896 → 2,694,968 B、クリーンの release 97 → 103 秒。`--find` (2 回): 95 MB 2/2 落ち (metrics-window)、100 MB 2/2 落ち (metrics-watch)、**105 MB 1/2**、2 回目の 110 MB は**この機械のメモリ不足** (利用者のほかの作業。空き約 0.5 GB)
         で Claude Code が止めたので未測定 → **毎回落ちる最大 100 / 105 は境目 / 毎回通る最小は 110 以上で未確定**。README は「約 110 MB (105 は境目)」と書いた。`--lite` 5.022 → 5.023 回/要求・確保 10.006 → 10.006、
         テスト 792 本通過、clippy 警告 0、check-docs 差分 0。CPU の A/B は方針どおり取っていない。**残り: 機械のメモリに余裕があるときに `--find 105 110 115` を 2 回** → 毎回通る最小を決めてから段 4 / 段 8。
         - **測り直し (2026-09-26、rustc stable 1.98.1、この機械 aarch64 2 コア・空き 9.6 GB)**: T16.99 で `codegen-units = 4` を残すと決めたあと、`cargo clean --release` +
           `scripts/build-memory.sh --find 100 105 110 115 120` を 2 回: **100 MB は 2/2 落ち (cgroup の OOM で exit 143)、105 MB は 2/2 通った** (1.96 では 1/2)。
           **毎回落ちる最大 100 / 毎回通る最小 105**。利用者の目安 (「通る最小が 100 MB 未満」、2026-09-19) にはまだ届かないので、**段 4 (関門を 120 に下げる) はやらない**。関門は 180 のまま。
           次に下げるなら上位 (`proxy-metrics-watch` 92.3 MB ほか、段 7 の表) を割る段 8 から。
       **2026-09-19、利用者の指摘で方針を変えた**: デプロイ先とこの機械は別物 (CPU も kernel も違う) で、しかも手元の CPU/要求 はぶれ ±4%。`codegen-units` の効きはユーザー空間 (CPU/要求 の約 3 割、T9.1) の
       さらに一部で、fat LTO でも −4.5% だった (T9.1) ことからぶれより小さい見込み → **手元で A/B を取っても何も決められないので測らない**。代わりに:
       (i) 手元では**揺れない数字だけ**を確かめる (`--lite` のシステムコール/要求と確保/要求が不変、全体テスト、バイナリの大きさ)。(ii) `[profile.release]` を 4 にして段 5 の上書き 9 個を消す。
       (iii) **速さはデプロイ先で見る**: 次の再デプロイの前後の雪像で conn 役の CPU (`scripts/snapshot-diff.py` の `_p15_conn_cores` と同じ見方: コア/要求) と `/profile` の段階の内訳を比べ、その Phase の `Tn.99` で判定する。
       **ただしデプロイ先もほかの利用者が出入りする共有の機械** (2026-09-19、利用者) なので、そこでの CPU も隣人しだいで揺れる。**数 % の差は読まない。見るのは「桁で悪くなっていないか」だけ**
       (conn 役のコア/要求が前の 1.3 倍を超える、`/profile` のユーザー空間の段階が目に見えて太る、`cpu_throttled` が出る、のどれか)。それ未満は「差なし」として 4 のままにする。
       悪くなっていたら `[profile.release]` を 1 に戻す 1 行 (と段 5 の上書きを戻す) で済む。T15.99 の 24 時間の観測を乱さないよう、**T15.99 の判定が終わるまでデプロイ先には入れない**。
       見込み: 全クレート 4 + 段 6' で、最大は `proxy-metrics-watch` 92.5 MB 前後 / 最後の段 約 77 MB → 通る最小 約 105〜110 MB。100 未満にはさらに上位 (`metrics-watch` / `metrics-window` / `endpoints`) を割る段が要る
       (割る前に、どのモジュールが何 MB 持っているかをモジュールを 1 つ抜いたビルドで測る)。
    8. 5〜7 のあと `--find` が 100 未満で通るようになったら、段 4 (関門を 120 に下げる書き換え) をやる。
  - 結果 (段 5・段 6、2026-09-19、波 `wave11`。段 5 は `2746075` / `687820f`、取り込み `65b7667`。段 6 はコミット無し): **通る最小 140 → 120 MB。関門の目標 120 にちょうど届いたが余裕は 0 で、床は bin の最後の段 (リンク中)**。
    全体チェック: fmt / clippy 警告 0 / **792 本通過** (1 回目に `history_depth_test` が 1 本揺れた。下) / 関門 180 通過 / `--find` は 100〜115 が本物の OOM で 120 が通る / `--lite` は 5.026 回・9.99 回で不変 /
    release 2,563,896 B (+65 KB)、`dist` 1,842,984 B (fat LTO が CGU の分割を呑むので前後で不変)。`.rs` は 1 本も変わっていない。
    - **段 5**: 親が挙げた 14 個のうち **5 個は要求の経路から呼ばれていた**ので実装担当が外した: `proxy-endpoints` (`endpoints::handle` → `local_path`。プロキシとして通す要求も毎回通る、`src/lib.rs:1639`)、
      `proxy-metrics-window` (`profile::mark` ほか + `Window::observe` = 毎要求・毎接続)、`proxy-selfbench` (`is_client`、`src/lib.rs:1431` = 毎要求)、`proxy-reload` (`Live::config` = accept のたび)、
      `proxy-sysinfo` (`drop_page_cache` = キャッシュ HIT / 書き込みごと)。要求の経路は `serve` (`src/lib.rs:122`) → `run_conn` (:1267) → `pump` (:1060) → `serve_one` (:1296) → forward / `tunnel`。
      残りの 9 個 (`proxy-metrics-watch` / `-endpoints-explain` / `-endpoints-core` / `-config` / `-prom` / `-rrd` / `-diskprobe` / `-capacity` / `-bench`) を `[profile.release.package.<名前>] codegen-units = 4` に。
      `proxy-metrics-watch` 121.7 → **92.2 MB**。いまの上位: `metrics-window` **107.7** > `endpoints` 98.7 > `metrics-recent` 94.9 > 本体 93.5 > `metrics-watch` 92.0 > `net-dns` 89.5 > `metrics-core` 88.8 > `http` 86.6。
      経験則「最大 + 18〜20」は今回は当たらない (107.7 + 12 = 120。決めているのは最後の段)。レビューの should_fix: `Cargo.toml` と README の `dist` / release のバイナリの大きさが 2026-09-08 の古い値のままだった (直した)。
      全体チェックの指摘: `proxy-sysinfo` / `proxy-reload` / `proxy-selfbench` を外した理由が `Cargo.toml` のコメントに無い (理由はこの段落。次に `Cargo.toml` を触るときに 1 行足す)。
    - **段 6 (変更なし。測った内訳が収穫)**: 最後の段を 20 ms ごとに rustc と `cc` / `ld` の全子孫の `/proc/<pid>/status` で見ると、同時刻の RssAnon の合計の最大は**リンク中**:
      `codegen-units = 1` で rustc 59 + `ld` 38 + cargo 9 = **106.1 MB**、`= 4` では rustc 55 + `ld` 49 + cargo 9 = **112.9 MB** (オブジェクトが増えて `ld` が太る = 4 は最後の段を悪くする)。
      cgroup での合否 (各 3 回): 90 MB 0/3、100 MB 0/3、110 MB 2/3、120 MB 3/3。**bin が重い正体は `serve` が `config_of: impl Fn()` の総称 (`src/lib.rs:126`) で、`serve` 全体が bin の側で単相化されること**
      (bin の `codegen_crate` +103 MB、LLVM 3.3 秒。lib の 95 MB には `serve` のコード生成が入っていない)。
      試した直し: `src/main.rs` の中身を lib の `pub fn run::main()` へ → 最後の段 106.1 → **66.0 MB** (95 MB の cgroup 3/3、70 MB でも 3/3)、`--help` / `-V` / `--check` / 終了コード / バイナリ 2,498,360 B まで同一。
      だが **lib が 95.3 → 134.8 MB** になり `--find` は **140 → 155 MB** に悪化 → 捨てた。呼び出しのクロージャの型を揃えて単相化を 1 回にしても lib は −1.7 MB だけ。
      本文の「新しいクレートへ」はそのままでは組めない (`crates/run` → ルートの lib → … ルートの bin → `crates/run` でパッケージが輪になる)。`build.rs` の `cargo::rustc-link-arg-bins=-Wl,--no-keep-memory` は約 8 MB。
      cgroup の `memory.peak` は読めない (`systemd-run --user --scope` の scope がこの機械の cgroup 名前空間の外に出る)。
    - 気づき: (1) **親の 2 つの誤り (山はコード生成 / 14 個は経路の外) はどちらも「測らずに / たどらずに書いた」もの**で、実装担当が測って / たどって見つけた。下読みを省いた代償。
      (2) 総量は移しても減らない (段 2 の canary と同じ形): `serve` のコード生成 約 40 MB は lib に置いても bin に置いても要る。減らすには**別の rustc プロセスに分ける** = クレートを分けるしかない。
      (3) 全体チェックが 1 時間半かかった — エージェントが裏のコマンドを毎回 10 分ちょうど待った (6 回) のと、親が測るものを積みすぎた。設定だけの波は fmt / clippy / テスト / 関門 1 回に絞り、待ちは 60 秒刻みで確かめさせる。
      (4) 揺れるテストがもう 1 本 (`history_depth_test`。Phase 14 の「既知の小物」に足した)。
  - **次の段 (2026-09-19 に足した。段 6 の置き換え)**:
    6'. **ルートの lib の中身を下のクレートへ下ろして、bin を薄くする** (輪を切る形。`t1512s6b`、枝 `wave12/t1512s6b`。2026-09-19 の下読みで書き直した)。
       - **効果の範囲 (下読みの指摘)**: 床は 2 つある — 最後の段 (106〜113 MB) と `proxy-metrics-window` (107.7 + 12 ≒ 120)。この段が外すのは前者だけなので、**通る最小は 120 のまま動かない見込み**。
         100 未満へ行くには段 7 (要求の経路のクレートも 4 に) が要り、この段はその前提 (段 7 で 4 にすると最後の段は 112.9 MB に悪化するので、先に薄くしておく)。
       - **移すもの**: `src/` は `lib.rs` / `idle.rs` (620 行) / `main.rs` (599 行) の 3 本だけ。`src/lib.rs` の自前の中身は `VERSION` (:54)・`Accepted` (:65)・`Limiter` (:77)・`serve` (:122)・`Conn` (:672)・`Step` (:731)・
         `attach_origin_rtt` (:830)・`run_conn` (:1267)・私有の `pump` / `serve_one` と、17 本の `pub use` (`Upstream` は `proxy_origin` の再輸出で、ここの中身ではない)。
         `crates/` の中でルートの lib に依存しているものは 0 件 (`crates/bench` も本体を 1 行も参照しない)。`rust_http_proxy::…` の綴りを守る相手は `tests/` だけ (62 種。全部 glob の再輸出で届く)。
       - **決めたこと**: (a) **版の文字列**: `src/lib.rs:54` の `env!("PROXY_VERSION")` はルートの `build.rs:47` の `cargo:rustc-env` なので、移した先では見えずビルドが落ちる → **`git mv build.rs crates/server/build.rs`**。
         `proxy-server` の `version` は `0.1.0` (ルートと揃える、と両方の `Cargo.toml` に注。`tests/proxy_test.rs:131` がルートの `CARGO_PKG_VERSION` と比べる)。`build.rs:30` の `git rev-parse --git-path` が
         クレートの直下から解決できるか (コミットを 1 つ積んで `--version` の hash が変わるか) を 1 回確かめる。`Dockerfile:11` の `COPY Cargo.toml Cargo.lock build.rs ./` から `build.rs` を消す。
         (b) **facade**: ルートの `src/lib.rs` は冒頭のコメント + `pub use proxy_server::*;` だけ。ルートの `[dependencies]` は `proxy-server` と `proxy-run` の 2 つに減らす
         (`[dev-dependencies]` はテストが使うものを残す)。`log_*` マクロ (`crates/base/src/log.rs:457-490`、`$crate` = `proxy_base`) は glob の再輸出で通る。
         (c) **`serve` の総称の引数 (`src/lib.rs:126`) は変えない** (`&dyn Fn` にすると単相化 約 40 MB が `proxy-server` の側に出て 95 → 135 MB になる。段 6 の実測)。単相化は `proxy-run` の側に出る。
         (d) `proxy-server` / `proxy-run` の `codegen-units` は 1 のまま (要求の経路)。
       - **道筋 (コミット 2 つ、段ごとに測る)**: 1. **server**: `git mv src/lib.rs src/idle.rs build.rs` → `crates/server/` (`src/lib.rs` / `src/idle.rs` / `build.rs`)、ルートの lib を facade に、`members` に追加 →
         `mx cargo test --workspace --no-fail-fast` (`-p proxy-server` に単体テスト 5 本が同じ名前で残る) + クレートごとの RssAnon。2. **run**: `git mv src/main.rs crates/run/src/lib.rs` → `pub fn main()`、
         `rust_http_proxy::` → `proxy_server::` (約 40 か所)、ルートの `src/main.rs` は `fn main() { proxy_run::main() }` → `--help` / `-V` / `--check` / 未知のフラグの出力と終了コードを前後で `diff`、
         最後の段の cgroup、`--find`、`--lite` の費用。
       - **直値で見張っているもの**: `scripts/check-docs.sh:191-205` の (e) は必ず落ちる → README のクレートの表に 2 行、「(37 + 本体)」→「(39 + 本体)」(README の 2 か所)。パスのコメント:
         `crates/selfbench/src/lib.rs:29,221`、`scripts/ci-snapshot.sh:127`、ほか `grep -rn "src/lib.rs\|src/main.rs"` で当たる所 (TODO.md は親が直す)。`check-docs.sh` が `src/lib.rs` / `src/main.rs` を直に開いていれば道具の側のパスも直す。
         無傷のはず: `env!("CARGO_BIN_EXE_rust-http-proxy")` の 8 ファイル、`tests/` の綴り、`build-memory.sh`、`ci.yml`。ついでに `src/lib.rs:3` の doc リンク `[proxy_stats]` (存在しないクレート) を直す。
       - 受け入れ基準: **最後の段だけの cgroup が 70 MB で 3 回中 3 回通る**、`--find` の通る最小が 120 から悪化しない、`proxy-server` と `proxy-run` の RssAnon がどちらも 100 MB 未満 (表)、
         `--help` / `-V` / `--check` / 終了コードが前後で同一、全体テスト全通過 (792 本から減らない)、`--lite` のシステムコール/要求・確保/要求が不変、`mx cargo build --profile dist` が通る。

