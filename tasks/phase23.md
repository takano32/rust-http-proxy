# Phase 23 — (2026-10-20 に決めた: エラーの物差しを使われ方から切り離す・無言の再起動を見えるようにする・10 秒待たせている CONNECT の締め切り。T23.3 は本体を変えるので再デプロイする)

> `TODO.md` の §5 から 2026-10-22 に移した (T24.9)。**本文は 1 文字も変えていない**。
> 番号 (`T23.0` 〜 `T23.99`) はそのまま。コードのコメントの `TODO.md Tn` は、T15 以降はこのディレクトリ、T15 より前は `docs/HISTORY.md` を指す。

**利用者の決定 (2026-10-20)**: Phase 22 は完了 (「T22.4 は戻さない」)。Phase 23 を計画する。
材料は §2 の「デプロイ先の現在地 (2026-10-20)」と T22.99 の「結果」。要点は 3 つ:
**(1)** エラー 6,609 件 (`dns` 2,324・`timeout` 4,212) のほぼ全部が、接続元 `10.255.0.1` の 1 本がメールサーバー (`pop` / `imap` の 995・993・143) を総当たりした分。phase22 の「接続 1 本あたりのエラー」はその使い方の変化で 7.80 倍に振れた (物差しの作りが悪い。T22.99)。
**(2)** `timeout` は 1 件 10 秒 (`PROXY_CONNECT_TIMEOUT_SECS` の既定) で、62 時間に 3,869 件 = **待ちの合計 10.4 分/時**。一方、成功した接続で 2.5 秒を超えたものは 62 時間で **0 本** (10 日の履歴でも 2.5〜5 秒に 4 本だけ。`$.history["3600"]` の `connect_buckets`)。
**(3)** 10-17 22:14:52Z の再起動は `shutdown` の行を残しておらず、heap の行が黙って「判定できず」になった。これを見えるようにしないと、次の再デプロイでも同じことが起きる。

波は `23a` (T23.0・T23.2・T23.3 の 3 本並列。触る所が別)・`23b` (T23.1。23a の計器と数字が入ってから)・`23c` (T23.99。再デプロイのあと)。
枝は `wave23a/<id>` / `wave23b/<id>`、worktree は `~/GitHub/rust-http-proxy-wt/<id>`。指示文は Phase 22 の型 + Phase 17 の「渡し方」の読み替え (「Phase N」→ 23、行番号は 2026-10-20 の main `59c7647` のもの)。**デプロイ先 `tokyo.sorahost.net:60357` は触らない** (再デプロイは利用者)。**クレートの版はタスクの中で上げる** (手放す前に `python3 scripts/crate-versions.py --task T23.x` と `--check`。T21.6)。

| `<ID>` | 波 | `<id>` | 触る所 | 先に main に入っていること |
|---|---|---|---|---|
| T23.0 | 23a | t230 | `scripts/error-mix.py` (新規)、`scripts/test_error_mix.py` (新規)、README の「道具」の節 | — |
| T23.1 | 23b | t231 | `scripts/snapshot_criteria.py`、`scripts/test_snapshot_criteria.py`、`scripts/collect-deployed.sh` の `CRITERIA` の既定、README の `snapshot-diff.py` の節 | T23.0・T23.2 |
| T23.2 | 23a | t232 | `scripts/snapshot-summary.py`、`scripts/snapshot_criteria.py` (行を 1 つ足すだけ)、その試験、README の `collect-deployed.sh` の節 | — |
| T23.3 | 23a | t233 | `crates/config/src/config.rs` (`DEFAULT_CONNECT_TIMEOUT` と試験)、`scripts/connect-timeout-replay.py` (新規)、README の環境変数の表と CONNECT の節 | — |
| T23.99 | 23c | — | 親 (TODO.md と README だけ) | 23a・23b と再デプロイ |

