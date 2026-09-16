# rust-http-proxy

依存クレートゼロ (Rust 標準ライブラリ `std` だけ) の、**認証不要**な HTTP/HTTPS プロキシです。
バイナリを 1 つ置いて起動するだけで使えます。

## クイックスタート

```bash
cargo build --release                    # ビルド (約 1.4 MB のバイナリが 1 つ)
./target/release/rust-http-proxy --lite -p 8080   # 起動 (最速の素通し設定)
curl -x localhost:8080 http://example.com/        # 動作確認
```

ブラウザに設定するなら、自動設定スクリプトの URL `http://<ホスト>:8080/proxy.pac` を
プロキシ設定の「自動プロキシ設定 URL」に入れるだけです (このプロキシが落ちていれば DIRECT に落ちます)。

`cargo install --git https://github.com/takano32/rust-http-proxy` でも入ります
(ビルドはメモリ 99 MB で通ります。下の「ビルド・テスト」を参照)。

キャッシュ・ダッシュボード・統計まで使うなら `--lite` を外します。

```bash
SERVER_PORT=8080 ./target/release/rust-http-proxy
# http://localhost:8080/dashboard  … グラフとホスト別統計
# http://localhost:8080/status     … JSON、/metrics … Prometheus 形式
```

## 性能

8 コア / 6.6 GiB (aarch64、big.LITTLE)、loopback で測った値です。

**どう測ったか**: `scripts/cpu-per-request.sh` が全部やります。プロキシを big (cpu4-7)・ベンチを LITTLE (cpu0-3) に
`taskset` で固定し、`--lite` + `PROXY_LOG_LEVEL=warn`、応答本文 1 KiB、10 秒 × 3 回以上の中央値
(「CONNECT トンネル 1 本」だけプロキシ cpu4-5 / ベンチ cpu6-7)。**この機械は固定の仕方で結果が最大 2 倍動く**ので、
比べるときは必ず同じ固定の仕方で測ってください。数字は 2026-09-08 の実測で、`TASKS.md` の §2 と同じものです
(出典: T10.11 / T11.4)。括弧の **(NN%)** は**プロキシが使った 1 コアぶんの割合** (割り当ては 4 コア = 400%)。
これが 100% に遠い行は**プロキシが律速していない** = その req/s はプロキシの上限ではありません。

| 項目 | `release` (`cargo build --release`) | `dist` (配布用) |
|---|---|---|
| forward, 8 並列 | **38,619 req/s, p50 0.177 ms** (157%) | **39,368 req/s, p50 0.175 ms** (154%) |
| forward, 8 並列 (キャッシュ HIT) | 76,964 req/s, p50 0.085 ms (169%) | **77,501 req/s, p50 0.085 ms** (165%) |
| forward, 64 並列 | 31,819 req/s, p50 1.79 ms, p99 6.5 ms (184%) | 31,443 req/s, p50 1.75 ms, p99 7.3 ms (181%) |
| 1 接続 1 要求 (keep-alive 無し) | 14,105 req/s, p50 0.308 ms (145%) | 13,995 req/s, p50 0.307 ms (141%) |
| CONNECT 確立 (**TIME_WAIT 律速** → 下) | 7,951 tunnels/s, p50 0.186 ms (111%) | 7,297 /s, p50 0.186 ms (100%) |
| CONNECT トンネル 1 本 (**ベンチ律速** → 下の注) | **3,960 MiB/s** (73%) | 3,950 MiB/s (72%) |
| CPU/要求 (forward / HIT / 64 並列) | **41.4 / 22.0 / 58.1 us** | **39.6 / 21.4 / 58.0 us** |
| CPU (1 接続 1 要求 / CONNECT 確立 / トンネル) | 103.3 us/接続 / 140.0 us/本 / 185.5 us/MiB | 101.3 / 138.5 / 183.3 |
| 1 要求あたりのシステムコール | **5.02** (`recvfrom` 3 + `sendto` 2) | 5.02 |
| 同時 5,000 トンネル (アイドル) | **68 スレッド (山 260) / RSS 29.9 MB** | 68 (260) / 28.7 MB |
| 暇な keep-alive 接続 2,000 本 | **25 スレッド / RSS 25.2 MB** | 28 スレッド / 25.4 MB |
| 起動直後のスレッド | `--lite` で 4 本、既定で 7 本 | 同左 |
| バイナリ | 1,381,872 B | **1,054,184 B** |

`release` は `cargo build --release` で作るもの、`dist` は配布用 (fat LTO、`cargo build --profile dist`) です。
違いは下の「プロファイルの設定」を見てください。

**ベンチ自身の上限** (`--only direct` = プロキシを通さないオリジン直結) は LITTLE に固定して **55,112 req/s**、
固定しなければ 263,821 req/s です。forward・HIT・64 並列・1 接続 1 要求はいずれもベンチが先に頭打ちになるので、
表の req/s はプロキシの上限ではありません。**CONNECT 確立はカーネルの TIME_WAIT が律速**で
(`tcp_max_tw_buckets = 32768` を 10 秒で使い切る)、同じ設定でも tunnels/s は ±25% 暴れます
(比べてよいのは CPU/本。こちらは ±4%)。

着手前 (2026-09-07 の Python ベンチ) は forward 8 並列 188 req/s / p50 42.0 ms、CONNECT 確立 3,506 tunnels/s、
バイナリ 857 KB でした (forward は **205 倍**)。当時の値と、コアを固定せずに測っていた頃の値は
`TASKS.md` の §2 と付録 A に残してあります。

### デプロイ先 (実際に使っているプロキシ) の数字

上の表は loopback = 「同じ機械の中でどこまで速いか」です。**利用者が実際に待つ時間**は
別に測っています (Pterodactyl のコンテナに置いた 1 台。要求の 99% が CONNECT、0.015〜0.068 req/s)。
道具は `/status` `/history?res=3600` `/metrics` と `scripts/status-diff.py` / `scripts/probe-deployed.sh` で、
**ホスト別の平均は 2 枚の差分で読みます** (`/status` の `hosts[]` は状態ファイルに残って再起動をまたいで
通算されるので、そのまま読むと直す前の値が何日も混ざります)。

**1 回で全部取るなら `scripts/collect-deployed.sh HOST:PORT [DIR]`** (T14.4)。`/snapshot` を
`DIR/<UTC 時刻>-snapshot.json` (既定 `~/rust-http-proxy-status/`) に保存し、要点
(`scripts/snapshot-summary.py`)・ホスト別 (`status-diff.py`。**前回の雪像があれば差分**)・
ダッシュボードの読み方 (`check-dashboard.js`)・手元から見た待ち (`probe-deployed.sh`) を
続けて回して **Markdown 1 枚**を標準出力に出します。`status-diff.py` は `/snapshot` の JSON を
そのまま読めるので、保存したファイルを 2 つ渡せばいつでも差分が取れます:

```bash
scripts/collect-deployed.sh nagoya.sorahost.net:50697 > today.md   # 1 日 1 回
PROBE=0 scripts/collect-deployed.sh nagoya.sorahost.net:50697      # 本物の要求を送らずに取る
scripts/status-diff.py ~/rust-http-proxy-status/*-snapshot.json    # 最初と最後で差分
```

保存先は**リポジトリの外**にしてください (個票には接続元 IP と宛先ホストが並びます)。

**2 枚の雪像から「何が変わったか」を全部読むのは `scripts/snapshot-diff.py A B`** (T14.17)。
再起動をまたいでいるかを `uptime_secs` と `version` で見て、**`/history` をその再起動時刻で切り**、
**バーストの無い時間帯 (1 時間 300 本未満の標本) どうし**で CONNECT 確立の p50 / p95 と
名前解決のミス率を並べます (`/status` の通算は再起動で 0 に戻るので、そのまま引き算すると
「いつからの値か」が混ざります)。続けてホスト別・接続元別 (**新しく現れた接続元**)・
名前解決の warm と引き直し・出来事・エラーの原因別・バーストを出し、`--criteria phase14` を
足すと**完了の定義に対する判定表** (満たした / 届かず / 判定できず) が最後に付きます。
出力は Markdown なので `TASKS.md` にそのまま貼れます。

```bash
scripts/snapshot-diff.py ~/rust-http-proxy-status/2026-09-1{2,6}*-snapshot.json --criteria phase14
scripts/snapshot-diff.py a.json b.json --aaaa aaaa.json --out json      # 機械で読む形
# `/snapshot` より前の形 (`/status` と `/history` を 1 本ずつ取ったファイル群) からも組めます
scripts/snapshot-diff.py --from-files ~/rust-http-proxy-status/2026-09-12T2018Z \
                         --from-files ~/rust-http-proxy-status/2026-09-16T0106Z
```

`collect-deployed.sh` は前回の雪像を見つけるとこれを呼び、**要約のいちばん最後に判定表**を置きます
(`CRITERIA=off` で止められます)。道具の単体テストは `python3 -m unittest discover -s scripts`
(架空の雪像 `scripts/testdata/snapshot-a.json` / `snapshot-b.json` で回ります)。

| 項目 | 直す前 (2026-09-10) | いま | 出どころ |
|---|---|---|---|
| CONNECT 確立、**AAAA のあるホスト** | **257 ms** (25 ホストの中央値) | **9 ms** (29 ホスト) | 2026-09-12、`status-diff.py --aaaa` |
| 接続元から見た応答 (全要求の p50) | **327.8 ms** (avg 200.5 / p95 484.0) | **8.0 ms** (avg 28.2 / p95 53.4) | 2026-09-12、接続元別 |
| CONNECT 確立 p50 / p95 (平常時の 72.7 時間) | (測れていなかった) | **8.3 / 80.7 ms** | 2026-09-16、`/history?res=3600` |
| 名前解決のミス | (測れていなかった) | 0.55 回/接続、1 回 11.5 ms | 2026-09-16、`/status` の `dns` |
| エラー | 12 件 (**原因が分からなかった**) | **72.7 時間で 0 件** (原因別と個票つき) | 2026-09-16、`/errors` `/metrics` |
| RSS | 215.9 MB (うち先行確保 201.3 MB) | **21.1 MB** (先行確保 0) | 2026-09-16、`/status` の `cache.system` |
| `GET /` (ブラウザでプロキシの URL を開く) | 502 | **200** (エンドポイントの一覧) | 2026-09-12 以降 |

**250 ms が消えたのは Happy Eyeballs の順番を変えたから**です。AAAA (IPv6) を持つホストへは IPv6 を先に試し、
返らなければ 250 ms (RFC 8305 の Connection Attempt Delay) 待ってから IPv4 に移ります。IPv6 が**黙って落ちる**
この環境では、AAAA のあるホスト全部でこの 250 ms を毎回払っていました。今はホストごとに最後に勝った族を覚え、
IPv6 が起動から 1 度も勝たずに 3 回続けて負けたら初めて見るホストも IPv4 から試します
(「特徴」の IPv4 / IPv6 の項。600 秒に 1 回は IPv6 を先頭に戻すので、IPv6 が生き返れば自動で戻ります)。

**残っている待ちは 2 つ**です。(1) **遠いホストの RTT** — p95 の 60〜90 ms や 250 ms の行はネットワークの
往復そのもの (Google の push 通知 `mtalk.google.com` で 30 ms、欧州のホストで 250 ms) で、プロキシ側では縮みません。
(2) **名前解決** — 接続 1 本あたり 6.3 ms (確立 15.3 ms の 41%) で、**ここはまだ作業中**です。

### デプロイ先に似せた条件で手元で測る (`scripts/deployed-like.sh`)

**上の 250 ms は loopback では再現しません。** ベンチは宛先を IP リテラルで書くので、プロキシから見た
候補が 1 つしかなく、**Happy Eyeballs (候補が 2 つ以上のときだけ通る道) に一度も入らない**からです。
`scripts/deployed-like.sh` は root なしで「**IPv6 が黙って落ちる**」小さな環境を作り、その中でベンチを回します。

```bash
scripts/deployed-like.sh -- scripts/cpu-per-request.sh --only connect-multi --seconds 5
scripts/cpu-per-request.sh --deployed-like --only connect-multi --seconds 5     # 同じもの
```

作るものは 4 つで、どれもデプロイ先 (Pterodactyl コンテナ) に合わせてあります。

- **ネット名前空間** (lo だけ)。外へは出られないので、オリジンはベンチの内蔵のものだけです
- **IPv6 の既定経路を `dev lo`** に置く。出ていった SYN は lo を回って戻り、自分宛てではないので黙って
  捨てられ、`connect` は**約 1 秒ハングしてから失敗**します (デプロイ先の実測と同じ姿)。
  **`blackhole` の経路ではこうなりません** — この機械では `connect` がその場で `EINVAL` を返すので、
  プロキシは待たずに IPv4 へ移り、250 ms が出ません
- **`/etc/hosts` に `multi.test` の 2 行** (`2001:db8::1` = 上の黒穴、`127.0.0.1` = 生きている)。
  プロキシの `getaddrinfo` が AAAA と A の 2 候補を返します
- **`ulimit -n 1024`** と **cgroup 256 MiB** (`systemd-run --user --scope`。デプロイ先の `max_conns` 240 は
  この `ulimit` から決まります)

`--only connect-multi` はこの `multi.test` 宛てに CONNECT を張り続け、**1 本目の確立時間**と、そのあとの
p50 / p95 / max を出します。Happy Eyeballs に記憶を持たせる前後で比べると:

| プロキシ | 1 本目 (8 スレッドの中央値) | p50 | p95 | 確立/秒 |
|---|---|---|---|---|
| 記憶を持つ前 (`41e918f`) | 257.6 ms | **251.7 ms** | 257.0 ms | 32 /s |
| いま | 261.7 ms | **0.57 ms** | 1.60 ms | 7,479 /s |

2026-09-16、`--conc 8 --seconds 5`。記憶を持つ前は**毎回** 250 ms を払い、いまは**同時に走り出した
8 本だけ**払います (2 本目からはホストごとの記憶が効く)。終わりに `/status` の `ipv6` を印字します
(いまのバイナリは `{"attempts":16,"wins":0,"losses":16,"v4_first":true}` = IPv6 は 1 度も勝たず、
IPv4 を先頭にする学習が効いた状態)。**`--conc` は 3 以上**にしてください (2 本目からは記憶が効いて
IPv6 を試さないので、「3 回続けて負けたら IPv4 を先頭」は同時に走り出した本数ぶんからしか立ちません)。

**名前空間が作れない機械では「使えない」と印字して終了コード 2** で終わります
(`--only connect-multi` を名前空間の外で回したときも同じく 2)。

### 注: トンネルの MiB/s はプロキシの上限ではありません

**この行だけはベンチ側が律速しています。** プロキシは `splice(2)` で 1 バイトもコピーしませんが、
ベンチは送る側 (blaster) と受ける側 (reader) で 1 回ずつコピーするので、**ベンチのスレッドの方が先に頭打ち**になります。
そのため MiB/s は「ベンチをどのコアに置いたか」で 2.6 倍動きます
(2026-09-08、同じ `release` バイナリ、`--only tunnel --conc 1` を 10 秒 × 3 回の中央値。
上の表の 3,960 MiB/s は同じレシピを別の回に測ったもので、この経路のぶれは ±15% あります):

| 置き方 | スループット | プロキシの CPU | プロキシが使ったコア |
|---|---|---|---|
| プロキシ cpu4-5 / ベンチ cpu6-7 (現在の既定) | **3,956 MiB/s** | 182.7 us/MiB (user 10.9) | 1 コアの **72%** |
| プロキシ cpu4-7 / ベンチ cpu0-3 (LITTLE) | 1,534 MiB/s | 223.2 us/MiB (user 14.4) | 1 コアの 34% |

プロキシ側は 1 コアを 72% しか使っておらず、飽和しているのはベンチのスレッド (1 本が 100%) の方です。
**この経路の主指標は CPU/MiB** で、`scripts/cpu-per-request.sh --only tunnel --conc 1` が
CPU/MiB と「プロキシが 1 コアの何 % を使ったか」を一緒に出します。

## 特徴

- **認証不要（No Auth）**: 事前設定なしで誰でも即座に利用可能
- **依存クレートゼロ**: 外部クレート依存がないため、ビルド負荷が最小限で高速にビルド可能
- **超軽量バイナリ**: `cargo build --release` で約 1.4 MB、配布用の `dist` プロファイル (fat LTO) なら約 1.0 MB
- **HTTP / HTTPS (CONNECTトンネリング) 対応**
- **同時ミスの合流 (collapsed forwarding)**: 同じ URL を複数のクライアントが同時に要求しても、オリジンへ行くのは
  最初の 1 本だけ。残りはその保存完了を待ってキャッシュから受け取る (`cache=COALESCED`)。保存されなかった場合は
  各自で取りに行く。**保存されないと分かった URL では次から合流を通さない** (待っても保存されないので待ち損。
  `Cache-Control: no-store` のオリジンへ 8 並列で同じ URL を叩くと 30,000 → 36,500 req/s)。
  覚えるのはブルームフィルタで、`PROXY_CACHE_TTL_SECS` と同じ周期で入れ替えて忘れる
  (オリジンが `Cache-Control` を変えたら、次の周期から合流に戻る)。入れ替えた回数は
  `/status` の `cache.not_stored_rotations` と `/metrics` の `sorahost_cache_not_stored_rotations_total` に出ます
