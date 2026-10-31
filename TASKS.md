# TASKS.md — 未着手のタスク

`rust-http-proxy` の**まだ着手していないタスクだけ**をここに置く (2026-10-22、T.24.13)。入り口は [`TODO.md`](TODO.md)。

## 決まり

- **済んだタスクはここに残さない。** タスクが終わったら、そのタスクの見出しごと [`tasks/TNN.md`](tasks/README.md) へ本文を 1 文字も変えずに移し、`TODO.md` の §5 には見出しと索引の 1 行だけ残す。
- **番号は使い回さない。** `T.n.m` は付けた順で、並び替えても振り直さない (コミットメッセージから引けるようにする)。**ただし、まだ一度も使っていない空きの番号 (タスク 19 がそうだった) は、新しい番号を増やさずに割り当ててよい** (2026-10-23、はかせの指示)。**サブタスクごとにファイルは作らない** — 1 つのタスクのサブタスクは、済んだら `tasks/TNN.md` の 1 本にまとめる。各タスクの `Tn.99` は「デプロイ後の様子見と修正、締めの文書」に予約する。**締めの番号は、そのデプロイに入る一番大きいタスクの番号に合わせる** (版と同じ。2026-10-24 の 0.27.8 はタスク 27 が最大なので **T.27.99**) (2026-10-23、はかせの指示)。
- **番号の書き方は `T.19.10`** (T のあとに点。`T19.10` とは書かない)。3 段目は必要なときだけ `T.19.10.1` のように足す。ファイル名は今までどおり `tasks/T19.md` ([`tasks/README.md`](tasks/README.md))。
- **1 タスク = 1 worktree = 1 コミット。** 手順の型は `TODO.md` の「タスク 15 の渡し方」と「実装を Opus に渡すときの決まり」。
- **クレートの版は「そのクレートの中身を最後に変えたタスクの番号」** `0.<タスク>.<番号>` (T.19.1 → `0.19.1`。2026-10-23、はかせの指示で T.26.3 の「デプロイの番号」を撤回して元に戻した)。タスクの中で `scripts/crate-versions.py --task T.19.1` を回すと、分かれ目から中身が変わったクレートが `0.19.1` になり、**本体と `proxy-server` はそのビルドの最大**になる。
- **デプロイまでの新しい仕事は、新しいタスクを起こさずに、いま開いている親タスクの小タスクとして足す** (2026-10-23 にはかせの指示で T.19.8・T.19.9 をそうした。2026-10-24 にも念を押された)。次のデプロイの前に見つけた仕事は、どんなに毛色が違っても `T.<親>.<次の番号>` にして同じ親タスクに入れ、そのデプロイの締め (`T.<親>.99`) までを 1 つのタスクで扱う。**新しいタスク (`タスク N+1`) を起こすのは、締め (`T.<親>.99`) が済んでから** (デプロイのあとに見つけた仕事)。番号を足したら `T.<親>.99` の前提の「版」と「入る小タスク」も書き直す。
- **タグは Claude がセッションの中から打つ。利用者の手元には回さない** (2026-10-24、はかせの指示「そっちでできるじゃん」)。まず `git push origin vX.Y.Z` を試し、通らなければ (クラウドのセッションはタグの git push も GitHub API の書き込みも通らない) **GitHub のツールで `release.yml` を `workflow_dispatch` (入力 `version` = `X.Y.Z`、`ref` = 既定の `main`) で起動して打つ** (workflow の中で本体の版と `crate-versions.py --check` を確かめ、注釈付きのタグを push してからビルドする)。打てたかは `git ls-remote origin refs/tags/vX.Y.Z` と release の実行で確かめ、締め (`T.<親>.99`) の前提に書く。できない理由があれば、手順を利用者に渡す前に、別の口 (workflow・GitHub のツール) を探してから言う。
- **デプロイは複数のタスクをまとめてよい。** そのときの版 (= タグ) は**まとめた中で一番大きいタスクの番号**。番号はタスクの番号なので、**前に出したタグより小さくなることもある** (T.19 は空いていた番号なので `v0.19.x` は `v0.23.3` より小さい)。タグはデプロイの合図の直前に打つ。
- このファイルと `tasks/` の 1 対 1 は `./scripts/check-docs.sh` の (g) が見る (`scripts/check-ledger.py`)。**未着手のタスクに `- [x]` が付いていたら NG** (移し忘れ)。

## 呼び方

**「Phase」という呼称は 2026-10-23 に廃止して、すべて「タスク」に統一した** (はかせの指示)。台帳は **`T.19.10` の形の番号** (2026-10-23 に `T19.10` から変更。はかせの指示) と `tasks/T19.md` のファイル名で引き、まとまりの単位も「タスク」と呼ぶ。

例外は**判定プロファイルの名前** `--criteria phase24` (`scripts/snapshot_criteria.py` の `RULES["phase24"]`) で、これは**過去の出力を 1 バイトも変えない**ために歴史的な綴りのままにしてある。台帳の呼称とは別物。

## いま開いているタスク

### タスク 32 (2026-10-31 に起こした: 0.30.4 の 48 時間で見えた RSS の段と、6 方向の洗い出し (メモリ・プロトコルとキャッシュ・頑丈さ・性能・実データと計器・道具と文書))

T.31.99 で 0.30.4 の RSS が 24 時間 48.6 MB → 48 時間 73.6 MB、`heap_free` が 19.9 → 36.1 MB に増え、heap の行 (起動 60 秒の基準から +7.3 MB、閾 5 MB) が届かなかった (数字は [`tasks/T31.md`](tasks/T31.md) の T.31.99 の結果)。そのあと利用者の「もっとたくさんのタスクを徹底的に限界まで積んでください」で、調査員 6 人が手元の release (127.0.0.1) と tokyo の雪像 (`status/tokyo/2026-10-31T063104Z-*` ほか。tokyo そのものには触らない) で洗い出した (2026-10-31)。**証拠は手元の再現か実データ**で、再現の道具は当日のコンテナの scratchpad (`t32/<方向>/`。報告の写しは `t32/reports-*.md`) にある (コンテナと一緒に消えうるので、要る数字は各小タスクの本文に写した)。本体を変えるものは 32a〜32d、道具と試験と文書は 32e。**利用者の判断が要るもの**は番号を振らず、節の最後の表に置いた。

**全体の見立て**: (1) RSS の段の主な仕組みは **glibc の「動く mmap の閾」**で、10-30 16Z の段はメモリのキャッシュに 13.9 MB 入った分 (名前解決の総当たりの時間は RSS が平ら)。(2) **計器の穴が大きい**: ホスト表 (1,000 行) が 10-16 から満杯で T.30.4 の宛先外しは本番で一度も効いておらず、異常検知 3 種は名前解決の失敗とメール宛だけで鳴っている。(3) **キャッシュの毒** (小文字のメソッド) と **304 の取り違え** が手元で再現した。(4) tokyo の CPU の 4 分の 3 は要求と関係しない裏の仕事 (`profile-sample` 21.8%・`cache-probe` 15.5%・`history` 9.6%・`idle-watch` 6.4%) と、出入りの多いトンネルの預け入れ。(5) 利用者の待ちのうちプロキシで減らせるものはほぼ残っていない (成功した接続の名前解決 0.7 ms/本、確立の尾は RTT + 0.2 ms)。

**32a メモリ (本体を変える)**