- [x] **T23.0 エラーを「使われ方」から切り離して数える道具 (T23.1 の段 1 の道具)**
  - 目的: phase22 の「接続 1 本あたりのエラー」は分母が `connects` なので、宛先の生きた割合が変わると簡単に振れる。10-20 の 7.80 倍は「1 本の接続元が死んだメールサーバーを総当たりした」だけで動いた (T22.99)。同じエラーを、**名前解決に成功して接続を試みた本数**を分母にし、**同じ接続元・同じ宛先の連続した失敗を 1 件にまとめて**数え直す。
  - 変更箇所: `scripts/error-mix.py` (新規)、`scripts/test_error_mix.py` (新規)、README の「道具」の節に 1 段落。
  - やること: 雪像の `recent` (`reason`・`target`・`client`・`at`・`ms.dns`)、`errors` (`at`・`last_at`・`repeats`・`cause`・`target`)、`hosts` (`requests`・`errors`)、`history["3600"]` (`errors_by_cause`・`connects`) から、次の 4 つを並べる。
    (1) いまの物差し = `errors_by_cause` の合計 ÷ `connects`。(2) 名前解決に成功した試行を分母にした率 (`cause == "dns"` を分母から外す)。(3) 接続元×宛先の連続した失敗を 1 件にまとめた率 (間隔の閾は N = 60 / 300 秒で 2 通り)。(4) 宛先ごとの「試行・成功・DNS 失敗・timeout・refused」の表 (上位 20)。
    **窓を必ず書く**: `recent` の `min(at)`〜`max(at)`、件数、`errors` の `count`/`kept`。`recent` は 2,000 件のリングなので、窓は 1 日より短いことが多い (10-20 の雪像では 7.9 時間)。
    雪像 4 枚で回す: `status/tokyo/2026-10-15T185514Z-snapshot.json`、`status/tokyo/2026-10-17T030107Z-snapshot.json`、`status/tokyo/2026-10-20T143453Z-snapshot.json`、`status/2026-10-08T092521Z-snapshot.json` (nagoya)。
  - 受け入れ基準: 4 枚 × 4 つの表 (窓と件数を添える)。`recent` が無い雪像では「判定できず」と書いて 0 と書かない。`python3 -m unittest discover -s scripts` 通過、`./scripts/check-docs.sh` 差分 0、外部クレートを足さない。

  - 結果 (2026-10-20、`2011c97`、取り込み `f95d13a`): `scripts/error-mix.py` (試験 14 本) を足し、エラーを 4 つの形で並べた — (1) いまの物差し (`history["3600"]` の `errors_by_cause` の合計 ÷ `connects`)、(2) 名前解決に成功した試行 (試行 − `dns` 失敗) を分母にした率、(3) 接続元×宛先の連続した失敗を 1 件にまとめた率 (間隔 N = 60 / 300 秒。間で成功したら一続きを切る)、(4) 宛先ごとの試行・成功・`dns`・`timeout`・`refused` の上位 20。**どの表にも窓** (`recent` の `min(at)`〜`max(at)`、件数、`errors` の `count`/`kept`) を添え、`recent` の無い雪像は「判定できず」と書く。雪像 4 枚 (`tokyo` 10-15 / 10-17 / 10-20、nagoya 10-08) の実測: (1) は 0.0130 / 0.0086 / 0.0753 / 0.0055、(2) の接続の失敗は 0.0018 / 0.0121 / **0.0164** / 0.3090、(3) の接続の失敗は N=300 で 1 / 2 / **25** / 4 件 (率 0.0018 / 0.0010 / 0.0137 / 0.0069)。`recent` の窓は 9.52 / 2.42 / 7.86 / 11.13 時間。**これで phase22 の 7.80 倍 (接続 1 本あたり) が、名前解決に成功した試行あたりでは 1.36 倍になる** (T23.1 の行に使う)。`recent` の `target` は 46 文字で切れる行があり、名前解決に失敗した行はホスト名・成功した行は IP のことがあるので、同じ相手でも綴りが違えば別行になる (SNI で結ばない)。nagoya の N=60 の 178 件が N=300 で 4 件になるので、閾の選び方は数字に出る。試験 14 本、unittest 490 本 OK (当時)、`check-docs.sh` 差分 0。

