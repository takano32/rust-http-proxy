# sorahost-http-proxy

`SERVER_PORT` 環境変数でポート番号を指定して起動する、依存クレートゼロ（Rust 標準ライブラリ `std` のみ使用）の軽量・認証不要な HTTP/HTTPS プロキシサーバーです。

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
- **2 段キャッシュ (メモリ 200MB + ディスク 2GB)**:
  - L1: メモリ LRU キャッシュ (既定 200MB)
  - L2: ディスク LRU キャッシュ (既定 2048MB = 2GB)、再起動後もインデックスを復元
  - `Cache-Control` / `s-maxage` / `max-age` / `no-store` / `private` / `Set-Cookie` を尊重した RFC 9111 準拠の簡易判定
  - ヒット時は `X-Cache` / `Age` ヘッダーを付与
- **詳細なアクセスログ**:
  - 既定 (INFO) で 1 リクエスト 1 行のアクセスログ (キャッシュ HIT/MISS 付き) を標準出力へ
  - `PROXY_LOG_LEVEL` で `error` / `warn` / `info` / `debug` / `trace` を切り替え
- **ヘルスチェック & メトリクス**:
  - `/healthz`, `/status` エンドポイントによる JSON 稼働状況・キャッシュ統計取得
- **タイムアウト制御**:
  - `PROXY_TIMEOUT_SECS` による接続および読み書きタイムアウト制御

## 環境変数

| 環境変数名 | デフォルト値 | 説明 |
|---|---|---|
| `SERVER_PORT` | `8080` | プロキシが待受を行うポート番号 |
| `PROXY_ALLOW_HOSTS` | なし (全許可) | 接続許可ホストのカンマ区切りリスト (例: `*.example.com,api.github.com`) |
| `PROXY_DENY_HOSTS` | なし | 接続拒否ホストのカンマ区切りリスト (例: `bad.com,*.blocked.org`) |
| `PROXY_TIMEOUT_SECS` | `30` | 接続およびデータ転送タイムアウト（秒） |
| `PROXY_LOG_LEVEL` | `info` | ログレベル (`error` / `warn` / `info` / `debug` / `trace`) |
| `PROXY_CACHE_ENABLED` | `true` | `0` / `false` / `off` / `no` でキャッシュを無効化 |
| `PROXY_MEM_CACHE_MB` | `200` | メモリキャッシュ上限（MiB） |
| `PROXY_DISK_CACHE_MB` | `2048` | ディスクキャッシュ上限（MiB, 既定 2GB） |
| `PROXY_CACHE_DIR` | `$TMPDIR/sorahost-http-proxy-cache` | ディスクキャッシュ格納先 |
| `PROXY_CACHE_TTL_SECS` | `300` | `Cache-Control` が無い場合の既定 TTL（秒） |
| `PROXY_CACHE_MAX_OBJECT_MB` | `32` | キャッシュする 1 オブジェクトの最大サイズ（MiB） |

## ログ

すべてのログはレベルによらず **標準出力 (stdout)** へ出力されます。既定の `info` レベルで
1 リクエストにつき 1 行のアクセスログが出ます。

```
2026-09-02T02:48:23.044Z INFO  [conn#1] ACCESS 127.0.0.1 "GET http://example.com/ HTTP/1.1" 200 204B 22.4ms cache=MISS stored ttl=300s
2026-09-02T02:48:23.087Z INFO  [conn#2] ACCESS 127.0.0.1 "GET http://example.com/ HTTP/1.1" 200 260B 0.4ms cache=HIT(memory) age=0s ttl_left=300s
2026-09-02T02:48:24.143Z INFO  [conn#3] ACCESS 127.0.0.1 "CONNECT example.com:443 HTTP/1.1" 200 7070B 1010.8ms cache=BYPASS(tunnel)
```

| レベル | 主な内容 |
|---|---|
| `error` | 起動失敗、致命的な接続エラー |
| `warn` | 403 (ACL)、502 (オリジン接続失敗)、キャッシュ I/O 失敗 |
| `info` | 起動時の設定サマリ、アクセスログ (キャッシュ HIT/MISS を含む) |
| `debug` | 接続の受付・切断、キャッシュ HIT/MISS/STORE/EVICT の詳細 |
| `trace` | リクエスト / 転送 / レスポンスの全ヘッダー、トンネル転送量 |

## キャッシュ

GET リクエストのみを対象に、レスポンスをワイヤ形式そのままで 2 段キャッシュへ格納します。

- L1 (メモリ, 既定 200MB) → L2 (ディスク, 既定 2GB) の順に探索し、L2 ヒットは L1 へ昇格
- どちらも LRU で上限を超えた分から追い出し
- ディスクキャッシュは起動時にディレクトリを走査してインデックスを復元し、期限切れは削除
- 次の場合はキャッシュしない: GET 以外 / `Authorization` 付き / リクエストまたはレスポンスの
  `no-store`・`no-cache`・`private` / `max-age=0` / `Set-Cookie` / `Vary: *` / 非キャッシュ対象ステータス /
  `PROXY_CACHE_MAX_OBJECT_MB` 超過

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
# 基本起動
SERVER_PORT=8080 ./target/release/sorahost-http-proxy

# ACL・タイムアウト付きで起動
SERVER_PORT=8080 \
PROXY_ALLOW_HOSTS="*.example.com,*.github.com" \
PROXY_DENY_HOSTS="evil.example.com" \
PROXY_TIMEOUT_SECS=15 \
./target/release/sorahost-http-proxy

# 詳細ログ + キャッシュ (メモリ 200MB / ディスク 2GB) を明示して起動
SERVER_PORT=8080 \
PROXY_LOG_LEVEL=debug \
PROXY_MEM_CACHE_MB=200 \
PROXY_DISK_CACHE_MB=2048 \
PROXY_CACHE_DIR=/var/cache/sorahost-http-proxy \
./target/release/sorahost-http-proxy
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

# ヘルスチェック・メトリクス確認 (キャッシュ統計を含む)
curl http://127.0.0.1:8080/healthz
```
