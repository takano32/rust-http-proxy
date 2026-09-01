# sorahost-http-proxy

`SERVER_PORT` 環境変数でポート番号を指定して起動する、依存クレートゼロ（Rust 標準ライブラリ `std` のみ使用）の軽量・認証不要な HTTP/HTTPS プロキシサーバーです。
Pterodactyl (Wings) のコンテナ内で動かすことを想定しています。

## 特徴

- **認証不要（No Auth）**: 事前設定なしで誰でも即座に利用可能
- **依存クレートゼロ**: 外部クレート依存がないため、ビルド負荷が最小限で高速にビルド可能
- **超軽量バイナリ**: リリースビルド時で約 300KB
- **HTTP / HTTPS (CONNECTトンネリング) 対応**
- **RFC 7230 / RFC 9110 準拠**:
  - Hop-by-hop ヘッダーの自動除去
  - `Via` ヘッダーおよび `X-Forwarded-For` ヘッダーの付与・伝搬
- **ACL / ホストフィルタリング**:
  - `PROXY_ALLOW_HOSTS` / `PROXY_DENY_HOSTS` による許可・拒否リスト（ワイルドカード対応）と 403 Forbidden 制御
- **2 段キャッシュ (メモリ + ディスク) — 使える資源を限界まで使う**:
  - 既定は **自動モード**: システム (またはコンテナ) の使用率が **90%** に達するまでメモリ・ディスクをキャッシュに充て、他プロセスが資源を使えばその分だけ自動で縮退
  - **先行確保**: 予算の未使用分をバラスト (メモリは実際にページを確保、ディスクは `fallocate`) として先に押さえ、キャッシュが増えるにつれて置き換える
  - **固まらない安全策**: cgroup 制限 / `SERVER_MEMORY` を上限として尊重、PSI (メモリ圧迫) を検知したら即座に返却、tmpfs 上のディレクトリは検知して警告
  - L1: メモリ LRU、L2: ディスク LRU (256 分割ディレクトリ、再起動後もインデックスを復元)
  - `Cache-Control` / `s-maxage` / `max-age` / `no-store` / `private` / `Set-Cookie` を尊重した RFC 9111 準拠の簡易判定
  - ヒット時は `X-Cache` / `Age` ヘッダーを付与
- **詳細なアクセスログ**:
  - 既定 (INFO) で 1 リクエスト 1 行のアクセスログ (キャッシュ HIT/MISS 付き) を標準出力へ
  - `PROXY_LOG_LEVEL` で `error` / `warn` / `info` / `debug` / `trace` を切り替え
- **ヘルスチェック & メトリクス**:
  - `/healthz`, `/status` エンドポイントによる JSON 稼働状況・キャッシュ統計・システム使用量取得
- **タイムアウト制御**:
  - `PROXY_TIMEOUT_SECS` による接続および読み書きタイムアウト制御

## 環境変数

| 環境変数名 | デフォルト値 | 説明 |
|---|---|---|
| `SERVER_PORT` | `8080` | プロキシが待受を行うポート番号 (Pterodactyl が自動設定) |
| `SERVER_MEMORY` | なし | コンテナのメモリ割当 (MB)。Pterodactyl が自動設定し、メモリキャッシュの上限として尊重される |
| `PROXY_ALLOW_HOSTS` | なし (全許可) | 接続許可ホストのカンマ区切りリスト (例: `*.example.com,api.github.com`) |
| `PROXY_DENY_HOSTS` | なし | 接続拒否ホストのカンマ区切りリスト (例: `bad.com,*.blocked.org`) |
| `PROXY_TIMEOUT_SECS` | `30` | 接続およびデータ転送タイムアウト（秒） |
| `PROXY_LOG_LEVEL` | `info` | ログレベル (`error` / `warn` / `info` / `debug` / `trace`) |
| `PROXY_CACHE_ENABLED` | `true` | `0` / `false` / `off` / `no` でキャッシュを無効化 |
| `PROXY_MEM_CACHE_MB` | `auto` | メモリキャッシュ上限。`auto` (使用率が目標に達するまで) か固定値 (MiB) |
| `PROXY_DISK_CACHE_MB` | `auto` | ディスクキャッシュ上限。`auto` か固定値 (MiB) |
| `PROXY_MEM_TARGET_PERCENT` | `90` | `auto` 時のメモリ使用率の目標 (1–100)。`PROXY_MEM_CACHE_MB=auto:85` の形でも指定可 |
| `PROXY_DISK_TARGET_PERCENT` | `90` | `auto` 時のディスク使用率の目標 (1–100) |
| `PROXY_CACHE_RESERVE` | `true` | 予算の未使用分を先行確保 (バラスト) するか。`0` / `false` / `off` / `no` で上限管理のみにする |
| `PROXY_CACHE_PROBE_SECS` | `2` | システム使用量を測り直して予算を更新する間隔 (秒)。`0` で起動時の 1 回だけ |
| `PROXY_DISK_QUOTA_MB` | なし | コンテナのディスク割当 (MB)。Pterodactyl のように「ディレクトリ合計サイズ」で制限される環境では **必ず設定する** |
| `PROXY_DISK_QUOTA_ROOT` | `$HOME` | `PROXY_DISK_QUOTA_MB` が適用されるディレクトリ (Pterodactyl では `/home/container`) |
| `PROXY_CACHE_DIR` | 自動選択 (後述) | ディスクキャッシュ格納先 |
| `PROXY_CACHE_TTL_SECS` | `300` | `Cache-Control` が無い場合の既定 TTL（秒） |
| `PROXY_CACHE_MAX_OBJECT_MB` | `32` | キャッシュする 1 オブジェクトの最大サイズ（MiB） |