- [x] **T23.1 `--criteria phase23` (エラーの行を差し替え、heap の「判定できず」に理由を書く)**
  - 目的: phase22 の判定表の「接続のエラーが接続 1 本あたり前の 1.5 倍」は、使われ方が変わると誤って当たる (T22.99 の 7.80 倍)。T23.0 の (3) で行を作り直す。あわせて、0 時間の雪像と起動が違うときに黙って「判定できず」になるのをやめる。
  - 変更箇所: `scripts/snapshot_criteria.py`、`scripts/test_snapshot_criteria.py`、`scripts/collect-deployed.sh` (`CRITERIA` の既定)、README の `snapshot-diff.py` の節。
  - やること: `RULES["phase23"]` = phase22 の 14 行から、`refused`+`unreachable`+`timeout` の行を **T23.0 の (3)** に差し替え、`dns` のミス率の行を「名前解決に成功した試行あたり」に改める (phase22 の行はそのまま残す)。heap の行は、起動が違うときに **`/events` の `start` の時刻を添えて理由を書く** (「0 時間の雪像と起動が違う (start が 2026-10-17T03:02:22Z と 22:14:52Z)」)。T23.2 が足す「無言の再起動」の行を 1 行入れる。`collect-deployed.sh` の `CRITERIA` の既定を `phase23` にする。
  - 受け入れ基準: **phase14〜22 の出力が 1 バイトも変わらない** (`--out json` の 56 本が前後 `cmp` 同一、phase22 の 10-17→10-20 の判定表も同一)。phase23 の行ごとの試験 (差し替えた 2 行 + heap の理由 + 無言の再起動の行)。`CRITERIA=phase23` と `CRITERIA=off` で `collect-deployed.sh` を 1 回ずつ回して動くこと。`python3 -m unittest discover -s scripts`、`check-docs.sh` 差分 0。

  - 結果 (2026-10-20、`e16b33d`、取り込み `f66b45e`): `RULES["phase23"]` を足した (**16 行** = phase22 の 14 行 + 2 行。うち 2 行を差し替え・1 行を追加)。(1) 接続のエラー (`refused`+`unreachable`+`timeout`) の行を **`/recent` の connect 1 行 = 1 試行とし、`error:dns` を分母から外した「名前解決に成功した試行あたり」**に差し替えた (前の期間の 1.5 倍で「届かず」)。phase22 の `/history` の `connects` を分母にした行はその直後に**表示だけ**で残した。(2) 名前解決のミスの行に、同じ雪像の `/recent` からの「試行あたり」「名前解決に成功した試行あたり」を並べた (閾は置かない)。(3) heap の行は、`--zero` の雪像と起動が違うときに `/events` の `start` の時刻を添え、`--zero` が渡されていないときも理由を書く。(4) 最後に**無言の再起動**の行 (T23.2 の `_p23_silent_restarts()`)。`collect-deployed.sh` の `CRITERIA` の既定を `phase23` にし、README の `snapshot-diff.py` の節に書いた。実データ (10-17 → 10-20) で回すと **満たした 6・届かず 2・判定できず 1 (ほかに表示だけ 6・参考 1)**。差し替えた行は **0.0164 / 名前解決に成功した試行 (30 / 1,830) で、前の 0.0121 の 1.36 倍 → 満たした** (phase22 の 7.80 倍がこの形で消える)。届かずは T22.4 の目安 (`expired` + `warm_stale` が 78.3 回/日) と、この期間の無言の再起動 1 回 (2026-10-17 22:14:52Z)。試験 14 本 (`Criteria23Rows` 13 本 + 実データの `Deployed23` 1 本) と `test_collect_deployed.py` (既定 phase23 の 27 本)、unittest 533 本 OK、phase14〜22 の出力は不変 (雪像 8 組 × 7 定義 × md/json の **112 本が `cmp` 同一**)、`check-docs.sh` 差分 0。**この行は窓の長さが違う 2 枚を比べている** (前 2.42 時間 1,991 試行、後 7.86 時間 2,000 試行) ので、実測の欄と出どころに窓と試行の本数を必ず書いた — T23.99 で読むときの前提。

