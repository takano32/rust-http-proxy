# sorahost-http-proxy

`SERVER_PORT` 環境変数でポート番号を指定して起動する、依存クレートゼロ（Rust 標準ライブラリ `std` のみ使用）の軽量・認証不要な HTTP/HTTPS プロキシサーバーです。
Pterodactyl (Wings) のコンテナ内で動かすことを想定しています。

## 特徴

- **認証不要（No Auth）**: 事前設定なしで誰でも即座に利用可能
- **依存クレートゼロ**: 外部クレート依存がないため、ビルド負荷が最小限で高速にビルド可能
- **超軽量バイナリ**: リリースビルド時で約 500KB
- **HTTP / HTTPS (CONNECTトンネリング) 対応**
- **同時ミスの合流 (collapsed forwarding)**: 同じ URL を複数のクライアントが同時に要求しても、オリジンへ行くのは
  最初の 1 本だけ。残りはその保存完了を待ってキャッシュから受け取る (`cache=COALESCED`)。保存されなかった場合は
  各自で取りに行く
- **keep-alive と接続プール**: クライアント接続は HTTP/1.1 の持続接続 (アイドル 15 秒)、オリジンへの接続は
  ホストごとにプールして再利用 (既定 8 本、再利用前に生存確認)。本文は必ず解読してから自前で枠付けし直す
  (Content-Length / 再 chunk / close) ので、HTTP/1.0 のオリジンやクライアントが混ざっても正しく持続する
- **Range / HEAD**: キャッシュ済みの完全な表現から `206 Partial Content` (単一範囲、`If-Range` 対応、範囲外は `416`) や
  `HEAD` 応答を切り出す。未キャッシュの Range 要求はそのまま転送
- **HTTPS のオリジンもキャッシュ (CA 不要)**: クライアントが `GET /https/example.com/path` (プロキシをオリジンとして叩く)
  か `GET https://example.com/path` で頼めば、プロキシがシステムの OpenSSL で HTTPS 取得し、平文 HTTP で返して保存する。
  応答の `Location` は `/https/...` 形式に書き換えるのでリダイレクトもプロキシに留まる。CONNECT トンネルは従来どおり素通し
- **IPv4 / IPv6 デュアルスタック**: `[::]` と `0.0.0.0` の両方で待ち受け (IPv6 が無ければ IPv4 のみ)、
  オリジンへは A / AAAA を引いて IPv6 優先の Happy Eyeballs (RFC 8305) で速い方に接続、
  `http://[2001:db8::1]:8080/` などの IPv6 リテラルにも対応。`PROXY_IPV6=off` で IPv4 のみにできる
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
  1 時間 × 30 日) を環状に、ホスト別・接続元別の上位 1000 を固定スロットに書く。ファイルは伸びず、再起動後も表とグラフが残る
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

## 環境変数

| 環境変数名 | デフォルト値 | 説明 |
|---|---|---|
| `SERVER_PORT` | `8080` | プロキシが待受を行うポート番号 (Pterodactyl が自動設定) |
| `PROXY_BIND` | 自動 (`::` + `0.0.0.0`) | 待ち受けアドレスのカンマ区切りリスト (例: `127.0.0.1,[::1]`)。未設定ならデュアルスタックで自動 |
| `PROXY_IPV6` | `on` | IPv6 を使う (待ち受けと AAAA での接続)。`off` で `0.0.0.0` のみ・A レコードのみ |
| `SERVER_MEMORY` | なし | コンテナのメモリ割当 (MB)。Pterodactyl が自動設定し、メモリキャッシュの上限として尊重される |
| `PROXY_DISK_QUOTA_MB` (別名 `SERVER_DISK`) | なし | コンテナのディスク割当。**Pterodactyl はこれを渡してくれない**ので、egg 変数として設定する。MB 数 = パネルの Disk Space、`0` = 無制限、`auto` = `df -B1 /home/container` の total を割当とみなす (下記)。Pterodactyl で未設定ならディスクキャッシュは 512 MiB 固定・先行確保なし |
| `PROXY_ALLOW_HOSTS` | なし (全許可) | 接続許可ホストのカンマ区切りリスト (例: `*.example.com,api.github.com`) |
| `PROXY_DENY_HOSTS` | なし | 接続拒否ホストのカンマ区切りリスト (例: `bad.com,*.blocked.org`) |
| `PROXY_TIMEOUT_SECS` | `30` | 接続およびデータ転送タイムアウト（秒） |
| `PROXY_KEEPALIVE_SECS` | `15` | クライアント接続を次の要求まで待つアイドル時間 (秒)。`0` で 1 接続 1 要求 |
| `PROXY_ORIGIN_POOL` | `8` | オリジンへのアイドル接続をホストごとに保持する本数。`0` で再利用しない |
| `PROXY_DNS_TTL_SECS` | `60` | 名前解決の結果を保持する秒数。`0` で毎回解決。解決に失敗したら 1 時間以内の古い結果を使い、失敗自体も 5 秒覚える。`.env` で即時反映 |
| `PROXY_BLOCKLIST_FILE` | なし | ドメインのブロックリスト (hosts 形式 `0.0.0.0 host` または 1 行 1 ドメイン)。親ドメインの登録で子ドメインも落ちる。`.env` で即時反映、ファイルの更新は 1 分以内に反映 |
| `PROXY_BLOCKLIST_URL` | なし | ブロックリストを取りに行く URL (StevenBlack の hosts など)。`$HOME/.sorahost-http-proxy.blocklist` に保存して再起動後も使う。ファイルと両方あれば和集合 |
| `PROXY_BLOCKLIST_REFRESH_SECS` | `86400` | URL を取り直す間隔 (最小 60)。失敗したら 10 分後に再試行し、その間は前の一覧を使う |
| `PROXY_BLOCKLIST_EXEMPT` | なし | ブロックリストの対象外にするホストのカンマ区切り (`*.example.com` 可) |
| `PROXY_STATS_PERSIST` | `on` | 統計と履歴を `$HOME/.sorahost-http-proxy.rrd` (固定 約 1 MiB) に残し、再起動後に読み戻す。`off` で無効 |
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
`PROXY_KEEPALIVE_SECS` / `PROXY_LOG_LEVEL` で、既存の keep-alive 接続には次の接続から効きます。ポート・bind・TLS・
オリジンプール・キャッシュ予算 (`SERVER_MEMORY` / `SERVER_DISK` / `PROXY_CACHE_*`) は起動時に固定なので、変更を検知すると
`/status` の `settings.restart_required` と `/dashboard` の帯に「再起動が必要」と出ます。解釈できない値を書いた場合は
前の設定を維持し、`settings.error` にメッセージが入ります。