`PROXY_CACHE_DIR` を指定しない場合は、書き込める最初の候補を使います:
`$XDG_CACHE_HOME/sorahost-http-proxy` (または `~/.cache/sorahost-http-proxy`) → `/var/cache/sorahost-http-proxy` → `$TMPDIR/sorahost-http-proxy-cache`。
Pterodactyl 以外で root 実行の場合は `/var/cache` を優先します。`$TMPDIR` は tmpfs (RAM) のことが多いので最後の手段です。

## ログ

すべてのログはレベルによらず **標準出力 (stdout)** へ出力されます。既定の `info` レベルで
1 リクエストにつき 1 行のアクセスログが出ます。

```
2026-09-02T02:48:22.900Z INFO  [main] disk cache ready at /home/container/.cache/sorahost-http-proxy (12034 entries restored, 210 expired removed, 0 migrated, 0 stray files removed)
2026-09-02T02:48:22.901Z INFO  [main] memory cache budget: 0 MiB -> 3482 MiB (system memory 36.2% used)
2026-09-02T02:48:22.901Z INFO  [main] disk cache budget: 0 MiB -> 87040 MiB (disk quota 5.0% used)
2026-09-02T02:48:24.310Z INFO  [main] reserved +3456 MiB memory / +85120 MiB disk (ballast now 3456 MiB / 85120 MiB)
2026-09-02T02:48:23.044Z INFO  [conn#1] ACCESS 127.0.0.1 "GET http://example.com/ HTTP/1.1" 200 204B 22.4ms cache=MISS stored ttl=300s
2026-09-02T02:48:23.087Z INFO  [conn#2] ACCESS 127.0.0.1 "GET http://example.com/ HTTP/1.1" 200 260B 0.4ms cache=HIT(memory) age=0s ttl_left=300s
2026-09-02T02:48:24.143Z INFO  [conn#3] ACCESS 127.0.0.1 "CONNECT example.com:443 HTTP/1.1" 200 7070B 1010.8ms cache=BYPASS(tunnel)
2026-09-02T03:10:01.502Z INFO  [main] memory pressure detected (PSI some=24.3% full=1.0%): releasing reservations
2026-09-02T03:10:01.502Z INFO  [main] memory cache budget: 3482 MiB -> 2790 MiB (system memory 89.7% used)
```

| レベル | 主な内容 |
|---|---|
| `error` | 起動失敗、致命的な接続エラー |
| `warn` | 403 (ACL)、502 (オリジン接続失敗)、キャッシュ I/O 失敗、資源が計測できない環境、設定の警告 |
| `info` | 起動時の設定サマリ、予算の大きな変化、まとまった先行確保、メモリ圧迫の検知、アクセスログ |
| `debug` | 接続の受付・切断、キャッシュ HIT/MISS/STORE/EVICT の詳細、予算の小さな変化 |
| `trace` | リクエスト / 転送 / レスポンスの全ヘッダー、トンネル転送量、L2 書き込み |

## キャッシュ

GET リクエストのみを対象に、レスポンスをワイヤ形式そのままで 2 段キャッシュへ格納します。

- L1 (メモリ) → L2 (ディスク) の順に探索し、L2 ヒットは L1 へ昇格
- どちらも LRU で上限を超えた分から追い出し (参照・追い出しともに O(log n))
- ディスクキャッシュは 256 分割ディレクトリに 1 エントリ 1 ファイルで置き、ファイルの mtime に有効期限を
  記録するので、起動時の走査はファイルを開かずに済む。期限切れは起動時と 30 秒ごとの掃除で削除