- [ ] **T.32.0 手元で RSS の段を再生して、どの確保が残るかを測る (調べる向きを 2026-10-31 の洗い出しで正した)**
  - 目的: 段の候補を確かめてから直す。メモリの調査員の手元の証拠: 起動直後に glibc の `mmap_threshold` がもう 2,101,248 (起動中に 2 MiB の `munmap` 1 回)、メモリのキャッシュの 14 MiB の本文を PURGE すると 14,684,160 に上がる。同じ負荷 (名前の引けない CONNECT 4,000 本) のあと、既定と `MALLOC_MMAP_THRESHOLD_=131072` で: 暇な RSS 37.7 / 32.1 MB、`heap_free` 7.0 / 1.0 MB、`/snapshot` (1.16 MB) ×5 の山 +8.5 / +0.8 MB、そのあと `malloc_trim(0)` で残るのが **+3.5 / +0.15 MB**、閾が上がった後の 14 MiB の GET→PURGE ×3 で 42.5 → 54.0 / 32.9 → 33.0 MB、trim 後 **53.0 (返ったのは 1.0 MB) / 32.7 MB**。tokyo の `history["60"]` では 10-30 16:21〜16:23 に `forwards` 3 → 16 → 2・`stores` 18 → 35・`mem_used` 0 → 13,857,210 で RSS 51.9 → 62.0 MB、10-31 02:34 の `evictions` 20 で `mem_used` 0・RSS −4.6、03:16 の trim で −6.8。総当たりが同じ濃さの時間 (`dns_fail_misses` 1,228〜1,835 本/時) の RSS は 38.6〜44.1 MB で平ら。手元の総当たり 10,000 本は 26.1 → 40.4 MB (表が上限まで埋まる 1 回きり)。10-30 09:02・12:36・16:19 の +6.4〜6.6 MB (保存 0) は次の :16 の trim で戻る形で、6 MB の `/snapshot` を誰かが引いたとみられる (未確認。`watch.yml` は軽い口 3 つだけ)。
  - やること: 上の再生を手順として残し (`scripts/` か `docs/perf.md`)、(a) 重い口と日次の雪像、(b) 名前解決の総当たり、(c) 両方、(d) メモリのキャッシュに forward GET を計 14 MB 入れて追い出し、閾が上がった状態で重い口、を流して、それぞれの前後と trim 後の `rss`・`heap_used`・`heap_free`・`mmap`・arena の数・`mp_.mmap_threshold` を表にする。T.32.3 の前と後の両方で回す。
  - 受け入れ基準: (a)〜(d) の山と trim 後の残りの表。tokyo の 16Z の +16.7 MB のうちキャッシュの分 (13.9 MB) とそれ以外が分けて書かれている。

- [ ] **T.32.3 glibc の mmap の閾を起動時に固定する (動く閾のせいで大きな一時の確保が arena に残って返らない)**
  - 目的: glibc は mmap で取った塊を free すると `mmap_threshold` をその大きさまで上げ、`trim_threshold` をその 2 倍にする。以後は閾より小さい大きな確保 (重い口・日次の雪像の `String`、キャッシュの本文) が arena から取られ、主でない arena の天辺の空きは free でも `malloc_trim(0)` でも返らない (glibc の `mtrim` は `main_arena` しか天辺を詰めない)。証拠は T.32.0 の表 (閾を固定すると残りが +3.5 → +0.15 MB、53.0 → 32.7 MB)。tokyo では 10-31 00:00 の日次の雪像 (主でない arena の `history` スレッドが組む) で 62.2 → 68.6 MB になり、00:16・01:16・02:16 の trim で戻っていない。
  - やること: `M_ARENA_MAX` を設定している所 (`crates/sys/src/sys.rs:86` 付近、`crates/run/src/lib.rs:259`) で `mallopt(M_MMAP_THRESHOLD, 256 KiB)` と `mallopt(M_TRIM_THRESHOLD, 1 MiB)` を呼ぶ (値を固定すると動く閾が止まる)。`PROXY_MALLOC_MMAP_THRESHOLD_KB` (0 = glibc の既定) で戻せるようにし、`--check` と `/config` に出す。`heap_used` が小さくなり同じ量が `mmap` に移る (和は同じ) ので、README の `memory` の説明と締めの読み方に書く。
  - 受け入れ基準: T.32.0 の手順で `/snapshot` ×5 のあと trim して残るのが 0.5 MB 未満、14 MiB×3 の GET→PURGE のあと trim した RSS が前 +1 MB 以内、gdb で `mp_.no_dyn_threshold == 1`。`scripts/cpu-per-request.sh` で CONNECT と forward の CPU/要求が 1.05 倍以内。

- [ ] **T.32.1 重い口と日次の雪像を、一度に全部組まずに書き出す (T.32.0・T.32.3 のあとも山が残るなら)**
  - 目的: `/snapshot` は部ごとの JSON を組んでから 1 つにまとめる形で、6 MiB の雪像で一時の確保が 2〜3 倍になる。手元で `/snapshot` (表を埋めて 699 KB) は 1 回 5.9 ms。T.32.3 で返るようになっても、山 (cgroup の上限 256 MB に対する瞬間の使用) は残る。
  - やること: 部ごとに組んではすぐ書き出す (クライアントへの応答・日次のファイル)、器を使い回す、のどちらかで一時の確保の山を下げる。応答の中身は 1 バイトも変えない。
  - 受け入れ基準: 手元の (a) の RSS の山が T.32.3 の後の値の半分以下。`/snapshot` の応答のバイト列が前と同じ (試験)。`check-docs` 差分 0。

- [ ] **T.32.2 名前解決の表の上限 4,096 を、失敗した名前解決の経路でも守る**
  - 目的: 成功したときは `table.len() >= MAX_ENTRIES` で 1 つ捨てる (`crates/net-dns/src/dns.rs:699-700`) が、失敗したときは無条件に `entry().or_insert_with` (同 `:718`) で、掃除も無い。tokyo の `status.dns.entries` は 48 時間で **7,511** (24 時間で 3,886)。一覧の古い側 4,763 件のうち 4,146 件は答えの無い失敗の行で、最古は 168,087 秒前 (serve-stale の 21,600 秒を大きく越える)。上限を越えたあとは、成功したミスのたびに `evict_oldest` が 7,500 行を鍵の内側で舐める。heap の行の +7.3 MB (`heap_used` 12.8 → 15.3 MB、+3,625 名前 ≈ 690 B/名前) の主な分とみられる。
  - やること: 失敗の経路でも上限を守る (失敗した名前は負のキャッシュの 60 秒を過ぎたら捨てる、か上限の内側で古い失敗から捨てる)。`evict_oldest` が全部を舐めないように (古い順の印を持つ) するかは測って決める。
  - 受け入れ基準: 単体試験で、引けない名前 10,000 個のあとに表が 4,096 以下。手元の総当たり 10,000 本の `heap_used` の増えが前の半分以下。

- [ ] **T.32.4 `/status` の `memory` の内訳を常駐の値にする (`stacks_estimate` は予約、`heap_free` は返した頁込み、file の RSS が出ていない)**
  - 目的: smaps で測ったスタックの常駐は 1 本 8〜28 KiB で合計 **0.30〜0.96 MB** なのに、`stacks_estimate` は **17.0〜25.4 MB** (`crates/metrics-core/src/metrics/status.rs:383`・`:476` 付近の `conn × 256K + other × 2M`)。tokyo の最後の 1 枚は `heap_used` 15.3 + `heap_free` 36.1 + `mmap` 11.7 + `stacks_estimate` 22.0 = 85.1 MB で、`rss` 73.6 MB を超える (T.31.1 の行が「説明」として並べる 2 つを足すと RSS より大きい)。`heap_free` は trim で返した頁も数える (`crates/sysinfo/src/sysinfo/malloc.rs:138`。手元の trim 後で `heap_free` 11.6 MB に対し常駐の空きは約 5.7 MB)。RSS のうち file の頁が約 9 MB (libcrypto 3.9・本体 2.8・libc 1.8 MB) あり、どの欄にも出ていない。起動 3 秒後の `/status` の `rss` が 4.76 MB で、そのときの VmRSS は 25.9 MB (プローブの古い値)。
  - やること: `/proc/self/status` から VmRSS を読んでいる同じ 1 回で `RssAnon`・`RssFile` も出す。`stacks_estimate` は `stacks_reserved` に名前を変えるか、`/status` のときだけ smaps からスタックの常駐 (`stacks_resident`) を足す (1 回の読みの時間を測る)。`heap_free` の説明に「返した頁を含む」と書く。起動直後の `rss` は今読んだ値にする。判定表の表示の行も新しい欄を並べる (欄の無い版は今のまま)。
  - 受け入れ基準: 手元で `rss_anon + rss_file == rss` (±1 頁)、`stacks_resident` と smaps の合計の差が 64 KiB 以内、`/status` 1 回の時間の増えが 1 ms 未満。

- [ ] **T.32.5 起動時の libssl の読み込みと CA の束の解析を、最初の https:// オリジンまで遅らせる**
  - 目的: 起動の callgrind で主スレッド 2.515 億 Ir の **70% (1.77 億) が `SSL_CTX_set_default_verify_paths`** (CA の束の PEM の解析。`crates/run/src/lib.rs:392` → `crates/tls/src/tls.rs:177-215`)。起動して `/healthz` が返るまで、既定 61〜82 ms・RSS 25.9 MB・`heap_used` 5,416 KiB、`PROXY_TLS=off` 43〜61 ms・20.4 MB・4,331 KiB (dlopen が約 +4.4 MB、CA の束が +1.1 MB)。tokyo は CONNECT が 99% 以上で forward は 48 時間で 450 本なので、https:// のオリジンを取りに行く要求が来ない限り、この約 **5.5 MB と 20 ms** は使われていない。
  - やること: `TlsClient` を OnceLock で最初の https:// オリジンへの取得のときに作る。`--check` と起動のログは「最初の利用で読む」と書き、読めるかどうかは今どおり `--check` で確かめる。**検証は切らない**。
  - 受け入れ基準: 起動 60 秒の RSS が −4 MB 以上、`/healthz` までが −15 ms。https:// オリジンの結合試験 (キャッシュの HTTPS) が通る。libssl が無い環境で今と同じ WARN と 502。