- [x] **T23.2 無言の再起動を要点と判定表に出す**
  - 目的: 2026-10-17 22:14:52Z に、`shutdown` の行を残さずに同じ版で起動し直しており (19.2 時間で終わった起動のあと)、heap の行が黙って「判定できず」になった (T22.99)。`/events` は 10 日以上残るので、`start` の直前が `shutdown` かどうかで分かる。**気づけないまま 24 時間を捨てるのを防ぐ計器**。
  - 変更箇所: `scripts/snapshot-summary.py` (要点の頭に 1 行)、`scripts/snapshot_criteria.py` (判定表に 1 行)、その試験 2 本、README の `collect-deployed.sh` の節。
  - やること: 雪像の `events` から `kind == "start"` を拾い、直前に `shutdown` が無いものを「無言の再起動」として、時刻・その前の起動が何時間続いたか・版を出す (`shutdown` の時刻があれば「止まっていた秒数」も)。0 件なら「0 件」と 1 行。**状態ファイルから読み戻した古い `start` は数えない** (10-20 の雪像では `restored` 由来の 227 件が混ざっている)。`start` が 1 つも無い雪像 (nagoya の 10-08 など) は「判定できず」と書く。
  - 受け入れ基準: 10-20 の雪像で「無言の再起動 1 回 (2026-10-17T22:14:52Z、前の起動は 19.2 時間)」、10-15 の雪像で「0 件」、`events` の無い雪像で「判定できず」。試験 2 本。`check-docs.sh` 差分 0。

  - 結果 (2026-10-20、`0406a35`、取り込み `7594fa3`): 「無言の再起動」= `/events` の `start` の直前に `shutdown` が無い起動を、`scripts/snapshot_criteria.py` の `silent_restarts()` / `silent_restart_text()` に置き、**要点の頭の 1 行と判定表の行 (`_p23_silent_restarts()`) が同じ関数を使う**形にした (`RULES` の行は T23.1 が入れる)。実測: tokyo 10-20 (覚えている 372 件 / うち読み戻し 241 件、起動から 64.3 時間) は **1 回 = 2026-10-17 22:14:52Z**、前の起動は **19.2 時間**、直前に `shutdown` の起動は 4 件 (止まっていた秒数は言えない — 前の起動のあとに `shutdown` の行が無い)。tokyo 10-15 (92 件 / 6 件) と nagoya 10-08 (210 件 / 174 件) は **0 件**。`/events` の部が無い雪像は「判定できず」。**3 枚とも窓の先頭の `start` (読み戻しの先頭) は直前に何も無いので数えず「判定できない」と併記**した (数えると 10-15 も 1 件になる)。`start` の種類は自己ベンチの要約にも使われるので、文面が `version …` で始まる行だけを起動と見る。試験 10 本、unittest 486 本 OK (当時)、`check-docs.sh` 差分 0、phase14〜22 の出力は不変 (`PastOutputs22` の sha256 が一致)。

- [x] **T23.3 CONNECT の締め切りを既定 10 → 5 秒にする (段 1 で決める)**
  - 目的: `timeout` 3,869 件 (62 時間) が 1 件 10 秒で、**待ちの合計 10.4 分/時**。成功した接続で 2.5 秒を超えたものは 62 時間で **0 本**、10 日の履歴でも 2.5〜5 秒に 4 本だけ (`connect_buckets` の 11 番目)。締め切りを 5 秒にすれば、落とす成功は 10 日で 4 本のまま、待ちは半分 (**約 2.1 時間/日**) になる。§0 の 6「既定で安全な方」との兼ね合いは、**外す成功が 10 日で 4 本**という実測で示す。
  - 段 1: `scripts/connect-timeout-replay.py` (新規) で、雪像 4 枚 (tokyo 10-15・10-17・10-20、nagoya 10-08) から T = 3 / 5 / 10 秒 × (落とす成功の本数、縮む待ち 分/日、`timeout` の件数) を出す。材料は `recent` の `ms.connect` (成功した接続の分布) と `reason == "error:timeout"` の `secs`、`history["3600"]` の `connect_buckets` (全期間の分布)。**`recent` は 2,000 件のリングなので、窓と件数を書く**。
  - 段 2 (落とす成功が 1 日 1 本未満なら): `crates/config/src/config.rs` の `DEFAULT_CONNECT_TIMEOUT` を 5 秒にする。試験 `connect_timeout_defaults_to_ten_seconds` を書き換え、`PROXY_CONNECT_TIMEOUT_SECS` を明示したときは**その値がそのまま効く**ことと、`PROXY_TIMEOUT_SECS` (30 秒) との `min` の関係が変わらないことを 1 本ずつ足す。README の環境変数の表と CONNECT の節 (「既定 10 秒」の 2 か所)。
  - 受け入れ基準: 段 1 の表 (4 枚 × 3 通り、窓つき)。段 2 なら `scripts/mx cargo clippy -p proxy-config --all-targets -- -D warnings` 警告 0、`cargo fmt --all -- --check`、名前で回した試験の本数、`crate-versions.py --check` が 0。**戻す条件**: 再デプロイ後に「5 秒で切った接続のうち、本当は繋がったはずのもの」が 1 日 1 本以上出たら既定を 10 秒に戻す (T23.99 で見る)。

  - 結果 (2026-10-20、`6e11de9` / `660a816`、取り込み `99fcf77`): **段 2 まで行った (`DEFAULT_CONNECT_TIMEOUT` を 10 → 5 秒)**。段 1 の `scripts/connect-timeout-replay.py` (試験 19 本) で雪像 4 枚を数えると、5 秒で切る成功は 1 日 1 本未満だった — tokyo 10-20 の 12.2 日 91,258 本で **5 秒以上 0 本・2.5〜5 秒 4 本** (1 日 0.33 本)、tokyo 10-15 / 10-17 は 5 秒以上 0 本、nagoya 10-08 の 28.0 日 258,951 本は 5 秒以上の区間に 22 本 (失敗も区間に入れた 0.18.0 の版なので `timeout` 18 本を引くと 4 本 = 1 日 0.14 本、引かない上限でも 0.79 本/日)。**3 秒は nagoya で 1 日 2.25 本 (上限) 落とすので採らない。** 縮む待ちは tokyo 10-20 の直近 63 時間 (timeout 3,869 本 = 10.4 分/時) で **122.8 分/日 = 2.1 時間/日**、12.2 日の平均で 29.5 分/日。本体: `connect_timeout_defaults_to_five_seconds` (5 通り) + 明示値がそのまま効く 1 本 + `PROXY_TIMEOUT_SECS` との `min` の関係が変わらない 1 本で固め、`tests/config_test.rs` の既定 10 を見ている 2 か所と `tests/connect_timeout_test.rs` の文面、README の環境変数の表・「タイムアウト制御」・`PROXY_CONNECT_TIMEOUT_SECS` の項を直した。`cargo test -p proxy-config` 18 本・`--test connect_timeout_test` 2 本・`--test config_test` 4 本 OK、fmt 差分 0、clippy 警告 0、unittest 495 本 OK (当時)、`check-docs.sh` 差分 0、`crate-versions.py --check` 0 (本体 `0.23.3`)。**戻す条件** (5 秒で切った接続のうち本当は繋がったはずのものが 1 日 1 本以上) は T23.99 で見る。気づき: `snapshot-diff.py` の `recent_connects` の注記は「`connect_buckets` に失敗も入る」と今も書いているが、T20.4 以降は成功だけ (0.18.0 など古い版だけが失敗込み)。