- 次の場合はキャッシュしない: GET 以外 / `Authorization` 付き / リクエストまたはレスポンスの
  `no-store`・`no-cache`・`private` / `max-age=0` / `Set-Cookie` / `Vary: *` / 非キャッシュ対象ステータス /
  `PROXY_CACHE_MAX_OBJECT_MB` 超過

### 自動モードの予算計算

`auto` では 2 秒ごと (`PROXY_CACHE_PROBE_SECS`) に使用量を測り直し、各層の上限 (予算) を次の式で更新します。

```
予算 = 自分が保持しているバイト数 (エントリ + バラスト) + (目標使用量 − 現在の使用量)
```

「現在の使用量」には自分の分も含まれるので、他プロセスが資源を使えばその分だけ予算が縮み、
バラスト → LRU 追い出しの順に手放します。逆に空けばまた育ちます。

- **メモリ**: `/proc/meminfo` の `MemTotal` / `MemAvailable` に加え、所属する cgroup (v1/v2) の制限と
  `SERVER_MEMORY` (Pterodactyl の割当) をそれぞれ同じ式で計算し、最も厳しい値を採用。
  cgroup の使用量は Docker / Wings と同じ「usage − inactive_file」で見るので、パネルの表示と一致します
- **ディスク**: `statvfs` で `df` と同じ流儀の使用率を見る。`PROXY_DISK_QUOTA_MB` があれば代わりに
  「割当 − 割当ディレクトリ内の自分以外のファイル」を分母にします (60 秒ごとに再計測)
- **PSI**: `/proc/pressure/memory` または cgroup の `memory.pressure` で `some avg10 >= 20%` か
  `full avg10 >= 5%` を観測したら、予算をさらに全体の 5% 分減らしてバラストを即時返却し、60 秒間は再確保しません
- 計測できない環境 (Linux 以外など) では固定値 (メモリ 200 MiB / ディスク 2048 MiB) にフォールバックして警告します

### 先行確保 (バラスト)

`PROXY_CACHE_RESERVE` が有効 (既定) なら、予算のうちエントリで埋まっていない分を先に確保します。

- メモリ: 64 MiB 単位の領域を 0 以外で埋めて全ページをコミット。エントリの追加時にその分だけ解放
- ディスク: `ballast.reserve` ファイルを `fallocate` で伸ばす。エントリの書き込み前に必要分だけ縮める
- 予算が縮んだときはまずバラストを返し、それでも足りないときだけエントリを追い出す
- tmpfs / ramfs 上のディレクトリでは「ディスク」の先行確保が RAM を食うだけなので無効化して警告
- バラストファイルは停止中も残りますが (シグナルでの終了時は消せない)、次回起動時に必ず空にしてから予算に合わせて作り直します

### Pterodactyl での注意

- メモリは cgroup と `SERVER_MEMORY` の両方で上限が決まるので追加設定は不要です
- ディスク割当は Wings が `/home/container` の合計サイズで強制し、超過すると次回起動できなくなります。
  `PROXY_DISK_QUOTA_MB` を egg 変数として用意し、サーバーのディスク割当 (MB) と同じ値にしてください。
  未設定のまま `auto` だとホスト全体を分母にせず 2048 MiB に抑えて警告します
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
# 基本起動 (メモリ・ディスクとも使用率 90% まで自動確保)
SERVER_PORT=8080 ./target/release/sorahost-http-proxy

# ACL・タイムアウト付きで起動
SERVER_PORT=8080 \
PROXY_ALLOW_HOSTS="*.example.com,*.github.com" \
PROXY_DENY_HOSTS="evil.example.com" \
PROXY_TIMEOUT_SECS=15 \
./target/release/sorahost-http-proxy

# 目標使用率を下げ、先行確保をやめて上限管理だけにする
SERVER_PORT=8080 \
PROXY_MEM_TARGET_PERCENT=80 \
PROXY_DISK_TARGET_PERCENT=70 \
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
PROXY_DISK_QUOTA_MB=10240 ./sorahost-http-proxy
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

# ヘルスチェック・メトリクス確認 (キャッシュ統計・予算・先行確保量・システム使用量を含む)
curl http://127.0.0.1:8080/status
```

`/status` の `cache` には各層の `used_bytes` / `limit_bytes` (現在の予算) / `reserved_bytes` (バラスト) /
`mode` (`auto` か `fixed`) と、`system` に直近の計測値 (メモリ総量と空き、cgroup 制限、PSI の有無、
ディスク総量と空き、自プロセスの RSS) が入ります。