- [ ] **T.32.6 キャッシュの `variants` の表に上限を置く (保存を始めるたびに 1 行、追い出しても消えない)**
  - 目的: `crates/cache/src/cache/ops.rs:188` の `remember_variant` は保存を始めるたび (`begin_store`、`ops.rs:244`。保存に至らなかった回も含む) に `KeyMap<Vec<CacheKey>>` へ足し、消すのは `invalidate` (`ops.rs:198`) と `clear_all` (`ops.rs:53`) だけ。メモリやディスクの追い出し・期限切れでは消えない。1 URL あたり約 100〜130 B の見積もりで、100 万 URL なら 100 MB を超える (実測は未。tokyo では 48 時間に保存 36 回で今は効かない。開いたプロキシに撃たれたら伸び続ける)。
  - やること: 追い出し・期限切れのときに一緒に消すか、上限 (メモリとディスクの件数の合計) を置いて古い順に捨てる。
  - 受け入れ基準: 名前の違うキャッシュ可能な GET を 50,000 本流し、キャッシュを満たして追い出したあとの `variants.len()` が上限以下。`heap_used − cache_memory` の伸びが 1 MB 未満。

- [ ] **T.32.7 大きな転送の間にメモリのキャッシュが全部追い出された理由を残す (未確認)**
  - 目的: 10-31 02:34〜02:36 に 32.6・375.5・253.8 MB を転送した間に、`evictions` 20 で `mem_used` が 13.87 MB → 0 になった (`history["60"]`)。動く余白 (`keep_free_bytes` = active file 1.35 GB) か cgroup の余白がページキャッシュで縮んだ可能性がある。
  - やること: そのときの `cache.memory.limit_bytes` の動きを `history` に足すか、追い出しの理由 (期限・予算) を `/events` に 1 件残す。原因が分かったら直すかを決める。
  - 受け入れ基準: 次の 48 時間で、追い出しの理由が雪像から読める。

**32b 正しさと頑丈さ (本体を変える)**

- [ ] **T.32.8 小文字のメソッド (`get`) の応答が GET の鍵に保存され、他の利用者の GET に配られる (キャッシュの毒)**
  - 目的: メソッドは大小を区別する (RFC 9110 §9.1) のに、`is_get` / `head_only` を `eq_ignore_ascii_case` で決めているので (`crates/http/src/http/mod.rs:687-688`)、`get /x` が GET として参照・保存の対象になる (`mod.rs:796-801`)。オリジンへは `get` のまま転送される。再現 (`t32/protocol/p3_lowercase.py`、オリジンは Python の `http.server`): 攻撃者が `get http://O/index.html` を 1 本送るとオリジンは `501 Unsupported method ('get')`、続く**別の利用者の正しい `GET` 3 本がすべて `X-Cache: HIT` で 501 の頁**、オリジンには 1 本だけ。501・405 は経験則で保存できる状態コード (`crates/freshness/src/freshness.rs:15`) で、鮮度の指示が無くても `negative_ttl` の 60 秒は残る (送り直せば延ばせる。`405` + `max-age=600` でも HIT)。`head` も HEAD 扱いで、本文を返すオリジンの応答を先頭だけで切る。
  - やること: GET / HEAD の判定を大小の区別ありにする (`method == "GET"`)。小文字のメソッドは未知のメソッドとして素通しし、保存も参照もしない (安全でないので無効化の側)。`CONNECT` (`crates/server/src/conn/serve_one.rs:488`) も揃えるかは任意 (今は `connect` でトンネルが開く。害は小さい)。
  - 受け入れ基準: 結合試験で `get` の後の `GET` が MISS でオリジンの本物の 200。`GET` / `HEAD` の HIT は今のまま。forward の Ir が増えない。

- [ ] **T.32.9 キャッシュ自身の再検証で、ETag が違う 304 を受けたら保存済みを延命しない**
  - 目的: RFC 9111 §4.3.4 では、304 の強い validator が保存済みと一致しなければ、その 304 で保存済みを更新してはいけない。クライアントの条件付きの経路 (T.29.6) は `not_modified_selects` (`mod.rs:1861`) で照合しているが、**キャッシュ自身の再検証は 2 つの経路とも照合していない** (同期は `mod.rs:1313-1360`、裏は `crates/http/src/http/refresh.rs:184-211`)。再現 (`t32/protocol/p4_reval.py`・`p5_reval_fg.py`): `max-age=1, ETag "e"` の `v005` が期限切れになり、オリジンが `304, ETag "other", max-age=600` を返すと、その後の 3 本は `ETag: "other"; max-age=600; X-Cache: REVALIDATED/HIT` で**古い本文 `v005` を 10 分配り続けた**。下流のブラウザは `If-None-Match: "other"` で 304 をもらい続ける。
  - やること: 両方の経路で、304 の ETag (無ければ Last-Modified) が「あって違う」なら更新しない。外れたら項目を消し、同期の経路は条件なしで 1 回取り直し、裏の経路は消して終える。validator の無い 304 は今どおり更新する。
  - 受け入れ基準: 上の再現で 2 本目以降が新しい 200 (`ETag "other"` を古い本文に付けない)。一致する 304 は今どおり REVALIDATED。試験は同期と裏の各 1 本。

- [ ] **T.32.10 ホスト別統計の表 (1,000 行) が満杯で入れ替わらず、T.30.4 の宛先外しが本番で効かない (直し方は下の判断で)**
  - 目的: `/hosts` の行は `restored_since` (10-08) からの通算で、日次の雪像では 10-16 (`2026-10-16T000000Z-snapshot.json` の count 1001) から毎日 1,000 行、`other` は 155 → 162,278 要求。満杯になると新しい鍵は全部 `other` に入り、行を捨てる仕組みが無い (`crates/metrics-core/src/metrics.rs:651-661`、`MAX_HOSTS` は `crates/metrics-types/src/metrics.rs:1037`)。`2026-10-31T063104Z-snapshot.json` の 1,000 行のうち 740 行は 7 日以上使われておらず (中央値 14.5 日前)、552 行は 2 要求以下。T.30.4 の `DestTable` は表の行でしか宛先を数えない (`other` は宛先にならない) ので、`2026-10-31T063104Z-slo.json` の `hourly` 49 時間で `excluded` は 0 時間。10-30 20Z は 443 のエラー 2 件 / 試行 347 本 (`api.debank.com:443` と `….cloudfront.hls.ttvnw.net:443` の時間切れ、どちらも表に無い) で 1 時間外れた (規則どおりなら 1/346 = 0.29% で満たす)。T.30.4 の動機の `pravda.me:443` も表に無かった。48 時間の `/slo` の外れ標本 631 のうち 589 (93%) がこの形の 2 時間で、日ごとの達成 96.0%・96.4% は外せていれば 99.8% 前後。`/hosts` の差分では窓の要求 53,429 本のうち 40,888 本 (76.5%) が `other` で見えない。
  - やること: 下の判断の表の (a)〜(c) から選んだ形で直す。
  - 受け入れ基準: 結合試験: 1,000 行が埋まり `last_seen` が 14 日前の表で、新しい宛先 `pravda.example:443` の時間切れ 4 件と 443 の成功 400 本を流すと `excluded` が出て満たす。tokyo の状態ファイルのまま再起動して 24 時間後、`/hosts` の Δ要求に占める `other` が 20% 未満 ((a)(b) のとき)。

