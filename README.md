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
  (オリジンが `Cache-Control` を変えたら、次の周期から合流に戻る)
- **keep-alive と接続プール**: クライアント接続は HTTP/1.1 の持続接続 (アイドル 15 秒)、オリジンへの接続は
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
  - `Via` ヘッダーおよび `X-Forwarded-For` ヘッダーの付与・伝搬
- **ACL / ホストフィルタリング**:
  - `PROXY_ALLOW_HOSTS` / `PROXY_DENY_HOSTS` による許可・拒否リスト（ワイルドカード対応）と 403 Forbidden 制御
- **2 段キャッシュ (メモリ + ディスク) — 固まらない限界まで使う**:
  - 既定は **自動モード**: 「これだけは空けておく」安全マージンを毎秒の観測から動的に決め、
    残りをすべてキャッシュに充てる。他プロセスが資源を使えばその分だけ自動で縮退
  - **先行確保**: 予算の未使用分をバラスト (メモリは実際にページを確保、ディスクは `fallocate`) として先に押さえ、キャッシュが増えるにつれて置き換える
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
- **DNS キャッシュ**: 名前解決の結果を 60 秒保持し、CONNECT ごとの解決をなくす。解決失敗時は古い結果で凌ぐ
- **入場制御 & ネガティブキャッシュ**: 層が埋まったら 2 回目に見た URL だけ保存。404 / 410 は既定 60 秒だけ保持
- **ドメインのブロックリスト**: hosts 形式のファイルや URL (1 日 1 回自動更新) から読み、広告・トラッカーを CONNECT の段階で 403 にする
- **統計と履歴の永続化**: 固定サイズ (約 1 MiB) の状態ファイルに、履歴 3 解像度 (5 秒 × 1 時間、1 分 × 1 日、
  1 時間 × 30 日) を環状に、ホスト別・接続元別の上位 1000 を固定スロットに書く。ファイルは伸びず、再起動後も表とグラフが残る。
  停止シグナル (SIGTERM / SIGINT) では表を書き出してから終了する (3 秒以内、2 回目のシグナルで即終了)
- **接続元別の統計**: 接続元 IP ごとの要求数・転送量・拒否数・応答時間を `/status` `/metrics` とダッシュボードに出す
- **`/proxy.pac`**: ブラウザの自動設定スクリプト。自分自身・ローカル・`PROXY_PAC_DIRECT` のホストは DIRECT、
  それ以外はこのプロキシ経由 (落ちていれば DIRECT)。ブラウザに `http://<host>:<port>/proxy.pac` を設定するだけ
- **ヘルスチェック & メトリクス & 操作**:
  - `/dashboard` (ブラウザ用のコントロールパネル: 要求/転送レート・命中率・メモリ/ディスクのグラフ、ホスト別統計、
    URL の照会と削除、全消去)、`/healthz`, `/status`, `/history` (JSON)、`/metrics` (Prometheus 形式)
  - `PURGE <url>` / `/purge?url=<url>` / `/purge?all=1` でキャッシュを消す、`/lookup?url=<url>` でエントリの状態を見る
  - `/history?res=5|60|3600` で 1 時間 / 1 日 / 30 日の履歴、`/blocklist?host=<h>` でブロックリストの判定、
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
  -h, --help           使い方を出して終了 (終了コード 0)
  -V, --version        版を出して終了