- **keep-alive と接続プール**: クライアント接続は HTTP/1.1 の持続接続 (アイドル 15 秒。
  **1 本の接続で 1,000 要求を捌いたら閉じます** — `crates/config` の `DEFAULT_MAX_REQUESTS_PER_CONN`。
  環境変数では変えられません。**1,000 要求目の応答には `Connection: close` が付く**ので、
  クライアントは「この応答で終わり」と分かってから閉じられます (入れ違いで次の要求を送って
  取りこぼすことがありません)。
  `.env` の読み直しは「次の接続から効く」ので、ずっと繋ぎっぱなしのクライアントでも
  遅くとも 1,000 要求後には新しい設定に入れ替わります)、オリジンへの接続は
  ホストごとにプールして再利用 (既定 64 本・全体 256 本まで、アイドル 60 秒まで保持、再利用前に 1 回の `recv(MSG_PEEK|MSG_DONTWAIT)` で生存確認)。
  再利用できた割合は `/status` の `origin_connections.pool_hit_ratio` に出ます。本文は必ず解読してから自前で枠付けし直す
  (Content-Length / 再 chunk / close) ので、HTTP/1.0 のオリジンやクライアントが混ざっても正しく持続する
- **Range / HEAD**: キャッシュ済みの完全な表現から `206 Partial Content` (単一範囲、`If-Range` 対応、範囲外は `416`) や
  `HEAD` 応答を切り出す。未キャッシュの Range 要求はそのまま転送
- **HTTPS のオリジンもキャッシュ (CA 不要)**: クライアントが `GET /https/example.com/path` (プロキシをオリジンとして叩く)
  か `GET https://example.com/path` で頼めば、プロキシがシステムの OpenSSL で HTTPS 取得し、平文 HTTP で返して保存する。
  応答の `Location` は `/https/...` 形式に書き換えるのでリダイレクトもプロキシに留まる。CONNECT トンネルは従来どおり素通し
- **IPv4 / IPv6 デュアルスタック**: `[::]` と `0.0.0.0` の両方で待ち受け (IPv6 が無ければ IPv4 のみ)、
  オリジンへは A / AAAA を引いて IPv6 優先の Happy Eyeballs (RFC 8305) で速い方に接続、
  `http://[2001:db8::1]:8080/` などの IPv6 リテラルにも対応。`PROXY_IPV6=off` で IPv4 のみにできる。
  **IPv6 が黙って落ちる環境 (経路はあるのに繋がらない) では自動で IPv4 を先にする**: ホストごとに
  最後に接続できた族を覚え、IPv6 が起動から 1 度も勝たずに 3 回続けて負けたら初めて見るホストも
  IPv4 から試す (RFC 8305 §8。600 秒に 1 回だけ IPv6 を先頭に戻して試すので、IPv6 が生き返れば自動で戻る)。
  試行そのものはやめないので、IPv4 が死んでいるホストは IPv6 で拾える。
  勝敗は `/status` の `ipv6` と `/metrics` の `sorahost_ipv6_*` に出る。**確実に IPv6 を避けたいなら `off`**
- **RFC 7230 / RFC 9110 準拠**:
  - Hop-by-hop ヘッダーの自動除去
  - `Via` ヘッダーおよび `X-Forwarded-For` ヘッダーの付与・伝搬。`Via` の印は
    `1.1 rust-http-proxy/<起動ごとの 8 桁 16 進>` で、**自分の印が付いた要求を受けたら `508 Loop Detected`**
    (印が起動ごとなので、rust-http-proxy を 2 段に並べた正当な構成は誤検出しません)
- **ACL / ホストフィルタリング**:
  - `PROXY_ALLOW_HOSTS` / `PROXY_DENY_HOSTS` による許可・拒否リスト（ワイルドカード対応）と 403 Forbidden 制御
  - `PROXY_ALLOW_CLIENTS` による**接続元**の許可リスト (`10.0.0.0/8` のような CIDR 可)。
    一覧に無い相手は accept 直後に閉じます (内部エンドポイントも含めて。既定は全許可)
  - `PROXY_MAX_CONNS_PER_CLIENT` による**接続元ごとの同時接続の上限** (既定 `0` = 無効)。
    1 人が `PROXY_MAX_CONNS` を使い切るのを防ぐ公平さの上限で、超えた接続は 503。
    自分宛て (`/status` など) は上限の外で受けるので、上限に当たっている相手からでも監視は取れます
- **2 段キャッシュ (メモリ + ディスク) — 固まらない限界まで使う**:
  - 既定は **自動モード**: 「これだけは空けておく」安全マージンを毎秒の観測から動的に決め、
    残りをすべてキャッシュに充てる。他プロセスが資源を使えばその分だけ自動で縮退
  - **先行確保は使われるまでしない**: 保存が 1 件も無い間はバラストを取らず、保存が始まったら実使用量の 2 倍までを先に押さえる (`PROXY_CACHE_RESERVE`。従来の「予算いっぱいを最初から」は `eager`)
  - **固まらない安全策**: cgroup 制限 / `SERVER_MEMORY` を上限として尊重、PSI (メモリ圧迫) や ENOSPC を検知したら即座に返却して以後のマージンを広げる、tmpfs 上のディレクトリは検知して警告
  - **RFC 9111 の再検証**: 期限切れでも ETag / Last-Modified 付きのエントリは残し、`If-None-Match` / `If-Modified-Since` で再検証 (304 なら本文転送なし)。オリジン障害時は stale を配信
  - **大きなオブジェクトもストリーミング**: 本文を RAM に溜めず、ディスクへ流しながら配信。ディスク層は 1 オブジェクト 4 GiB まで
  - L1: メモリ LRU、L2: ディスク LRU (256 分割ディレクトリ、再起動後もインデックスを復元)
  - `Cache-Control` / `s-maxage` / `max-age` / `Expires` / `Last-Modified` からの経験則 / `no-store` / `private` / `Set-Cookie` / `Vary` を尊重
  - ヒット時は `X-Cache` / `Age` ヘッダーを付与。クライアントの条件付き要求にはキャッシュから 304 を返す
- **詳細なアクセスログ**:
  - 既定 (INFO) で 1 リクエスト 1 行のアクセスログ (キャッシュ HIT/MISS/REVALIDATED/STALE 付き) を標準出力へ
  - `PROXY_LOG_LEVEL` で `error` / `warn` / `info` / `debug` / `trace` を切り替え
- **`.env` の自動再読込**: `$HOME/.env` の保存を inotify で検知し、ACL・タイムアウト・ログレベルを再起動なしで反映
- **DNS キャッシュ**: 名前解決の結果を 60 秒保持し、CONNECT ごとの解決をなくす。解決失敗時は古い結果で凌ぐ。
  **直近 60 秒に使われた名前は期限の 3/4 (45 秒) で裏で引き直す**ので、使い続けているホストは
  期限切れのミス (`getaddrinfo` の待ち) を払いません。引き直しは専用のスレッド 1 本 (`dns-refresh`、
  最初の引き直しで起きる) が順にやり、要求の経路は待ちません。そのぶん**古い答えで繋ぐ窓が
  最長 TTL の 1/4 (15 秒)** できますが、その間に IP が変わっていても次の候補へ行くだけです
  (Happy Eyeballs)。解決に失敗した名前は 60 秒覚えて引き直しません。
  さらに **keep-warm**: **直近 15 分 (`PROXY_DNS_WARM_SECS`) に 2 回以上使われた名前 (warm) は、
  使われていなくても 45 秒 (3/4 TTL) ごとに裏で引き直し続ける**ので、TTL より長い間隔で来る
  ホスト (数分おきの push 通知など) もミスになりません。同時に warm でいられるのは 32 件まで
  (最後の使用が古いものから外れ、最後の使用から 15 分過ぎたら止まります)。
  **名前解決は 1 要求 1 回**: ローカル宛て (SSRF) の判定で引いた答えをそのまま接続に使うので、
  キャッシュを切っていても判定と接続が別の答えを見ることはない
- **入場制御 & ネガティブキャッシュ**: 層が埋まったら 2 回目に見た URL だけ保存。404 / 410 は既定 60 秒だけ保持
- **ドメインのブロックリスト**: hosts 形式のファイルや URL (1 日 1 回自動更新) から読み、広告・トラッカーを CONNECT の段階で 403 にする
- **統計と履歴の永続化**: 固定サイズ (4 MiB) の状態ファイルに、履歴 3 解像度 (5 秒 × 1 時間、1 分 × 1 日、
  1 時間 × 30 日) を環状に、ホスト別・接続元別の上位 1000 を固定スロットに書く。ファイルは伸びず、再起動後も表とグラフが残る。
  **形式に版があり、版が変わったら古いファイルは読み捨てて作り直す** (統計は運用の参考値なので移行はしない)。
  停止シグナル (SIGTERM / SIGINT) では表を書き出してから終了する (3 秒以内、2 回目のシグナルで即終了)。
  あわせて **1 日 1 行の要約**を `$HOME/.rust-http-proxy.daily.jsonl` に**永久に**残す (下記 `/daily`)
- **個票の永続化**: 上の状態ファイルの隣にもう 1 つ、**固定サイズ (4 MiB) の個票のファイル**
  (`$HOME/.rust-http-proxy.recent`) を置き、`/recent` (閉じた接続) ・`/errors` ・`/bursts` (山の写真) ・
  `/events` (出来事) ・`/log` を再起動をまたいで残す。書くのは**履歴スレッドの 5 秒の周期だけ** (接続を受ける経路は今までどおりメモリのリングに
  書くだけで、1 命令も増えない) と、停止シグナルの最後の 1 回。これも伸びず、版が違うファイルは読み捨てて作り直す。
  統計の `.rrd` とファイルを分けてあるのは、**個票のために統計の版を上げて全部捨てることにならないようにする**ため
- **接続元別の統計**: 接続元 IP ごとの要求数・転送量・拒否数・応答時間を `/status` `/metrics` とダッシュボードに出す。
  **個票 `/clients`** には「初めて見た時刻・`User-Agent`・宛先の種類・使ったポート・IP リテラル宛て」も出る
  (認証なしの公開プロキシなので、**見知らぬ接続元が誰のどのプログラムか**を読むため)
- **`/proxy.pac`**: ブラウザの自動設定スクリプト。自分自身・ローカル・`PROXY_PAC_DIRECT` のホストは DIRECT、
  それ以外はこのプロキシ経由 (落ちていれば DIRECT)。ブラウザに `http://<host>:<port>/proxy.pac` を設定するだけ