- [ ] **T.32.11 名前解決の待ちで接続の枠が全部ふさがる (getaddrinfo はキャンセルできない。直し方は下の判断で)**
  - 目的: 要求の経路の名前解決 (`crates/net/src/acl.rs:299` `resolve_target` → `crates/net-dns/src/dns.rs:682` `system_resolve` → `getaddrinfo`) は接続を処理しているワーカーの上で同期に走り、その間ずっと接続の枠 (`OpenGuard`) とワーカー 1 本を握る。`getaddrinfo` には締め切りもキャンセルも無く、`PROXY_HEADER_TIMEOUT_SECS` (ヘッダー読了まで) も `PROXY_CONNECT_TIMEOUT_SECS` (TCP 確立だけ) も解決の待ちを縛らない。相異なる名前なら負のキャッシュも当たらない。再現 (`t32/robust/REPRO.md`): `unshare -m` の中で応答しないリゾルバ (127.0.0.1:53) を向け、`PROXY_MAX_CONNS=8` で相異なる名前 8 本の CONNECT を同時に出すと、8 本とも 4.01 秒枠を握って 502、その間 `live_threads=8`・`idle_threads=0`、0.5 秒後の**正当な要求 (即解決・近いオリジン) は 503 を即時に受けた**。tokyo は `PROXY_MAX_CONNS=240`・`PROXY_MAX_THREADS=128`・`PROXY_MAX_CONNS_PER_CLIENT=0` で、接続元が NAT の 1 つなので 1 人で 128 本の同時解決を握れる (リゾルバの不調や、攻撃者の権威サーバが応答を落とすと 1 回が数十秒になる)。
  - やること: 下の判断の表の (a)〜(c) から選んだ形で直す。
  - 受け入れ基準: 上の再現 (相異なる遅い名前 N 本 = `max_threads`) で、正当な速い要求が窓の中で 200 を受ける。締め切りを超えた接続は 504 で枠を手放し `live_threads` が張り付かない結合試験。IP リテラル・キャッシュ当たりの速い道の CPU/要求が 1.05 倍以内。

- [ ] **T.32.12 不正な `Host` や authority を 400 にする (今は 502 nxdomain で 80 番宛ての origin エラーとして数える)**
  - 目的: RFC 9112 §3.2 は不正な `Host` に 400 を MUST とする。今は `check_authority` (`crates/origin/src/request.rs:387-403`) が空と `@` しか見ないので、名前解決まで行って 502。再現 (`t32/protocol/p11_badhost.py`): `Host: a b`・`a, b`・`a/b`・`h:99999` など 6 本とも 502 で `rejected_requests.total` は 0、`/errors` と `/hosts` に `http://example.invalid x:80`・`http://a, b:80` などが並ぶ。`http://h:+22501/` は Rust の `parse` が `+` を受けて通り、オリジンへ `Host: 127.0.0.1:+22501` が届く。クライアントの誤りが 80 番の origin エラー・nxdomain・負のキャッシュの名前に乗り、`/slo` の `error_rate` と T.32.2 の名前の数にも効く。
  - やること: authority を検証する (reg-name の文字・`[` IP リテラル `]`・ポートは 1〜5 桁の数字で 65535 以下、空のポートは既定のポート)。`Host` にも同じ検証を当て、外れたら 400 (`bad_uri` / `no_host`)。
  - 受け入れ基準: 上の 7 形が 400 で `rejected_requests` に数えられ、`/hosts` と `/dns` に行が増えない。正当な `[::1]:p`・`h:080`・大文字のホストは通る。`framing-probe.py` に足す。

- [ ] **T.32.13 裏の再検証がオリジンの 4xx (404・410) で古い表現を残す**
  - 目的: `crates/http/src/http/refresh.rs:212-215` は 200〜399 以外をすべて「stale を残す」にしていて、オリジンが 404 を返した後も grace (既定 60 秒) の間は古い 200 を `REFRESHING` で配り、配るたびに再検証を 1 本出す。再現 (`t32/protocol/p4_reval.py` の D): 期限切れの後の 3 本がすべて `200 REFRESHING` の古い本文で、オリジンへは 4 本。同期の経路 (`p5_reval_fg.py` の D) は 404 で置き換わる。RFC 9111 §4.3.3 で古い表現を残してよいのは 5xx。
  - やること: 5xx (と接続の失敗) だけ残し、保存できる 4xx は同期の経路と同じく置き換える (保存できないものは消す)。
  - 受け入れ基準: D の再現で 2 本目の後は 404。5xx では今どおり stale を残す。

- [ ] **T.32.14 Happy Eyeballs の上限外のスレッドと記述子に本数の上限を置く**
  - 目的: `crates/net-conn/src/net.rs:628` の `connect_candidates` は候補アドレスごとに `thread::spawn` し、join しない・`PROXY_MAX_THREADS` に数えない・候補数に上限が無い。勝者が返ると敗者は `PROXY_CONNECT_TIMEOUT_SECS` (3 秒) まで connect を握ったまま残り、各自が記述子 1 つ + 既定 2 MiB のスタック (予約) を使う。`stagger` 250 ms ごとなので 3 秒の窓で 1 接続あたり最大 ~12 本。`auto_max_conns` は「1 接続あたり 4 記述子」(`crates/config/src/config.rs:37-45`) を前提にしている。実機の枯渇は未確認 (sandbox が私設宛てを横取りし、SYN を落とす多アドレス宛ての再現が作りにくい)。
  - やること: 候補アドレスを接続時に N 件 (例 8) で切る。`auto_max_conns` の記述子の前提を README に書く。
  - 受け入れ基準: 多アドレスのホストへの 1 接続で起こる上流接続のスレッド・記述子が N で頭打ちになる試験。Happy Eyeballs の既存の試験が通る。

- [ ] **T.32.15 小物: 要求の行末・応答の不正なヘッダー名・chunked の甘さ・`Via` の版・CONNECT の 502**
  - 目的: (1) `write_request_headers` が行をそのまま書く (`crates/msg/src/headers.rs:121`) ので、LF だけの行末の要求は LF のまま転送され、プロキシが足す行は CRLF (`t32/protocol/p8_client.py`。CRLF でしか区切らないオリジンでは食い違いうる。実在のオリジンでは未確認)。(2) オリジンの応答の `X A: b` と `: v` をそのまま渡す (`p1_resp_heads.py`。`sanitize_response_head` (`headers.rs:507`))。(3) 要求の chunked でデータの後ろのごみ (`2\r\nokXX\r\n`)・`+2`・LF だけの行を受ける (`crates/msg/src/body.rs:357`・381。符号化し直すので密輸にはならない)。(4) `Via` の受けた版が常に `1.1` (RFC 9110 §7.6.3 は受けた版)。(5) CONNECT の 502 に `Date`・`Content-Length` が無い (T.29.9 の漏れ)。(6) `http/1.1`・`connect` (小文字) を受ける。
  - やること: (1) 書くときに各行を `\r\n` に正規化する。(2) token でない名前の行を落とす。(3) データの後ろが空行でなければ 400。(4)〜(6) は直すか、そのままにする理由を台帳に書く。
  - 受け入れ基準: `framing-probe.py` に各 1 行足して期待どおり。forward の Ir の増えを測って記す。

**32c 計器と物差し (本体を変える)**

- [ ] **T.32.16 異常検知の `dns_slow` を、名前解決に成功したミスで判定する (名前解決の失敗の遅さで鳴っている)**
  - 目的: 0.30.4 の起動から 62 件 (1.28 件/時。T.17〜T.23 のころは 0.02〜0.19 件/時)。`/recent` で直接確かめた 10-31 03:46:26 (3 ミス、合計 2,297 ms) は 3 回とも nxdomain で `imap.froggy.com.au` 1 回が 2,151 ms、03:23:50 (7 ミス、2,363 ms) は nxdomain 1 回が 1,991 ms。`/errors` で突き合わせた 13 件のうち 12 件は、失敗した名前解決を除くと条件 (3 回以上・合計 1,000 ms 以上) に届かない。`history["60"]` の 26 件のうち 12 件は名前解決に成功したミスが窓に 3 回未満。標本の `dns_ms_sum` には失敗した名前解決の ms も入っていて (`crates/metrics-core/src/metrics.rs:552-557`)、失敗のぶんの ms は持っていない。判定表 phase28 の「`dns_slow` が合計 1,000 ms 未満の窓で立っていない」行は、コードが T22.2 から合計 1,000 ms を条件にしているので構造上落ちない (T.23.99 から毎回 0 件)。
  - やること: 5 秒の標本に `dns_fail_ms_sum` を 1 項目足す (`.rrd` 版 3 の予備 6 → 5、`/history` の鍵 54 → 55、古いレコードは 0 で読み戻る)。`dns_slow` は `(dns_ms_sum − fail) ÷ (dns_misses − fail_misses)` と、その回数・合計で判定する。判定表の行は新しい phase で「名前解決に成功したミスで立った件/時」に差し替える (過去の phase は動かさない)。
  - 受け入れ基準: 雪像の再生で 26 → 2 件以下。単体試験: 失敗 3 回 × 2 秒では立たない、成功した名前 10 回の平均 300 ms で立つ。