```

`--port=3128` の形式も使えます。知らない引数は使い方を出して終了コード 2 になります。

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
| `PROXY_KEEPALIVE_SECS` | `15` | クライアント接続を次の要求まで待つアイドル時間 (秒)。`0` で 1 接続 1 要求 |
| `PROXY_ORIGIN_POOL` | `64` | オリジンへのアイドル接続をホストごとに保持する本数。`0` で再利用しない。少ないと同時要求数が多いときに張り直しが増える (実測: 8 本だと 64 並列で p99 が 40 ms 台、64 本なら 10 ms 前後) |
| `PROXY_PARK_IDLE` | `on` | アイドルな keep-alive 接続と、両方向とも暇な CONNECT トンネルをスレッドから外し、1 本の監視スレッド (epoll) に預ける。`off` で「1 接続 = 1 スレッドが専任」の動きに戻る。Linux 専用 (それ以外では自動的に無効)。実測: 暇な接続 2,000 本でスレッド 2,004 → 25 本、RSS 68.0 → 25.2 MB、暇なトンネル 5,000 本でスレッド 5,005 → 68 本、RSS 93.9 → 29.9 MB。忙しいときの CPU/要求とシステムコール数は変わらない |
| `PROXY_PARK_GRACE_MS` | `3` | 預ける前に同じスレッドで待ってみる時間 (ミリ秒。CONNECT トンネルは下限 100 ms)。続けて要求が来る忙しい接続に、預ける/戻すの往復 (`epoll_ctl` 2 回 + ワーカーの受け渡し、実測 18 µs/要求) を払わせないための猶予。読み取りタイムアウトをこの長さにして空振りを「暇だ」と解釈するので、システムコールは増えない。`0` なら猶予なしで即座に預ける |
| `PROXY_MALLOC_ARENAS` | `8` | malloc のアリーナ数の上限。`0` で glibc の既定 (コア数 × 8) のまま。接続ごとにスレッドが増えるため、既定のままだとアイドル接続を多く抱えたときに使われないアリーナが RSS に居座る (実測: 2,000 本のアイドル接続で 80.5 → 26.8 kB/接続)。代償は高並列でのロック競合 (実測: 64 並列で CPU/要求 +4%、8 並列では差なし) |
| `PROXY_ORIGIN_POOL_TOTAL` | `256` | アイドル接続の全ホスト合計の上限。多数のホストへ行くときに `ホスト数 × PROXY_ORIGIN_POOL` まで増えないようにする |
| `PROXY_DNS_TTL_SECS` | `60` | 名前解決の結果を保持する秒数。`0` で毎回解決。解決に失敗したら 1 時間以内の古い結果を使い、失敗自体も 5 秒覚える。`.env` で即時反映 |
| `PROXY_BLOCKLIST_FILE` | なし | ドメインのブロックリスト (hosts 形式 `0.0.0.0 host` または 1 行 1 ドメイン)。親ドメインの登録で子ドメインも落ちる。`.env` で即時反映、ファイルの更新は 1 分以内に反映 |
| `PROXY_BLOCKLIST_URL` | なし | ブロックリストを取りに行く URL (StevenBlack の hosts など)。`$HOME/.rust-http-proxy.blocklist` に保存して再起動後も使う。ファイルと両方あれば和集合 |
| `PROXY_BLOCKLIST_REFRESH_SECS` | `86400` | URL を取り直す間隔 (最小 60)。失敗したら 10 分後に再試行し、その間は前の一覧を使う |
| `PROXY_BLOCKLIST_EXEMPT` | なし | ブロックリストの対象外にするホストのカンマ区切り (`*.example.com` 可) |
| `PROXY_CONNECT_PORTS` | なし (制限なし) | `CONNECT` を許すあて先ポート。`443,80,8080-8099` のようにカンマ区切り (範囲可)。ここに無いポートは 403。`.env` で即時反映 |
| `PROXY_ALLOW_LOCAL` | `off` | ループバック (`127.0.0.0/8`, `::1`) とリンクローカル (`169.254.0.0/16`, `fe80::/10`) 宛てのオリジンを許すか。既定では 403 にしてクラウドのメタデータ (`169.254.169.254`) 経由の SSRF を防ぐ。ローカルのサービスへプロキシしたいときだけ `on`。`.env` で即時反映 |
| `PROXY_TUNNEL_IDLE_SECS` | `300` | CONNECT トンネルのアイドル打ち切り。双方向とも無通信がこれだけ続いたら両側を閉じる (`PROXY_PARK_IDLE=on` なら、預かり所が期限を見て引き上げる)。`0` で無期限。`.env` で即時反映 |
| `PROXY_PROFILE` | なし | `lite` で最速の素通しプロファイル (`--lite` と同じ)。キャッシュ・統計の永続化・ブロックリストを止め、ログを `warn` にする |
| `PROXY_MAX_CONNS` | `auto` | 同時に受ける接続数の上限。超えた接続にはスレッドを起こさず `503 Service Unavailable` + `Retry-After: 1` を返して閉じる。`auto` は記述子の上限から `min(4096, (RLIMIT_NOFILE の soft − 予備 64) ÷ 4)` (1 接続が最悪で使う記述子は クライアント 1 + オリジン 1 + 素通しのパイプ 2 = 4 本。`ulimit -n` が 1024 の環境なら 240、4096 なら 1008)。記述子が余っていても 4096 で頭打ちにするのは、上限が fd 以外の資源 (スレッド・RSS) の歯止めでもあるため (同時 5,000 本で RSS 198 MiB の実測)。数値を書けばその値、`0` で無制限。決まった値は起動ログの `max connections:` と `/status` の `max_conns` (`/metrics` は `sorahost_max_connections`) に出る。`.env` で即時反映。断った数は `/status` の `rejected_overload` と `/metrics` の `rejected_overload_total` |
| `PROXY_MAX_THREADS` | `auto` | 同時に生きていてよい接続スレッドの上限。上限に達したら**新しいスレッドを起こさず、その仕事を待たせる** (捨てない。空いたスレッドが順に引き取る)。`auto` は `min(PROXY_MAX_CONNS, コア数 × 64 を 128〜512 に収めた値)` で、コア数は `taskset` で絞られていればその数。数値を書けばその値、`0` で無制限 (T10.5 以前の動き)。上限があるのは、預けた接続が一斉に切れたときにスレッドが跳ねないようにするため (暇なトンネル 5,000 本の一斉 close で、上限なしだと一時的に 4,400〜4,700 スレッド・RSS 65 MB、上限 256 なら 260 スレッド・RSS 27 MB)。`.env` で即時反映 (次に受ける接続から効く。**下げても走っているスレッドは殺さず**、仕事を終えたスレッドから順に減ります。`auto` のときは `PROXY_MAX_CONNS` を変えるとこちらも決め直します)。決まった値は起動ログの `max connection threads:` と `/status` の `max_threads` に出る (いまの本数は `/status` の `live_threads` / `idle_threads`、上限に当たって待たせている仕事は `queued_jobs`。`/metrics` にも `sorahost_max_threads` / `sorahost_live_threads` / `sorahost_idle_threads` / `sorahost_queued_jobs` として出る)。**裏側の再検証 (stale-while-revalidate) もこの上限の内側で走ります**が、こちらは待たせず捨てます (`/status` の `revalidations_dropped`) |
| `PROXY_STATS_PERSIST` | `on` | 統計と履歴を `$HOME/.rust-http-proxy.rrd` (固定 約 1 MiB) に残し、再起動後に読み戻す。`off` で無効 (履歴の収集スレッドも起動しないので `/history` とダッシュボードのグラフは空になる) |
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
| `PROXY_CACHE_RESERVE` | `true` | 予算の未使用分を先行確保 (バラスト) するか。`0` / `false` / `off` / `no` で上限管理のみにする |
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
`PROXY_KEEPALIVE_SECS` / `PROXY_LOG_LEVEL` / `PROXY_MAX_CONNS` / `PROXY_MAX_THREADS` などで、
既存の keep-alive 接続には次の接続から効きます (どの値を当てたかは `/status` の `settings.applied` に出ます)。ポート・bind・TLS・
オリジンプール・キャッシュ予算 (`SERVER_MEMORY` / `SERVER_DISK` / `PROXY_CACHE_*`) は起動時に固定なので、変更を検知すると
`/status` の `settings.restart_required` と `/dashboard` の帯に「再起動が必要」と出ます。解釈できない値を書いた場合は
前の設定を維持し、`settings.error` にメッセージが入ります。

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

`PROXY_CACHE_RESERVE` が有効 (既定) なら、予算のうちエントリで埋まっていない分を先に確保します。

- メモリ: 64 MiB 単位の領域を 0 以外で埋めて全ページをコミット。エントリの追加時にその分だけ解放
- ディスク: `ballast.reserve` ファイルを `fallocate` で伸ばす。エントリの書き込み前に必要分だけ縮める
- 予算が縮んだときはまずバラストを返し、それでも足りないときだけエントリを追い出す
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
| `proxy-base` | ロック、壁時計、JSON の組み立て、`.env` の読み取り、ログ、HTTP 日付、コマンドライン引数、ASCII だけを見る文字列の分割 |
| `proxy-rrd` | 固定長のリングバッファ (状態ファイルの保存形式) |
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
預けているあいだも同時接続数には数えるので `PROXY_MAX_CONNS` の意味は変わりません。
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
PROXY_CACHE_RESERVE=0 \
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

# ヘルスチェック・メトリクス確認 (キャッシュ統計・予算・マージン・先行確保量・システム使用量を含む)
curl http://127.0.0.1:8080/status
curl http://127.0.0.1:8080/metrics                      # Prometheus 形式

# キャッシュの操作・確認
curl -X PURGE -x http://127.0.0.1:8080 http://example.com/file.zip     # 1 URL (全バリアント) を消す
curl "http://127.0.0.1:8080/purge?url=http://example.com/file.zip"     # 同じことを GET で
curl "http://127.0.0.1:8080/purge?all=1"                               # 全消去
curl "http://127.0.0.1:8080/lookup?url=http://example.com/file.zip"    # 保存状態 (層・サイズ・期限)
```