- **ヘルスチェック & メトリクス & 操作**:
  - `/` (エンドポイントの一覧。ブラウザでプロキシの URL を開いた人への案内。`--lite` でも出ます)
  - `/config` (**効いている設定とその出どころ**。下記「環境変数」の節)
  - `/dashboard` (ブラウザ用のコントロールパネル: 要求/転送レート・命中率・**CONNECT 確立 p50 / p95**・
    **名前解決ミス / 秒 とエラー / 秒**・**スレッド / fd**・メモリ/ディスクのグラフ、ホスト別統計、
    **最近のエラー (直近 20)** と **いまの接続 (上位 50)** の表 (どちらも 5 秒ごと)、
    URL の照会と削除、全消去)、`/status`, `/history` (JSON)、`/metrics` (Prometheus 形式)
  - **`/healthz` (本当の健康診断)**: `{"ok":bool,"checks":{...}}` の**軽い JSON** (1 KiB 弱) で、
    検査が 1 つでも偽なら **`503 Service Unavailable`** を返します (Pterodactyl やモニタが 200 / 503 で
    判断できるように。以前は `/status` の写しで、いつでも 200 でした)。検査は 6 つ:
    `listening` (待ち受けが生きている = この応答が届いている)、`fds` (開いている記述子が `max_fds` の 90% 未満)、
    `connections` (いまの接続数が `PROXY_MAX_CONNS` 未満。**この `/healthz` 自身の 1 本は除きます** —
    上限に当たっている最中でも T13.2 の「上限 + 4 本」の枠でこの応答は届くため)、
    `state_file` (状態ファイルの書込エラーが直近 5 分で増えていない)、
    `listen_overflows` (受け入れ待ち行列が直近 5 分で溢れていない。**ネットワーク名前空間ごとの数**なので、
    同じ名前空間に他の待ち受けがあるとそちらの溢れも数えます。コンテナなら実質このプロキシのぶんです)、
    `resolver` (名前解決の最後のミスが 2 秒未満 = リゾルバが死んでいない)。
    **この環境で調べられないものは `null`** で、`ok` の判定に入れません
    (Linux 以外・`/proc/net` の無いコンテナ・状態ファイル無し・まだ名前解決をしていない・
    履歴スレッドが動いていない `--lite` / `PROXY_STATS_PERSIST=off`)。問い合わせ (`?sort=` など) は読みません
  - **`capabilities` (この環境で何が読めるか)**: `/status` と `/config` の `capabilities` に
    `{"proc_syscall":true,"tcp_info":true,"cgroup_cpu":true,"cgroup_pressure":true,"ipv6_route":true,"resolver_ms":9,"home_writable":true,"checked_at":1758...}`。
    統計の `null` が「無かった」のか「読めなかった」のかを先に答えるためのもので、
    `proc_syscall` は `/proc/self/task/<tid>/syscall` (スレッドの状態)、`tcp_info` は待ち受けソケットへの
    `getsockopt(SOL_TCP, TCP_INFO)` (カーネルの RTT と再送)、`cgroup_cpu` / `cgroup_pressure` は自分の cgroup の
    `cpu.stat` / `cpu.pressure` (CPU の絞りと PSI)、`ipv6_route` は `/proc/net/ipv6_route` の既定経路、
    `resolver_ms` は `example.com` を 1 回引くのにかかった ms (締め切り 2 秒、失敗は `null`)、
    `home_writable` は `$HOME` に書けるか (状態ファイルの置き場) を見ます。
    **判定は起動時 1 回と 1 時間ごと**で (`.env` の監視スレッドのついで。要求の経路では何もしません)、
    `checked_at` がその時刻です。Linux 以外では `/proc` も cgroup も無いので `false` になります
  - **個票 (集計では読めない「誰が・いつ・なぜ」。T13.4)**: `/errors?n=100` で直近のエラー
    (時刻・`connect` / `forward` / `canary`・宛先・原因・名前解決 ms・接続 ms・返した状態コード・接続元。
    500 件の環状、プロセスのメモリだけ)。
    **403 で拒否した要求もここに入ります** (原因は `acl` / `blocklist` / `connect_port` / `local`、状態コード 403)。
    403 は 5xx ではないので `/status` の `errors_by_cause` (8 つの原因) には乗らず、
    集計では `hosts[]` の `blocked` に数えるだけです。**誰が何を拒否されたか**はこの個票でだけ読めます、
    `/connections` でいま開いている接続の一覧 (接続 id・接続元・宛先 (CONNECT はトンネルの相手、
    keep-alive の HTTP は**最初の要求の宛先**。どちらも接続あたり 1 回しか書きません)・`connect` / `http`・
    状態 `relaying` / `parked` / `reading` / `serving` / `queued`・開始からの秒・転送バイト・記述子の数。`--lite` では空)、
    `/dns?sort=age|host|misses&limit=300` で名前解決の表の中身 (ホスト・アドレス・解決からの秒・残り TTL・
    最後に使ってからの秒・勝った族・負のキャッシュなら理由・裏で引き直し中か・**warm か (`warm`) と
    次に裏で引き直すまでの秒 (`next_refresh_secs`)**・OS に問い合わせた回数)、
    `/log?n=200` で **warn 以上**の直近の行 (1,000 行の環状、1 行 256 B まで。
    `info` のアクセスログは写しません — 熱い経路を重くしないため。コンソールが流れて消える環境向け。
    個票のファイルには 1 行 219 B まで残します)、
    `/hosts?sort=requests|errors|dns|slow&limit=200` (最大 1,000) で `.rrd` にある**全ホスト**を `/status` の `hosts[]` と同じ形で
    (`/status` の上位 50 は変えません。`scripts/status-diff.py` がこの JSON もそのまま読みます)、
    **`/clients?sort=requests|recent|targets|literal&limit=200`** (最大 1,000) で**全接続元**の個票 (T14.7)。
    `/status` の `clients[]` と同じ欄に加えて `first_seen` (初めて見た時刻。`0` はこの起動より前から居る)、
    `agents` (見た `User-Agent` 最大 4 種・先頭 128 バイト。**拾うのは接続の最初の要求だけ**) と `agents_dropped`、
    `distinct_targets` (宛先ホストの種類。最大 256 で頭打ちになり、そのときは `distinct_targets_capped` が `true`)、
    `ports` (使ったポートと要求数、最大 8 種) と `ports_other`、`literal_targets` (IP リテラル宛ての要求数 =
    名前を引かずに繋いでいる数)、`nonstandard_ports` (443 / 80 以外への要求数)、
    `rejected` (`PROXY_MAX_CONNS_PER_CLIENT` に当たって断った本数。T14.13) が出ます。
    **`"persisted": false`** は「これらの新しい欄は状態ファイルに残らない (再起動で消える)」の意味です
    (`.rrd` の 1 スロット 572 B は既存の 49 項目で 520 B 使っていて、`agents` だけで 4 × 128 B 要るため。
    版を上げると統計を全部捨てることになるので上げていません)。
    **T14.5 の RTT の 4 欄 (32 B) はスロットの余白に入ったので残ります** (552 B。余白は 20 B)
  - **閉じた接続の個票 (T14.4)**: `/recent?n=200&since=<epoch>&client=<ip>&sort=time|slow|bytes` (既定 200、最大 2,000)。
    `/connections` が「いま」しか見せないのに対し、こちらは「**起きたこと**」です。1 件 = 接続 id・開いた時刻 (`at`、epoch 秒)・
    接続元 (`client`)・宛先 (`target`)・種類 (`kind` = `connect` / `http`)・寿命 (`secs`)・要求数 (`reqs`、http だけ)・
    **上り / 下り別のバイト** (`up` / `down`)・**閉じた理由** (`reason`)・最後の応答の状態コード (`status`、http だけ)・
    預かり所にいた合計秒と回数 (`parked_secs` / `parks`)・**段階の ms** (`ms` = `dns` / `connect`。`first_byte` は 0 でなければ)・
    **カーネルの RTT と再送** (`rtt_ms` と `retrans`。どちらも `{"client":…,"origin":…}` で
    `client` = 利用者 → プロキシ、`origin` = プロキシ → 宛先。T14.5)。
    理由は `client_eof` (クライアントが先に EOF) / `server_eof` (宛先が先に EOF) / `idle_timeout` (トンネルの無通信打ち切り) /
    `keepalive_timeout` (次の要求を待ちきれなかった) / `evicted` (上限に当たって席を作るために閉じた。`PROXY_MAX_CONNS`) /
    `limit` (1 接続あたりの要求数の上限) / `error:<原因>` (原因は `/status` の `errors_by_cause` と同じ 8 つ) /
    `shutdown` (その他のプロキシ側の都合) の 8 種類。**書くのは接続の終了で 1 回だけ**で、要求ごとにも中継のバイトごとにも
    何も書きません。2,000 件の環状 (ありふれた 1 件 297 B)。`?since=` は「開いた時刻がこれ以降」、`?client=` は接続元の完全一致、
    `?sort=slow` は確立 (`ms.connect`) の遅い順、`?sort=bytes` は転送の多い順。
    **自分宛て (`/status` `/dashboard` …) だけで終わった接続は残しません** — 監視が 5 秒おきに引くとリングがそれで埋まるためです
    (数は `/status` にあります)
  - **カーネルの RTT と再送 (T14.5)**: Linux では接続が閉じるときに `getsockopt(SOL_TCP, TCP_INFO)` を読み、
    **平滑化 RTT (`tcpi_rtt`) と再送の通算 (`tcpi_total_retrans`)** を残します。「30 ms は RTT か、それとも
    プロキシの待ちか」が初めて切り分けられ、利用者側の回線の質 (再送) も数字になります。
    読むのは**接続の終わりだけ**です: CONNECT トンネルは終わりに両側 (`getsockopt` 2 回)、
    keep-alive の HTTP 接続は終わりにクライアント側 (1 回)、オリジンへのプールの接続は
    **捨てるとき** (期限切れか相手が閉じていたとき) にオリジン側。**要求ごとには 1 度も読みません**。
    出るのは 3 か所: `/recent` の 1 件 (`rtt_ms` / `retrans` の両側)、`/hosts` と `/status` の
    `hosts[]` (オリジン側) と `clients[]` (クライアント側) の `rtt_ms` (`avg` / `min` / `samples`) と `retrans`、
    `/metrics` の `sorahost_rtt_seconds_sum` / `_count` (`{side="client"|"origin"}` の 2 系列だけ。
    **ホスト別は出しません** — 系列が増えすぎるため)。**Linux 以外と `--lite` では読まないので `null` / 0 です**
    (`/recent` の `rtt_ms` はその側が `null`、`hosts[]` / `clients[]` は `"rtt_ms":null`)
  - **山の写真 (T14.6)**: `/bursts?n=50` は、同時接続数が `PROXY_MAX_CONNS × PROXY_BURST_PERCENT`
    (既定 50%) を**下から上に越えた瞬間**に自動で撮った `/connections` の要約です。
    上限に当たったときの動き (暇なトンネルを 1 本閉じる。`PROXY_MAX_CONNS`) が効いているかは
    「山が来たとき」にしか見えませんが、来たときに `/connections` を見ている人はいないので、
    越えた瞬間を機械に撮らせておきます。1 枚 = 撮った時刻 (`at`)・通し番号 (`seq`)・そのときの本数
    (`active`、旗が立った瞬間は `trigger_active`)・上限と閾 (`max_conns` / `threshold`)・
    **接続元ごとの本数** (`clients`、多い順に 16 件 + `clients_other`)・**宛先の上位 10** (`targets` + `targets_other`)・
    状態別 (`states` = `serving` / `reading` / `parked` / `queued` / `relaying`)・種類別 (`kinds`)・
    `evicted_idle` と `rejected_overload` の累計・スレッド数と記述子 (`threads` / `fds` / `max_fds`)。
    **同じ山では 1 枚だけ**で、同時接続数が閾の 80% を下回るまで次の 1 枚は撮りません。
    50 枚の環状 (1 枚 4 KiB 以下)。**撮るのは履歴スレッド** (5 秒周期) で、接続を受けたスレッドは
    閾を越えた瞬間に旗を 1 つ立てるだけなので、accept の経路には比較が 1 回増えるだけです。
    `--lite` と `PROXY_BURST_PERCENT=0` では撮りません。**撮るのは履歴スレッドなので
    `PROXY_STATS_PERSIST=off` (履歴スレッドを起こさない設定) でも撮りません**
  - **起きたことの時系列 (T14.11)**: `/events?n=200&since=<epoch>` は、**プロキシに起きた出来事**を
    1 本の時系列にしたものです (新しい順、既定 200 件、512 件の環状)。数字が動いたときに
    「**そのとき何を変えたか**」を読むための口で、`/status` の `settings` は最後の 1 回しか残さず、
    `/log` は warn 以上なので info の出来事 (再読込・ブロックリストの取得・バラストの増減) が入りません。
    1 件 = 時刻 (`at`、epoch 秒)・種類 (`kind`)・短い説明 (`text`、128 バイトまで)。
    種類は `start` (起動。版と設定の要約) / `reload` (`.env` の再読込。**変わった名前と前後の値**、
    再起動が要る項目はその旨) / `blocklist` (一覧を組み直した。件数と取得の成否) / `ipv6`
    (IPv4 優先への切替と解除) / `pressure` (メモリの圧迫の検知と解消) / `ballast` (先行確保が
    ±64 MiB 以上動いた) / `state_file` (状態ファイルの書込エラー。**最初の 1 回だけ**) / `evict`
    (上限に当たって暇なトンネルを閉じた。**1 時間に初めて起きたときだけ**) / `emfile`
    (accept の失敗。同じく 1 時間に 1 回) / `shutdown` (停止シグナル) の **10 種で固定**です
    (`kinds` にも並びます)。**書くのは稀な経路だけ**で、要求ごとの経路には 1 命令も増えていません。
    `ipv6` / `pressure` / `ballast` の 3 つだけは履歴スレッドの周期 (5 秒) で状態の変わり目を拾うので、
    時刻は最大 5 秒遅れ、`--lite` と `PROXY_STATS_PERSIST=off` (履歴スレッドを起こさない設定) では
    残りません。残りの 7 種は `--lite` でも残ります。**メモリだけ**なので再起動で消えます
    (512 件で 87 KiB、応答は 256 KiB 以下)

  - **1 要求で全部取る (T14.4)**: `/snapshot` は上の口を **1 つの JSON** にまとめて返します
    (`status` / `status_errors` / `status_dns` / `history` (`5` / `60` / `3600`) / `dns` / `errors` /
    `connections` / `recent` / `hosts` / `clients` / `bursts` / `events` / `log`。何が入っているかは `parts` に並びます)。
    デプロイ先の様子を見るのに 17 本の URL を手で叩いていたのを 1 回で済ませるための口で、
    **組み立ては同じプロセス内の関数呼び出し** (自分へ HTTP で繋ぎ直さないので、接続を 17 本増やしませんし、
    上限に当たっている最中でも取れます)。上限は **4 MiB** で、越えたら `recent` → `log` → `history.5` の順に
    `null` へ落として `dropped` に名前を出します。保存して読むのは `scripts/collect-deployed.sh` です
  - **canary (利用者の要求が無い時間帯も待ちを測る。T14.10)**: プロキシ自身が **60 秒に 1 回**
    (`PROXY_CANARY_SECS`)、`PROXY_CANARY` で決めた宛先へ **名前解決 → TCP 接続 → 即 `close`** だけを行い、
    その時間を残します。**TLS も HTTP も送りません** (相手に届くのは 1 分に 1 回の名前解決と SYN / FIN だけです。
    利用者と同じ Happy Eyeballs を通るので、A と AAAA の両方を持つ相手には 250 ms ずらして
    もう 1 本 SYN が出ることがあります)。
    平常時の CONNECT 確立 p50 は利用者の要求があった時間帯だけの値なので、深夜や利用者が居ない日は 1 点も無く、
    「遅かったのはプロキシか、回線か、利用者の端末か」が切り分けられませんでした。canary の値が利用者の値と
    合っていれば回線 (またはリゾルバ)、合っていなければ利用者側、と読めます。
    最後の 1 回は `/status` の `canary` (`mode` / `secs` / `runs` / `failures` / `at` / `host` / `dns_ms` /
    `connect_ms` / `error`)、`/metrics` の `sorahost_canary_seconds{stage="dns"|"connect"}` (最後の値。
    1 回も回っていなければ 1 行も出しません。**失敗した回は届かなかった段階が 0 になる**ので、
    成否は `/status` の `canary.error` と `canary.failures`、`/errors` で見てください)。時系列は **`/history?res=5|60` の `canary`** で、
    `{"keys":["t","canary_dns_ms","canary_connect_ms","canary_host"],"samples":[[…]]}` という
    **別の配列**です (既存の `keys` / `samples` は 1 列も変えていません。`.rrd` の標本には書かないので
    再起動で消えます。窓は 5 秒 × 720 と 60 秒 × 1,440)。失敗は `/errors` に `kind: "canary"` で 1 件だけ残り、
    利用者に返したエラーの集計 (`errors_by_cause`) には混ざりません。
    名前解決は**名前解決の表を通さず** OS に直接聞くので (リゾルバの実力を測るため)、`/status` の
    `dns.hits` / `dns.misses` にも keep-warm にも影響しません。TCP 接続は利用者と同じ経路
    (Happy Eyeballs と IPv4 優先の学習) を通ります。回すのは **`canary` スレッド 1 本**だけで、
    利用者の要求の経路には 1 命令も足していません。**履歴スレッドが動いているときだけ回ります**
    (`--lite` と `PROXY_STATS_PERSIST=off` では履歴スレッドごと止まるので canary も回りません)
  - **再起動をまたぐか (`persisted` / `restored`)**: `/recent` `/errors` `/bursts` `/events` `/log` の 5 つは、
    5 秒ごとに `$HOME/.rust-http-proxy.recent` (固定 4 MiB、統計の `.rrd` とは別のファイル) へ新しい分だけ追記され、
    次の起動で読み戻されます。**`"persisted": true|false`** がその可否 (`PROXY_STATS_PERSIST=off` と、
    ファイルが開けなかったときは `false` = 「この口の中身は再起動で消える」)、**`"restored": N`** が
    **再起動前から引き継いだ件数**です。ファイルの中は 5 本の環状の領域で、
    閉じた接続 4,096 件 (1 件 512 B) / エラー 2,048 件 / 山の写真 128 枚 / 出来事 512 件 / ログ 3,568 行
    (どれもメモリのリングと同じか多く持つので、
    何度か再起動しても前の版の個票が残ります)。書くのは**履歴スレッド**と停止シグナルだけで、
    1 周期に書くのは 64 KiB まで (それを越えた分は古い方から落とし、`/status` の `state_file.recent.dropped` に出ます)。
    `/history` の `closed` (分布) と `/connections` (いまの接続) と `/dns` (表) は**メモリだけ**のままです
  - どの個票も応答は 256 KiB 以下 (件数の上限とは別にバイト数でも打ち切り、切ったら `"truncated": true`)。
    **どれも認証なしで見えます** (このプロキシの方針。`/purge` と同じ)。接続元の IP と宛先ホストが並ぶので、
    公開ポートに出すなら ACL や到達制御で守ってください
    (`PROXY_ALLOW_CLIENTS` で接続元を絞れば、これらの個票にも届きません)。
    個票に入れるのは**接続元 IP・宛先のホスト:ポート・時刻・数字だけ**です。
    **`/clients` の `User-Agent` (先頭 128 バイト) が入る唯一のヘッダー**で、URL のパスや問い合わせ文字列、
    本文、その他のヘッダーは 1 つも記録しません
  - **カーネルと cgroup の窓 (バーストのときカーネル側で何が起きていたか。T14.12)**:
    プロキシの統計には残らない事象を、**5 秒の標本のときだけ** `/proc` と `/sys/fs/cgroup` を読んで
    メモリ上の窓 (5 秒 × 720 = 1 時間 と 60 秒 × 1,440 = 1 日。1 標本 200 B なので約 420 KiB、
    環状バッファの伸び方しだいで最大 600 KiB) に残します。**要求ごとには 1 回も読みません**。
    - **受け入れ待ち行列の溢れ** (`listen_overflows` / `listen_drops`)。溢れるとクライアントは SYN を
      1〜3 秒後に再送するので、**プロキシから見ると「遅い接続」としてすら残りません**
      (`/proc/net` の数はどれも**ネットワーク名前空間ごと**で、このプロキシの待ち受けのぶんだけではありません)
    - **再送** (`retrans_segs` / `syn_retrans` / `tcp_timeouts` / `abort_on_timeout`)
    - **TIME_WAIT の本数** (`time_wait`) と TCP ソケットの数 (`sockets_inuse` / `sockets_alloc` / `curr_estab`)。
      手元で CONNECT のベンチを回すと `tcp_max_tw_buckets` (この機械は 32,768) に張り付きます
    - **cgroup の CPU の絞り** (`cpu_nr_throttled` / `cpu_throttled_usec` と `cpu.max` の `quota_cores`)。
      CPU 上限を持つコンテナで「自分が遅い」のか「絞られて待たされた」のかが分かれます
    - **PSI** (`psi_cpu_some_avg10` など。直近 10 秒のうち、その資源を待って進めなかった時間の割合 %)。
      隣のコンテナに CPU を取られている時間が読めます
    - 一緒に取るもの: 名前解決のミスの回数とミス 1 回の ms、状態ファイルの書込エラー

    累計のものは**増分** (その 5 秒に何回起きたか)、値のものは値で入ります (1 分へ畳むときは
    増分は足し合わせ、値と PSI は**最大**)。最新の値と累計は `/status` の `kernel`
    (`tcp` / `cgroup_cpu` / `psi` / 直近 5 分の `last_5m`)、時系列は `/history` の `kernel`、
    Prometheus では `sorahost_kernel_listen_overflows_total` などの累計と
    `sorahost_kernel_time_wait` / `sorahost_cgroup_cpu_throttled_seconds_total` /
    `sorahost_psi_some_avg10{resource="cpu"|"memory"|"io"}` です。
    **読めない源は `null`** (Linux 以外、`/proc/net` の無いコンテナ、cgroup v1、PSI 無しのカーネル)。
    `.rrd` (状態ファイル) には書かないので**再起動で消えます** (標本のレコードに余白が 4 B しか無いため)。
    履歴の収集スレッドが動いていない `--lite` / `PROXY_STATS_PERSIST=off` では窓は空 (`kernel` は `null`) です
  - **日次の要約 (永久に残る。T14.20)**: `/daily?n=365` (既定 1 年、最大 4,096 日) は
    `$HOME/.rust-http-proxy.daily.jsonl` の中身を**古い順**にそのまま返します。`/history` は 30 日で消えますが、
    こちらは**「いつから遅くなったか」「デプロイの前後で何が変わったか」を年単位で**追うためのもので、
    1 日 1 行 (ありふれた 1 行 336 B、上限 512 B) なので 1 年で約 120 KB です。
    1 行 = `day` (UTC の日付) ・`t` (その日の 0 時の epoch 秒)・`secs` と `samples` (**その日をどれだけ見ていたか**。
    再起動した日は 1 日ぶんに満たない)・`requests`・`bytes`・`connects`・`connect_p50_ms` / `connect_p95_ms`・
    `dns_misses` / `dns_per_connect` / `dns_miss_ms`・`errors`・`bursts` (山の写真の枚数)・`active_max`・
    `evicted_idle`・`rss_max` / `rss_avg`・`version` (その日動いていたバイナリの版)。
    累計 (要求数・バイト・追い出し・山) は**日の境目の値の差**、区間の値 (確立時間の分布・エラー・名前解決) は
    **その日の標本の足し合わせ**、ゲージ (同時接続数・RSS) は**最大と平均**です。
    **書くのは履歴スレッドが UTC の日付をまたいだ瞬間の 1 回だけ** (要求の経路の費用は 0)。
    ファイルは**追記のみ**で上限 **2 MiB** (越えたら古い行から捨てる = 約 11 年ぶん)、
    起動時に最後の行の日付を見るので**同じ日に 2 回起動しても 1 行のまま**です。
    `PROXY_STATS_PERSIST=off` では 1 行も書きません (`/daily` の `path` が `null`)。
    応答は他の個票と同じく 256 KiB 以下 (切ったら `truncated`)
  - **RSS の内訳** (`/status` の `memory`。T14.21): 256 MiB のコンテナで「RSS が何でできているか」
    (ヒープの断片・スレッドのスタック・キャッシュ・記録のリング) を 1 枚から読むためのものです。
    **読むのは `/status` に来たときだけ**で、要求の経路には 1 命令も増えません
    (`mallinfo2(3)` はアリーナの鍵を順に取るので数 us かかります)。
    - `heap_used` / `heap_free` / `mmap` は glibc の `mallinfo2(3)` の `uordblks` / `fordblks` / `hblkhd`。
      **glibc 2.33 以上でだけ読めます**: リンク時に決め打ちせず `dlsym(RTLD_DEFAULT, "mallinfo2")` で
      実行時に探すので、musl や古い glibc、Linux 以外では落ちずに 3 つとも `null` になります
    - `rss` はキャッシュのプローブ (既定 1 秒ごと) が読んだ値で、**同じ応答の
      `cache.system.process_rss_bytes` と同じ値**です (1 枚の中に食い違う RSS が 2 つ並ばないように)。
      プローブが止まっている (`PROXY_CACHE_PROBE_SECS=0` / キャッシュ無効 / `--lite`) ときだけ、
      その場で `/proc/self/status` を読みます
    - `stacks_estimate` は**予約**の合計です (接続スレッド `conn` は 256 KiB、それ以外は Rust の既定 2 MiB)。
      実際に触ったページとの差は出せないので、RSS のうちスタックのぶんはこれより小さくなります
    - `cache_memory` はキャッシュがヒープに持っている量 (`cache.memory` の `used_bytes` +
      先行確保の `reserved_bytes`)
    - `rings` は記録のリングが**満杯になったときの見積もり** (バイト): `recent` (2,000 件)、
      `errors` (500 件)、`bursts` (50 枚)、`log` (1,000 行)、`events` (512 件)、
      `history` (5 秒 × 720 + 60 秒 × 1,440 + 1 時間 × 720 の標本と、閉じた接続の窓) と、
      その合計 `total` (この機械では 3.5 MiB)。
      いま何件入っているかは `/recent` や `/errors` の `total` を見てください
    - `arenas` は `PROXY_MALLOC_ARENAS` で実際に掛けた `M_ARENA_MAX` (`0` = glibc の既定のまま)

    **足して RSS になる形ではありません**: `heap_free` は「アロケータが返していない」だけで常駐しているとは
    限らず (`MADV_DONTNEED` 済みのページ)、`mmap` も確保しただけで触っていないページは常駐しません。
    「RSS のうち説明できる部分を上から並べたもの」として読んでください。
  - `PURGE <url>` / `/purge?url=<url>` / `/purge?all=1` でキャッシュを消す、`/lookup?url=<url>` でエントリの状態を見る
  - `/history?res=5|60|3600` で 1 時間 / 1 日 / 30 日の履歴。標本の後ろに **`closed`** が付きます (T14.6):
    その窓に**閉じた接続**の分布で、閉じた理由 8 種の件数 (`reasons`。`/recent` の `reason` と同じ綴り。
    `error:*` は `error` 1 つにまとめます)・寿命の 12 段 (`life`、境目は `life_bounds_secs`。
    `PROXY_KEEPALIVE_SECS` の 15 秒と `PROXY_TUNNEL_IDLE_SECS` の 300 秒が境目にあります)・
    上り / 下りバイトの 12 段 (`up` / `down`、境目は `byte_bounds` = 1 KiB から 4 倍ずつ)・
    寿命と預かり秒とバイトの合計が並びます。**標本 (`samples`) の形と読み方は変えていません** (別の配列です)。
    5 秒 × 720 と 60 秒 × 1,440 を**メモリだけ**に持ちます (`res=3600` は `null`、状態ファイルには書きません。
    標本 1 本の余白が 4 B しか無いため)。件数 0 の窓は出しません (行の先頭に窓の始まりの時刻があります)。
    `/blocklist?host=<h>` でブロックリストの判定、
    `&action=block|allow|clear[&ttl_secs=N]` で一時的な上書き (既定 24 時間、`0` で無期限。状態ファイルに 256 件まで残る)