- [ ] **T.32.17 名前解決のミスの率の表示から、名前解決の失敗を除く (`/slo` だけが除いている)**
  - 目的: 0.30.4 の起動からのミス 31,348 回のうち名前解決に失敗したもの 29,742 回、確立 33,177 本。snapshot-diff の §2 は「0.94 回/接続、接続 1 本あたり 55.7 ms」(`scripts/snapshot-diff.py:1076-1079`) だが、`/recent` で成功した接続の名前解決の待ちは 1,956 本で計 1,343 ms = 0.7 ms/本。判定表の表示だけの行は 0.95 回/接続 (失敗を除けば 0.048)。`/daily` の `dns_per_connect` は 1.137 / 1.09 で、`/slo` の同じ日は 0.039 / 0.053。ダッシュボードの KPI「名前解決」は 64.5% (`crates/web/src/dashboard.html:269-270,982`、misses ÷ (hits + misses))、失敗を除き `stale_hits` を当たりに数えれば 5.1%。
  - やること: 道具 (snapshot-diff と phase の表示) は `history` の `dns_fail_misses` を引く。ダッシュボードは `/history` の和から出す (か `/status` の `dns` に失敗したミスの数を足す)。`/daily` に失敗を除いた率の欄を足す。
  - 受け入れ基準: 実の 1 枚で 0.048 回/接続・KPI 5.1% が出る。欄の無い古い版では今どおり読める。

- [ ] **T.32.18 `/daily` が、再起動した日の前のプロセスのぶんを落とさない**
  - 目的: `2026-10-31T063104Z-daily.json` の 10-28 は `secs` 16,819 (4.7 時間)・要求 1,675 で、0.29.36 の 00:00〜19:18 (19.3 時間) が無い。書く側 (`crates/metrics-slo/src/daily.rs`) はプロセスのメモリの中の積み上げしか持たない。`.rrd` から読み戻した `history["60"]` には前日 06:16 からの 1,440 行があるが使っていない。デプロイの多い時期は日次の 14 日のうち 10 日が欠けた。
  - やること: 起動したら、読み戻した `history["60"]` の UTC 0 時からの標本で今日の積み上げを組み直す。`version` は「その日に動いた版」の並びにする。
  - 受け入れ基準: 結合試験で、日の途中で再起動してもその日の `secs` が 86,400 前後になり、要求の和が前後の和と一致する。

**32d 性能 (本体を変える。tokyo の CPU は 1,204 us/s、要求 1 本あたり 1,951 us で、役割は `conn` 22.7%・`profile-sample` 21.8%・`cache-probe` 15.5%・`history` 9.6%・`idle-watch` 6.4%・`persist` 4.1%)**

- [ ] **T.32.19 出入りの多いトンネルの預け入れの往復を減らす (猶予 100 ms の下限)**
  - 目的: tokyo の `/recent` (`2026-10-31T063104Z-snapshot.json`、4.3 時間・2,000 本) で預け入れ (`parks`) が 9,857 回 = **0.64 回/秒** (10-30 の 1 枚は 1.16 回/秒)。最多は login.live.com の 1 本で 220 秒に 407 回 (0.54 秒ごと)。預け入れの 74〜81% は平均の間隔が 1 秒未満のトンネル。手元で 4 本のトンネルが 0.5 秒ごとに 200 B を往復する形 (`t32/perf/chatty-ab.sh`、交互に 3 組) で、既定 (猶予 100 ms) は conn と idle-watch を合わせて 1 往復 **約 230 us**、`PROXY_PARK_GRACE_MS=1000` (預けない) は **約 110〜125 us**。預けて引き上げる 1 回が約 115 us で CONNECT を 1 本張るのより重い (`strace -c` で 1 往復ごとに epoll_ctl 4・futex 約 3.7・poll 3、スレッドの起床 3)。tokyo の idle-watch の 77〜102 us/s はこの往復と桁が合い、conn 側を合わせて **tokyo の CPU の 13〜18%** の見込み。コードは `crates/tunnel/src/tunnel.rs:681` (`MIN_PARK_GRACE = 100 ms`)、`crates/server/src/idle.rs` 267-341。
  - やること (案): (a) 前回の預け入れが短く終わった (例 1 秒未満で起こされた) トンネルは次の猶予を倍にしていく (上限 1〜2 秒)。(b) トンネルの下限を 1 秒に。(c) epoll を `EPOLLONESHOT` と `EPOLL_CTL_MOD` で使い、出し入れ 4 回を 2 回に。本当に暇なトンネル (秒〜分) の預け入れは今のまま。
  - 受け入れ基準: 手元の chatty (4 本・0.5 秒・40 往復) で 1 往復の CPU が −40% 以上、`--only connect` の CPU/本が +2% 以内、`--only idle-tunnels --conc 5000` の RSS とスレッドの数が前と同じ、idle・tunnel・overload の結合試験が通る。次の締めで tokyo の idle-watch の us/s と parks が下がる。

- [ ] **T.32.20 `cache-probe` が 60 秒ごとに割当の下のファイルを全部たどるのを間引く**
  - 目的: tokyo の雪像の `/profile` (5 秒の標本 717 本) を 1 分の中の位相で数えると、cache-probe の CPU の **73% が毎分同じ 5 秒**に集まる (`t32/perf/phase.py`)。1 分に 1 回動くのは `refresh_other_disk_usage` (`crates/cache/src/cache/probe.rs:22` の `OTHERS_INTERVAL_SECS = 60` と 151-158) で、tokyo は割当が fixed (3,072 MB) なので毎分 `sysinfo::dir_size_excluding(/home/container, キャッシュの置き場)` (`crates/sysinfo/src/sysinfo/fs.rs:147`) で全部のファイルを stat する (1 回およそ 10 ms ≈ 150〜170 us/s、tokyo の CPU の約 13%)。手元の再現 (`quota-walk.sh`、`SERVER_DISK=3072`): 0 ファイルで 69 us/s、5,000 ファイルで **248 us/s** (1 回 10.7 ms、1 ファイル 2.1 us)。tokyo の `/home/container` の件数は未確認 (割当の使用 427 MB、キャッシュ 57 MB)。
  - やること: (a) 間隔を 600 秒にし、割当の残りが少ない (使用 80% 以上や ENOSPC の後) ときは 60 秒に戻す。(b) 前回から mtime の変わらないディレクトリは前回の合計を使う ((a) と組み合わせる)。README の「60 秒ごとに再計測」も直す。
  - 受け入れ基準: 5,000 ファイルで cache-probe が 100 us/s 以下、割当に近づいたときは 60 秒以内に予算が縮む、cache 系の結合試験が通る。次の締めで tokyo の cache-probe の割合が 15.5% から下がる。

- [ ] **T.32.21 毎秒・5 秒ごとに読む `/proc`・`/sys` の小さなファイルを開いたまま pread で読む**
  - 目的: T.29.20 (1 本 4 回) の残り。history は 5 秒ごとに 16 ファイル (`/proc/self/cgroup` は 2 回)、cgroup v1 では無い `*.pressure` 5 本も毎回 open して ENOENT、cache-probe は 5 秒ごとに 12 ファイル。`procbench` (暖かい状態) で `/proc/thread-self/stat` 2,531 → 1,033 ns、`/proc/self/status` 7,063 → 3,016 ns、`/proc/net/netstat` 45,363 → 15,628 ns。試作 B (`t32/perf/protoB-keep-proc-fds.patch`) で暇な全体のシステムコール 63.8 → 27.2 回/秒 (history 11.7 → 5.8、cache-probe 9.4 → 2.8)、cache-probe 60 → 47 us/s、合計の中央値 340 → 311 us/s (−8.5%)。`read_small` は呼ぶたびに 4 KiB の Vec を作ってゼロで埋める (history の memset 約 4.7 万 Ir/回)。
  - やること: `sysinfo` に「開いたままの小さなファイル」の型 (読めなくなったら開き直す) を置き、history・cache-probe・profile-sample の固定のパスに使う。無いと分かったパスは次の起動まで開き直さない。`/proc/self/cgroup` は一度だけ読む。`read_small` は器を使い回す。
  - 受け入れ基準: 暇な既定 60 秒の `strace -f` でシステムコールが −50% 以上。`/status` と `/history` の値が前後で同じ。試験: ファイルが消えて現れる、cgroup の移動、4 KiB を超える `netstat`。

- [ ] **T.32.22 `persist` が 60 秒ごとにホスト表 1,000 行を 1 行ずつ pwrite するのを、変わった行だけにする**
  - 目的: tokyo の hosts は上限の 1,000 件で persist は 49.7 us/s、1 分の中の位相で 83% が同じ 5 秒。手元でホストを 1,200 件埋めると persist は **62 us/s**、1 分に `pwrite64` **1,001 回** (1 回の flush で 3.75 ms)。コードは `crates/metrics/src/persist.rs:191-216` (`flush_stats` → `write_table`) で、毎回 `hosts_sorted()` の 1,000 行を並べ替えて写してもいる。
  - やること: 行ごとに前回書いた中身の hash を持ち、同じなら書かない。または区画をまとめて組んで 1 回で書く。
  - 受け入れ基準: 1,200 ホストのうち 10 件だけ動く形で pwrite が 1 分 50 回以下、persist の CPU が −70%。再起動で読み戻す試験は今と同じ。