`PROXY_CACHE_DIR` を指定しない場合は、書き込める最初の候補を使います:
`$XDG_CACHE_HOME/sorahost-http-proxy` (または `~/.cache/sorahost-http-proxy`) → `/var/cache/sorahost-http-proxy` → `$TMPDIR/sorahost-http-proxy-cache`。
Pterodactyl 以外で root 実行の場合は `/var/cache` を優先します。`$TMPDIR` は tmpfs (RAM) のことが多いので最後の手段です。

## ログ

すべてのログはレベルによらず **標準出力 (stdout)** へ出力されます。既定の `info` レベルで
1 リクエストにつき 1 行のアクセスログが出ます。

```
2026-09-02T02:48:22.900Z INFO  [main] disk cache ready at /home/container/.cache/sorahost-http-proxy (12034 entries restored, 210 expired removed, 0 migrated, 0 stray files removed)
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
  再検証は同時に 1 本、全体で 32 本まで
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
    状態は `/home/container/.sorahost-http-proxy.state` に残ります。`PROXY_DISK_PROBE=off` で探索を止められます
    (その場合は 512 MiB 固定)。割当が分かっているなら `.env` に `SERVER_DISK` を書く方が早いです
- `SERVER_DISK=auto` を明示すると、`df -B1 /home/container` 相当 (statvfs) の total を割当として使います。
  これが正しいのは、ホストが XFS のプロジェクトクォータや ZFS データセットなどでサーバーごとに領域を切っていて、
  `df` の total がパネルの Disk Space と一致する場合だけです。単なる bind mount ではホストのディスク全体が
  見えるので、起動時に `df /` と比べて同じファイルシステムなら「不明」扱い (512 MiB 上限) に落として警告します。
  Pterodactyl では判断しやすいよう、起動ログに `df -B1 /home/container: total ... used ...` を常に出します
- 超過で止められてしまったら、ファイルマネージャか SFTP で `/home/container/.cache/sorahost-http-proxy/`
  (特に `ballast.reserve`) を削除すれば起動できます
- 既定のキャッシュ先は `/home/container/.cache/sorahost-http-proxy` (ボリューム内なので再起動後も残る)

## ビルド・テスト

```bash
# テスト実行
cargo test

# 通常ビルド
cargo build

# 最適化リリースビルド
cargo build --release
```

## 起動方法

```bash
# 基本起動 (メモリ・ディスクとも動的マージンだけ残して限界まで自動確保)
SERVER_PORT=8080 ./target/release/sorahost-http-proxy

# ACL・タイムアウト付きで起動
SERVER_PORT=8080 \
PROXY_ALLOW_HOSTS="*.example.com,*.github.com" \
PROXY_DENY_HOSTS="evil.example.com" \
PROXY_TIMEOUT_SECS=15 \
./target/release/sorahost-http-proxy

# 使用率を 80% / 90% で頭打ちにし、先行確保をやめて上限管理だけにする
SERVER_PORT=8080 \
PROXY_MEM_TARGET_PERCENT=80 \
PROXY_DISK_TARGET_PERCENT=90 \
PROXY_CACHE_RESERVE=0 \
./target/release/sorahost-http-proxy

# 従来どおり固定上限 (メモリ 200MB / ディスク 2GB) で起動
SERVER_PORT=8080 \
PROXY_LOG_LEVEL=debug \
PROXY_MEM_CACHE_MB=200 \
PROXY_DISK_CACHE_MB=2048 \
PROXY_CACHE_DIR=/var/cache/sorahost-http-proxy \
./target/release/sorahost-http-proxy

# Pterodactyl (SERVER_PORT / SERVER_MEMORY / P_SERVER_UUID はパネルが渡す。ディスク割当だけ egg 変数で)
PROXY_DISK_QUOTA_MB=51200 ./sorahost-http-proxy
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

`/status` の `hosts` にはホスト (`scheme://host:port`、CONNECT は `connect://host:port`) ごとの要求数・ヒット・ミス・
バイパス・エラー・バイト数が要求数順に最大 50 件入ります (1000 ホストを超えた分は `other` にまとめます)。
`/metrics` も同じ内容を `sorahost_*` 系列で出します。`origin_connections` にオリジンへの新規接続数と再利用回数、`cache` には各層の `used_bytes` / `limit_bytes` (現在の予算) / `reserved_bytes` (バラスト) /
`keep_free_bytes` (動的マージン) / `mode` (`auto` か `fixed`) と、`system` に直近の計測値 (メモリ総量と空き、
活性ページキャッシュ、cgroup 制限と使用量、PSI の有無、ディスク総量と空き、自プロセスの RSS) が入ります。