- **タイムアウト制御**:
  - `PROXY_TIMEOUT_SECS` による接続および読み書きタイムアウト制御

## コマンドライン引数

環境変数を書かなくても、よく使う設定は引数で渡せます (優先順位は **引数 > `$HOME/.env` > 環境変数**)。

```
  -p, --port <PORT>    待ち受けポート        (SERVER_PORT)
      --bind <ADDRS>   待ち受けアドレス      (PROXY_BIND、カンマ区切り)
      --no-cache       キャッシュを止める    (PROXY_CACHE_ENABLED=off)
      --quiet          警告以上だけ出す      (PROXY_LOG_LEVEL=warn)
      --lite           最速の素通しプロファイル (PROXY_PROFILE=lite)
      --check          起動せずに環境と効く設定を出して終了 (下記)
  -h, --help           使い方を出して終了 (終了コード 0)
  -V, --version        版を出して終了
```

`--port=3128` の形式も使えます。知らない引数は使い方を出して終了コード 2 になります。

`--check` は**起動せずに**「この環境で何が読めるか」(`capabilities` の 7 項目) と
「この設定で起動したら何が効くか」(`/config` と同じ全 `PROXY_*` / `SERVER_*` と出どころ) を印字して終わります。
Pterodactyl のように触れないコンテナで、**起動前の確認**と**統計の `null` の理由の切り分け**に使えます
(他の引数も一緒に効くので `--check -p 3128 --lite` のように「その設定なら何が効くか」も見られます)。
終了コードは `capabilities` の 6 項目 (`resolver_ms` を除く) が全部読めれば **0**、1 つでも読めなければ **1** です
(名前解決を外すのは、リゾルバが遅い環境でもプロキシとしては動く — そしてそれ自体が測りたい数字 — ため)。

```
$ rust-http-proxy --check
rust-http-proxy 0.1.0+28f9064 --check
settings file: /home/container/.env (3 variables)

capabilities (what this environment lets the proxy read):
  [ok] proc_syscall     /proc/self/task/<tid>/syscall (per-thread state)
  [ok] tcp_info         getsockopt(SOL_TCP, TCP_INFO) (kernel RTT and retransmits)
  [ok] cgroup_cpu       cgroup cpu.stat (CPU throttling)
  [ok] cgroup_pressure  cgroup cpu.pressure (PSI: waiting for the CPU)
  [ok] ipv6_route       a default route in /proc/net/ipv6_route
  [ok] home_writable    $HOME is writable (statistics file, blocklist)
  [ok] resolver_ms      9 ms for one lookup (not part of the exit code)

settings (source, name, effective value):
  default   SERVER_PORT                    8080
  env_file  PROXY_DNS_TTL_SECS             30
  ...

check: ok (everything this proxy reads is readable)
```

`-V` が出す版は `0.1.0+144b992` のように **`Cargo.toml` の版 + ビルドしたときの git の短いハッシュ**です
(作業ツリーに未コミットの変更があれば `0.1.0+144b992-dirty`)。`git` や `.git` の無いところでビルドすると
`0.1.0+unknown` になります (ビルドは通ります)。同じ文字列を**起動ログの `rust-http-proxy ... listening on ...` の行**と
**`/status` のトップレベルの `version`** にも出すので、デプロイ先でどのコミットが動いているかが分かります。

## プロファイル

`--lite` (= `PROXY_PROFILE=lite`) は「認証なし・手軽・最速」の素通し設定です。キャッシュ・統計の永続化・
ブロックリストの取得を止め、ログを `warn` にします (個別の環境変数を明示すればそちらが勝ちます)。
`/dashboard` は 1 行の `lite mode: ...` を返し、起動ログの 1 行目に `profile: lite` が出ます。
起動直後のスレッドは 4 本 (待ち受け + `env-reload` + `shutdown` + `idle-watch`) だけです
(`PROXY_PARK_IDLE=off` なら 3 本)。

```bash
./rust-http-proxy --lite -p 8080
```

## 環境変数