**32e 道具・試験・文書・CI (本体を変えない。デプロイは要らない)**

- [ ] **T.32.23 秒の境目で落ちる試験を直す (`resp_framing_test` が `Date` を含めたまま比べている)**
  - 目的: `tests/resp_framing_test.rs:510-513` の `test_integration_chunked_with_content_length_is_unchanged` が 2 つの生の応答を `assert_eq!` で比べていて、T.29.9 からプロキシが `Date` を付けるので秒の境目をまたぐと落ちる。負荷 3 本の下で 20 回に 1 回、負荷なしで 300 回に 2 回 (0.67%)。同じファイルの 265・273・281 行は `without_date` で外している (抜けはこの 1 か所)。
  - やること: 両方の応答に `without_date(&…)` を被せる。
  - 受け入れ基準: その 1 本を負荷 3 本の下で 300 回まわして 0 回落ちる。

- [ ] **T.32.24 この環境だけで落ちる既知の 6 本を、試験の側で直す**
  - 目的: 毎回「既知の 6 本」と覚えて読み飛ばしていて、本物の退行が紛れる。canary ×4 (`crates/metrics-watch/src/canary.rs:790,840,889,929`) は `TcpListener::bind("[::1]:0").expect(…)` が EAFNOSUPPORT で落ちる (同じ型の `crates/sys/src/sys.rs:1184` と `crates/net-conn/src/net.rs:1072` は bind できなければ飛ばす)。`tests/config_test.rs:402` は `cgroup_cpu`・`ipv6_route` が読めない機械で `proc_schedstat` だけを許している。`tests/tunnel_test.rs:415` は sandbox の網が `192.0.2.1:443` を即 `ECONNREFUSED` にするので時間切れにならない。
  - やること: 試作 (`t32/tools/canary-skip.patch`・`config-check-probe.patch`・`tunnel-blackhole.patch`) の形で直す: canary は bind に失敗したら飛ばす、config_test は試験のプロセスで `capabilities::probe().missing()` を取って同じ機械で読めないものも許す、tunnel_test は `listen_socket(127.0.0.1:0, backlog 0)` で 1 本繋いで accept の列を満たし、SYN を捨てる相手を手元で作る (sandbox で 20/20、0.34 秒)。
  - 受け入れ基準: この sandbox で `cargo test --workspace` の失敗が 0 本、CI でも 0 本。

- [ ] **T.32.25 `watch.yml` の無言の再起動の見張りを、実際の実行の間隔に合わせる**
  - 目的: cron は `17 * * * *` なのに、10-26 01:08Z〜10-31 02:20Z の 121.2 時間で schedule の実行は 23 回 (0.19 回/時、間隔の中央値 5.5 時間・最大 7.5 時間、22 の間隔の全部が 2 時間以上)。`scripts/watch-deployed.py:7,83` は「直近 75 分の無言の再起動」だけを見るので、約 8 割の期間の再起動が見えない。README の「1 時間に 1 回 (毎時 17 分)」も事実と違う。この期間に無言の再起動は 0 回で、まだ実害は無い。
  - やること: workflow の中で前回の成功した実行の時刻を `gh run list --workflow watch.yml --status success -L 2` (`permissions: actions: read`) で取って `--since` に渡す。取れなければ 480 分。README を実際の間隔に直す。
  - 受け入れ基準: 実行の間隔が 7.5 時間でもその間の無言の再起動を鳴らす試験 (`test_watch_deployed.py`)。README と実際が合う。

- [ ] **T.32.26 台帳の締めを道具 `scripts/ledger-close.py` にする**
  - 目的: このセッションの締めは scratchpad の写し (`close_task.py`・`close_task29.py`〜`close_task31.py`) を毎回書き換えていた (差は番号・題・日付・「前のタスク」の正規表現だけ、どれにも `\*\*29` の書き残し)。写しが直さないもの (TODO.md 冒頭の「T.15〜T.N」、`tasks/README.md` の「タスク 0〜N」、全部済んだときの前置きの移動、§5 の「タスク N は全部済んだ」(75305de では「。 タスク 31」と空白が 2 つ入った)) は毎回手で直した。10-25 以降の 193 コミットのうち版上げが 28 本、台帳の移しが 31 本。3cd4499 に `close_task31.py` を当てると TASKS.md と `tasks/T31.md` は 95c7566 と同じバイト列になった (`t32/tools/replay_close.sh`)。
  - やること: `ledger-close.py T.x.y 結果.txt [--title]` (見出しから N を取り、`[x]` にして `tasks/` へ移し、§5・索引・範囲の印 2 つを直す) と `ledger-close.py --finish N [--deployed X.Y.Z+h]` (前置きを移し、TASKS.md の節を消し、§5 に「全部済んだ」を足し、デプロイしたなら §2 の見出しの版を直す)。最後に `check-ledger.py` を回す。README の運用 4 に使い方。
  - 受け入れ基準: 3cd4499→95c7566 と 95c7566→75305de を再生して 4 ファイルとも同じバイト列になる試験 (75305de の空白 2 つは直した形で)。

- [ ] **T.32.27 `crate-versions.py --task` を取り込んだあとの main で回しても正しく上げる**
  - 目的: 既定の基準 `git merge-base HEAD main` が HEAD そのものになるので、上げるものが 0 本で exit 0 になる (再現: `git checkout -B main ed4aea5` のあと `--task T.30.4` で「上げた: 0」・rc 0、`--since ed4aea5~1` なら 7 本)。そのまま `--check` は 5 本を決まり (c) で落とすので、気づくのは後。このセッションでは毎回 `--since` を手で渡していた。
  - やること: (a) 基準 = HEAD で上げるものが 0 本なら exit 2 で `--since` を求める、(b) `--task T.x --fix` を足して `--check` の (c) が落とすものだけを上げる (取り込みの運用はこちらが合う)。README の運用 0 に注意を書く。
  - 受け入れ基準: main の上で取り込んだ直後に回すと正しい 7 本が上がるか、使い方の誤りとして止まる試験。

- [ ] **T.32.28 README・docs の古い数字と、指す先の切れた「上の」「下の」を直し、check-docs で見る**
  - 目的: README 1143 の `cargo test` は `default-members = ["."]` で 1,069 本のうち 337 本しか回らない (CI は `--workspace`)。README 703「日次は 4 MiB まで (30 日で 120 MiB)」は T.29.16 から 6 MiB (`crates/metrics-slo/src/snapshots.rs:54`。tokyo の日次 3,906,230 B は 4 MiB の 93%)。README の「性能」の表のバイナリ 1,381,872 / 1,054,184 B (v0.30.4 の dist は aarch64 1,962,008 B・x86_64 2,158,664 B)、TODO §2 のバイナリと「テスト 793 本」(今は Rust 1,069 本 + Python 726 本)、`docs/perf.md:372` の「いまの数字」が 0.27.8 のまま。指す先の切れた「上の」「下の」(`t32/tools/updown.py`): README 455・1167・1170 → `docs/perf.md`、README 1214・1217 は実際には下、`docs/features.md:472`・`:968`、`docs/perf.md:9,22,310`。
  - やること: 上を直し、移った先はリンクにする。`check-docs.sh` に (j)「上の/下の『…』」の見出しが実在して向きが合う、(k) `docs/perf.md` の「いまの数字」の版が §2 と同じ、を足す。
  - 受け入れ基準: `updown.py` が 0 件、check-docs の差分 0。