- **波 23a・23b の全体チェック (2026-10-20、親)**: `2011c97` (T23.0)・`0406a35` (T23.2)・`6e11de9`/`660a816` (T23.3)・`e16b33d` (T23.1) を main に取り込んだ (`f95d13a` / `7594fa3` / `99fcf77` / `f66b45e`) うえで、`cargo fmt --all -- --check` 差分 0、`cargo clippy --workspace --all-targets -- -D warnings` 警告 0 (42 クレート)、`cargo test --workspace --no-fail-fast` は **139 バイナリ 853 通過 / 0 失敗 / 1 ignored**、`python3 -m unittest discover -s scripts` **533 本 OK**、`check-docs.sh` 差分 0、`crate-versions.py --check` 0 (本体 `0.23.3`)、`cargo clean --release` からの `build-memory.sh 120` 通過、`-V` は `0.23.3+1ecc09f` (2,760,632 B)。**本体が変わったのは T23.3 (CONNECT の締め切り 10 → 5 秒) だけ**なので、再デプロイして T23.99 で見る。**揺れ**: 負荷の下で `stages_test` と `newclient_test` が 1 回ずつ落ちた (単独では通り、負荷の無い 2 回目の全体実行は 139 バイナリ全通過。§5 の「既知の小物」に足した)。**気づき**: 新しい道具に実行ビットが無く、README が直接叩く形で書いてあったので `1ecc09f` で直した (T23.3 の段 1 の成果物)。