| 環境変数名 | デフォルト値 | 説明 |
|---|---|---|
| `SERVER_PORT` | `8080` | プロキシが待受を行うポート番号 (Pterodactyl が自動設定) |
| `PROXY_BIND` | 自動 (`::` + `0.0.0.0`) | 待ち受けアドレスのカンマ区切りリスト (例: `127.0.0.1,[::1]`)。未設定ならデュアルスタックで自動 |
| `PROXY_IPV6` | `on` | IPv6 を使う (待ち受けと AAAA での接続)。`off` で `0.0.0.0` のみ・A レコードのみ。`on` のままでも、IPv6 が黙って落ちる環境では自動で IPv4 を先に試す (上記。確実に避けたいなら `off`) |
| `SERVER_MEMORY` | なし | コンテナのメモリ割当 (MB)。Pterodactyl が自動設定し、メモリキャッシュの上限として尊重される |
| `PROXY_DISK_QUOTA_MB` (別名 `SERVER_DISK`) | なし | コンテナのディスク割当。**Pterodactyl はこれを渡してくれない**ので、egg 変数として設定する。MB 数 = パネルの Disk Space、`0` = 無制限、`auto` = `df -B1 /home/container` の total を割当とみなす (下記)。Pterodactyl で未設定ならディスクキャッシュは 512 MiB 固定・先行確保なし |
| `PROXY_ALLOW_HOSTS` | なし (全許可) | 接続許可ホストのカンマ区切りリスト (例: `*.example.com,api.github.com`) |
| `PROXY_DENY_HOSTS` | なし | 接続拒否ホストのカンマ区切りリスト (例: `bad.com,*.blocked.org`) |
| `PROXY_TIMEOUT_SECS` | `30` | 接続およびデータ転送タイムアウト（秒）。`0` で**無期限** (`PROXY_TUNNEL_IDLE_SECS` と同じ意味): 何も送ってこないクライアント接続を閉じず、オリジンへの接続と読み書きにも締め切りを置かない (OS 既定に任せる)。`PROXY_KEEPALIVE_SECS` (要求と要求の間) と `PROXY_TUNNEL_IDLE_SECS` は別に効く |
| `PROXY_KEEPALIVE_SECS` | `15` | クライアント接続を次の要求まで待つアイドル時間 (秒)。`0` で 1 接続 1 要求。**待つ長さとは別に、1 本の接続で 1,000 要求を捌いたらその接続は閉じます** (下記) |
| `PROXY_ORIGIN_POOL` | `64` | オリジンへのアイドル接続をホストごとに保持する本数。`0` で再利用しない。少ないと同時要求数が多いときに張り直しが増える (実測: 8 本だと 64 並列で p99 が 40 ms 台、64 本なら 10 ms 前後) |
| `PROXY_PARK_IDLE` | `on` | アイドルな keep-alive 接続と、両方向とも暇な CONNECT トンネルをスレッドから外し、1 本の監視スレッド (epoll) に預ける。`off` で「1 接続 = 1 スレッドが専任」の動きに戻る。Linux 専用 (それ以外では自動的に無効)。実測: 暇な接続 2,000 本でスレッド 2,004 → 25 本、RSS 68.0 → 25.2 MB、暇なトンネル 5,000 本でスレッド 5,005 → 68 本、RSS 93.9 → 29.9 MB。忙しいときの CPU/要求とシステムコール数は変わらない |
| `PROXY_PARK_GRACE_MS` | `3` | 預ける前に同じスレッドで待ってみる時間 (ミリ秒。CONNECT トンネルは下限 100 ms)。続けて要求が来る忙しい接続に、預ける/戻すの往復 (`epoll_ctl` 2 回 + ワーカーの受け渡し、実測 18 µs/要求) を払わせないための猶予。読み取りタイムアウトをこの長さにして空振りを「暇だ」と解釈するので、システムコールは増えない。`0` なら猶予なしで即座に預ける |
| `PROXY_MALLOC_ARENAS` | `8` | malloc のアリーナ数の上限。`0` で glibc の既定 (コア数 × 8) のまま。接続ごとにスレッドが増えるため、既定のままだとアイドル接続を多く抱えたときに使われないアリーナが RSS に居座る (実測: 2,000 本のアイドル接続で 80.5 → 26.8 kB/接続)。代償は高並列でのロック競合 (実測: 64 並列で CPU/要求 +4%、8 並列では差なし)。実際に掛かった値は `/status` の `memory.arenas` に出ます |
| `PROXY_ORIGIN_POOL_TOTAL` | `256` | アイドル接続の全ホスト合計の上限。多数のホストへ行くときに `ホスト数 × PROXY_ORIGIN_POOL` まで増えないようにする |
| `PROXY_DNS_TTL_SECS` | `60` | 名前解決の結果を保持する秒数。`0` で毎回解決 (それでも 1 要求につき 1 回。`/status` の `dns.misses` がその回数)。**直近この秒数以内に使われた名前は期限の 3/4 を過ぎたところで裏で 1 回だけ引き直す**ので、使い続けているホストはミスになりません (`/status` の `dns.refreshes`)。解決に失敗したら 1 時間以内の古い結果を使う。`.env` で即時反映 |
| `PROXY_DNS_NEGATIVE_SECS` | `60` | 名前解決の**失敗**を覚えておく秒数 (`0` で覚えない)。覚えている間は OS に問い合わせずに同じエラーを返します (`/status` の `dns.negative_hits`)。引けない名前 1 つで 2 秒待たされることがあるための蓋で、古い答えが 1 時間以内にあるときはエラーより古い答えを優先します。`.env` で即時反映 |
| `PROXY_DNS_WARM_SECS` | `900` | **keep-warm**: 直近この秒数に **2 回以上**使われた名前 (warm) は、使われていなくても `PROXY_DNS_TTL_SECS` の 3/4 ごとに裏で引き直し続けます (`0` で無効 = 直近 TTL 内に使われた名前だけ 1 回先回りする動きに戻る)。TTL (60 秒) を熱さの物差しにすると、間隔が TTL より長いホストは 1 つも救えません (デプロイ先の主要 3 件は 2〜10 分間隔で、ミス率は 0.31 / 0.76 / 1.00 でした)。1 回だけ使われた名前は warm にしません (引き直しても二度と来ない)。答えを持っている名前だけが warm になります (引けない名前は負のキャッシュの担当)。同時に warm でいられるのは **32 件**まで (最後の使用がいちばん古いものから外す) で、最後の使用からこの秒数を過ぎたら引き直しを止めます。最悪でも 32 件 ÷ 45 秒 ≈ 0.7 回/秒。いま warm な名前の数は `/status` の `dns.warm`、名前ごとの予定は `/dns` の `warm` / `next_refresh_secs`。`.env` で即時反映 |
| `PROXY_CANARY` | `auto` | **canary** (利用者の要求が無い時間帯も待ちを測る): `PROXY_CANARY_SECS` 秒に 1 回、宛先へ**名前解決と TCP 接続だけ**を行って時間を残します (握ったらすぐ閉じ、TLS も HTTP も送りません。相手に届くのは 1 分に 1 回の SYN と FIN だけ)。`auto` は**直近 1 時間で最も要求の多い CONNECT の宛先** (`/status` の上位ホストの先頭で、最後に使ってから 1 時間以内のもの) を毎周期選び直します (1 件も無ければ何もしません = 誰も使っていないプロキシは誰にも繋ぎません)。`off` で止める。ホストをカンマ区切りで書けばその全部 (最大 8、ポートを省くと 443)。結果は `/status` の `canary`、`/history?res=5|60` の `canary` の配列、`/metrics` の `sorahost_canary_seconds{stage="dns"|"connect"}`、失敗は `/errors` に `kind: "canary"` で 1 件。名前解決は表を通さず OS に聞くので利用者の `dns` の数字には混ざりません。**履歴スレッドが動いているときだけ回ります** (`--lite` と `PROXY_STATS_PERSIST=off` では回りません)。`.env` で即時反映 |
| `PROXY_CANARY_SECS` | `60` | canary の周期 (秒、最小 1)。**試験で短くするための口**で、運用では触りません (60 秒に 1 回・1 宛先 1 本なら、相手にも自分にも負荷はありません)。`.env` で即時反映 |
| `PROXY_BLOCKLIST_FILE` | なし | ドメインのブロックリスト (hosts 形式 `0.0.0.0 host` または 1 行 1 ドメイン)。親ドメインの登録で子ドメインも落ちる。`.env` で即時反映、ファイルの更新は 1 分以内に反映 |
| `PROXY_BLOCKLIST_URL` | なし | ブロックリストを取りに行く URL (StevenBlack の hosts など)。`$HOME/.rust-http-proxy.blocklist` に保存して再起動後も使う。ファイルと両方あれば和集合 |
| `PROXY_BLOCKLIST_REFRESH_SECS` | `86400` | URL を取り直す間隔 (最小 60)。失敗したら 10 分後に再試行し、その間は前の一覧を使う |
| `PROXY_BLOCKLIST_EXEMPT` | なし | ブロックリストの対象外にするホストのカンマ区切り (`*.example.com` 可) |
| `PROXY_CONNECT_PORTS` | なし (制限なし) | `CONNECT` を許すあて先ポート。`443,80,8080-8099` のようにカンマ区切り (範囲可)。ここに無いポートは 403。`.env` で即時反映 |
| `PROXY_ALLOW_LOCAL` | `off` | ループバック (`127.0.0.0/8`, `::1`) とリンクローカル (`169.254.0.0/16`, `fe80::/10`) 宛てのオリジンを許すか。既定では 403 にしてクラウドのメタデータ (`169.254.169.254`) 経由の SSRF を防ぐ。ローカルのサービスへプロキシしたいときだけ `on`。`.env` で即時反映 |
| `PROXY_ALLOW_CLIENTS` | なし (全許可) | **受ける接続元**のカンマ区切りリスト (`1.2.3.4,10.0.0.0/8,2001:db8::/32`。1 つの IP は `/32` `/128` と同じ)。ここに無い相手は **accept した直後に、要求を 1 バイトも読まずに閉じます** (応答も返しません)。**内部エンドポイントも含めて閉じる**ので、公開ポートで `/status` や `/clients` の個票が見られることもありません。`PROXY_MAX_CONNS` の 「上限 + 4 本」の枠より**前**で判定します。断った数は `/status` の `rejected_client_acl` と `/metrics` の `sorahost_rejected_client_acl_total`。v4-mapped IPv6 (`::ffff:1.2.3.4`) は IPv4 として照合するので、デュアルスタックで 待ち受けていても `1.2.3.4` の 1 行で書けます。書式が違う項目は読み飛ばします (起動ログの `allowed clients:` に実際に読めた項目が出るので、書き損じはそこで分かります)。**宛先の `PROXY_ALLOW_HOSTS` / `PROXY_ALLOW_LOCAL` とは無関係**で、**認証でもありません** (同じアドレスから来られれば誰でも通ります)。`.env` で即時反映 (次に受ける接続から) |
| `PROXY_ENDPOINTS_READONLY` | `off` | `on` にすると内部エンドポイントの**書き換える口だけ**を `405 Method Not Allowed` で断ります (`/purge?url=` / `/purge?all=1` / `PURGE <url>` / `/blocklist?...&action=block|allow|clear`)。読む口 (`/status` `/healthz` `/history` `/daily` `/metrics` `/hosts` `/clients` `/errors` `/connections` `/recent` `/bursts` `/events` `/dns` `/log` `/lookup` `/proxy.pac` `/dashboard` と、判定だけの `/blocklist?host=`) は今までどおりです。**認証ではありません** (読める人は読めます)。公開ポートに出していて「誰でもキャッシュを消せる」のだけを止めたいときのつまみです。`.env` で即時反映 |
| `PROXY_TUNNEL_IDLE_SECS` | `300` | CONNECT トンネルのアイドル打ち切り。双方向とも無通信がこれだけ続いたら両側を閉じる (`PROXY_PARK_IDLE=on` なら、預かり所が期限を見て引き上げる)。`0` で無期限。`.env` で即時反映 |
| `PROXY_PROFILE` | なし | `lite` で最速の素通しプロファイル (`--lite` と同じ)。キャッシュ・統計の永続化・ブロックリストを止め、ログを `warn` にする |
| `PROXY_MAX_CONNS` | `auto` | 同時に受ける接続数の上限。上限に当たったら、まず**預かり所の暇な CONNECT トンネルを最古から 1 本閉じて**席を作り、その接続を受ける (閉じた数は `/status` の `evicted_idle` と `/metrics` の `sorahost_evicted_idle_total`。**暇な keep-alive 接続は閉じない** — 次の要求を待っているだけなので、閉じると入れ違いで届いた要求を取りこぼすため)。閉じるものが無い (トンネルが全部中継中、または預かり所が空) ときは、スレッドを起こさず `503 Service Unavailable` + `Retry-After: 1` を返して閉じる。ただし**自分宛て (`/status` `/metrics` などの内部エンドポイント) は上限 + 4 本まで受ける**: accept の時点では要求が読めないので、4 本までは受けて要求行と `Host` を読み、自分宛てなら普通に応答、それ以外は 503 で閉じる (上限に当たっている最中でも監視が取れるようにするため。この枠で受けた接続は要求行が 2 秒来なければ 503 で閉じる)。`auto` は記述子の上限から `min(4096, (RLIMIT_NOFILE の soft − 予備 64) ÷ 4)` (1 接続が最悪で使う記述子は クライアント 1 + オリジン 1 + 素通しのパイプ 2 = 4 本。`ulimit -n` が 1024 の環境なら 240、4096 なら 1008)。記述子が余っていても 4096 で頭打ちにするのは、上限が fd 以外の資源 (スレッド・RSS) の歯止めでもあるため (同時 5,000 本で RSS 198 MiB の実測)。数値を書けばその値、`0` で無制限。決まった値は起動ログの `max connections:` と `/status` の `max_conns` (`/metrics` は `sorahost_max_connections`) に出る。`.env` で即時反映。断った数は `/status` の `rejected_overload` と `/metrics` の `rejected_overload_total` |
| `PROXY_MAX_CONNS_PER_CLIENT` | `0` (無効) | **1 つの接続元から同時に受ける接続数の上限**。認証なしの公開ポートで、見知らぬ接続元 1 人が `PROXY_MAX_CONNS` (既定 240) を使い切ると**本人が 503 になる**ため、その手前で頭を押さえるつまみです。**認証ではなく公平さの上限**です (同じアドレスから来られれば誰でも通ります)。設定すると accept の直後にその接続元の**いま生きている接続の本数**を数え、上限以上なら `503 Service Unavailable` + `Retry-After: 1` を返して閉じます。断った数は `/status` の `rejected_per_client` と `/metrics` の `sorahost_rejected_per_client_total`、接続元ごとの内訳は `/clients` の行の `rejected`。**自分宛て (`/status` などの内部エンドポイント) は数えません**: accept の時点では要求が読めないので、`PROXY_MAX_CONNS` と同じ「上限 + 4 本」の枠で受けてから要求行を読み、自分宛てなら普通に応答、それ以外は 503 で閉じます (上限に当たっている接続元からでも監視が取れるように)。**数え方**: 数えるのは `/connections` の表と同じ「接続の開始と終了」で ±1 する本数で、鍵は接続元 IP (v4-mapped IPv6 は IPv4 として数えます)。NAT の内側の複数台は 1 人として数えられます。数えるのは**上限を設定している間だけ**で、`0` に戻すと表ごと捨てます (既定の費用は accept ごとの分岐 1 回)。`--lite` でも効きます (`/connections` の行は作らずに本数だけ数えます)。同時に来た数本は上限を少し超えて通ることがあります (数えるのは登録済みの本数のため)。`.env` で即時反映 (次に受ける接続から。あとから入れたときは、そのとき生きている接続から数え直します) |
| `PROXY_BURST_PERCENT` | `50` | 同時接続数が `PROXY_MAX_CONNS` のこの割合を**越えた瞬間**に `/connections` の写真を 1 枚撮って `/bursts` に残す (T14.6)。`0` で撮らない。**同じ山では 1 枚だけ**で、閾の 80% を下回るまで次は撮りません。撮るのは履歴スレッド (5 秒周期) なので、接続を受ける経路に増えるのは比較 1 回だけです。割合を当てるのは `PROXY_MAX_CONNS` だけで、上限の外の枠 4 本 (自分宛て用) は含めません。`PROXY_MAX_CONNS=0` (無制限) と `--lite` では撮りません。**履歴スレッドが撮るので `PROXY_STATS_PERSIST=off` でも撮りません**。`.env` で即時反映 |
| `PROXY_MAX_THREADS` | `auto` | 同時に生きていてよい接続スレッドの上限。上限に達したら**新しいスレッドを起こさず、その仕事を待たせる** (捨てない。空いたスレッドが順に引き取る)。`auto` は `min(PROXY_MAX_CONNS, コア数 × 64 を 128〜512 に収めた値)` で、コア数は `taskset` で絞られていればその数。数値を書けばその値、`0` で無制限 (T10.5 以前の動き)。上限があるのは、預けた接続が一斉に切れたときにスレッドが跳ねないようにするため (暇なトンネル 5,000 本の一斉 close で、上限なしだと一時的に 4,400〜4,700 スレッド・RSS 65 MB、上限 256 なら 260 スレッド・RSS 27 MB)。`.env` で即時反映 (次に受ける接続から効く。**下げても走っているスレッドは殺さず**、仕事を終えたスレッドから順に減ります。`auto` のときは `PROXY_MAX_CONNS` を変えるとこちらも決め直します)。決まった値は起動ログの `max connection threads:` と `/status` の `max_threads` に出る (いまの本数は `/status` の `live_threads` / `idle_threads`、上限に当たって待たせている仕事は `queued_jobs`。`/metrics` にも `sorahost_max_threads` / `sorahost_live_threads` / `sorahost_idle_threads` / `sorahost_queued_jobs` として出る)。**裏側の再検証 (stale-while-revalidate) もこの上限の内側で走ります**が、こちらは待たせず捨てます (`/status` の `revalidations_dropped`) |
| `PROXY_STATS_PERSIST` | `on` | 統計と履歴を `$HOME/.rust-http-proxy.rrd` (固定 4 MiB) に、**個票 (`/recent` `/errors` `/bursts` `/events` `/log`) を `$HOME/.rust-http-proxy.recent` (固定 4 MiB)** に残し、再起動後に読み戻す。**1 日 1 行の要約 `$HOME/.rust-http-proxy.daily.jsonl` (追記のみ、上限 2 MiB) もこの設定で書きます** (`/daily`)。`off` で無効 (どちらの固定長ファイルも作らず、履歴の収集スレッドも起動しないので `/history` とダッシュボードのグラフ、**カーネルと cgroup の窓** (`/status` の `kernel`) は空になり、個票の `"persisted"` は `false`、日次の要約も 1 行も書きません) |
| `PROXY_PAC_DIRECT` | なし | `/proxy.pac` でプロキシを通さず DIRECT にするホストのカンマ区切り (`*.example.com` 可)。`.env` で即時反映 |
| `PROXY_TLS` | `on` | HTTPS のオリジンから取得するか (システムの OpenSSL を実行時に読み込む)。`off` で無効 |
| `PROXY_TLS_VERIFY` | `on` | オリジンの証明書を検証するか。`off` は自己署名の内部オリジン向け (推奨しない) |
| `PROXY_TLS_CA_FILE` | なし (システムの CA) | 追加で信頼する CA 証明書 (PEM) |
| `PROXY_LOG_LEVEL` | `info` | ログレベル (`error` / `warn` / `info` / `debug` / `trace`) |
| `PROXY_CACHE_ENABLED` | `true` | `0` / `false` / `off` / `no` でキャッシュを無効化 |
| `PROXY_MEM_CACHE_MB` | `auto` | メモリキャッシュ上限。`auto` (動的マージンだけ残して限界まで) か固定値 (MiB) |
| `PROXY_DISK_CACHE_MB` | `auto` | ディスクキャッシュ上限。`auto` か固定値 (MiB) |
| `PROXY_MEM_TARGET_PERCENT` | `100` | `auto` 時に使用率をこの割合で頭打ちにする (任意のキャップ)。`PROXY_MEM_CACHE_MB=auto:85` の形でも指定可 |
| `PROXY_DISK_TARGET_PERCENT` | `100` | 同上 (ディスク) |
| `PROXY_MEM_KEEP_FREE_MB` | `0` | 動的マージンに加えて手動で必ず空けておく量 (MiB) |
| `PROXY_DISK_KEEP_FREE_MB` | `0` | 同上 (ディスク) |
| `PROXY_CACHE_RESERVE` | `staged` | 先行確保 (バラスト) の仕方。`staged` (既定。**使われるまで確保しない**: 保存 0 件なら 0、以後は実使用量の 2 倍まで) / `eager` (予算の未使用分を最初から全部) / `off` (`0` / `false` / `no` も同じ。上限管理のみ) |
| `PROXY_CACHE_PROBE_SECS` | `1` | 使用量を測り直して予算を更新する間隔 (秒)。`0` で起動時の 1 回だけ |
| `PROXY_DISK_QUOTA_ROOT` | `$HOME` | ディスク割当が適用されるディレクトリ (Pterodactyl では `/home/container`) |
| `PROXY_DISK_PROBE` | `on` | 割当が分からないとき Wings の挙動から割当を探るか (後述)。`off` なら 512 MiB 固定 |
| `PROXY_CACHE_DIR` | 自動選択 (後述) | ディスクキャッシュ格納先 |
| `PROXY_CACHE_TTL_SECS` | `300` | `Cache-Control` も `Last-Modified` も無い場合の TTL（秒）。経験則 TTL の下限でもある |
| `PROXY_CACHE_HEURISTIC_PERCENT` | `10` | `Last-Modified` からの経過時間のこの割合を TTL にする (RFC 9111 4.2.2)。`0` で無効 |
| `PROXY_CACHE_HEURISTIC_MAX_SECS` | `604800` | 経験則 TTL の上限 (既定 7 日) |
| `PROXY_CACHE_MAX_STALE_SECS` | `2592000` | 期限切れでも再検証できるエントリを保持しておく最長時間 (既定 30 日) |
| `PROXY_CACHE_GRACE_SECS` | `60` | 期限切れ後この秒数以内なら、保存済みの表現をすぐ返して裏で再検証する (stale-while-revalidate)。`0` で無効 |
| `PROXY_STALE_WAIT_SECS` | `5` | 期限切れの表現があるとき、オリジンの接続と最初の応答を待つ上限 (秒)。超えたら stale を返す |
| `PROXY_CACHE_MAX_OBJECT_MB` | `4096` | ディスク層に置く 1 オブジェクトの最大サイズ（MiB） |
| `PROXY_MEM_CACHE_MAX_OBJECT_MB` | `32` | メモリ層に置く 1 オブジェクトの最大サイズ（MiB）。これを超えるものはディスクからストリーミング配信 |
| `PROXY_DISK_MAX_ENTRIES` | `2000000` | ディスク層の索引に保持するエントリ数の上限 (1 件あたり RAM 約 100 バイト)。超えた分は LRU で追い出す |
| `PROXY_CACHE_ADMISSION` | `on` | 入場制御。最後の層 (ディスク、無ければメモリ) が 90% 埋まったら、2 回目に要求された URL だけ保存する (一度きりの URL で追い出しを起こさない)。見たキーは 512 KiB のブルームフィルタで覚える |
| `PROXY_NEGATIVE_TTL_SECS` | `60` | 404 / 410 などの否定応答に `max-age` / `Expires` が無いときの TTL 上限 (明示があればそちらを使う) |

環境変数を設定できない環境 (Pterodactyl で egg 変数を追加する権限が無い等) では、`$HOME/.env` に `KEY=VALUE` を
1 行ずつ書けば同じ効果になります (`#` はコメント、ファイルの値が実際の環境変数より優先)。Pterodactyl ならファイルマネージャで
`/home/container/.env` を置くだけです。

`.env` は起動後も監視していて、保存すると再起動なしで読み直します (`$HOME` を inotify で監視、使えないファイルシステムでは
30 秒ごとの mtime 確認)。即時に反映されるのは `PROXY_ALLOW_HOSTS` / `PROXY_DENY_HOSTS` / `PROXY_TIMEOUT_SECS` /
`PROXY_KEEPALIVE_SECS` / `PROXY_LOG_LEVEL` / `PROXY_MAX_CONNS` / `PROXY_MAX_THREADS` /
`PROXY_ALLOW_CLIENTS` / `PROXY_ENDPOINTS_READONLY` / `PROXY_MAX_CONNS_PER_CLIENT` などで、
既存の keep-alive 接続には次の接続から効きます (どの値を当てたかは `/status` の `settings.applied` に出ます)。ポート・bind・TLS・
オリジンプール・キャッシュ予算 (`SERVER_MEMORY` / `SERVER_DISK` / `PROXY_CACHE_*`) は起動時に固定なので、変更を検知すると
`/status` の `settings.restart_required` と `/dashboard` の帯に「再起動が必要」と出ます。解釈できない値を書いた場合は
前の設定を維持し、`settings.error` にメッセージが入ります。

**いま何が効いているかは `/config` で 1 枚に出ます** (JSON)。全 `PROXY_*` / `SERVER_*` について
`{"PROXY_DNS_TTL_SECS":{"value":30,"source":"env_file"}, ...}` の形で、`value` は**いま効いている値**、
`source` はそれが来た層 (`default` = このコードの既定 / `env` = 実際の環境変数 / `env_file` = `$HOME/.env` /
`cli` = コマンドライン引数) です。**書いたのに読めない書き方だった行は `default` のまま**出るので、
「`.env` に書いたのに効かない」がその場で分かります (再起動が要る項目は値が古いままなので、
同じ応答の `reload.restart_required` を見てください)。別名のあるキー (`SERVER_DISK` は
`PROXY_DISK_QUOTA_MB` の別名) は、実際に効いた方にだけ `source` が付きます。
一覧の値 (`PROXY_PAC_DIRECT` など) は 1 KiB で切って `"+N more"` を付けます (全部見たいときは `.env` を読む)。
同じ応答に `capabilities` (上記) と `.env` の監視の状態も入るので、**データを取るときは `/config` を 1 枚
一緒に保存しておけば「そのときの設定」が後から読めます**。応答は 64 KiB 以下です。

`PROXY_CACHE_DIR` を指定しない場合は、書き込める最初の候補を使います:
`$XDG_CACHE_HOME/rust-http-proxy` (または `~/.cache/rust-http-proxy`) → `/var/cache/rust-http-proxy` → `$TMPDIR/rust-http-proxy-cache`。
Pterodactyl 以外で root 実行の場合は `/var/cache` を優先します。`$TMPDIR` は tmpfs (RAM) のことが多いので最後の手段です。

## ログ

すべてのログはレベルによらず **標準出力 (stdout)** へ出力されます。既定の `info` レベルで
1 リクエストにつき 1 行のアクセスログが出ます。

```
2026-09-02T02:48:22.900Z INFO  [main] disk cache ready at /home/container/.cache/rust-http-proxy (12034 entries restored, 210 expired removed, 0 migrated, 0 stray files removed)
2026-09-02T02:48:22.901Z INFO  [main] memory cache budget: 0 MiB -> 3482 MiB (container memory 12.1% used)
2026-09-02T02:48:22.901Z INFO  [main] disk cache budget: 0 MiB -> 97800 MiB (disk quota 2.0% used)
2026-09-02T02:48:24.310Z INFO  [main] reserved +3456 MiB memory / +97792 MiB disk (ballast now 3456 MiB / 97792 MiB)
2026-09-02T02:48:23.044Z INFO  [conn#1] ACCESS 127.0.0.1 "GET http://example.com/ HTTP/1.1" 200 204B 22.4ms cache=MISS stored ttl=300s
2026-09-02T02:48:23.087Z INFO  [conn#2] ACCESS 127.0.0.1 "GET http://example.com/ HTTP/1.1" 200 260B 0.4ms cache=HIT(memory) age=0s ttl_left=300s
2026-09-02T02:53:40.512Z INFO  [conn#3] ACCESS 127.0.0.1 "GET http://example.com/ HTTP/1.1" 200 260B 18.9ms cache=REVALIDATED(memory) age=317s ttl_left=300s
2026-09-02T02:53:41.002Z INFO  [conn#4] ACCESS 127.0.0.1 "GET http://example.com/ HTTP/1.1" 304 171B 0.2ms cache=HIT(memory,304) age=318s
2026-09-02T02:48:24.143Z INFO  [conn#5] ACCESS 127.0.0.1 "CONNECT example.com:443 HTTP/1.1" 200 7070B 1010.8ms cache=BYPASS(tunnel)
2026-09-02T03:10:01.502Z INFO  [main] memory pressure detected (PSI some=24.3% full=1.0%): releasing reservations
2026-09-02T03:10:01.502Z INFO  [main] memory cache budget: 3482 MiB -> 2790 MiB (container memory 89.7% used)
```