- [ ] **T.32.29 締めの判定表の CPU の行を、名前解決の失敗の割合で動かない形にし、前の版の 24 時間の `/profile` を使う**
  - 目的: phase28 の「cgroup の CPU ÷ 要求」は 2,893 → 2,000 us (0.69 倍) で満たしたが、名前解決に失敗した要求 (ほとんどが負のキャッシュで即 502) の割合が前 40% (17,048 / 42,672) → 後 66% (71,894 / 108,756) に増えていて、名前解決に成功した要求あたりでは 4,817 → 5,902 us (1.23 倍) で届かず。「`conn` 役の CPU/要求」と「裏の役割の割合」は前の版の材料が停止の 1 枚の `/profile` (0.5 時間) しか無く T.29.99 から 3 回続けて「参考」だが、0.29.36 の 24 時間の `2026-10-28T143043Z-profile_res_60.json` (1,439 標本) が手元にあった。それで比べると `profile-sample` 21.4 → 21.8%、`cache-probe` 13.6 → 15.5%、`conn` us/名前解決に成功した要求 1,063 → 1,179 (1.11 倍)、裏の役割 (`conn`・`accept` 以外) の us/秒 890 → 894 (量で動かない)。
  - やること: `--closeout` は、前の版の最後の `-profile_res_60.json` (同じ版で 12 時間以上) を前の材料に選ぶ。新しい phase で CPU の行を (a) 暇なときの費用 = 裏の役割の us/秒が前の版の 1.1 倍未満、(b) 本業 = `conn` us/名前解決に成功した要求が前の版の 1.1 倍未満、の 2 つにし、コアの合計と要求あたりは表示だけ。
  - 受け入れ基準: 実の 2 組 (`2026-10-28T143043Z-profile_res_60` → `2026-10-31T063104Z-profile_res_60`) で上の数が出る、「参考」が 0 行、名前解決の失敗の割合だけを 40% → 66% に変えた作り物で判定が動かない、check-docs (h) 差分 0。

- [ ] **T.32.30 締めの判定表の接続のエラーの行を、起動からの `/history` の 443・80 で読む**
  - 目的: 今の行は `/recent` の末尾 (48.3 時間のうち 2026-10-31 02:12〜06:30Z の 4.3 時間 = 9%) だけで判定していて、0.0609 → 0.0121 (0.20 倍) で満たした。同じ定義を起動からの `/history?res=3600` で数えると 0.0393 (前、1,004 / 25,537) → 0.0832 (後、3,010 / 36,187) = 2.1 倍で逆の結論 (中身はメール宛)。443・80 だけ (`other_port_*` を引く) なら後は 8 / 29,008 = 0.00028。
  - やること: 新しい phase で、この行を「起動からの `/history` の (エラー − 名前解決の失敗 − `other_port_errors`) ÷ (確立 + 同じエラー − `other_port_attempts`)」にする。前の版に欄が無ければ「判定できず」。`/recent` の窓の値は表示だけ。
  - 受け入れ基準: 実の 1 枚で後 0.00028 が出る、前が 0.29.36 なら「判定できず」で理由が出る、check-docs (h) 差分 0。

- [ ] **T.32.31 締めの要約の小さな誤り (切れた一覧から出した数・誤った警告・`/status` を取る順)**
  - 目的: (1) `/dns` の一覧は古い順 (`sort: age`) で `MAX_PAGES=8` で止まると古い側 4,763 / 7,511 件しか来ないのに、§5 は「最後に使ってから 1 時間以上 4,763 件 (表 4,763 件中)」と書く (`scripts/snapshot-diff.py:1269-1272`)。(2) `/events` の「リングが満杯 … 起動直後のぶんが落ちているかもしれない」(`scripts/snapshot-summary.py:693-694`) は、起動より前の 308 件と起動の `start` が残っていても出る。(3) `--closeout` は日次の雪像 (3.9 MB。`/snapshots/<date>` はファイルを丸ごと `String` に読む) を取ってから `STATUS_BEFORE` を取る (`scripts/collect-deployed.sh:220-228`・474)。
  - やること: (1) 切れた一覧なら「古い側の N 件だけ」と書く。(2) 起動より前の出来事が 1 件でも残っていれば警告しない。(3) closeout の最初の 1 本を `/status` にする。
  - 受け入れ基準: 実の 1 枚で (1)(2) の文面が変わる試験、`test_collect_deployed.py` で要求の順が `/status` → `/snapshots` → `/snapshot`。

- [ ] **T.32.32 `release.yml` は CI が通ったコミットにだけタグを打つ**
  - 目的: v0.28.48 (`b56160a`) は release の起動 02:31:14Z が同じコミットの CI (02:30:38〜02:34:59Z) の終わりより前だった (結果は success、その 1 つ前のコミットは fmt で落ちていた)。ほかの 4 本は CI が通ってから打っている。
  - やること: `tag` ジョブで `gh api repos/…/commits/$commit/check-runs` を読み、`check` が success でなければ落とす (`permissions: checks: read`)。
  - 受け入れ基準: CI が終わる前か落ちたコミットを渡すと理由を出して落ち、通ったコミットなら打つ (手で 1 回ずつ)。

- [ ] **T.32.33 ci.yml の小物: Benchmark が落ちても成功になる・`check` のキャッシュに `restore-keys` が無い**
  - 目的: (1) step の shell は `bash -e {0}` で `pipefail` が無く、`bench … | tee` の終了コードは `tee` のもの。起動の待ちも `sleep 2` だけ。(2) `check` のキャッシュの鍵が `hashFiles('**/Cargo.toml')` で版を上げるたびに外れる (外れた ba512ad は job 7 分 0 秒、当たった 96b4f49 は 5 分 56 秒)。
  - やること: (1) その step に `shell: bash` を付け、ポートが開くのを待つ。(2) deployed-like のジョブと同じ `restore-keys` を足す。
  - 受け入れ基準: bench をわざと落とした枝で step が落ちる (1 回だけ)。版を上げたコミットでも部分一致で戻り 6 分台。

- [ ] **T.32.34 どの機械でも回らない試験 18 本 (nagoya の雪像が前提) を片づける**
  - 目的: `scripts/test_snapshot_criteria.py:562` などは `status/` 直下の 2026-09 の雪像を待ち、main の `status/` を繋いでも 23 本が飛ばされ、そのうち 18 本がこれ (CI では 29 本全部)。T.31.99 の 48 時間を固定する試験も無い。
  - やること: 匿名化して `scripts/testdata/` に寄せるか、消して記録を残す。
  - 受け入れ基準: 飛ばす理由が「tokyo の実データが無い」だけになる。

- [ ] **T.32.99 締める (タスク 32 の版を 48 時間走らせたあとに見る・決める・書く)**
  - 前提: デプロイ先は `tokyo.sorahost.net:60357`。版は本体を最後に変えた小タスクの番号 (前のタグとぶつかるなら本体と proxy-server だけ最後の小タスクの番号に上げる)。デプロイそのものは利用者が決める。タグはデプロイの合図の直前に、クラウドのセッションから `workflow_dispatch` で打つ (T.32.32 が入っていれば CI が通ったコミットだけ)。「前」は 0.30.4 の最後の停止、0 時間は新しい版の起動 60 秒 (`--closeout` が版で選ぶ)。**今回は 24 時間ではなく 48 時間で見る** (段は 1 日 1 回の日次の雪像と、日によって来る総当たりで出るため)。
  - 判定: 新しい phase (T.32.16・T.32.29・T.32.30 で作る。無ければ phase28)。ほかに見ること: `/history?res=3600` の `rss` に段が出ていないか (00Z と総当たりの時間、`mem_used`・`stores`・`evictions`)、`heap_free` の山、`memory.trim`、`dns.entries` が 4,096 以下 (T.32.2)、`/slo` の `excluded` が出ているか (T.32.10)、異常検知の件/時 (T.32.16 と下の判断)、tokyo の役割ごとの CPU (T.32.19〜22)、キャッシュに当たった要求の数と HIT の中身 (T.32.8・T.32.9)。

**利用者の判断が要るもの (番号は決まってから振る)**

