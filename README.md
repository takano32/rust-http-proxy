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
- **ヘルスチェック & メトリクス**:
  - `/healthz`, `/status` エンドポイントによる JSON 稼働状況・統計取得
- **タイムアウト制御**:
  - `PROXY_TIMEOUT_SECS` による接続および読み書きタイムアウト制御

## 環境変数

| 環境変数名 | デフォルト値 | 説明 |
|---|---|---|
| `SERVER_PORT` | `8080` | プロキシが待受を行うポート番号 |
| `PROXY_ALLOW_HOSTS` | なし (全許可) | 接続許可ホストのカンマ区切りリスト (例: `*.example.com,api.github.com`) |
| `PROXY_DENY_HOSTS` | なし | 接続拒否ホストのカンマ区切りリスト (例: `bad.com,*.blocked.org`) |
| `PROXY_TIMEOUT_SECS` | `30` | 接続およびデータ転送タイムアウト（秒） |

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
```

## 動作確認 (curl)

```bash
# HTTP プロキシ経由のリクエスト
curl -x http://127.0.0.1:8080 http://example.com/ -I

# HTTPS (CONNECT) プロキシ経由のリクエスト
curl -x http://127.0.0.1:8080 https://example.com/ -I

# ヘルスチェック・メトリクス確認
curl http://127.0.0.1:8080/healthz
```