| レベル | 主な内容 |
|---|---|
| `error` | 起動失敗、致命的な接続エラー |
| `warn` | 403 (ACL)、502 (オリジン接続失敗)、キャッシュ I/O 失敗、資源が計測できない環境、設定の警告 |
| `info` | 起動時の設定サマリ、予算の大きな変化、まとまった先行確保、圧迫 / ENOSPC の検知、アクセスログ |
| `debug` | 接続の受付・切断、キャッシュ HIT/MISS/STORE/EVICT/REFRESH の詳細、予算の小さな変化 |
| `trace` | リクエスト / 転送 / レスポンスの全ヘッダー、トンネル転送量、L2 書き込み |

**`warn` 以上の行は直近 1,000 行だけプロセスのメモリにも残り、`/log?n=200` で読めます** (1 行 256 B まで、新しい順)。コンソールが流れて消える環境 (Pterodactyl) で「さっき何を警告したか」を後から見るための口です。`info` のアクセスログは写しません (1 行 7.2 us/要求 の熱い経路を重くしないため)。

アクセスログの `cache=` の値: `HIT(層)` 新鮮なヒット / `HIT(層,304)` クライアントの条件付き要求に 304 で応答 /
`REVALIDATED(層)` オリジンに再検証して 304 を受け延命 / `REFRESHING(層)` 期限切れ直後の表現を返し裏で再検証 /
`COALESCED(層)` 同時に進行中だった取得の完了を待ってキャッシュから配信 /
`STALE(層)` オリジン障害・待ち切れ時に期限切れを配信 /
`MISS stored` 取得して保存 / `MISS` 取得したが保存対象外 / `BYPASS` キャッシュ対象外の要求。

## キャッシュ

GET リクエストのみを対象に、レスポンスをワイヤ形式そのままで 2 段キャッシュへ格納します。

- 保存するのは解読済みの本文 (chunked を外したもの) と、枠組み・接続管理のヘッダーを除いた先頭部分。配信時に
  `Content-Length` / `Connection` を付け直す。形式は `SHPC2` で、旧形式のエントリは起動時に捨てる
- L1 (メモリ) → L2 (ディスク) の順に探索し、`PROXY_MEM_CACHE_MAX_OBJECT_MB` 以下の L2 ヒットは L1 へ昇格。
  それより大きいものはディスクからそのままストリーミング配信
- どちらも LRU で上限を超えた分から追い出し (参照・追い出しともに O(log n))。書き込み中の一時ファイルの分も
  容量計算に含めるので、同時ダウンロードやバラストの再充填で上限を超えることはない
- ディスク上のエントリはペイロード長をヘッダーに記録し、読むときにファイルサイズと照合して途中で切れたものを弾く
- ディスクキャッシュは 256 分割ディレクトリに 1 エントリ 1 ファイルで置き、ファイルの mtime に有効期限を
  記録するので、起動時の走査はファイルを開かずに済む。書き込みは一時ファイルへのストリーミング
- キャッシュキーは メソッド + URL + 正規化した `Accept-Encoding`。`Vary` に `Accept-Encoding` 以外があれば保存しない
- `Range` 付きの要求は、キャッシュ済みの完全な表現があれば `206` で切り出し、無ければそのまま転送して保存しない。
  `HEAD` はキャッシュがあればヘッダーだけ返し、無ければ転送 (保存はしない)
- 次の場合は保存しない: GET 以外 / `Authorization` 付き / `Range` 付き / クライアントの `no-store` /
  レスポンスの `no-store`・`private` / `Set-Cookie` / 非対応の `Vary` / 非キャッシュ対象ステータス / 上限超過
- クライアントの `no-cache` / `max-age=0` は「バイパス」ではなく「オリジンで再検証」として扱う

### HTTPS のオリジン

HTTPS の中身をキャッシュするには TLS を終端する必要があり、CONNECT トンネル (クライアントが `https://` を普通にプロキシ
経由で開く形) では暗号化されたまま素通しになります。代わりに、クライアント側で URL を次の形にしてプロキシへ平文 HTTP
で頼めば、プロキシが HTTPS で取得して保存・配信します。

```bash
# プロキシをオリジンとして叩く形 (スクリプトやダウンロードツール向け)
curl http://127.0.0.1:8080/https/example.com/file.zip -o file.zip
```

要求行に `GET https://example.com/file.zip HTTP/1.1` と絶対 URL を書いて平文で送るクライアント (自作スクリプト等) も
同じように扱います。普通のプロキシ設定で `https://` を開くクライアントは CONNECT を使うので対象外です。

- TLS はシステムの OpenSSL (`libssl.so.3` / `libssl.so.1.1`) を実行時に読み込んで使います。クレートもビルド時の依存もなく、
  ライブラリが無い環境では HTTPS 取得だけが無効になります (起動ログに出ます)
- 証明書は既定で検証します (システムの CA ストア、`SSL_CERT_FILE` / `SSL_CERT_DIR`、または `PROXY_TLS_CA_FILE`)
- 応答の `Location` / `Content-Location` が絶対 URL なら `/https/host/path` 形式に書き換えるので、リダイレクトを追っても
  プロキシから外れません
- キャッシュキーには `https://host:443/path` の形で保存されるので、`/https/...` 形式と絶対 URL 形式は同じエントリに当たります

### 鮮度と再検証 (RFC 9111)

TTL は `s-maxage` → `max-age` → `Expires` → `Last-Modified` からの経験則 (経過時間の 10%、下限 `PROXY_CACHE_TTL_SECS`、
上限 7 日) → 既定 TTL の順で決めます。

- ETag か Last-Modified を持つレスポンスは、期限切れでも `PROXY_CACHE_MAX_STALE_SECS` (既定 30 日) の間は残します。
  次の要求では `If-None-Match` / `If-Modified-Since` を付けてオリジンに問い合わせ、304 なら本文を転送せずに
  保存済みの表現を延命して配信します (`cache=REVALIDATED`)。`no-cache` や `max-age=0` のレスポンスも
  バリデータがあれば「毎回再検証」として保存します
- 期限切れから `PROXY_CACHE_GRACE_SECS` (既定 60 秒) 以内なら、保存済みの表現をすぐ返して裏で再検証します
  (`cache=REFRESHING`、RFC 5861 の stale-while-revalidate)。オリジンが `stale-while-revalidate=N` を付けていれば
  その値も使います。`max-age=0` (毎回再検証) の表現は、オリジンが明示したときだけ対象です。同じ URL の裏側の
  再検証は同時に 1 本、全体で 32 本まで。**裏側の再検証は接続スレッドと同じ置き場で走ります**
  (`PROXY_MAX_THREADS` の内側)。空いているスレッドが無ければ**待たせずに捨て**、その要求はそのまま
  同期の再検証に回ります (捨てても正しさは崩れません。その項目は次の要求で取り直されます)。
  捨てた回数は `/status` の `revalidations_dropped` と `/metrics` の `cache_revalidations_dropped_total` に出ます
- 期限切れの表現があるのにオリジンが遅いときは、接続と最初の応答を `PROXY_STALE_WAIT_SECS` (既定 5 秒) までしか
  待たず、超えたら期限切れの表現を配信します (`cache=STALE`)
- オリジンに繋がらない・5xx を返す場合は、`must-revalidate` でない限り期限切れの表現を配信します (`cache=STALE`)
- クライアントが `If-None-Match` / `If-Modified-Since` を付けてきて保存済みの表現と一致すれば、キャッシュから 304 を返します
- `no-cache` / `s-maxage` / `must-revalidate` 付きの表現と、クライアントが `no-cache` を付けた要求では stale を配信しません
- 上流のキャッシュを経てきた応答は `Age` と `Date` から経過時間を差し引いて保存し、配信時の `Age` はプロキシが付け直します
- POST / PUT / DELETE などへの成功応答 (2xx/3xx) を受けたら、その URL (と同じサーバーへの `Location` / `Content-Location`)
  のキャッシュを無効化します (RFC 9111 §4.4)。本文付きの GET と `Range` 要求の応答は保存しません

### 自動モードの予算と動的マージン

`auto` では毎秒 (`PROXY_CACHE_PROBE_SECS`) 使用量を測り直し、各層の上限 (予算) を次の式で更新します。

```
予算 = 自分が保持しているバイト数 (エントリ + バラスト) + (全体 − 安全マージン − 現在の使用量)
```

「現在の使用量」には自分の分も含まれるので、他プロセスが資源を使えばその分だけ予算が縮み、
バラスト → LRU 追い出しの順に手放します。逆に空けばまた育ちます。安全マージンは固定値ではなく、
次の最大値として毎秒決め直します。

- **変動幅**: 他プロセスの使用量が直近 60 秒の窓で 1 秒間にどれだけ動いたかの最大値 × 2 (次の測定までに他者が伸びても吸収できる量)
- **床**: メモリは `vm.min_free_kbytes` × 4 と全体の 1% (最低 64 MiB、ただし全体の 1/10 まで) の大きい方、ディスクは全体の 1% (最低 256 MiB、同上)
- **活性ページキャッシュ**: ホストの `Active(file)` (他者が実際に使っているキャッシュ) は奪わない
- **バックオフ**: PSI (`/proc/pressure/memory` または cgroup の `memory.pressure`) で `some avg10 >= 20%` か `full avg10 >= 5%`、
  ディスクは自分の書き込みが ENOSPC になったら、全体の 5% から始めて倍々に増やす。平穏が 30 秒続くごとに 3/4 に減衰。
  圧迫を検知した後 60 秒はバラストを再確保しない
- **手動の最低値**: `PROXY_*_KEEP_FREE_MB`

上限を割合で抑えたいときは `PROXY_*_TARGET_PERCENT` (既定 100 = 抑えない) を下げます。

計測の分母は次のとおりです。

- **メモリ**: `/proc/meminfo` の `MemTotal` / `MemAvailable` に加え、所属する cgroup (v1/v2) の制限と
  `SERVER_MEMORY` (Pterodactyl の割当) をそれぞれ同じ式で計算し、最も厳しい値を採用。
  cgroup の使用量は Docker / Wings と同じ「usage − inactive_file」で見るので、パネルの表示と一致します
- **ディスク**: `statvfs` で `df` と同じ流儀の使用率を見る。`PROXY_DISK_QUOTA_MB` があれば代わりに
  「割当 − 割当ディレクトリ内の自分以外のファイル」を分母にします (60 秒ごとに再計測)
- 計測できない環境 (Linux 以外など) では固定値 (メモリ 200 MiB / ディスク 2048 MiB) にフォールバックして警告します

### 先行確保 (バラスト)

**既定 (`staged`) は「使われるまで確保しない」**です。保存が 1 件も無い間はバラストを 0 に保ち、
最初の保存以降は「エントリ + バラスト ≤ 実使用量 × 2」の範囲でだけ (もちろん予算の範囲でも) 先に押さえます。
`PROXY_CACHE_RESERVE=eager` にすると従来どおり、起動直後から予算の未使用分を全部確保します。

`eager` を既定から外したのは、**256 MiB のコンテナで測ったら何も買っていなかった**からです
(2026-09-10。`scripts/cgroup-run.sh` で再現できます)。

- バラスト無し (`off`) でもメモリ層は `limit_bytes` (197.6 MiB) まで埋まり、`oom_kill` は 0。
  **先に押さえなくても予算は取れる** (64 KiB × 4,160 URL を保存させた実測)
- 逆に `eager` は 192 MiB を先に押さえるので、256 MiB のコンテナに残る空きが 43 MiB しかありません。
  そこへコンテナの中の別プロセスが 80 MiB 取ろうとすると、**1 秒周期のプローブがバラストを返す前に
  カーネルの OOM killer がプロキシを殺します** (同じ条件を 4 回試して 2 回。`off` / `staged` なら
  同じ 80 MiB でも 142 MiB でも平気で、cgroup のピークは 86〜147 MiB に収まりました)
- 段階化すると、要求 0 件のコンテナの RSS は **215.9 MB → 23.1 MB**、
  `disk.reserved_bytes` は **2.9 GB → 0** になります (どちらも 256 MiB の cgroup で 60 秒後の実測)

確保するときの動きは `staged` も `eager` も同じです。

- メモリ: 64 MiB 単位の領域を 0 以外で埋めて全ページをコミット。エントリの追加時にその分だけ解放
- ディスク: `ballast.reserve` ファイルを `fallocate` で伸ばす。エントリの書き込み前に必要分だけ縮める
- 予算が縮んだときはまずバラストを返し、それでも足りないときだけエントリを追い出す
- Pterodactyl で割当を探っている最中 (`PROXY_DISK_PROBE`) だけは、探索そのものが
  `fallocate` で上限を確かめる仕組みなので段階化しません
- tmpfs / ramfs 上のディレクトリでは「ディスク」の先行確保が RAM を食うだけなので無効化して警告
- SIGTERM / SIGINT を受けたら `ballast.reserve` を切り詰めてから終了します (Pterodactyl の停止中にディスク使用量として
  残らない)。強制終了で残っても、次回起動時に必ず空にしてから予算に合わせて作り直します

### Pterodactyl での注意

- メモリは cgroup と `SERVER_MEMORY` の両方で上限が決まるので追加設定は不要です。コンテナ内の `/proc/meminfo` は
  ホスト全体を示しますが、実際に効くのは cgroup 側の予算です
- **ディスク割当は Pterodactyl がコンテナに渡してくれません。** Wings は `/home/container` の合計サイズを監視していて、
  割当を超えると **即座にプロセスを停止し** (`Server is exceeding the assigned disk space limit, stopping process now`)、
  減らすまで起動もできません。egg 変数 `PROXY_DISK_QUOTA_MB` (または `SERVER_DISK`) を用意し、サーバーの
  Disk Space (MB) と同じ値にしてください。無制限なら `0`。**未設定のときはディスクキャッシュを 512 MiB 固定・
  先行確保なしに抑えます** (起動時に警告します)。egg の Configuration Files 機能で
  `{{server.build.disk_space}}` を書き出せる環境なら、それを起動スクリプトで環境変数に渡す手もあります
- **未設定でも自動判断します**: `df -B1 /home/container` 相当 (statvfs) が `/` と別のファイルシステムで、かつ `/` より
  小さければ、それを割当とみなして使います (ボリュームが別ディスクにあるだけのホストで、そのディスク丸ごとを割当と
  誤認しないための条件)。条件に合わなければ、**割当を探ります**:
  - まず 512 MiB ずつ素早くバラスト (fallocate) を伸ばします (実データの上限は 512 MiB のまま)。fallocate が失敗する、
    またはファイルシステムの空きの都合で伸びなくなったら、そこを割当の候補にします。候補のまま 10 分止められなければ
    確定し、実データもそこまで使います。ファイルシステム側で割当が効いているホストなら数分で終わります
  - Wings がディレクトリの合計サイズで止めるタイプのホストでは、素早く伸ばしている間に止められます。停止シグナルで
    バラストは切り詰められるので使用量は 512 MiB 以下に戻り、そのまま再起動できます。再起動時に「起動 10 分以内に
    途切れた」と分かるので、以後は緩やかな探索に切り替えます: 10 分止められないごとに 512 MiB ずつ上げ、増えた分は
    バラストだけで埋め (実データは確認済みの上限まで)、上げた直後に止められたら確認済みの値を割当として記憶します
    (7 日は探りません)。例えば割当 3 GiB なら約 1 時間で 2.5〜3 GiB に落ち着きます
  - つまり最悪 2 回 Wings に止められて再起動が必要になりますが、以降は無設定で割当いっぱいまで使います。
    状態は `/home/container/.rust-http-proxy.state` に残ります。`PROXY_DISK_PROBE=off` で探索を止められます
    (その場合は 512 MiB 固定)。割当が分かっているなら `.env` に `SERVER_DISK` を書く方が早いです
- `SERVER_DISK=auto` を明示すると、`df -B1 /home/container` 相当 (statvfs) の total を割当として使います。
  これが正しいのは、ホストが XFS のプロジェクトクォータや ZFS データセットなどでサーバーごとに領域を切っていて、
  `df` の total がパネルの Disk Space と一致する場合だけです。単なる bind mount ではホストのディスク全体が
  見えるので、起動時に `df /` と比べて同じファイルシステムなら「不明」扱い (512 MiB 上限) に落として警告します。
  Pterodactyl では判断しやすいよう、起動ログに `df -B1 /home/container: total ... used ...` を常に出します
- 超過で止められてしまったら、ファイルマネージャか SFTP で `/home/container/.cache/rust-http-proxy/`
  (特に `ballast.reserve`) を削除すれば起動できます
- 既定のキャッシュ先は `/home/container/.cache/rust-http-proxy` (ボリューム内なので再起動後も残る)

## クレート構成

**外部クレートは 1 つも使っていません** (すべて `std` のみ)。`crates/` にあるのは全部このリポジトリのコードで、
責務ごとの層に分けてあります (26 + 本体)。分けている理由は 2 つで、責務を 1 つに保つことと、`rustc` がクレート単位で
全部を一度に抱えるためビルドのメモリがそのまま行数に比例すること (動作環境の `SERVER_MEMORY` は 256 MiB)。