- [x] **T23.99 締める (波 23a・23b の版を 24 時間走らせたあとに見る・決める・書く)**
  - 前提 (§0 の 9 の形): デプロイ先は `tokyo.sorahost.net:60357`。版 **`0.23.3+1119abd`** (利用者が 2026-10-20 15:52:08 UTC にデプロイ。前の版の `shutdown` は 15:51:54、止まっていたのは 14 秒)。「前」の雪像 `status/tokyo/2026-10-20T155032Z-snapshot.json` (前の版 `0.22.6+000681e`、起動から 66.4 時間)、「0 時間」の雪像 `status/tokyo/2026-10-20T155229Z-snapshot.json` (起動 21 秒後。`heap_used + mmap − cache_memory` **17.3 MB**・RSS 30.1 MB、`dns` は `warm_secs` 0・`misses` 4、`ipv6.request_probes` 0、canary 1 回)。`/config` で `PROXY_CONNECT_TIMEOUT_SECS` = **5**・`source` = `default`、`PROXY_TIMEOUT_SECS` = 30、`PROXY_DNS_WARM_SECS` = 0 を確認済み (T23.3 は効いている)。
    **そのあと 2 回、無言で起動し直している** (2026-10-21 14:31:31Z と 15:11:20Z、どちらも `/events` に `shutdown` の行が無い。版は 14:31 から `0.23.3+ae7c190`)。**これはホストの管理者による再起動で、落ちたのではない** (2026-10-22、利用者の談)。この版は落ちていないが、外からの再起動は `start` の並びだけでは区別できないので、**「無言の再起動 0 回」の行は「こちらが原因のもの 0 回」と読み、外からの再起動はその旨を書いて数から外す** (数えたまま「届かず」にすると、自分の版の欠陥と読めてしまう。T23.2 の計器の役目は「気づくこと」で、原因の切り分けは人に聞く)。「後」は **2026-10-22T15:11Z 以降**に撮る (24 時間は 15:11:20Z の起動から数え直し)。
  - 判定: `BEFORE=<前> ZERO=<0 時間> scripts/collect-deployed.sh tokyo.sorahost.net:60357 status/tokyo` の `--criteria phase23`。見ること: T23.3 を戻す条件 (5 秒で切った接続のうち本当は繋がったはずのものが 1 日 1 本以上、または `timeout` の待ちが減っていない)、無言の再起動が 0 件 (**ホストの管理者による再起動は除く。前提の 2 回**)、エラーの行が T23.0 の形で前と同じ桁、`dns_slow` が合計 1,000 ms 未満の窓で 0 件 (T22.2 の閾の維持)、`stale_retries` < `stale_hits` の 1%。
  - 次のターンの準備: Phase 24 の完了の定義の下書き (候補: 名前解決 11.9 ms/接続の内訳と負のキャッシュ、`conn` 役の CPU/要求の内訳、`wait` の尾の内訳)、それぞれに要る計器がその雪像で出ているかの確認、足りない計器を `T24.0` として再デプロイの前に。

  - 結果 (2026-10-22、雪像 `status/tokyo/2026-10-22T210213Z-snapshot.json`、要約 `-collect.md`): **判定は満たした 7・届かず 2・判定できず 1** (ほかに表示だけ 6)。撮ったのは最後の起動から 29.8 時間 (2026-10-22 21:02:13Z = `1792702933`)。
    満たした 7 つ: 引き直しが 1 名前あたり **57.9 回/時** (≤80)、接続のエラー (名前解決に成功した試行あたり) **0.0010** (前 0.0156 の 0.07 倍、≤1.5 倍)、`conn` 役 **1.19 倍** (≤1.3)、cgroup **0.0015 コア**で同桁、`dns_slow` が合計 1,000 ms 未満の窓で **0 件**、`stale_retries` **0.00 %** (0 / 6,712)、`ipv6.request_probes` **0**。
    届かず 2 つ: T22.4 の目安 (引き直し **264.3 回/時** = 前の 0.69 倍で 0.5 倍に届かず。ただし `expired` + `warm_stale` は 77.9 → **20.1 回/日**で後半は満たした) と、無言の再起動 3 回 — **うち 2 回はホストの管理者による再起動** (2026-10-21 14:31:31Z・15:11:20Z。2026-10-22 に利用者から聞いた) で、**こちらが原因のものは 0**。
    判定できず 1 つ: heap (`heap_used + mmap − cache_memory` が 0 時間 17.3 → 19.5 MB)。管理者の再起動で起動が変わり、版も `0.23.3+1119abd` → `ae7c190` になったので同じ起動の 2 枚にならない。
    **T23.3 は戻さない**。`timeout` の 1 件あたりの待ちが 10,010 → **5,000 ms** になり、`timeout` は 13.74 → **1.59 件/時**。直近の個票 2,000 件 (3.5 時間) では `timeout` が 1 本だけで、その宛先が後に成功した形跡も無い (誤って切った本数 0)。**件数の減りにはメール宛の総当たりが止んだ分も入っている** (要求 747 → 520 本/時、接続 1,622 → 485 本/時、メール系の宛先 10% → 2.4%。利用者がエージェントの設定から外した) ので、5 秒の効果は 1 件あたりの待ちのほうで示す。
    **物差しの作り直し (T23.0・T23.1) は効いた**: 同じ 2 枚を phase22 の物差しで見ると 7.80 倍だったものが、名前解決に成功した試行あたりでは 1.36 倍、今回は **0.07 倍**。`/recent` の窓は 2.4 → 3.5 時間なので、率には窓と試行数を併記した。