`/dashboard` はブラウザで開くコントロールパネルです (依存なしの 1 ページ。2 秒ごとに `/status`、5 秒ごとに `/history` を
取って描きます)。`/history` は 5 秒間隔・直近 1 時間分の累計値 (要求数・転送量・接続数・命中/ミス・メモリ/ディスク使用量・
RSS) の JSON です。ブラウザの HTTP プロキシにこのプロキシを設定した状態で `http://ホスト:ポート/dashboard` を開くと要求は
絶対形式で届きますが、ポートが自分の待ち受けポートなら自分宛てとして応答します (自分へ転送してループしません)。

これらのパスはプロキシ自身が応答し、オリジン形式の要求 (`GET /status` + `Host:`) より優先します。認証は無いので、
到達できる人は誰でも purge できます (公開ポートで動かすなら到達制御を)。

ホスト別統計には応答時間 (平均・p50・p95・最大 ms、CONNECT は接続確立までの時間) も入り、`/metrics` では
`sorahost_host_request_duration_seconds` ヒストグラムとして出ます。ダッシュボードのホスト表は要求数・遅い順 (p95)・
エラー率・転送量で並べ替えられます。

`/status` には上限といまの混み具合も出ます: `max_conns` / `max_threads` (`auto` で決まった値。`0` は無制限)、
`live_threads` (生きている接続スレッド) / `idle_threads` (そのうち仕事待ち) / `queued_jobs` (上限に当たって
待たせている仕事。捨てていません)。`auto` が何を選んだかは起動ログを見なくてもここで分かります。
同じ 5 つは `/metrics` にも gauge で出ます (`sorahost_max_connections` / `sorahost_max_threads` /
`sorahost_live_threads` / `sorahost_idle_threads` / `sorahost_queued_jobs`)。ダッシュボードの「接続中」にも
`スレッド 7 / 256 (空き 3) · 接続上限 1008` の 1 行が出ます。数えるには接続スレッドで共有している鍵が要るので、
**`/status` と `/metrics` に来たときだけ**数えます (要求ごとの仕事は増えません)。
`cache` の `revalidations_dropped` は、上限に当たって捨てた裏側の再検証の数です
(こちらは「後でやればいい仕事」なので待たせません)。

`/status` の `hosts` にはホスト (`scheme://host:port`、CONNECT は `connect://host:port`) ごとの要求数・ヒット・ミス・
バイパス・エラー・バイト数が要求数順に最大 50 件入ります (1000 ホストを超えた分は `other` にまとめます)。
`/metrics` も同じ内容を `sorahost_*` 系列で出します。`origin_connections` にオリジンへの新規接続数と再利用回数、`cache` には各層の `used_bytes` / `limit_bytes` (現在の予算) / `reserved_bytes` (バラスト) /
`keep_free_bytes` (動的マージン) / `mode` (`auto` か `fixed`) と、`system` に直近の計測値 (メモリ総量と空き、
活性ページキャッシュ、cgroup 制限と使用量、PSI の有無、ディスク総量と空き、自プロセスの RSS) が入ります。