| クレート | 責務 |
|---|---|
| `proxy-sys` | Linux のシステムコールを直接叩く薄い層 (`poll`/`epoll`/`splice`/`pipe2`/`recv`) とシグナル |
| `proxy-base` | ロック、壁時計、JSON の組み立て、`.env` の読み取り、ログ、HTTP 日付、コマンドライン引数、ASCII だけを見る文字列の分割、起動ごとの `Via` の印 |
| `proxy-rrd` | 固定長のリングバッファ (状態ファイルと個票のファイルに共通の保存形式) |
| `proxy-sysinfo` | 機械の観測 (メモリ、ディスク、cgroup の上限、`inotify`) |
| `proxy-workers` | 接続スレッドの使い回し |
| `proxy-tls` | システムの OpenSSL を `dlopen` で使う TLS クライアント |
| `proxy-msg` | HTTP メッセージの表現 (ヘッダー、本文の枠、読み取りバッファ、応答の先頭) |
| `proxy-net` | 名前解決 (Happy Eyeballs)、接続、アドレスの判定 |
| `proxy-origin` | オリジンへの接続とその使い回し、要求 URL の解釈 |
| `proxy-cachekey` | キャッシュの保存形式と鍵 |
| `proxy-cachecfg` | キャッシュの設定 |
| `proxy-diskprobe` | ディスクの実測 (実際に書いてみて、どれだけ入るかを確かめる) |
| `proxy-capacity` | 使ってよい量の見積もり (空きメモリ・cgroup の上限・コンテナの quota) |
| `proxy-cachemem` | キャッシュのメモリ側 (LRU、エントリ、合流、受け入れ判定、保存されない鍵の記憶) |
| `proxy-cachedisk` | キャッシュのディスク側 (ファイル形式、走査、書き出し) |
| `proxy-cache` | キャッシュ本体 (メモリ側とディスク側を束ねる) |
| `proxy-config` | 起動時の設定 |
| `proxy-metrics` | 計測 (ホスト別・接続元別の統計、時系列の履歴、状態ファイルへの読み書き) |
| `proxy-blocklist` | ドメインのブロックリスト |
| `proxy-reload` | `$HOME/.env` の再読込 (`inotify` で見張る) |
| `proxy-prom` | Prometheus 形式の出力 |
| `proxy-freshness` | RFC 9111 の鮮度判定 |
| `proxy-http` | 中継の本体 |
| `proxy-tunnel` | CONNECT トンネル |
| `proxy-endpoints` | プロキシ自身のエンドポイント (`/dashboard` `/status` `/metrics` …) |
| `proxy-bench` | 計測用の道具 (既定のビルド対象から外してあります。`cargo build --release -p proxy-bench`) |
| `rust-http-proxy` | 接続の受け付け、keep-alive、アイドル接続の預かり |

## ビルド・テスト


> **メモリの小さい環境向けの設定**: `.cargo/config.toml` で `jobs = 1` にしてあります。
> このリポジトリは **99 MB のメモリでリリースビルドが通ります** (2026-09-08、手元の aarch64 で実測。
> 98 MB は落ちます。CI が毎回確かめているのは **200 MB の関門を通るかどうかだけ**で、CI で通る最小は
> 測っていません)。LTO を既定で切っているのもこのためです (下の「プロファイルの設定」)。
> `rustc` はクレート単位で全部を一度に抱えるため、いちばん大きいクレート (`proxy-http`) の 1 プロセスで
> **RssAnon 85.5 MB** 使い (通る最小はここに 14 MB ほど足した値になります。`/usr/bin/time -v` の最大 RSS は
> 205 MB と出ますが、そのうち 110〜122 MB は `librustc_driver` のファイル由来のページで、ページキャッシュに
> 載っていれば cgroup には課金されません)、既定の並列数だと
> その合計がコンテナのメモリ上限を超えて OOM killer に落とされます (実測: 200 MB の cgroup で、
> 並列だと落ち `-j 1` なら通る)。潤沢な機械で急ぐときは `cargo build --release -j 8` で上書きできます
> (8 コアで 32.3 秒 → 14.9 秒)。
>
> 手元で確かめるなら `cargo clean && scripts/build-memory.sh 200` (通る最小を探すなら
> `scripts/build-memory.sh --find 100 110 120 130 140 150`)。このスクリプトは **実際にその上限の
> cgroup の中でビルドします** — RSS を測るだけだと、メモリ圧のかかっていない機械ほど大きく出て
> 機械をまたいだ判定にならないためです。cgroup は systemd に作らせます (システムの `systemd-run --scope`、
> 無ければ**ユーザーの** `systemd-run --user --scope`。どちらも無い環境では参考の RSS だけ出して判定しません)。
```bash
# テスト実行
cargo test

# 通常ビルド
cargo build

# 最適化リリースビルド
cargo build --release

# 起動 (bin が 2 つあるが default-run でプロキシ本体が選ばれる。ベンチはビルドされない)
cargo run --release

# ベンチ (オリジンもベンチ内で起動する。プロキシは別端末で PROXY_ALLOW_LOCAL=on を付けて先に上げておく)
cargo run --release --bin bench -- --proxy 127.0.0.1:18080 --conc 8 --seconds 5
# --conc 並列数 / --seconds 測定秒数 / --body-bytes 応答本文の大きさ
# --only direct|forward|tunnel|connect|idle-tunnels|idle-conns|syscall-cost|all で 1 種だけ測れる
# direct 行はプロキシを通さないオリジン直結 (ベンチ自身の上限。固定しなければ 26〜30 万 req/s、
#   LITTLE に固定すると 5.5 万 req/s)
# tunnel 行も --seconds 秒だけ 1 本のトンネルに流す (この行はベンチ律速。上の「性能」の注)
# idle-tunnels / idle-conns は --conc 本を張ったまま --seconds 秒握る (スレッド数と RSS 用)。
#   idle-conns は 1 要求ずつ通してから握る。どちらも PROXY_MAX_CONNS=8192 を付けて測る

# この機械での sendto / recvfrom 1 回の実費 (プロキシは使わない。CPU の固定が要る)
taskset -c 4-7 cargo run --release --bin bench -- --only syscall-cost --seconds 3
```

### 起動直後のスレッド数

「使わない機能にはコストを払わない」方針なので、無効にした機能のスレッドは起動しません。

| 設定 | スレッド | 内訳 |
|---|---|---|
| 既定 (キャッシュ・統計あり) | 7 | 待ち受け + `env-reload` + `cache-probe` + `persist` + `history` + `shutdown` + `idle-watch` |
| `PROXY_CACHE_ENABLED=off PROXY_STATS_PERSIST=off` (= `--lite`) | 4 | 待ち受け + `env-reload` + `shutdown` + `idle-watch` |
| 上記 + `PROXY_PARK_IDLE=off` | 3 | `idle-watch` が減る |

ブロックリストの取得スレッドは `PROXY_BLOCKLIST_FILE` / `PROXY_BLOCKLIST_URL` が設定されたときだけ動きます
(`$HOME/.env` の再読込で後から設定された場合もその時点で起動します)。

接続 1 本ごとのスレッドはこれとは別です (CONNECT トンネルは Linux では中継中 1 本あたり 1 スレッド、
暇なあいだは 0 本)。

**接続スレッドは使い回します**: 仕事を終えたスレッドは空き置き場に戻り、30 秒使われなければ自分で
終わります (空きは最大 64 本まで)。生成と破棄のシステムコール (実測で 1 接続あたり約 16 回) を
接続ごとに払わない形です。

**同時に生きているスレッドには上限があります** (`PROXY_MAX_THREADS`、既定 `auto`)。
「空いている数」(最大 64 本) と「生きている数」は別のもので、後者がこの上限です。
上限に達したら新しいスレッドを起こさず、その仕事を待たせます (**捨てません**。空いたスレッドが
順に引き取ります)。上限が無かったころは、預けた 5,000 本のトンネルが一斉に切れると 1 本ずつ
ワーカーへ渡すので**一時的に 4,400〜4,700 スレッド**まで増えていました (閉じるのに shutdown・
アクセスログ・統計が要るので、監視スレッドの中では落とせません)。上限 256 なら同じ場面で
**260 スレッド・ピーク RSS 65 → 27 MB** で、閉じきるまでの時間は変わりません (0.66 秒)。

上限は `.env` の再読込で変えられます。当て直すのは接続を受けたときなので、書き換えてから
**次の接続 1 本**で効きます。**上げた場合**は待たせていた仕事をその場で起こし、**下げた場合は
走っているスレッドを殺しません** (代理の途中で切らないため)。新しいスレッドを起こさなくなり、
仕事を終えたスレッドから順に終わるので、しばらくは `/status` の `live_threads` が
`max_threads` を上回ったままになります。

**裏側の再検証 (stale-while-revalidate) もこの置き場で走ります**。以前は再検証のたびに
スレッドを起こしていたので上限の外にいました。ただし待ち方は接続と逆で、**空きが無ければ
待ち行列に積まずに捨てます**。再検証は「後でやればいい仕事」で、捨てても次の要求で
普通に取り直されるだけなのに対し、積むと (1) 新しい接続の処理がその後ろに並び、
(2) 順番が来るまで「このキーは再検証中」の印を握り続けるためです。捨てた要求はそのまま
同期の再検証に回るので、クライアントには新しい表現が返ります。

代償は「暇なトンネルを預かる速さ」です。トンネルは 1 本ごとに猶予 100 ms のあいだワーカーを
握るので、**次々に張られる CONNECT を受けられる速さの天井が「上限 ÷ 100 ms」**になります
(上限 256 で毎秒 2,560 本。実測で 5,000 本の確立が 1.9 → 2.45 秒)。それより速く暇なトンネルが
増える環境では `PROXY_MAX_THREADS` を上げてください。短命な CONNECT (張ってすぐ閉じる) と
HTTP の代理はワーカーをすぐ手放すので、この天井には当たりません (実測でも変化なし)。

**要求を処理していない keep-alive 接続はスレッドを握りません** (`PROXY_PARK_IDLE`、既定 on、
Linux のみ)。次の要求が猶予 (既定 3 ms) のあいだ来なければ、その接続は `idle-watch` スレッドの
epoll に預けられ、ワーカースレッドは解放されます。読めるようになったら空いているワーカーに戻します。
実測で暇な接続 2,000 本のとき 2,004 スレッド・RSS 68.0 MB → **25 スレッド・25.2 MB**、
忙しいときの CPU/要求とシステムコール数は変わりません。`off` にすると
「1 接続 = 1 スレッドが専任する」元の動きに戻ります
(2026-09-08、`release`、`PROXY_MAX_CONNS=8192 scripts/cpu-per-request.sh --only idle-conns --conc 2000`
の 3 回の中央値。`off` 側は同じコマンドに `PROXY_PARK_IDLE=off PROXY_MAX_THREADS=0` を足したもの)。

**暇な CONNECT トンネルも同じ預かり所に預けます** (同じ `PROXY_PARK_IDLE`、Linux のみ)。
両方向とも 100 ms 動きが無ければ、クライアント側とサーバー側の記述子 2 本を epoll に預けて
スレッドを手放します (中継に使うパイプもこのとき手放します)。どちらかが動いたら空いている
ワーカーへ戻し、`PROXY_TUNNEL_IDLE_SECS` を過ぎたものは預かり所が引き上げて閉じます。
預けているあいだも同時接続数には数えます (`PROXY_MAX_CONNS` の意味は変わりません)。数えているので、
上限に当たった接続はこの中の**最古の 1 本を閉じて**席を作ります (`PROXY_MAX_CONNS` の行を参照)。
実測で暇なトンネル 5,000 本のとき 5,005 スレッド・RSS 93.9 MB → **68 スレッド・29.9 MB**
(`/status` の `parked_tunnels` で本数が見えます)。片方向だけ閉じたトンネルは預けません。

中継に使う `splice(2)` のパイプは、**空になったものをワーカースレッドが 2 本まで持ち越して
次のトンネルで使い回します**。短命なトンネル (CONNECT を張ってすぐ閉じる) は 1 本あたり
`pipe2` 2 回・`fcntl(F_SETPIPE_SZ)` 2 回・`close` 4 回を払っていたので、CONNECT 確立の
システムコールが 29.1 → 20.1 回/本、CPU/本 が 163.5 → 139.7 us になりました。
中身が残ったままのパイプ (送り先が途中で消えたとき) は使い回さずに閉じます。

### 開発者向け: プロファイル取得

```bash
# シンボル付きでビルドする (profile.release は strip = true なので strip も止める)
CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_PROFILE_RELEASE_STRIP=none cargo build --release
perf record -g -p $(pgrep -f 'rust-http-proxy$') -- sleep 10   # 別端末でベンチを回している間に
perf report --stdio | head -40                                 # ホットパスを見る
strace -c -f -p $(pgrep -f 'rust-http-proxy$')                 # perf が無ければシステムコールの回数で見る
strace -f -e trace=write,sendto -c -p $(pgrep -f 'rust-http-proxy$')  # 1 要求あたりの write 回数
```

`[profile.release] debug = 1` は入れません (バイナリが太るため)。必要なときだけ上の環境変数で付けます。

## 配布 (Docker / Release バイナリ)

```bash
# Docker (2 段ビルド。実行イメージは debian-slim + libssl3)
docker build -t rust-http-proxy .
docker run -p 8080:8080 rust-http-proxy
curl -x localhost:8080 http://example.com/
```

イメージに入れるのは `Cargo.toml` / `Cargo.lock` / `build.rs` / `.cargo` / `crates` / `src` だけです
(本体は 26 個のクレートに分かれているので `crates` が要ります)。`.git` は入れないので、
イメージの中の版は `rust-http-proxy 0.1.0+unknown` になります。
`.dockerignore` で `target/` と `**/target/` を送らないようにしてあります
(イメージには元から入りませんが、ビルドコンテキストとして daemon へ送ると
`cargo build` 済みの作業ツリーでは 1 GB を超えて遅くなるため。除いたコンテキストは約 2 MB)。

タグ `v*` を push すると `.github/workflows/release.yml` が x86_64 / aarch64 の
バイナリ (glibc 2.35 以上) を作って Release に添付します。

### musl 静的リンクを採らない理由 (実測)

「1 ファイルでどこにでも置ける」ので一度は入れましたが、実測して外しました。

| | forward, 8 並列 | CONNECT 確立 |
|---|---|---|
| glibc (通常ビルド) | **32,169 req/s, p50 0.204 ms** | 8,071 tunnels/s |
| musl 静的リンク | 10,846 req/s, p50 0.602 ms (**1/3**) | 7,277 tunnels/s |

加えて、

1. **TLS が使えない**: `dlopen` が機能しないので `libssl` を実行時に読み込めず、起動ログに
   `TLS: libssl not found` が出ます。`https://` オリジンの取得とキャッシュが無効になります
   (`CONNECT` トンネル = ブラウザの HTTPS は影響なし)
2. **名前解決が NSS を通らない**: musl は `/etc/resolv.conf` だけを見ます (systemd-resolved / mDNS が効かない)
3. **ビルドが 200 MB に収まらない**: 静的リンクにしてもビルドが軽くなるわけではありません

### プロファイルの設定 (実測で決めたもの)

| | `release` (既定) | `dist` (配布用) |
|---|---|---|
| `opt-level` | `3` | `"s"` |
| `lto` | `false` | `true` (fat) |
| `codegen-units` / `strip` | `1` / あり | 同じ |
| ビルドが通る最小のメモリ | **99 MB** (手元。CI では未測定) | 350 MB 以上 |
| バイナリ | 1,381,872 B | **1,054,184 B** (327 KB 小さい) |
| CPU/要求 (forward 8 並列) | 41.4 us | **39.6 us** |

(2026-09-08 の実測。速さの測り方は上の「性能」と同じで、`TASKS.md` の §2 と同じ値です)

**LTO を既定で切っている理由**: `cargo build --release` を **メモリ 200 MB のコンテナで通す** ためです。
LTO の重さは最終リンクの 1 回にかかり、クレートを 26 に割ってもそこは小さくなりません。
26 クレート・`jobs = 1` で 3 つの選択肢を同じ日に測った結果 (2026-09-07。手元の aarch64 で、その上限の cgroup に
入れて実際にビルドしたもの。**バイナリと CPU はこのときのコードの値**で、いまの `release` は上の表のとおりです。
CI で通る最小は測っていません):

| `[profile.release]` | 通る最小のメモリ (手元) | クリーンビルド | バイナリ | CPU/要求 (forward) |
|---|---|---|---|---|
| `lto = false` (既定) | **100 MB** | 38.7 秒 | 1,316,336 B | 50.19 us |
| `lto = "thin"` | 170 MB | 55.4 秒 | 1,316,328 B | 50.09 us (-0.2%) |
| `lto = "fat"` | 350 MB | 48.8 秒 | 1,185,248 B | 47.95 us (-4.5%) |

`fat` は効きますが 200 MB の 1.75 倍のメモリが要ります。`thin` は 170 MB 払ってもバイナリが
8 バイトしか変わらず、速さもぶれの中です。配布バイナリだけ `dist` (fat LTO) で作ります。

**最適化レベル**: `release` は `opt-level = 3`、`dist` は `"s"` です (`dist` はサイズを優先)。
fat LTO と合わせた `dist` の実測は `release` に対して forward 8 並列 **+1.9%** (CPU/要求 41.4 → 39.6 us で -4.4%)、
キャッシュ HIT +0.7%、64 並列 -1.2%、CONNECT 確立 -8% (この経路のぶれ ±25% の中) で、**CPU 以外はほぼぶれの中**です。
それでも `dist` は 327 KB 小さいので、配布は `dist` のままにしています。

## 起動方法