| 何を | 材料 (2026-10-31) | 選択肢 |
|---|---|---|
| ホスト別統計の表が満杯で入れ替わらない (T.32.10) をどう直すか | 1,000 行のうち 740 行が 7 日以上使われていない。T.30.4 の宛先外しは表の行しか数えないので本番で 0 回。`/hosts` の差分の 76.5% が `other` | (a) 満杯なら `last_seen` がいちばん古い行 (か N 日使われていない行) を捨てて新しい鍵を入れる (`/hosts` の通算をその日数で捨てる) / (b) 読み戻すときに N 日以上前の行を捨てる / (c) `/slo` の宛先の数え方だけを表から切り離した小さな表 (その時間のエラーの宛先だけ、上限 64 ほど) にする (`/hosts` は今のまま) / (a) か (b) と (c) の両方 |
| 名前解決の待ちで接続の枠がふさがる (T.32.11) をどう直すか | 応答しないリゾルバで 8 本の相異なる名前が枠 8 を 4 秒握り、正当な要求は 503。tokyo は 128 本で全停止しうる | (a) 名前解決を上限付きの別スレッド群に逃がし、要求側は締め切り (新しい鍵 `PROXY_DNS_TIMEOUT_SECS` か `PROXY_HEADER_TIMEOUT_SECS`) で待って、超えたら枠を手放して 504 / (b) 同時に名前解決しているワーカーの数を `PROXY_MAX_THREADS` の一定割合で頭打ちにし、超過分は待たずに 503 / (c) 案内だけ (`PROXY_MAX_CONNS_PER_CLIENT` とリゾルバの `timeout`・`attempts` の推奨を README と `--check` に) |
| 鮮度の手がかりが何も無い応答を 300 秒共有する | `Cache-Control`・`Expires`・`Last-Modified` の無い 200 に既定 TTL 300 秒 (`crates/freshness/src/freshness.rs:118-127`)。再現 (`t32/protocol/p9_pool_cookie.py`): `Cookie: user=alice` の `GET /whoami` の `you are alice` を、`user=bob`・`user=carol` が HIT で受けた。RFC 9111 §4.2.2 の経験則は「してよい」で違反ではないが、Squid の既定は Last-Modified の無い応答を新鮮とみなさない | (a) Last-Modified の無い応答の経験則の TTL を 0 にする (validator が無ければ保存しない) / (b) 要求に `Cookie` があり、応答に明示の鮮度も `public` も無ければ保存しない / (c) 既定の 300 を下げる / (d) 今のまま (README に危うさを書く) |
| 異常検知の `errors` と `dns_miss_rate` を `/slo` と同じ数え方にするか | 0.30.4 の 48.3 時間の `/events` 185 件のうち 184 件が `dns_slow`・`errors`・`dns_miss_rate`。`errors` は「5 分のエラー − nxdomain ≥ 5」でポートを見ない (`crates/metrics-watch/src/anomaly.rs:491-493,546`)、`history["60"]` の再生で今の数え方 27 回・`other_port_errors` を引けば 0 回 (名前解決の失敗以外のエラー 855 件のうち 443・80 宛ては 2 件)。10-29 06:40 から 523 分立ちっ放しで、その間に本当の 443 の障害が来ても新しい 1 件は書かれない。`dns_miss_rate` は名前解決に失敗したミスも数え (`crates/metrics-window/src/history/summary.rs:149`)、1 時間の値は最大 2.76 (失敗を引けば最大 0.136、閾 0.20)。512 件のリングは今の速さで 5.6 日しかもたない | (a) 両方とも `/slo` と同じに (`errors` は 443・80 のエラーで名前解決の失敗を除く、`dns_miss_rate` は名前解決に失敗したミスを除く。T.30.3 の決定を異常検知にも当てる) / (b) 名前解決の失敗を除くだけ (`errors` はポートを問わない) / (c) 今のまま |
| `profile-sample` の暇な秒の読み飛ばし | tokyo の 21.8% (263 us/s、うち kernel 208)。試作 C (`t32/perf/protoC-skip-idle-scan.patch`): `/proc/self/stat` の utime + stime とスレッド数が前回と同じなら全スレッドを読まず前回の状態をもう 1 標本積む。暇な 45 秒で profile-sample 166〜169 → 81〜93 us/s (−48%)、システムコール 63.8 → 12.2 回/秒。状態の分布は「10 ms 以上走ったときだけ読み直す」近似になる | (a) 試作 C の形 (新しい要求が来た秒も全部読む条件に足す) / (b) 既定の間隔を 1 → 5 秒 (190 → 44 us/s) / (c) 今のまま |
| `tasks/` の切れたリンク 2 本を直してよいか | `tasks/T30.md:53` と `tasks/T31.md:28` の `tasks/T29.md`・`tasks/T30.md` は TASKS.md の相対パスのまま移したもので、`tasks/` の中からは切れている。台帳の決まりは「本文は 1 文字も変えずに移す」 | (a) 直す (リンクだけは移すときに書き換える、と決まりに書く。T.32.26 の道具もそうする) / (b) 今のまま (切れたまま) |
| メモリの異常検知を足すか | RSS が 10-30 16Z に 43.8 → 56.1 MB、10-31 00Z に 69.2 MB と段で上がったが `/events` には 1 件も出ていない。`anomaly.rs` の 8 種にメモリの種類が無い。cgroup の上限 256 MB の 29% で、困ったことはまだ起きていない | (a) 足す (例「cgroup の使用量が上限の 75% 以上」か「1 時間で RSS +20%」) / (b) T.32 で段の正体が分かったあとに決める / (c) 足さない |

**調べたが問題が無かったもの・挙げるほどでないもの** (重ねて調べないため。2026-10-31):
- メモリ: 表の上限 (`dns` 4,096 は失敗の経路の穴 = T.32.2 のほかは守られている、ホスト別・接続元別 1,000、`readers` 256、`/events` 512、`/errors` 500、`/log` 1,000、`/recent` 2,000、ホスト別の時系列 16、オリジンの遊休の表は空の行を消す)、状態ファイルの 60 秒ごとの書き出しは約 0.5 MB で器を使い回す、1 接続あたりの器 (読みの 8 KiB をスレッドごとに 4 本まで使い回す、64 KiB の中継の器は splice が使えないときだけ)、起動時のリングの先触り 15 MB (T.17.8)、毎時の段 09:02・12:36 は `watch.yml` ではない。
- プロトコル: 応答の先頭の各形 (obs-fold・裸の CR / NUL・`Content-Length :`・CL の食い違い・状態行のごみ・HTTP/1.0 のオリジン・LF だけの先頭・`Connection` の指名・TE の各形・壊れた chunk サイズ・本文付きの 204・trailer の禁止フィールド)、1xx (103 の整え・頼んでいない 100・102 ×3・199)、キャッシュの多数の形 (must-revalidate + 503・`Age` > `max-age`・`Date` のずれ・過去の `Expires`・素の GET への 206・304 の `private`・複数範囲・`If-None-Match: *`・安全でないメソッドの無効化・HEAD の MISS・`s-maxage=0`・Authorization・`max-age="600"`・再検証の 200 + no-store・同期の再検証の 404)、TLS と `/https/` (SAN の検証・close_notify の有無・`Location` の書き換え)、クライアントの各形 (`Expect: 100-CONTINUE`・HTTP/1.0 の keep-alive・CONNECT の不正な形・TRACE の `Max-Forwards`・裸の CR・タブ・`HTTP/1.2`)、プール (途中で切れた接続を使い回さない・暇で閉じる境目の POST 40 本で落ち 0)。挙げるほどでない: `If-Match` が外れても HIT、`max-stale` を無視 (MAY)、HEAD の応答で保存済みの GET を更新しない (SHOULD、forward が少ない)。
- 頑丈さ: 時計が戻ったとき (締め切り・最小の速さ・名前解決の齢は単調時計、キャッシュの齢は saturating で 0)、`/proc` が読めない環境、IPv4 だけ・IPv6 だけ、上限の外の枠 4 本 (名前解決が全部止まっても `/status` は取れた)、ワーカーの待ち行列・上限の差し替え・最小の速さ。
- 性能: 1 本あたりの Ir は T.29.24 から変わらない (CONNECT `--lite` 17,746・既定 21,786、forward 20,079、1 接続 1 要求 25,179、HIT 18,187)、history の user 側 (1 回 22 万 Ir、約 7 us/s)、内部の口 (`/status` 0.98 ms・`/metrics` 1.3 ms・`/snapshot` 5.9 ms、読み手は 1 者)、日次の雪像の CPU、`getaddrinfo` (147〜344 us/回、tokyo で 40〜90 us/s、自前のリゾルバは割に合わない)、起動の状態ファイルの読み戻し、dns-refresh・canary・accept (各 21〜38 us/s)、idle-watch の暇なときの起床 (預かり 0 なら 0)。
- 実データ: 成功した接続の名前解決の待ち (1,956 本で 1,343 ms)、確立 (取り分 −1.2 ms、尾は RTT + 0.2 ms、成功の最大 359 ms)、3 秒の締め切りで切ったもの (全部メール宛)、遠い接続元の first_relay (端末側)、負のキャッシュ (命中 114,046 本)、serve-stale (`stale_hits` 12,591・`stale_retries` 0)、キャッシュ (forward 450 本・命中 15、量が小さい)、ブロックリスト (設定なし)、IPv6 (v4 first・`request_probes` 0)、`/slo` の確立の速さの外れ 0.24%、`rejected_requests` (`header_timeout` だけ)、`sni_mismatches` 4,756 (IP リテラル宛て)。
- 道具: 揺れる試験 (36 のバイナリ × 20 回で落ちたのは T.32.23 の 1 回だけ)、遅い試験 (`cargo test --workspace` 171 秒・unittest 59〜69 秒、削れても 1 分前後)、起動直後のスレッド数、目次と相対リンク 119 本 (上の 2 本のほか切れなし)、`release.yml` の直近 8 回、テストの無い大事な経路 (証拠なし)。