```bash
# 基本起動 (メモリ・ディスクとも動的マージンだけ残して限界まで自動確保)
SERVER_PORT=8080 ./target/release/rust-http-proxy

# ACL・タイムアウト付きで起動
SERVER_PORT=8080 \
PROXY_ALLOW_HOSTS="*.example.com,*.github.com" \
PROXY_DENY_HOSTS="evil.example.com" \
PROXY_TIMEOUT_SECS=15 \
./target/release/rust-http-proxy

# 使用率を 80% / 90% で頭打ちにし、先行確保をやめて上限管理だけにする
SERVER_PORT=8080 \
PROXY_MEM_TARGET_PERCENT=80 \
PROXY_DISK_TARGET_PERCENT=90 \
PROXY_CACHE_RESERVE=off \
./target/release/rust-http-proxy

# 従来どおり固定上限 (メモリ 200MB / ディスク 2GB) で起動
SERVER_PORT=8080 \
PROXY_LOG_LEVEL=debug \
PROXY_MEM_CACHE_MB=200 \
PROXY_DISK_CACHE_MB=2048 \
PROXY_CACHE_DIR=/var/cache/rust-http-proxy \
./target/release/rust-http-proxy

# Pterodactyl (SERVER_PORT / SERVER_MEMORY / P_SERVER_UUID はパネルが渡す。ディスク割当だけ egg 変数で)
PROXY_DISK_QUOTA_MB=51200 ./rust-http-proxy
```

## 動作確認 (curl)

```bash
# HTTP プロキシ経由のリクエスト
curl -x http://127.0.0.1:8080 http://example.com/ -I

# HTTPS (CONNECT) プロキシ経由のリクエスト
curl -x http://127.0.0.1:8080 https://example.com/ -I

# キャッシュヒットの確認 (2 回目は X-Cache: HIT ヘッダーが付く)
curl -x http://127.0.0.1:8080 http://example.com/ -I
curl -x http://127.0.0.1:8080 http://example.com/ -I | grep -i x-cache

# 条件付き要求にはキャッシュから 304
curl -x http://127.0.0.1:8080 http://example.com/ -H 'If-None-Match: "<ETag>"' -I

# エンドポイントの一覧 (ブラウザでプロキシの URL を開いたときと同じ案内)
curl http://127.0.0.1:8080/

# ヘルスチェック (200 = 健康、503 = どれかの検査が偽。軽い JSON)
curl -i http://127.0.0.1:8080/healthz

# メトリクス確認 (キャッシュ統計・予算・マージン・先行確保量・システム使用量・カーネルと cgroup の窓を含む)
curl http://127.0.0.1:8080/status
curl "http://127.0.0.1:8080/status?sort=errors"         # 上位 50 をエラーの多い順で切り出す (dns / slow も)
curl http://127.0.0.1:8080/metrics                      # Prometheus 形式

# 個票 (誰が・いつ・なぜ)
curl "http://127.0.0.1:8080/connections"                # いま開いている接続
curl "http://127.0.0.1:8080/recent?n=200"               # 閉じた接続 (新しい順)
curl "http://127.0.0.1:8080/recent?sort=slow&n=20"      # 確立のいちばん遅かった 20 本
curl "http://127.0.0.1:8080/recent?client=198.51.100.7" # ある接続元だけ
curl "http://127.0.0.1:8080/recent?since=$(( $(date +%s) - 3600 ))"   # 直近 1 時間に開いたもの
curl "http://127.0.0.1:8080/bursts?n=50"                # 山が立った瞬間の写真 (新しい順)
curl "http://127.0.0.1:8080/events?n=200"               # 起動・再読込・圧迫などの出来事 (新しい順)
curl "http://127.0.0.1:8080/events?since=$(( $(date +%s) - 86400 ))"  # 直近 1 日の出来事だけ
curl "http://127.0.0.1:8080/history?res=5"              # 時系列 + 閉じた接続の分布 (closed)
curl "http://127.0.0.1:8080/daily?n=365"                # 1 日 1 行の要約 (永久に残る。古い順)
curl -s http://127.0.0.1:8080/snapshot > snap.json      # 上の全部を 1 要求で (4 MiB まで)

# キャッシュの操作・確認
curl -X PURGE -x http://127.0.0.1:8080 http://example.com/file.zip     # 1 URL (全バリアント) を消す
curl "http://127.0.0.1:8080/purge?url=http://example.com/file.zip"     # 同じことを GET で
curl "http://127.0.0.1:8080/purge?all=1"                               # 全消去
curl "http://127.0.0.1:8080/lookup?url=http://example.com/file.zip"    # 保存状態 (層・サイズ・期限)
```

`/history` の応答には、上の標本 (`keys` / `samples`) とは**別に** `canary` の配列が付きます
(`res=5` と `res=60` のときだけ中身が入ります。T14.10)。

`/dashboard` はブラウザで開くコントロールパネルです (依存なしの 1 ページ。2 秒ごとに `/status`、5 秒ごとに `/history`、
**30 秒ごとに `/status?sort=errors` と `?sort=dns`** を取って描きます。並べ替えた 2 本だけ間隔を空けているのは、
2 秒ごとに引くとプロキシ側で 50 ホストぶんの組み立てが 3 倍になるためです)。`/history` は 5 秒間隔・直近 1 時間分の JSON です。**標本は配列の配列**で、列名は先頭の `keys` に
1 回だけ出ます (項目が 32 に増えたため)。要求数・転送量・命中/ミス・メモリ/ディスク使用量・RSS・
**上限に当たって暇なトンネルを追い出した回数** (`evicted_idle`) は累計 (ブラウザ側で
差分を取ってレートにする)、応答時間の分布 (`connect_*` / `forward_*` の件数・合計 ms・最大・12 段の区間) と
エラー (`errors` / `errors_by_cause`)・名前解決 (`dns_misses` / `dns_ms_sum`) は**その区間だけ**の値、
接続数・スレッド数・記述子数 (`active` / `threads` / `fds`) はゲージで、粗い解像度へ畳むときは平均と
**最大** (`active_max` / `threads_max` / `fds_max`) の両方を残します (平均に畳むと山が消えるため)。
`/history` の応答には**別の配列** `"kernel":{"keys":[...],"samples":[[...]]}` が付きます
(上の「カーネルと cgroup の窓」。`res=3600` は `null` = この解像度の窓は持っていません)。ブラウザの HTTP プロキシにこのプロキシを設定した状態で `http://ホスト:ポート/dashboard` を開くと要求は
絶対形式で届きますが、ポートが自分の待ち受けポートなら自分宛てとして応答します (自分へ転送してループしません)。

**自分宛てかどうかはポートだけで決めます**: 絶対形式 (`GET http://host:PORT/status`) は URL の、
**オリジン形式 (`GET /status` + `Host:`) は `Host` のポート (無ければ 80) が自分の待ち受けポートと同じときだけ**
自分宛てです。自分宛てで知らないパスは 404、`/` は 200 でエンドポイントの一覧を返します
(ブラウザでプロキシの URL を直接開いた人への案内。`--lite` でも出ますが、持っていない `/dashboard` は載せません)。
`Host` のポートが違うオリジン形式は今までどおり `Host` 宛てに転送します (透過プロキシの使い方を壊さないため)。
`Host` の無い HTTP/1.0 のオリジン形式は今までどおり 400 です。
以前はオリジン形式を無条件に自分宛て扱いにしていたため、知らないパスが `Host` 宛ての転送に落ち、
**`Host` が自分自身だと自分へ `max_conns` 本つないでループしていました** (手元の再現: 1 要求で 33 本)。
保険として、転送する要求には `Via: 1.1 rust-http-proxy/<起動ごとの 8 桁 16 進>` を付け、
自分の印が付いた要求を受けたら `508 Loop Detected` で閉じます。

これらのパスはプロキシ自身が応答し、同じポート宛てのオリジン形式の要求より優先します。認証は無いので、
到達できる人は誰でも purge できます (公開ポートで動かすなら到達制御を)。
**`PROXY_ENDPOINTS_READONLY=on`** にすると、書き換える口 (`/purge` / `PURGE` / `/blocklist?action=`) だけを
405 で断ります (読む口はそのまま)。**公開ポートで見知らぬ接続元が増えたら `PROXY_ALLOW_CLIENTS` で絞れます**
(一覧に無い相手は accept 直後に閉じるので、内部エンドポイントにも届きません)。
**`PROXY_MAX_CONNS_PER_CLIENT=N`** は、1 つの接続元が同時に開ける本数を N 本に抑えます (超えた接続は 503。
自分宛ては上限の外で受けるので監視は取れます)。どれも**認証ではありません** — このプロキシに `Proxy-Authorization` は
無く、入れる予定もありません。経路を絞る (`PROXY_ALLOW_CLIENTS`) か、消せる口を閉じる (`PROXY_ENDPOINTS_READONLY`) か、
1 人の取り分を抑える (`PROXY_MAX_CONNS_PER_CLIENT`) かの 3 つだけです。

ホスト別統計には応答時間 (平均・p50・p95・最大 ms、CONNECT は接続確立までの時間) も入り、`/metrics` では
`sorahost_host_request_duration_seconds` ヒストグラムとして出ます。ダッシュボードのホスト表は要求数・遅い順 (p95)・
エラー率・**名前解決 (合計)**・**確立の平均**・転送量で並べ替えられ、「名前解決 / 接続 (ms)」と「v4 / v6」の列で
**待ちの内訳**が読めます (この並べ替えは受け取った 50 件の中での並び替えで、**どの 50 件を切り出すか**は
`/status?sort=` の側です)。

ダッシュボードの図は 9 枚で、上段の KPI に「**CONNECT 確立 p50 (直近 5 分)**」と「**名前解決**」
(ミス率 = `misses ÷ (hits + misses)`、ミス 1 回の値段 `miss_avg_ms`、あれば先回りの回数) が出ます
(区間ごとのヒストグラムを直近 60 標本ぶん足し合わせて分位点を出したもの)。「接続中」のカードには
`上限 240 · 山 218 (直近 5 分) · 503 0` の行が出ます (山は `/history` の直近 5 分の `active_max` の最大。
上限の 80% で黄、95% で赤)。その下の「**悪いホスト (上位 10)**」の表は `/status?sort=errors` と `?sort=dns` の
2 枚を混ぜたもので、エラー数・原因の内訳・名前解決の平均 (`dns_ms_sum ÷ dns_misses`) と合計・確立の平均
(`connect_ms_sum ÷ timed`) が読めます (エラーも名前解決も無いホストは出しません)。
ヘッダーには動いている版と、**どちらの窓か** (起動から / 通算) が出ます。
外部ライブラリは 1 つも読み込まない 1 ページのままです。ブラウザの無い環境では
`node scripts/check-dashboard.js [/history の出力] [/status の出力]` が「JS の構文」と
「`/history` の配列の配列・`/status` の読み方が実出力と合っていること」を確かめます
(Node があるときだけの補助的な確認。引数を省くと `scripts/testdata/` の見本を読みます)。

`/status` のトップレベルの `version` には動いているバイナリの版が出ます (`-V` と起動ログと同じ文字列)。

**`/status` は 2 つの窓が混ざっている**ので、どちらの窓かが分かるように目印を出します:
`since_start_secs` から下 (`total_requests` / `bytes_forwarded` / `cache_hits` …) は**この起動から**、
`hosts[]` と `clients[]` は状態ファイルに残る**通算**で、`restored_since` がその通算の始まり
(最も古い `last_seen` の epoch 秒。`0` なら表が空) です。あわせてプロセス全体の数え物
`threads` (`/proc/self/status` の `Threads`。接続スレッドだけを数える `live_threads` とは別で、
監視・履歴・接続試行のスレッドも入ります) と `fds` / `max_fds` (`/proc/self/fd` の数と `RLIMIT_NOFILE`) も出ます。
`/proc` を読むのは **`/status` と `/metrics` に来たときと 5 秒ごとの履歴の標本のときだけ**です。

`/status` には上限といまの混み具合も出ます: `max_conns` / `max_threads` (`auto` で決まった値。`0` は無制限)、
`live_threads` (生きている接続スレッド) / `idle_threads` (そのうち仕事待ち) / `queued_jobs` (上限に当たって
待たせている仕事。捨てていません)。`auto` が何を選んだかは起動ログを見なくてもここで分かります。
同じ 5 つは `/metrics` にも gauge で出ます (`sorahost_max_connections` / `sorahost_max_threads` /
`sorahost_threads{state="live"|"idle"}` / `sorahost_queued_jobs`)。
**接続スレッドの 2 つはラベル付きの 1 系列にそろえました**。旧名 `sorahost_live_threads` /
`sorahost_idle_threads` はこの版だけ両方出るので、監視側は次の版までに移してください。
`/metrics` にはこのほか `sorahost_connect_seconds`(`_bucket{le=}` / `_sum` / `_count`。CONNECT 確立の
ヒストグラム。区間は `/history` と同じ 12 段)、`sorahost_dns_seconds_sum` / `_count` (名前解決のミスに
かかった時間)、`sorahost_errors_total{cause="dns|refused|unreachable|timeout|reset|tls|loop|other"}`、
`sorahost_fds` / `sorahost_max_fds` / `sorahost_process_threads`、
**`sorahost_rtt_seconds_sum` / `_count`** (`{side="client"|"origin"}`。カーネルの平滑化 RTT。
標本は接続 1 本の終わりに 1 つで、ホスト別は出しません) が出ます。
ホスト別の応答時間ヒストグラム (`sorahost_host_request_duration_seconds`) は区間が 24 段になったので
**上位 50 ホストまで**です (数え上げの系列はこれまでどおり上位 100 ホスト)。ダッシュボードの「接続中」にも
`スレッド 7 / 256 (空き 3) · 接続上限 1008` の 1 行が出ます。数えるには接続スレッドで共有している鍵が要るので、
**`/status` と `/metrics` に来たときだけ**数えます (要求ごとの仕事は増えません)。
`cache` の `revalidations_dropped` は、上限に当たって捨てた裏側の再検証の数です
(こちらは「後でやればいい仕事」なので待たせません)。`cache` の `not_stored_rotations` は
「保存されないと分かった URL」の記憶 (ブルームフィルタ) を入れ替えた回数です。

`/status` の `hosts` にはホスト (`scheme://host:port`、CONNECT は `connect://host:port`) ごとの要求数・ヒット・ミス・
バイパス・エラー・バイト数が要求数順に最大 50 件入ります (1000 ホストを超えた分は `other` にまとめます)。
**`?sort=requests|errors|dns|slow` でこの 50 件の切り出し方を変えられます** (既定は `requests` = 今までどおりの要求数順。
`errors` はエラー件数、`dns` は名前解決に費やした合計 `dns_ms_sum`、`slow` は応答 (CONNECT は確立) の平均 `avg_ms` の
多い / 遅い順。**知らない値は既定に倒します**)。JSON の形も件数も変わらず、変わるのは `hosts[]` の並びだけです
(`clients[]` は要求数順のまま、`/healthz` は問い合わせを読みません)。要求数の上位 50 には「悪いホスト」が出てこない
のが動機で、デプロイ先の実測 (58.6 時間) では**エラー 99 件のうち 80 件 (名前解決の失敗) を抱えたホストが
1 つも上位 50 に居ませんでした**。`curl "http://127.0.0.1:8080/status?sort=errors"` で見られます。
ホスト別の行にはさらに**待ちの内訳**が入ります: `dns_ms_sum` / `dns_misses` (名前解決を OS に聞いた合計時間と回数)、
`connect_ms_sum` (接続にかかった合計時間。名前解決のぶんは含みません)、`v4_wins` / `v6_wins` (確立した族)、
`errors_by_cause` (`[dns, refused, unreachable, timeout, reset, tls, loop, other]` の順の件数。
`loop` は自分の `Via` が付いて `508` で閉じたもの)、**`rtt_ms`** (`{"avg":…,"min":…,"samples":N}`。
カーネルの平滑化 RTT (`TCP_INFO`)。標本は**接続 1 本の終わりに 1 つ**なので `timed` (要求数) とは数が合いません。
1 本も閉じていなければ `null`) と **`retrans`** (その接続たちが再送したセグメントの通算)。
**測るための費用は熱い経路に乗せていません**:
名前解決の時計はキャッシュを外したときだけ読み、内訳はホスト別統計が既に取っている鍵の内側で足します
(原子操作もシステムコールも増えません。実測: forward の確保 8.03 → 8.03 回/要求、
`--lite` のシステムコール 5.00 → 5.00 回/要求)。`dns` には `miss_ms_sum` / `miss_avg_ms` (ミス 1 回の値段) と
`refreshes` (期限前に裏で引き直した回数) / `negative_ttl_secs` (失敗を覚えておく秒数) /
`warm_secs` (keep-warm の窓) / **`warm` (いま warm な名前の数)** が出ます。
**裏の引き直しはミスに数えません** (利用者は待っていないので、`misses` と `miss_avg_ms` に混ぜると
「ミス 1 回の値段」が読めなくなる)。`/metrics` では `sorahost_dns_lookups_total{result="refresh"}` です。
`clients[]` にも同じ `rtt_ms` / `retrans` が出ますが、**こちらはクライアント側** (利用者 → プロキシの往復) です。
`/status` の `clients[]` には接続元 IP ごとの要求数・転送量・拒否数・応答時間が要求数順に最大 50 件入り、
末尾に **`first_seen`** (初めて見た時刻。`0` = この起動より前から居る) / **`agent`** (最後に見た `User-Agent` 1 つ。
無ければ `null`) / **`distinct_targets`** (宛先ホストの種類) / **`literal_targets`** (IP リテラル宛ての要求数) が付きます
(既存の欄の順は変えていません)。`User-Agent` を読むのは**接続の最初の要求だけ**で、2 要求目からは旗で飛ばすので
要求ごとの費用は 0 です (`--lite` では読みません)。全接続元と残りの欄は `/clients` で見られます。
`/metrics` も同じ内容を `sorahost_*` 系列で出します。`origin_connections` にオリジンへの新規接続数と再利用回数、`cache` には各層の `used_bytes` / `limit_bytes` (現在の予算) / `reserved_bytes` (バラスト) /
`keep_free_bytes` (動的マージン) / `mode` (`auto` か `fixed`) と、`system` に直近の計測値 (メモリ総量と空き、
活性ページキャッシュ、cgroup 制限と使用量、PSI の有無、ディスク総量と空き、自プロセスの RSS) が入ります。
